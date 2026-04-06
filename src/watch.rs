use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::Result;
use tracing::{debug, error, info, warn};
use crossbeam_channel::{select, unbounded, Sender};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};

use crate::{
    annotate::annotate_with_conn,
    config,
    ipc::{sock_path, WatchMsg, WatchResponse},
    lsp_client::LspClient,
};

/// Messages sent from the socket-handler threads to the main loop.
enum Inbox {
    FileChanged,
    Client(WatchMsg, Sender<WatchResponse>),
}

pub fn run(root: &str) -> Result<()> {
    let root = config::find_root(root).unwrap_or_else(|_| root.to_string());
    // Connections are opened per-discover run; watcher only needs them for annotate/rename lookup.
    let conn: Arc<Mutex<rusqlite::Connection>> = {
        let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
        let db_path = format!("{}/codegraph.db", graphlite_dir);
        Arc::new(Mutex::new(crate::schema::open_or_init_db(&db_path)?))
    };

    // LSP client is lazily initialized on the first rename request and kept
    // alive for subsequent calls (zero warm-up cost after first rename).
    let lsp: Arc<Mutex<Option<LspClient>>> = Arc::new(Mutex::new(None));

    let sock = sock_path(&root);
    // Remove stale socket if present.
    let _ = std::fs::remove_file(&sock);

    let (tx, rx) = unbounded::<Inbox>();

    // ── File watcher thread ─────────────────────────────────────────────────
    {
        let tx = tx.clone();
        let root_clone = root.clone();
        thread::spawn(move || {
            if let Err(e) = watch_files(&root_clone, tx) {
                error!(error = %e, "file watcher error");
            }
        });
    }

    // ── Unix socket listener thread ─────────────────────────────────────────
    {
        let tx = tx.clone();
        let sock_clone = sock.clone();
        thread::spawn(move || {
            if let Err(e) = serve_socket(&sock_clone, tx) {
                error!(error = %e, "socket server error");
            }
        });
    }

    // ── Ctrl-C handler ──────────────────────────────────────────────────────
    let sock_for_ctrlc = sock.clone();
    ctrlc::set_handler(move || {
        let _ = std::fs::remove_file(&sock_for_ctrlc);
        std::process::exit(0);
    })
    .ok();

    info!(root = %root, "watching");
    info!(socket = %sock.display(), "IPC socket ready");

    // ── Main loop (owns conn) ────────────────────────────────────────────────
    let mut pending_reindex = false;
    let debounce = Duration::from_millis(500);

    loop {
        // Use a timed receive to flush debounced reindex.
        let msg = if pending_reindex {
            select! {
                recv(rx) -> m => match m {
                    Ok(m) => Some(m),
                    Err(_) => break,
                },
                default(debounce) => None,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            }
        };

        match msg {
            None => {
                // Debounce timer fired — run full re-index (tree-sitter + rustdoc).
                pending_reindex = false;
                if let Err(e) = crate::discover::run(&root) {
                    error!(error = %e, "re-index failed");
                }
            }
            Some(Inbox::FileChanged) => {
                pending_reindex = true;
            }
            Some(Inbox::Client(msg, reply_tx)) => {
                let response = dispatch(&conn, &lsp, &root, msg);
                let _ = reply_tx.send(response);
            }
        }
    }

    let _ = std::fs::remove_file(&sock);
    Ok(())
}

fn dispatch(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    lsp: &Arc<Mutex<Option<LspClient>>>,
    root: &str,
    msg: WatchMsg,
) -> WatchResponse {
    match msg {
        WatchMsg::Ping => WatchResponse { ok: true, error: None, data: None },
        WatchMsg::Annotate {
            symbol,
            intent,
            behavior,
            tags,
            source,
            confidence,
        } => match conn.lock() {
            Ok(c) => {
                let result = annotate_with_conn(
                    &c,
                    &symbol,
                    intent.as_deref(),
                    behavior.as_deref(),
                    tags.as_deref(),
                    &source,
                    confidence,
                );
                match result {
                    Ok(()) => WatchResponse { ok: true, error: None, data: None },
                    Err(e) => WatchResponse { ok: false, error: Some(e.to_string()), data: None },
                }
            }
            Err(e) => WatchResponse { ok: false, error: Some(e.to_string()), data: None },
        },
        WatchMsg::Reindex { file: _ } => match crate::discover::run(root) {
            Ok(()) => WatchResponse { ok: true, error: None, data: None },
            Err(e) => WatchResponse { ok: false, error: Some(e.to_string()), data: None },
        },
        WatchMsg::Rename { symbol, new_name } => {
            dispatch_rename(conn, lsp, root, &symbol, &new_name)
        }
    }
}

fn dispatch_rename(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    lsp: &Arc<Mutex<Option<LspClient>>>,
    root: &str,
    symbol: &str,
    new_name: &str,
) -> WatchResponse {
    // 1. Look up symbol location from DB.
    let node = match conn.lock() {
        Ok(c) => resolve_node_for_rename(&c, symbol),
        Err(e) => return WatchResponse { ok: false, error: Some(e.to_string()), data: None },
    };
    let (file, range_start, name) = match node {
        Ok(n) => n,
        Err(e) => return WatchResponse { ok: false, error: Some(e.to_string()), data: None },
    };

    // 2. Get or lazily initialize the LSP client.
    let mut lsp_guard = match lsp.lock() {
        Ok(g) => g,
        Err(e) => return WatchResponse { ok: false, error: Some(e.to_string()), data: None },
    };
    if lsp_guard.is_none() {
        info!("warming rust-analyzer for first rename");
        match LspClient::connect(root) {
            Ok(client) => { *lsp_guard = Some(client); }
            Err(e) => {
                return WatchResponse {
                    ok: false,
                    error: Some(format!("failed to start rust-analyzer: {}", e)),
                    data: None,
                }
            }
        }
    }

    // 3. Perform the rename.
    let client = lsp_guard.as_mut().unwrap();
    match client.rename(&file, range_start, &name, new_name) {
        Ok(edit) => {
            match serde_json::to_string(&edit) {
                Ok(json) => WatchResponse { ok: true, error: None, data: Some(json) },
                Err(e) => WatchResponse { ok: false, error: Some(e.to_string()), data: None },
            }
        }
        Err(e) => {
            // Reset the client on error — it may be in an inconsistent state.
            *lsp_guard = None;
            WatchResponse { ok: false, error: Some(e.to_string()), data: None }
        }
    }
}

/// Look up `(file, range_start, name)` for a symbol selector from the DB.
/// Accepts: integer id, `sym:stable_id`, stable_id (contains "::"), or plain name.
fn resolve_node_for_rename(
    conn: &rusqlite::Connection,
    symbol: &str,
) -> Result<(String, u32, String), anyhow::Error> {
    let key = symbol.strip_prefix("sym:").unwrap_or(symbol);

    // Integer id
    if let Ok(id) = key.parse::<i64>() {
        return conn
            .query_row(
                "SELECT file, range_start, name FROM nodes WHERE id = ?1",
                rusqlite::params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| anyhow::anyhow!("node id {} not found", id));
    }

    // stable_id (contains "::")
    if key.contains("::") {
        if let Ok(row) = conn.query_row(
            "SELECT file, range_start, name FROM nodes WHERE stable_id = ?1 LIMIT 1",
            rusqlite::params![key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ) {
            return Ok(row);
        }
    }

    // Plain name — prefer orchestrator/infra roles, then take first match
    conn.query_row(
        "SELECT file, range_start, name FROM nodes
         WHERE name = ?1
         ORDER BY CASE role
             WHEN 'orchestrator' THEN 0
             WHEN 'infra'        THEN 1
             WHEN 'domain'       THEN 2
             ELSE 3
         END
         LIMIT 1",
        rusqlite::params![key],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .map_err(|_| anyhow::anyhow!("symbol '{}' not found in database", symbol))
}

fn watch_files(root: &str, tx: Sender<Inbox>) -> Result<()> {
    let (notify_tx, notify_rx) = std::sync::mpsc::channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(notify_tx, NotifyConfig::default())?;
    watcher.watch(Path::new(root), RecursiveMode::Recursive)?;

    for result in notify_rx {
        match result {
            Ok(event) => {
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(CreateKind::File)
                        | EventKind::Modify(ModifyKind::Data(_))
                        | EventKind::Remove(RemoveKind::File)
                );
                if relevant {
                    // Only care about source files, not .graphlite/ churn.
                    let is_source = event.paths.iter().any(|p| {
                        let in_graphlite = p.to_string_lossy().contains("/.graphlite/");
                        !in_graphlite && crate::language::detect_language(p).is_some()
                    });
                    if is_source {
                        let _ = tx.send(Inbox::FileChanged);
                    }
                }
            }
            Err(e) => warn!(error = %e, "notify error"),
        }
    }
    Ok(())
}

fn serve_socket(sock: &Path, tx: Sender<Inbox>) -> Result<()> {
    let listener = UnixListener::bind(sock)?;
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let tx = tx.clone();
                thread::spawn(move || handle_client(s, tx));
            }
            Err(e) => warn!(error = %e, "accept error"),
        }
    }
    Ok(())
}

fn handle_client(stream: UnixStream, tx: Sender<Inbox>) {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let msg: WatchMsg = match serde_json::from_str(line.trim()) {
        Ok(m) => m,
        Err(e) => {
            let resp = WatchResponse {
                ok: false,
                error: Some(format!("parse error: {}", e)),
                data: None,
            };
            let _ = write_response(stream, &resp);
            return;
        }
    };

    let (reply_tx, reply_rx) = unbounded::<WatchResponse>();
    if tx.send(Inbox::Client(msg, reply_tx)).is_err() {
        return;
    }
    // 120s: allows for rust-analyzer cold-start (~60s) on first rename.
    if let Ok(response) = reply_rx.recv_timeout(Duration::from_secs(120)) {
        let _ = write_response(stream, &response);
    }
}

fn write_response(mut stream: UnixStream, resp: &WatchResponse) -> Result<()> {
    let line = serde_json::to_string(resp)? + "\n";
    stream.write_all(line.as_bytes())?;
    Ok(())
}
