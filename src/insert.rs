use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;

use crate::parser::{RawEdge, Symbol};

pub fn bulk_insert_symbols(conn: &Connection, symbols: &[Symbol]) -> Result<HashMap<String, i64>> {
    let mut name_to_id: HashMap<String, i64> = HashMap::new();

    let tx = conn.unchecked_transaction()?;
    {
        let mut node_stmt = tx.prepare_cached(
            "INSERT INTO nodes (file, language, kind, name, range_start, range_end, signature, content_hash, visibility, doc, stable_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        )?;
        let mut fts_stmt = tx.prepare_cached(
            "INSERT INTO fts_symbols (name, signature, file, language, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;

        for sym in symbols {
            node_stmt.execute(rusqlite::params![
                sym.file,
                sym.language,
                sym.kind,
                sym.name,
                sym.range_start,
                sym.range_end,
                sym.signature,
                sym.content_hash,
                sym.visibility,
                sym.doc,
                sym.stable_id,
            ])?;
            let node_id = tx.last_insert_rowid();

            fts_stmt.execute(rusqlite::params![
                sym.name,
                sym.signature,
                sym.file,
                sym.language,
                node_id,
            ])?;

            // Last writer wins for duplicate names - sufficient for v0.1
            name_to_id.insert(sym.name.clone(), node_id);
        }
    }
    tx.commit()?;

    Ok(name_to_id)
}

pub fn bulk_insert_edges(
    conn: &Connection,
    edges: &[RawEdge],
    name_to_id: &HashMap<String, i64>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO edges (from_id, to_id, edge_type, source, confidence)
             VALUES (?1, ?2, ?3, 'tree-sitter', 0.8)",
        )?;

        for edge in edges {
            let from_id = match name_to_id.get(&edge.from_name) {
                Some(id) => *id,
                None => continue, // External or unknown caller - skip
            };
            let to_id = match name_to_id.get(&edge.to_name) {
                Some(id) => *id,
                None => continue, // External callee (stdlib, etc.) - skip
            };

            stmt.execute(rusqlite::params![from_id, to_id, edge.edge_type])?;
        }
    }
    tx.commit()?;

    Ok(())
}

pub fn upsert_file_hash(
    conn: &Connection,
    path: &str,
    hash: &str,
    doc: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO files (path, file_hash, doc) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET file_hash = excluded.file_hash, doc = excluded.doc",
        rusqlite::params![path, hash, doc],
    )?;
    Ok(())
}
