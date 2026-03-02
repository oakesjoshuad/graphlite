use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::Result;
use crossbeam_channel::{select, unbounded, Sender};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};

use crate::{
    annotate::annotate_with_conn,
    config,
    discover::run_fast,
    ipc::{sock_path, WatchMsg, WatchResponse},
    schema::open_or_init_db,
};

/// Messages sent from the socket-handler threads to the main loop.
enum Inbox {
    FileChanged,
    Client(WatchMsg, Sender<WatchResponse>),
}

pub fn run(root: &str, lsp: bool) -> Result<()> {
    let root = config::find_root(root).unwrap_or_else(|_| root.to_string());
    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    let db_path = format!("{}/codegraph.db", graphlite_dir);

    // Open (or create) the DB; the watcher owns this connection for all writes.
    let conn = open_or_init_db(&db_path)?;
    // Wrap in Arc<Mutex> so socket-handler threads can post work back; but
    // we keep all actual writes on the main thread via the Inbox channel instead.
    // (conn is !Send, so we use the channel pattern from Phase B.)
    let conn = Arc::new(Mutex::new(conn));

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
                eprintln!("[watch] file watcher error: {}", e);
            }
        });
    }

    // ── Unix socket listener thread ─────────────────────────────────────────
    {
        let tx = tx.clone();
        let sock_clone = sock.clone();
        thread::spawn(move || {
            if let Err(e) = serve_socket(&sock_clone, tx) {
                eprintln!("[watch] socket server error: {}", e);
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

    eprintln!("[watch] watching {} (lsp={})", root, lsp);
    eprintln!("[watch] socket: {}", sock.display());

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
                // Debounce timer fired — run the fast re-index.
                pending_reindex = false;
                if let Ok(c) = conn.lock() {
                    if let Err(e) = run_fast(&root, &c) {
                        eprintln!("[watch] re-index error: {}", e);
                    }
                }
            }
            Some(Inbox::FileChanged) => {
                pending_reindex = true;
            }
            Some(Inbox::Client(msg, reply_tx)) => {
                let response = dispatch(&conn, &root, msg, lsp);
                let _ = reply_tx.send(response);
            }
        }
    }

    let _ = std::fs::remove_file(&sock);
    Ok(())
}

fn dispatch(
    conn: &Arc<Mutex<rusqlite::Connection>>,
    root: &str,
    msg: WatchMsg,
    lsp: bool,
) -> WatchResponse {
    match msg {
        WatchMsg::Ping => WatchResponse {
            ok: true,
            error: None,
        },
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
                    Ok(()) => WatchResponse {
                        ok: true,
                        error: None,
                    },
                    Err(e) => WatchResponse {
                        ok: false,
                        error: Some(e.to_string()),
                    },
                }
            }
            Err(e) => WatchResponse {
                ok: false,
                error: Some(e.to_string()),
            },
        },
        WatchMsg::Reindex { file: _ } => {
            if lsp {
                // Full re-index with LSP: spawn discover::run in a thread and report async.
                // For simplicity, run synchronously here (watcher was started with --lsp).
                let result = crate::discover::run(root, None);
                match result {
                    Ok(()) => WatchResponse {
                        ok: true,
                        error: None,
                    },
                    Err(e) => WatchResponse {
                        ok: false,
                        error: Some(e.to_string()),
                    },
                }
            } else {
                match conn.lock() {
                    Ok(c) => match run_fast(root, &c) {
                        Ok(()) => WatchResponse {
                            ok: true,
                            error: None,
                        },
                        Err(e) => WatchResponse {
                            ok: false,
                            error: Some(e.to_string()),
                        },
                    },
                    Err(e) => WatchResponse {
                        ok: false,
                        error: Some(e.to_string()),
                    },
                }
            }
        }
    }
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
            Err(e) => eprintln!("[watch] notify error: {}", e),
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
            Err(e) => eprintln!("[watch] accept error: {}", e),
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
            };
            let _ = write_response(stream, &resp);
            return;
        }
    };

    let (reply_tx, reply_rx) = unbounded::<WatchResponse>();
    if tx.send(Inbox::Client(msg, reply_tx)).is_err() {
        return;
    }
    if let Ok(response) = reply_rx.recv_timeout(Duration::from_secs(30)) {
        let _ = write_response(stream, &response);
    }
}

fn write_response(mut stream: UnixStream, resp: &WatchResponse) -> Result<()> {
    let line = serde_json::to_string(resp)? + "\n";
    stream.write_all(line.as_bytes())?;
    Ok(())
}
