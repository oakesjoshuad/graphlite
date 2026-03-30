use anyhow::Result;
use rusqlite::Connection;

use crate::parser::Symbol;

pub fn bulk_insert_symbols(conn: &Connection, symbols: &[Symbol]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut node_stmt = tx.prepare_cached(
            "INSERT INTO nodes (file, language, kind, name, range_start, range_end, signature, content_hash, visibility, doc, stable_id, qualified_name, role, role_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )?;
        let mut fts_stmt = tx.prepare_cached(
            "INSERT INTO fts_symbols (name, qualified_name, signature, file, language, node_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
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
                sym.qualified_name,
                if sym.is_test_fn { "test" } else { "unknown" },
                if sym.is_test_fn { 0.99f64 } else { 0.0f64 },
            ])?;
            let node_id = tx.last_insert_rowid();

            fts_stmt.execute(rusqlite::params![
                sym.name,
                sym.qualified_name,
                sym.signature,
                sym.file,
                sym.language,
                node_id,
            ])?;
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
