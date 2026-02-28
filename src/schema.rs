use std::{fs, path::Path};

use anyhow::Result;
use rusqlite::Connection;

pub const SCHEMA_SQL: &str = "
PRAGMA foreign_keys = ON;

CREATE TABLE nodes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file TEXT NOT NULL,
    language TEXT NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    range_start INTEGER NOT NULL,
    range_end INTEGER NOT NULL,
    signature TEXT
);

CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id INTEGER NOT NULL,
    to_id INTEGER NOT NULL,
    edge_type TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'tree-sitter',
    confidence REAL NOT NULL DEFAULT 0.8,
    FOREIGN KEY(from_id) REFERENCES nodes(id),
    FOREIGN KEY(to_id) REFERENCES nodes(id)
);

CREATE VIRTUAL TABLE fts_symbols USING fts5(
    name,
    signature,
    file,
    language,
    node_id UNINDEXED,
    tokenize='porter unicode61'
);

CREATE INDEX idx_edges_from ON edges(from_id);
CREATE INDEX idx_edges_to ON edges(to_id);
";

pub fn init_db(path: &str) -> Result<Connection> {
    if Path::new(path).exists() {
        fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn schema_initializes() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        // Remove the temp file so init_db creates it fresh
        drop(tmp);
        let conn = init_db(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let _ = fs::remove_file(&path);
    }
}
