use anyhow::Result;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
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

fn detect_lsp_from_files(files: &[PathBuf]) -> Vec<String> {
    use std::collections::HashSet;
    // Collect the set of languages that have files in this project
    let present: HashSet<String> = files
        .iter()
        .filter_map(|p| detect_language(p))
        .filter_map(|lang| lang.lsp_server_cmd().map(|_| lang.as_str().to_string()))
        .collect();
    // For each present language, check if server is in PATH
    present
        .into_iter()
        .filter(|lang_str| {
            // Re-derive server cmd from language string
            let ext = match lang_str.as_str() {
                "rust" => "rs",
                "typescript" => "ts",
                "javascript" => "js",
                "svelte" => "svelte",
                _ => return false,
            };
            let cmd = detect_language(std::path::Path::new(&format!("x.{}", ext)))
                .and_then(|l| l.lsp_server_cmd());
            if let Some(cmd) = cmd {
                std::process::Command::new("which")
                    .arg(cmd)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .is_some()
            } else {
                false
            }
        })
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

    // Delete old edges and nodes for changed files.
    // Edges are deleted explicitly first because older db instances may lack ON DELETE CASCADE.
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

    let effective_lsp: Vec<String> = if let Some(lang) = lsp_lang {
        vec![lang.to_string()]
    } else if !config.lsp.is_empty() {
        config.lsp.clone()
    } else {
        detect_lsp_from_files(&files)
    };
    for lang in &effective_lsp {
        lsp::enrich(&conn, root, lang)?;
    }

    roles::infer_roles(&conn)?;

    Ok(())
}
