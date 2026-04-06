use anyhow::Result;
use tracing::{info, warn};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::{
    audit,
    clippy_enricher,
    config,
    insert::{bulk_insert_symbols, upsert_file_hash},
    language::detect_language,
    parser::{parse_file, ParseResult, Symbol},
    resolver,
    roles,
    rustdoc_enricher,
    schema::open_or_init_db,
    workspace,
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

pub fn run(root: &str) -> Result<()> {
    let config = config::load(root);
    if !config.lsp.is_empty() {
        warn!(fields = ?config.lsp, "lsp config field is deprecated; LSP enrichment replaced by rustdoc (ignoring)");
    }

    // Refuse to index if any workspace layer is still unassigned.
    if let Some(ws) = &config.workspace {
        let mut unassigned: Vec<&str> = ws
            .layers
            .iter()
            .filter(|(_, v)| v.as_str() == "?")
            .map(|(k, _)| k.as_str())
            .collect();
        unassigned.sort_unstable();
        if !unassigned.is_empty() {
            anyhow::bail!(
                "workspace layers not assigned in .graphlite/config.toml: {}\n\
                 Edit [workspace.layers] and set each to one of: \
                 shared | domain | port | application | infra | adapter | composition\n\
                 Then re-run: graphlite discover .",
                unassigned.join(", ")
            );
        }
    }

    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    std::fs::create_dir_all(&graphlite_dir)?;

    let files = collect_source_files(root, &config.ignore)?;
    info!(count = files.len(), "source files found");

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
        info!("no files changed, index is up to date");
        // Workspace crate graph is always re-synced (cheap, ensures doc/layer metadata is current).
        sync_workspace_crate_graph(&conn, root)?;
        run_optional_enrichers(root, &conn);
        return Ok(());
    }
    info!(count = changed.len(), "files changed, re-indexing");

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
                    warn!(path = %path.display(), error = %e, "parse error");
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

    bulk_insert_symbols(&conn, &symbols)?;
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

    // Sync workspace crate nodes and crate-level dependency edges from cargo metadata.
    sync_workspace_crate_graph(&conn, root)?;

    info!(count = symbols.len(), db = %db_path, "symbols inserted");

    match rustdoc_enricher::enrich(root, &changed, &conn) {
        Ok(n) => info!(count = n, "rustdoc nodes enriched"),
        Err(e) => warn!(error = %e, "rustdoc enrichment skipped"),
    }

    match resolver::resolve(&conn) {
        Ok(n) => info!(count = n, "resolver edges written"),
        Err(e) => warn!(error = %e, "resolver warning"),
    }

    // Enrich node diagnostics / complexity from cargo clippy.
    run_optional_enrichers(root, &conn);

    roles::infer_roles(&conn)?;

    Ok(())
}

fn run_optional_enrichers(root: &str, conn: &rusqlite::Connection) {
    match clippy_enricher::enrich(root, conn) {
        Ok(n) => info!(count = n, "clippy diagnostics upserted"),
        Err(e) => warn!(error = %e, "clippy enrichment skipped"),
    }

    match audit::enrich(root, conn) {
        Ok(n) => info!(count = n, "audit advisories upserted"),
        Err(e) => warn!(error = %e, "audit enrichment skipped"),
    }
}

/// Extract the leading `//!` doc block from a crate entry file.
/// Strips the `//! ` / `//!` prefix and joins lines with `\n`.
fn extract_crate_doc(root: &str, entry_file: &str) -> Option<String> {
    let path = std::path::Path::new(root).join(entry_file.trim_start_matches("./"));
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content
        .lines()
        .take_while(|l| l.starts_with("//!"))
        .map(|l| l.trim_start_matches("//!").trim_start_matches(' '))
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn sync_workspace_crate_graph(conn: &rusqlite::Connection, root: &str) -> Result<()> {
    let ws = match workspace::detect(root) {
        Some(w) => w,
        None => return Ok(()),
    };

    // Replace only workspace-derived crate topology on each run.
    conn.execute("DELETE FROM edges WHERE source = 'cargo-metadata' AND edge_type = 'CRATE_DEP'", [])?;
    conn.execute("DELETE FROM nodes WHERE kind = 'crate' AND language = 'workspace'", [])?;
    conn.execute("DELETE FROM fts_symbols WHERE language = 'workspace'", [])?;

    let tx = conn.unchecked_transaction()?;
    let mut name_to_id: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    {
        let mut node_stmt = tx.prepare_cached(
            "INSERT INTO nodes
             (file, language, kind, name, range_start, range_end, signature, content_hash, visibility, doc, stable_id, qualified_name, role, role_confidence)
             VALUES (?1, 'workspace', 'crate', ?2, 1, 1, ?3, '', 'public', ?6, ?4, ?5, 'model', 1.0)",
        )?;
        let mut fts_stmt = tx.prepare_cached(
            "INSERT INTO fts_symbols (name, qualified_name, signature, file, language, node_id)
             VALUES (?1, ?2, ?3, ?4, 'workspace', ?5)",
        )?;

        for m in &ws.members {
            let file = if m.entry_file.starts_with("./") {
                m.entry_file.clone()
            } else {
                format!("./{}", m.entry_file)
            };
            let sig = format!("crate {}", m.name);
            let stable_id = format!("crate::{}", m.name);
            let qualified_name = m.name.clone();
            let doc = extract_crate_doc(root, &m.entry_file);
            node_stmt.execute(rusqlite::params![file, m.name, sig, stable_id, qualified_name, doc])?;
            let node_id = tx.last_insert_rowid();
            fts_stmt.execute(rusqlite::params![m.name, m.name, sig, file, node_id])?;
            name_to_id.insert(m.name.clone(), node_id);
        }
    }
    {
        let mut edge_stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO edges (from_id, to_id, edge_type, source, confidence)
             VALUES (?1, ?2, 'CRATE_DEP', 'cargo-metadata', 1.0)",
        )?;
        for (from, to) in &ws.deps {
            let (Some(from_id), Some(to_id)) = (name_to_id.get(from), name_to_id.get(to)) else {
                continue;
            };
            edge_stmt.execute(rusqlite::params![from_id, to_id])?;
        }
    }
    tx.commit()?;
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
