//! Enrich the symbol graph with `cargo clippy` diagnostics.
//!
//! Runs `cargo clippy --message-format=json-diagnostic-rendered-ansi` and
//! maps each warning/error to the enclosing function node in the DB. Results
//! are stored in the `node_diagnostics` table so they appear in graph/context
//! XML output without re-running clippy on every query.
//!
//! The enricher is best-effort: if clippy is not installed or fails, it logs a
//! warning and returns `Ok(0)` so the rest of the index pipeline is unaffected.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use anyhow::Result;
use rusqlite::Connection;

use crate::schema::open_or_init_db;

// ── Public entry point ───────────────────────────────────────────────────────

/// Run `cargo clippy` in `root` and insert diagnostics into `conn`.
/// Returns the number of diagnostic rows inserted.
pub fn enrich(root: &str, conn: &Connection) -> Result<usize> {
    let output = Command::new("cargo")
        .args([
            "clippy",
            "--message-format=json",
            "--quiet",
            "--all-targets",
            "--",
            "-W",
            "clippy::all",
            "-W",
            "clippy::cognitive_complexity",
        ])
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[clippy] warning: could not run cargo clippy: {}", e);
            return Ok(0);
        }
    };

    let reader = BufReader::new(output.stdout.as_slice());
    let diagnostics = parse_diagnostics(reader, root);

    if diagnostics.is_empty() {
        return Ok(0);
    }

    insert_diagnostics(conn, &diagnostics)
}

pub fn run(root: &str) -> Result<()> {
    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    std::fs::create_dir_all(&graphlite_dir)?;
    let db_path = format!("{}/codegraph.db", graphlite_dir);
    let conn = open_or_init_db(&db_path)?;

    let inserted = enrich(root, &conn)?;
    let warning_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_diagnostics WHERE level = 'warning'",
        [],
        |r| r.get(0),
    )?;
    let error_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM node_diagnostics WHERE level = 'error'",
        [],
        |r| r.get(0),
    )?;

    println!(
        "check: inserted={} warnings={} errors={}",
        inserted, warning_count, error_count
    );

    let mut stmt = conn.prepare(
        "SELECT name, file, complexity
         FROM nodes
         WHERE complexity IS NOT NULL
         ORDER BY complexity DESC, name ASC
         LIMIT 10",
    )?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;

    if !rows.is_empty() {
        println!("top_complexity:");
        for (name, file, complexity) in rows {
            println!("  {} {} ({})", complexity, name, file);
        }
    }

    Ok(())
}

// ── Diagnostic types ─────────────────────────────────────────────────────────

struct Diagnostic {
    file: String,
    line: i32,
    code: String,
    level: String,
    message: String,
    suggestion: Option<String>,
    complexity: Option<i64>,
}

// ── Parsing ──────────────────────────────────────────────────────────────────

fn parse_diagnostics(reader: impl BufRead, root: &str) -> Vec<Diagnostic> {
    let mut result = Vec::new();

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only process compiler-message packets.
        if msg["reason"].as_str() != Some("compiler-message") {
            continue;
        }

        let diag = &msg["message"];
        let level = match diag["level"].as_str() {
            Some(l) if l == "warning" || l == "error" => l.to_string(),
            _ => continue,
        };

        let message = match diag["message"].as_str() {
            Some(m) => m.to_string(),
            None => continue,
        };

        // Extract lint code (e.g. "clippy::needless_pass_by_value").
        let code = match diag["code"]["code"].as_str() {
            Some(c) => c.to_string(),
            None => continue,
        };

        // Find the primary span.
        let spans = match diag["spans"].as_array() {
            Some(s) => s,
            None => continue,
        };
        let primary = match spans.iter().find(|s| s["is_primary"].as_bool() == Some(true)) {
            Some(s) => s,
            None => continue,
        };

        let file_name = match primary["file_name"].as_str() {
            Some(f) => f,
            None => continue,
        };

        // Normalise to the ./src/... form stored in the DB.
        let db_path = span_to_db_path(file_name, root);

        let line_start = match primary["line_start"].as_i64() {
            Some(l) => l as i32,
            None => continue,
        };

        // Collect the first suggested replacement, if any.
        let suggestion = diag["children"]
            .as_array()
            .and_then(|children| {
                children.iter().find_map(|c| {
                    c["spans"]
                        .as_array()
                        .and_then(|ss| ss.first())
                        .and_then(|s| s["suggested_replacement"].as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                })
            });

        result.push(Diagnostic {
            file: db_path,
            line: line_start,
            code,
            level,
            message,
            suggestion,
            complexity: extract_complexity(diag["message"].as_str().unwrap_or("")),
        });
    }

    result
}

/// Convert a span file_name (e.g. `src/foo.rs`) to the DB path format (`./src/foo.rs`).
fn span_to_db_path(file_name: &str, root: &str) -> String {
    // Absolute paths: make relative to root, then prepend ./
    if file_name.starts_with('/') {
        let root_prefix = root.trim_end_matches('/');
        if let Some(rel) = file_name.strip_prefix(root_prefix) {
            return format!(".{}", rel);
        }
        return file_name.to_string();
    }
    // Already relative (e.g. "src/foo.rs") → prepend ./
    if file_name.starts_with("./") {
        file_name.to_string()
    } else {
        format!("./{}", file_name)
    }
}

// ── DB insertion ─────────────────────────────────────────────────────────────

fn insert_diagnostics(conn: &Connection, diagnostics: &[Diagnostic]) -> Result<usize> {
    // Build a lookup of (file, range_start, range_end) → node_id for fn nodes.
    let mut file_fns: std::collections::HashMap<String, Vec<(i32, i32, i64)>> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT file, range_start, range_end, id FROM nodes WHERE kind = 'fn'",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows.filter_map(|r| r.ok()) {
            file_fns
                .entry(row.0)
                .or_default()
                .push((row.1, row.2, row.3));
        }
    }

    // Clear existing clippy diagnostics so re-runs replace rather than append.
    conn.execute(
        "DELETE FROM node_diagnostics WHERE code LIKE 'clippy::%' OR code LIKE 'E%' OR level = 'warning'",
        [],
    )?;
    conn.execute("UPDATE nodes SET complexity = NULL", [])?;

    let tx = conn.unchecked_transaction()?;
    let mut count = 0usize;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO node_diagnostics
             (node_id, code, level, message, suggestion)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut seen = std::collections::HashSet::new();

        for d in diagnostics {
            let node_id = match find_enclosing_fn(&file_fns, &d.file, d.line) {
                Some(id) => id,
                None => continue,
            };
            if !seen.insert((node_id, d.code.as_str(), d.message.as_str())) {
                continue;
            }

            stmt.execute(rusqlite::params![
                node_id,
                &d.code,
                &d.level,
                &d.message,
                d.suggestion.as_deref(),
            ])?;
            if d.code == "clippy::cognitive_complexity" {
                if let Some(c) = d.complexity {
                    tx.execute(
                        "UPDATE nodes
                         SET complexity = CASE
                             WHEN complexity IS NULL OR complexity < ?2 THEN ?2
                             ELSE complexity
                         END
                         WHERE id = ?1",
                        rusqlite::params![node_id, c],
                    )?;
                }
            }
            count += 1;
        }
    }
    tx.commit()?;
    Ok(count)
}

fn find_enclosing_fn(
    file_fns: &std::collections::HashMap<String, Vec<(i32, i32, i64)>>,
    file: &str,
    line: i32,
) -> Option<i64> {
    let fns = file_fns.get(file)?;
    fns.iter()
        .filter(|(rs, re, _)| *rs <= line && *re >= line)
        .min_by_key(|(rs, re, _)| re - rs)
        .map(|(_, _, id)| *id)
}

fn extract_complexity(msg: &str) -> Option<i64> {
    let marker = "complexity of ";
    let idx = msg.find(marker)?;
    let tail = &msg[idx + marker.len()..];
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::extract_complexity;

    #[test]
    fn complexity_parser_extracts_numeric_score() {
        let msg = "the function has a cognitive complexity of 24";
        assert_eq!(extract_complexity(msg), Some(24));
    }

    #[test]
    fn complexity_parser_returns_none_without_marker() {
        let msg = "function is never used";
        assert_eq!(extract_complexity(msg), None);
    }
}
