use anyhow::Result;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::thread;
use walkdir::WalkDir;

use crate::{
    config,
    insert::{bulk_insert_edges, bulk_insert_symbols, upsert_file_hash},
    language::detect_language,
    lsp,
    parser::{parse_file, ParseResult, RawEdge, Symbol},
    roles,
    schema::open_or_init_db,
};

const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    ".svelte-kit",
    ".git",
    ".claude",
    ".cache",
    ".next",
    ".nuxt",
    "__pycache__",
];

fn is_ignored_with_extra(entry: &walkdir::DirEntry, extra: &[String]) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .map(|name| IGNORED_DIRS.contains(&name) || extra.iter().any(|e| e == name))
        .unwrap_or(false)
}

fn compute_file_hash(path: &Path) -> String {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    let mut hash: u64 = 14695981039346656037u64;
    for byte in &buf {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211u64);
    }
    format!("{:016x}", hash)
}

fn collect_source_files(root: &str, extra_ignore: &[String]) -> Result<Vec<PathBuf>> {
    let files = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored_with_extra(e, extra_ignore))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| detect_language(p).is_some())
        .collect();
    Ok(files)
}

/// Languages excluded from auto-detection on init.
/// TypeScript, JavaScript, and Svelte servers are slow to start and often add
/// little value over tree-sitter alone. Users opt-in by editing config.toml.
const LSP_AUTODETECT_EXCLUDE: &[&str] = &["typescript", "javascript", "svelte"];

fn detect_lsp_from_files(files: &[PathBuf]) -> Vec<String> {
    use std::collections::HashSet;
    // Collect the set of languages that have files in this project
    let present: HashSet<String> = files
        .iter()
        .filter_map(|p| detect_language(p))
        .filter_map(|lang| lang.lsp_server_cmd().map(|_| lang.as_str().to_string()))
        .filter(|lang_str| !LSP_AUTODETECT_EXCLUDE.contains(&lang_str.as_str()))
        .collect();
    // For each present language, check if server is in PATH
    present
        .into_iter()
        .filter(|lang_str| lsp::which_server_for_language(lang_str).is_some())
        .collect()
}

/// Detect languages in `root` and return their LSP language strings
/// if suitable servers are available in PATH. Used by `init` before the index
/// is built so it can write the result into config.toml.
pub(crate) fn detect_lsp(root: &str) -> Vec<String> {
    let files = collect_source_files(root, &[]).unwrap_or_default();
    detect_lsp_from_files(&files)
}

pub fn run(root: &str, lsp_lang: Option<&str>) -> Result<()> {
    let config = config::load(root);

    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    std::fs::create_dir_all(&graphlite_dir)?;

    let files = collect_source_files(root, &config.ignore)?;
    eprintln!("Found {} source files", files.len());

    let db_path = format!("{}/codegraph.db", graphlite_dir);
    let conn = open_or_init_db(&db_path)?;

    // Determine which files need re-indexing by comparing file hashes
    let changed: Vec<PathBuf> = files
        .iter()
        .filter(|path| {
            let path_str = path.to_string_lossy();
            let new_hash = compute_file_hash(path);
            let stored: Option<String> = conn
                .query_row(
                    "SELECT file_hash FROM files WHERE path = ?1",
                    rusqlite::params![path_str.as_ref()],
                    |r| r.get(0),
                )
                .ok();
            stored.as_deref() != Some(&new_hash)
        })
        .cloned()
        .collect();

    if changed.is_empty() {
        eprintln!("No files changed, index is up to date");
        return Ok(());
    }
    eprintln!("{} file(s) changed, re-indexing", changed.len());

    // Determine LSP languages now (files is available) and start warming in background
    // so LSP server indexes the workspace while tree-sitter parses.
    let effective_lsp: Vec<String> = if let Some(lang) = lsp_lang {
        vec![lang.to_string()]
    } else if !config.lsp.is_empty() {
        config.lsp.clone()
    } else {
        detect_lsp_from_files(&files)
    };
    let root_owned = root.to_string();
    let warm_handles: Vec<(String, thread::JoinHandle<Option<lsp::LspClient>>)> = effective_lsp
        .iter()
        .map(|lang| {
            let lang_clone = lang.clone();
            let root_clone = root_owned.clone();
            let h = thread::spawn(move || lsp::warm_client(&lang_clone, &root_clone).ok());
            (lang.clone(), h)
        })
        .collect();

    // Delete old edges and nodes for changed files, preserving annotations for re-linking.
    // Edges are deleted explicitly first because older db instances may lack ON DELETE CASCADE.
    let saved_annotations = save_annotations_for_files(&conn, &changed)?;
    for path in &changed {
        let path_str = path.to_string_lossy();
        conn.execute(
            "DELETE FROM edges WHERE from_id IN (SELECT id FROM nodes WHERE file = ?1)
                                  OR to_id   IN (SELECT id FROM nodes WHERE file = ?1)",
            rusqlite::params![path_str.as_ref()],
        )?;
        conn.execute(
            "DELETE FROM nodes WHERE file = ?1",
            rusqlite::params![path_str.as_ref()],
        )?;
    }

    // Parse changed files in parallel
    let results: Vec<(PathBuf, ParseResult)> = changed
        .par_iter()
        .filter_map(|path| {
            parse_file(path)
                .map_err(|e| {
                    eprintln!("Warning: {}: {}", path.display(), e);
                    e
                })
                .ok()
                .map(|r| (path.clone(), r))
        })
        .collect();

    let symbols: Vec<Symbol> = results
        .iter()
        .flat_map(|(_, r)| r.symbols.iter().cloned())
        .collect();
    let edges: Vec<RawEdge> = results
        .iter()
        .flat_map(|(_, r)| r.edges.iter().cloned())
        .collect();

    let name_to_id = bulk_insert_symbols(&conn, &symbols)?;
    bulk_insert_edges(&conn, &edges, &name_to_id)?;
    restore_annotations(&conn, &saved_annotations)?;

    // Build a lookup so each file's parsed doc can be passed to the upsert
    let file_docs: std::collections::HashMap<&PathBuf, Option<&str>> = results
        .iter()
        .map(|(p, r)| (p, r.file_doc.as_deref()))
        .collect();

    // Upsert file hashes for all changed files (including failed parses, which get doc=None)
    for path in &changed {
        let path_str = path.to_string_lossy();
        let hash = compute_file_hash(path);
        let doc = file_docs.get(path).copied().flatten();
        upsert_file_hash(&conn, path_str.as_ref(), &hash, doc)?;
    }

    eprintln!(
        "Inserted {} symbols, {} edges -> {}",
        symbols.len(),
        edges.len(),
        db_path
    );

    // Spawn one enricher thread per language; they run in parallel.
    // Edges are funneled through a channel to a single writer in the main thread.
    let (edge_tx, edge_rx) = crossbeam_channel::unbounded::<lsp::EdgeMsg>();

    let enricher_handles: Vec<(
        String,
        thread::JoinHandle<anyhow::Result<lsp::EnrichReport>>,
    )> = warm_handles
        .into_iter()
        .map(|(lang, warm_handle)| {
            let db_path_clone = db_path.clone();
            let root_clone = root_owned.clone();
            let tx_clone = edge_tx.clone();
            let lang_clone = lang.clone();
            let h = thread::spawn(move || {
                let pre_warmed = warm_handle.join().ok().flatten();
                lsp::enrich_parallel(
                    &db_path_clone,
                    &root_clone,
                    &lang_clone,
                    pre_warmed,
                    tx_clone,
                )
            });
            (lang, h)
        })
        .collect();

    // Drop our sender so the channel closes when all enricher threads finish.
    drop(edge_tx);

    // Drain the channel and write to the DB on the main thread (owns conn).
    {
        let mut edges: Vec<(i64, i64, &'static str, &'static str)> = Vec::new();
        let mut sigs: Vec<(i64, String)> = Vec::new();
        while let Ok(msg) = edge_rx.recv() {
            match msg {
                lsp::EdgeMsg::Edge {
                    from_id,
                    to_id,
                    edge_type,
                    source,
                } => {
                    edges.push((from_id, to_id, edge_type, source));
                }
                lsp::EdgeMsg::SigUpdate { node_id, sig } => {
                    sigs.push((node_id, sig));
                }
            }
        }
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO edges (from_id, to_id, edge_type, source, confidence)
                 VALUES (?1, ?2, ?3, ?4, 1.0)",
            )?;
            for (from_id, to_id, edge_type, source) in &edges {
                stmt.execute(rusqlite::params![from_id, to_id, edge_type, source])?;
            }
        }
        if !sigs.is_empty() {
            let mut stmt = tx.prepare_cached("UPDATE nodes SET signature = ?1 WHERE id = ?2")?;
            for (node_id, sig) in &sigs {
                stmt.execute(rusqlite::params![sig, node_id])?;
            }
        }
        tx.commit()?;
    }

    for (lang, handle) in enricher_handles {
        match handle.join() {
            Ok(Ok(report)) => eprintln!("{}", report),
            Ok(Err(e)) => eprintln!("LSP[{}]: enrichment failed: {}", lang, e),
            Err(_) => eprintln!("LSP[{}]: enricher thread panicked", lang),
        }
    }

    roles::infer_roles(&conn)?;

    Ok(())
}

// ─── Annotation preservation helpers ─────────────────────────────────────────

struct SavedAnnotation {
    stable_id: String,
    intent: Option<String>,
    behavior: Option<String>,
    tags: Option<String>,
    source: String,
    confidence: f64,
}

fn save_annotations_for_files(
    conn: &rusqlite::Connection,
    paths: &[PathBuf],
) -> Result<Vec<SavedAnnotation>> {
    let mut saved = Vec::new();
    for path in paths {
        let path_str = path.to_string_lossy();
        let mut stmt = conn.prepare_cached(
            "SELECT a.intent, a.behavior, a.tags, a.source, a.confidence, n.stable_id
             FROM annotations a JOIN nodes n ON a.node_id = n.id
             WHERE n.file = ?1 AND n.stable_id != ''",
        )?;
        let rows = stmt.query_map(rusqlite::params![path_str.as_ref()], |r| {
            Ok(SavedAnnotation {
                intent: r.get(0)?,
                behavior: r.get(1)?,
                tags: r.get(2)?,
                source: r.get(3)?,
                confidence: r.get(4)?,
                stable_id: r.get(5)?,
            })
        })?;
        for row in rows {
            saved.push(row?);
        }
    }
    Ok(saved)
}

fn restore_annotations(conn: &rusqlite::Connection, saved: &[SavedAnnotation]) -> Result<()> {
    if saved.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO annotations (node_id, intent, behavior, tags, source, confidence, content_hash_at_annotation)
         SELECT n.id, ?1, ?2, ?3, ?4, ?5, n.content_hash
         FROM nodes n WHERE n.stable_id = ?6
         ON CONFLICT(node_id) DO NOTHING",
    )?;
    for ann in saved {
        stmt.execute(rusqlite::params![
            ann.intent,
            ann.behavior,
            ann.tags,
            ann.source,
            ann.confidence,
            ann.stable_id,
        ])?;
    }
    Ok(())
}

// ─── Fast re-index (tree-sitter only, no LSP) ─────────────────────────────────

/// Re-index changed files using tree-sitter only (no LSP enrichment).
/// Returns the list of changed paths for callers that need to drive incremental LSP enrichment.
pub fn run_fast(root: &str, conn: &rusqlite::Connection) -> Result<Vec<PathBuf>> {
    let config = config::load(root);
    let files = collect_source_files(root, &config.ignore)?;

    let changed: Vec<PathBuf> = files
        .iter()
        .filter(|path| {
            let path_str = path.to_string_lossy();
            let new_hash = compute_file_hash(path);
            let stored: Option<String> = conn
                .query_row(
                    "SELECT file_hash FROM files WHERE path = ?1",
                    rusqlite::params![path_str.as_ref()],
                    |r| r.get(0),
                )
                .ok();
            stored.as_deref() != Some(&new_hash)
        })
        .cloned()
        .collect();

    if changed.is_empty() {
        return Ok(vec![]);
    }
    eprintln!(
        "[watch] {} file(s) changed, re-indexing (fast)",
        changed.len()
    );

    let saved_annotations = save_annotations_for_files(conn, &changed)?;

    for path in &changed {
        let path_str = path.to_string_lossy();
        conn.execute(
            "DELETE FROM edges WHERE from_id IN (SELECT id FROM nodes WHERE file = ?1)
                                  OR to_id   IN (SELECT id FROM nodes WHERE file = ?1)",
            rusqlite::params![path_str.as_ref()],
        )?;
        conn.execute(
            "DELETE FROM nodes WHERE file = ?1",
            rusqlite::params![path_str.as_ref()],
        )?;
    }

    let results: Vec<(PathBuf, crate::parser::ParseResult)> = changed
        .par_iter()
        .filter_map(|path| {
            crate::parser::parse_file(path)
                .map_err(|e| {
                    eprintln!("Warning: {}: {}", path.display(), e);
                    e
                })
                .ok()
                .map(|r| (path.clone(), r))
        })
        .collect();

    let symbols: Vec<crate::parser::Symbol> = results
        .iter()
        .flat_map(|(_, r)| r.symbols.iter().cloned())
        .collect();
    let edges: Vec<crate::parser::RawEdge> = results
        .iter()
        .flat_map(|(_, r)| r.edges.iter().cloned())
        .collect();

    let name_to_id = bulk_insert_symbols(conn, &symbols)?;
    bulk_insert_edges(conn, &edges, &name_to_id)?;
    restore_annotations(conn, &saved_annotations)?;

    let file_docs: std::collections::HashMap<&PathBuf, Option<&str>> = results
        .iter()
        .map(|(p, r)| (p, r.file_doc.as_deref()))
        .collect();

    for path in &changed {
        let path_str = path.to_string_lossy();
        let hash = compute_file_hash(path);
        let doc = file_docs.get(path).copied().flatten();
        upsert_file_hash(conn, path_str.as_ref(), &hash, doc)?;
    }

    roles::infer_roles(conn)?;
    eprintln!("[watch] re-index complete: {} symbols", symbols.len());
    Ok(changed)
}
