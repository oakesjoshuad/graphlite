use anyhow::Result;
use rayon::prelude::*;
use std::path::PathBuf;
use walkdir::WalkDir;

use crate::{
    insert::{bulk_insert_edges, bulk_insert_symbols},
    language::detect_language,
    parser::{parse_file, ParseResult, RawEdge, Symbol},
    schema::init_db,
};

const IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".svelte-kit",
    ".git",
    ".cache",
    ".next",
    ".nuxt",
    "__pycache__",
];

fn is_ignored(entry: &walkdir::DirEntry) -> bool {
    if entry.file_type().is_dir() {
        entry
            .file_name()
            .to_str()
            .map(|name| IGNORED_DIRS.contains(&name))
            .unwrap_or(false)
    } else {
        false
    }
}

fn collect_source_files(root: &str) -> Result<Vec<PathBuf>> {
    let files = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| detect_language(p).is_some())
        .collect();
    Ok(files)
}

pub fn run(root: &str) -> Result<()> {
    let files = collect_source_files(root)?;
    eprintln!("Found {} source files", files.len());

    let results: Vec<ParseResult> = files
        .par_iter()
        .filter_map(|path| {
            parse_file(path)
                .map_err(|e| {
                    eprintln!("Warning: {}: {}", path.display(), e);
                    e
                })
                .ok()
        })
        .collect();

    let symbols: Vec<Symbol> = results
        .iter()
        .flat_map(|r| r.symbols.iter().cloned())
        .collect();
    let edges: Vec<RawEdge> = results
        .iter()
        .flat_map(|r| r.edges.iter().cloned())
        .collect();

    let conn = init_db("codegraph.db")?;
    let name_to_id = bulk_insert_symbols(&conn, &symbols)?;
    bulk_insert_edges(&conn, &edges, &name_to_id)?;

    eprintln!(
        "Inserted {} symbols, {} edges -> codegraph.db",
        symbols.len(),
        edges.len()
    );
    Ok(())
}
