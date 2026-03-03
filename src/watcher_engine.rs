use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossbeam_channel::{unbounded, Receiver, Sender};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};

use crate::{discover, lsp, roles, schema::open_or_init_db};

/// Handle returned to the caller of `start`. Signals whenever the graph has
/// been updated (tree-sitter re-index + any available LSP enrichment done).
pub struct EngineHandle {
    pub changed_rx: Receiver<()>,
}

/// Start the watcher engine in a background thread.
///
/// * Warms LSP clients for each language in `lsp_langs` concurrently.
/// * Watches `root` for source-file changes.
/// * On debounced change: runs `run_fast` then incremental LSP enrichment for
///   whichever clients are ready; signals `changed_rx` after each cycle.
pub fn start(root: String, lsp_langs: Vec<String>) -> Result<EngineHandle> {
    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    let db_path = format!("{}/codegraph.db", graphlite_dir);

    let (changed_tx, changed_rx) = unbounded::<()>();
    let db_path_clone = db_path.clone();

    thread::spawn(move || {
        if let Err(e) = engine_loop(&root, &db_path_clone, &lsp_langs, changed_tx) {
            eprintln!("[engine] fatal error: {}", e);
        }
    });

    Ok(EngineHandle { changed_rx })
}

fn engine_loop(
    root: &str,
    db_path: &str,
    lsp_langs: &[String],
    changed_tx: Sender<()>,
) -> Result<()> {
    let conn = open_or_init_db(db_path)?;

    // Kick off LSP warm-up in background threads; collect results via channel.
    let (warm_tx, warm_rx) = unbounded::<(String, Option<lsp::LspClient>)>();
    for lang in lsp_langs {
        let tx = warm_tx.clone();
        let lang_c = lang.clone();
        let root_c = root.to_string();
        thread::spawn(move || {
            let client = lsp::warm_client(&lang_c, &root_c).ok();
            let _ = tx.send((lang_c, client));
        });
    }
    drop(warm_tx); // so warm_rx closes when all warm threads finish

    let (file_tx, file_rx) = unbounded::<()>();

    {
        let tx = file_tx;
        let root_c = root.to_string();
        thread::spawn(move || {
            if let Err(e) = watch_source_files(&root_c, tx) {
                eprintln!("[engine] file watcher error: {}", e);
            }
        });
    }

    let debounce = Duration::from_millis(500);
    let mut pending = false;
    // Clients keyed by language; None = not yet warmed (or crashed).
    let mut lsp_clients: HashMap<String, Option<lsp::LspClient>> =
        lsp_langs.iter().map(|l| (l.clone(), None)).collect();

    loop {
        // Collect any newly-warmed clients (non-blocking).
        while let Ok((lang, client)) = warm_rx.try_recv() {
            lsp_clients.insert(lang, client);
        }

        let got_another_event = if pending {
            crossbeam_channel::select! {
                recv(file_rx) -> r => match r {
                    Ok(()) => true,
                    Err(_) => return Ok(()),
                },
                default(debounce) => false,
            }
        } else {
            match file_rx.recv() {
                Ok(()) => {
                    pending = true;
                    continue;
                }
                Err(_) => return Ok(()),
            }
        };

        if got_another_event {
            // More events arrived before the debounce window expired; keep draining.
            continue;
        }

        // Debounce window expired — run the re-index cycle.
        pending = false;

        let changed = match discover::run_fast(root, &conn) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[engine] fast re-index error: {}", e);
                continue;
            }
        };

        if changed.is_empty() {
            continue;
        }

        // Collect any newly-warmed clients one more time before LSP work.
        while let Ok((lang, client)) = warm_rx.try_recv() {
            lsp_clients.insert(lang, client);
        }

        // Incremental LSP enrichment for each ready language.
        run_lsp_incremental(root, db_path, &mut lsp_clients, &changed);

        let _ = changed_tx.send(());
    }
}

/// For each language with a ready LSP client, enrich the changed files.
/// Clients that crash are set to None and will be re-warmed on next cycle.
fn run_lsp_incremental(
    root: &str,
    db_path: &str,
    lsp_clients: &mut HashMap<String, Option<lsp::LspClient>>,
    changed: &[PathBuf],
) {
    let langs: Vec<String> = lsp_clients.keys().cloned().collect();
    for lang in langs {
        let client = match lsp_clients.remove(&lang).flatten() {
            Some(c) => c,
            None => {
                lsp_clients.insert(lang, None);
                continue;
            }
        };

        let (edge_tx, edge_rx) = unbounded::<lsp::EdgeMsg>();
        let db = db_path.to_string();
        let r = root.to_string();
        let l = lang.clone();
        let files = changed.to_vec();

        let handle =
            thread::spawn(move || lsp::enrich_incremental(&db, &r, &l, client, &files, edge_tx));

        // Drain the edge channel while the enricher runs.
        let mut edges: Vec<(i64, i64, &'static str, &'static str)> = Vec::new();
        let mut sigs: Vec<(i64, String)> = Vec::new();
        while let Ok(msg) = edge_rx.recv() {
            match msg {
                lsp::EdgeMsg::Edge {
                    from_id,
                    to_id,
                    edge_type,
                    source,
                } => edges.push((from_id, to_id, edge_type, source)),
                lsp::EdgeMsg::SigUpdate { node_id, sig } => sigs.push((node_id, sig)),
            }
        }

        // Open a fresh write connection just for the commit (conn is owned by main loop).
        match rusqlite::Connection::open(db_path) {
            Ok(wconn) => {
                if let Err(e) = write_lsp_results(&wconn, &edges, &sigs) {
                    eprintln!("[engine] LSP[{}] write error: {}", lang, e);
                }
                // Re-run role inference after new trusted edges are written.
                if let Err(e) = roles::infer_roles(&wconn) {
                    eprintln!("[engine] role inference error: {}", e);
                }
            }
            Err(e) => eprintln!("[engine] db open error: {}", e),
        }

        match handle.join() {
            Ok(Ok((report, client))) => {
                eprintln!("[engine] {}", report);
                lsp_clients.insert(lang, Some(client));
            }
            Ok(Err(e)) => {
                eprintln!("[engine] LSP[{}] enrichment error: {}", lang, e);
                lsp_clients.insert(lang, None);
            }
            Err(_) => {
                eprintln!("[engine] LSP[{}] thread panicked", lang);
                lsp_clients.insert(lang, None);
            }
        }
    }
}

fn write_lsp_results(
    conn: &rusqlite::Connection,
    edges: &[(i64, i64, &'static str, &'static str)],
    sigs: &[(i64, String)],
) -> Result<()> {
    if edges.is_empty() && sigs.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO edges (from_id, to_id, edge_type, source, confidence)
             VALUES (?1, ?2, ?3, ?4, 1.0)",
        )?;
        for (from_id, to_id, edge_type, source) in edges {
            stmt.execute(rusqlite::params![from_id, to_id, edge_type, source])?;
        }
    }
    if !sigs.is_empty() {
        let mut stmt = tx.prepare_cached("UPDATE nodes SET signature = ?1 WHERE id = ?2")?;
        for (node_id, sig) in sigs {
            stmt.execute(rusqlite::params![sig, node_id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn watch_source_files(root: &str, tx: Sender<()>) -> Result<()> {
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
                    let is_source = event.paths.iter().any(|p| {
                        let in_graphlite = p.to_string_lossy().contains("/.graphlite/");
                        !in_graphlite && crate::language::detect_language(p).is_some()
                    });
                    if is_source {
                        let _ = tx.send(());
                    }
                }
            }
            Err(e) => eprintln!("[engine] notify error: {}", e),
        }
    }
    Ok(())
}
