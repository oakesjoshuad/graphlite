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
    ipc::{sock_path, WatchMsg, WatchResponse},
};

/// Messages sent from the socket-handler threads to the main loop.
enum Inbox {
    FileChanged,
    Client(WatchMsg, Sender<WatchResponse>),
}

pub fn run(root: &str) -> Result<()> {
    let root = config::find_root(root).unwrap_or_else(|_| root.to_string());
    // Connections are opened per-discover run; watcher only needs them for annotate.
    let conn: Arc<Mutex<rusqlite::Connection>> = {
        let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
        let db_path = format!("{}/codegraph.db", graphlite_dir);
        Arc::new(Mutex::new(crate::schema::open_or_init_db(&db_path)?))
    };

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

    eprintln!("[watch] watching {}", root);
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
                // Debounce timer fired — run full re-index (tree-sitter + rustdoc).
                pending_reindex = false;
                if let Err(e) = crate::discover::run(&root) {
                    eprintln!("[watch] re-index error: {}", e);
                }
            }
            Some(Inbox::FileChanged) => {
                pending_reindex = true;
            }
            Some(Inbox::Client(msg, reply_tx)) => {
                let response = dispatch(&conn, &root, msg);
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
        WatchMsg::Reindex { file: _ } => match crate::discover::run(root) {
            Ok(()) => WatchResponse { ok: true, error: None },
            Err(e) => WatchResponse { ok: false, error: Some(e.to_string()) },
        },
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
