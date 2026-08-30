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
    signature TEXT,
    content_hash TEXT NOT NULL DEFAULT '',
    visibility TEXT NOT NULL DEFAULT 'private',
    doc TEXT,
    role TEXT NOT NULL DEFAULT 'unknown',
    role_confidence REAL NOT NULL DEFAULT 0.0,
    complexity INTEGER,
    stable_id TEXT NOT NULL DEFAULT '',
    qualified_name TEXT NOT NULL DEFAULT '',
    trait_impl TEXT
);

CREATE TABLE edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    from_id INTEGER NOT NULL,
    to_id INTEGER NOT NULL,
    edge_type TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'tree-sitter',
    confidence REAL NOT NULL DEFAULT 0.8,
    confidence_tier TEXT,
    FOREIGN KEY(from_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY(to_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE TABLE files (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT UNIQUE NOT NULL,
    file_hash TEXT NOT NULL,
    doc TEXT
);

CREATE TABLE annotations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id INTEGER NOT NULL UNIQUE REFERENCES nodes(id) ON DELETE CASCADE,
    intent TEXT,
    behavior TEXT,
    tags TEXT,
    source TEXT NOT NULL DEFAULT 'llm',
    confidence REAL NOT NULL DEFAULT 0.8,
    content_hash_at_annotation TEXT NOT NULL DEFAULT ''
);

CREATE TABLE crate_advisories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_name TEXT NOT NULL,
    version TEXT NOT NULL,
    advisory_id TEXT NOT NULL,
    title TEXT NOT NULL,
    cvss TEXT,
    severity TEXT,
    category TEXT,
    kind TEXT NOT NULL,
    date TEXT,
    UNIQUE(package_name, version, advisory_id)
);

CREATE TABLE node_diagnostics (
    id INTEGER PRIMARY KEY,
    node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    code TEXT NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL,
    suggestion TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE VIRTUAL TABLE fts_symbols USING fts5(
    name,
    qualified_name,
    signature,
    file,
    language,
    node_id UNINDEXED,
    tokenize='porter unicode61'
);

CREATE INDEX idx_edges_from ON edges(from_id);
CREATE INDEX idx_edges_to ON edges(to_id);
CREATE INDEX idx_edges_to_source ON edges(to_id, source);
CREATE INDEX idx_nodes_file ON nodes(file);
CREATE INDEX idx_node_diagnostics_node ON node_diagnostics(node_id);
CREATE INDEX idx_node_diagnostics_code ON node_diagnostics(code);
";

pub fn init_db(path: &str) -> Result<Connection> {
    if Path::new(path).exists() {
        fs::remove_file(path)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;\n{}",
        SCHEMA_SQL
    ))?;
    Ok(conn)
}

/// Opens an existing db if it has incremental-indexing support (files table),
/// otherwise wipes and recreates it. Creates fresh if the file is absent.
pub fn open_or_init_db(path: &str) -> Result<Connection> {
    if !Path::new(path).exists() {
        return init_db(path);
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys = ON;",
    )?;
    let has_files: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='files'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;
    if !has_files {
        drop(conn);
        return init_db(path);
    }
    let _ = conn.execute("ALTER TABLE files ADD COLUMN doc TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE nodes ADD COLUMN role TEXT NOT NULL DEFAULT 'unknown'",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE nodes ADD COLUMN role_confidence REAL NOT NULL DEFAULT 0.0",
        [],
    );
    let _ = conn.execute("ALTER TABLE nodes ADD COLUMN complexity INTEGER", []);
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_edges_to_source ON edges(to_id, source)",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE nodes ADD COLUMN stable_id TEXT NOT NULL DEFAULT ''",
        [],
    );
    // Add qualified_name column if not present
    let _ = conn.execute(
        "ALTER TABLE nodes ADD COLUMN qualified_name TEXT NOT NULL DEFAULT ''",
        [],
    );
    // Add trait_impl column if not present
    let _ = conn.execute("ALTER TABLE nodes ADD COLUMN trait_impl TEXT", []);
    // Add confidence_tier column to edges if not present
    let _ = conn.execute("ALTER TABLE edges ADD COLUMN confidence_tier TEXT", []);
    // Add node_diagnostics table if not present (cargo clippy enrichment)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS node_diagnostics (
            id        INTEGER PRIMARY KEY,
            node_id   INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
            code      TEXT NOT NULL,
            level     TEXT NOT NULL,
            message   TEXT NOT NULL,
            suggestion TEXT,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_node_diagnostics_node
            ON node_diagnostics(node_id);
        CREATE INDEX IF NOT EXISTS idx_node_diagnostics_code
            ON node_diagnostics(code);",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS crate_advisories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            package_name TEXT NOT NULL,
            version TEXT NOT NULL,
            advisory_id TEXT NOT NULL,
            title TEXT NOT NULL,
            cvss TEXT,
            severity TEXT,
            category TEXT,
            kind TEXT NOT NULL,
            date TEXT,
            UNIQUE(package_name, version, advisory_id)
        );
        CREATE INDEX IF NOT EXISTS idx_crate_advisories_package
            ON crate_advisories(package_name);
        CREATE INDEX IF NOT EXISTS idx_crate_advisories_kind
            ON crate_advisories(kind);",
    )?;
    // Rebuild FTS to include qualified_name column if it was added after initial creation
    let needs_fts_rebuild: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='fts_symbols'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|sql| !sql.contains("qualified_name"))
        .unwrap_or(true);
    if needs_fts_rebuild {
        conn.execute_batch(
            "DROP TABLE IF EXISTS fts_symbols;
             CREATE VIRTUAL TABLE fts_symbols USING fts5(
                 name, qualified_name, signature, file, language,
                 node_id UNINDEXED,
                 tokenize='porter unicode61'
             );
             INSERT INTO fts_symbols (name, qualified_name, signature, file, language, node_id)
             SELECT name, qualified_name, signature, file, language, id FROM nodes;",
        )?;
    }
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
