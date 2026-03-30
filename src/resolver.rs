//! Use-statement scope resolver: emits `CALLS_RESOLVED` edges by resolving
//! `call_expression` targets against the symbol DB using `use`-declaration scope maps.
//!
//! Confidence tiers
//! ----------------
//! HIGH   (1.0) — call text matched the scope map; `qualified_name` hit in DB.
//! MEDIUM (0.7) — unqualified name, unique match in the same file.
//! LOW    (0.4) — unqualified name, unique match across all files.
//!
//! Scoped calls (`module::function()`) are tried as direct qualified-name lookups
//! before falling back to the name-only tiers, so they typically land at HIGH.

use std::collections::{BTreeSet, HashMap};
use std::fs;

use anyhow::Result;
use rusqlite::Connection;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

// ── Constants ────────────────────────────────────────────────────────────────

const HIGH: f64 = 1.0;
const MEDIUM: f64 = 0.7;
const LOW: f64 = 0.4;

/// Extract use_declaration arguments for scope-map building.
const USE_QUERY: &str = "(use_declaration argument: (_) @use_arg)";

/// Extract call targets.  Scoped identifiers are captured whole so we can do
/// direct qualified-name lookups (e.g. `roles::infer_roles` as a single token).
const CALL_QUERY: &str = "
(call_expression function: (identifier) @call)
(call_expression function: (scoped_identifier) @call)
(call_expression function: (field_expression field: (field_identifier) @call))
(call_expression function: (generic_function function: (identifier) @call))
(call_expression function: (generic_function function: (scoped_identifier) @call))
(call_expression function: (generic_function function: (field_expression field: (field_identifier) @call)))
";

// ── Public entry point ───────────────────────────────────────────────────────

/// Resolve call expressions for all Rust files already in the DB and emit
/// `CALLS_RESOLVED` edges.  Returns the number of unique edges written.
pub fn resolve(conn: &Connection) -> Result<usize> {
    let files: Vec<String> = {
        let mut stmt =
            conn.prepare_cached("SELECT DISTINCT file FROM nodes WHERE language = 'rust'")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if files.is_empty() {
        return Ok(0);
    }

    let ts_lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&ts_lang)?;
    let use_query = Query::new(&ts_lang, USE_QUERY)?;
    let call_query = Query::new(&ts_lang, CALL_QUERY)?;

    let mut all_edges: Vec<(i64, i64, f64, &'static str)> = Vec::new();
    let mut site_count = 0usize;

    for file in &files {
        let source = match fs::read_to_string(file) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => continue,
        };

        let module_prefix = module_prefix_from_file(file);
        let scope_map = build_scope_map(&tree, &source, &use_query, &module_prefix);

        // Load all nodes for this file.
        let file_nodes: Vec<FileNode> = {
            let mut stmt = conn.prepare_cached(
                "SELECT id, range_start, range_end, kind, qualified_name FROM nodes WHERE file = ?1",
            )?;
            let rows: Vec<FileNode> = stmt
                .query_map(rusqlite::params![file], |r| {
                    Ok(FileNode {
                        id: r.get(0)?,
                        range_start: r.get(1)?,
                        range_end: r.get(2)?,
                        kind: r.get(3)?,
                        qualified_name: r.get(4)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };

        if file_nodes.is_empty() {
            continue;
        }

        for call in extract_calls(&tree, &source, &call_query) {
            let caller = match find_enclosing_fn(&file_nodes, call.line) {
                Some(c) => c,
                None => continue,
            };

            if let Some((to_id, conf, tier)) =
                resolve_callee(conn, &call, &scope_map, file, &module_prefix, caller)
            {
                if caller.id != to_id {
                    all_edges.push((caller.id, to_id, conf, tier));
                    site_count += 1;
                }
            }
        }
    }

    // Dedup by (from_id, to_id), keeping the highest-confidence entry.
    all_edges.sort_by(|a, b| {
        (a.0, a.1)
            .cmp(&(b.0, b.1))
            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
    });
    all_edges.dedup_by_key(|e| (e.0, e.1));

    let total = all_edges.len();

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT OR IGNORE INTO edges
             (from_id, to_id, edge_type, source, confidence, confidence_tier)
             VALUES (?1, ?2, 'CALLS_RESOLVED', 'resolver', ?3, ?4)",
        )?;
        for (from_id, to_id, conf, tier) in &all_edges {
            stmt.execute(rusqlite::params![from_id, to_id, conf, tier])?;
        }
    }
    tx.commit()?;

    eprintln!(
        "[resolver] {} call site(s) → {} unique CALLS_RESOLVED edge(s)",
        site_count, total
    );
    Ok(total)
}

// ── Scope map ────────────────────────────────────────────────────────────────

/// Build a `short_name → qualified_path` map from all `use` declarations in
/// the file.  `crate::`, `self::`, and `super::` prefixes are stripped so the
/// values match the `qualified_name` column format (module-relative paths).
fn build_scope_map(
    tree: &tree_sitter::Tree,
    source: &str,
    query: &Query,
    module_prefix: &str,
) -> ScopeMap {
    let mut map = HashMap::new();
    let mut wildcards = BTreeSet::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            collect_bindings(cap.node, "", source, module_prefix, &mut map, &mut wildcards);
        }
    }
    ScopeMap {
        direct: map,
        wildcards: wildcards.into_iter().collect(),
    }
}

/// Recursively extract `(short_name, qualified_path)` pairs from a use-arg node.
/// `prefix` accumulates the path from an enclosing `scoped_use_list`.
fn collect_bindings(
    node: tree_sitter::Node,
    prefix: &str,
    source: &str,
    module_prefix: &str,
    map: &mut HashMap<String, String>,
    wildcards: &mut BTreeSet<String>,
) {
    match node.kind() {
        "identifier" => {
            let name = node_text(node, source);
            let full = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}::{}", prefix, name)
            };
            map.insert(name, normalize_path(&full, module_prefix));
        }
        "scoped_identifier" => {
            let full_path = node_text(node, source);
            let qualified = normalize_path(&full_path, module_prefix);
            if let Some(short) = qualified.split("::").last() {
                if !short.is_empty() {
                    map.insert(short.to_string(), qualified);
                }
            }
        }
        "use_as_clause" => {
            if let (Some(p), Some(a)) = (
                node.child_by_field_name("path"),
                node.child_by_field_name("alias"),
            ) {
                let qualified = normalize_path(&node_text(p, source), module_prefix);
                let alias = node_text(a, source);
                map.insert(alias, qualified);
            }
        }
        "scoped_use_list" => {
            let new_prefix = node
                .child_by_field_name("path")
                .map(|p| {
                    let clean = normalize_path(&node_text(p, source), module_prefix);
                    if prefix.is_empty() {
                        clean
                    } else {
                        format!("{}::{}", prefix, clean)
                    }
                })
                .unwrap_or_else(|| prefix.to_string());

            if let Some(list) = node.child_by_field_name("list") {
                let mut c = list.walk();
                for child in list.children(&mut c) {
                    let k = child.kind();
                    if k != "," && k != "{" && k != "}" && k != "comment" {
                        collect_bindings(
                            child,
                            &new_prefix,
                            source,
                            module_prefix,
                            map,
                            wildcards,
                        );
                    }
                }
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for child in node.children(&mut c) {
                let k = child.kind();
                if k != "," && k != "{" && k != "}" && k != "comment" {
                    collect_bindings(child, prefix, source, module_prefix, map, wildcards);
                }
            }
        }
        "use_wildcard" => {
            if !prefix.is_empty() {
                wildcards.insert(normalize_path(prefix, module_prefix));
            }
        }
        _ => {}
    }
}

fn node_text(node: tree_sitter::Node, source: &str) -> String {
    node.utf8_text(source.as_bytes())
        .unwrap_or("")
        .to_string()
}

fn normalize_path(path: &str, module_prefix: &str) -> String {
    let mut segments: Vec<&str> = module_prefix
        .split("::")
        .filter(|s| !s.is_empty())
        .collect();
    let mut rest = path.trim();

    while let Some(r) = rest.strip_prefix("super::") {
        if !segments.is_empty() {
            segments.pop();
        }
        rest = r;
    }

    if let Some(r) = rest.strip_prefix("crate::") {
        rest = r;
        segments.clear();
    }

    if let Some(r) = rest.strip_prefix("self::") {
        rest = r;
        if segments.is_empty() {
            return rest.to_string();
        }
        let mut out = segments;
        out.push(rest);
        return out.join("::");
    }

    if !rest.contains("::") {
        return rest.to_string();
    }

    if segments.is_empty() {
        rest.to_string()
    } else if rest.is_empty() {
        segments.join("::")
    } else {
        let mut out = segments;
        out.push(rest);
        out.join("::")
    }
}

fn module_prefix_from_file(file: &str) -> String {
    let p = file.trim_start_matches("./");
    let rel_src = match p.split_once("/src/") {
        Some((_, right)) => right,
        None => p.strip_prefix("src/").unwrap_or(p),
    };

    if rel_src == "lib.rs" || rel_src == "main.rs" {
        return String::new();
    }
    if let Some(dir) = rel_src.strip_suffix("/mod.rs") {
        return dir.replace('/', "::");
    }
    if let Some(stem) = rel_src.strip_suffix(".rs") {
        return stem.replace('/', "::");
    }
    String::new()
}

// ── Call extraction ──────────────────────────────────────────────────────────

/// Returns `(1-indexed line, call text)` for every call captured in the file.
fn extract_calls(
    tree: &tree_sitter::Tree,
    source: &str,
    query: &Query,
) -> Vec<CallSite> {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());
    let mut calls = Vec::new();
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let text = node_text(cap.node, source);
            if text.is_empty() {
                continue;
            }
            let line = cap.node.start_position().row as u32 + 1;
            calls.push(CallSite {
                line,
                text,
                is_method_style: cap.node.kind() == "field_identifier",
            });
        }
    }
    calls
}

// ── Enclosing function lookup ────────────────────────────────────────────────

fn find_enclosing_fn(file_nodes: &[FileNode], call_line: u32) -> Option<&FileNode> {
    let cl = call_line as i32;
    file_nodes
        .iter()
        .filter(|n| n.kind == "fn" && n.range_start <= cl && n.range_end >= cl)
        .min_by_key(|n| n.range_end - n.range_start)
}

// ── Callee resolution ────────────────────────────────────────────────────────

fn resolve_callee(
    conn: &Connection,
    call: &CallSite,
    scope_map: &ScopeMap,
    file: &str,
    module_prefix: &str,
    caller: &FileNode,
) -> Option<(i64, f64, &'static str)> {
    let call_text = call.text.as_str();
    // ── Scoped call (e.g. `roles::infer_roles` or `crate::foo::bar`) ────────
    if call_text.contains("::") {
        let mut qualified = normalize_path(call_text, module_prefix);
        if let Some(stripped) = call_text.strip_prefix("Self::") {
            if let Some(impl_type) = infer_impl_type(&caller.qualified_name) {
                qualified = format!("{}::{}", impl_type, stripped);
            }
        }

        // Direct qualified_name match → HIGH.
        if let Ok(id) = conn.query_row(
            "SELECT id FROM nodes WHERE qualified_name = ?1 AND kind = 'fn' LIMIT 1",
            rusqlite::params![&qualified],
            |r| r.get::<_, i64>(0),
        ) {
            return Some((id, HIGH, "high"));
        }

        // Fall back to last-segment name lookup → MEDIUM if unique in this file,
        // LOW if globally unique.
        let short = qualified.split("::").last()?;
        return resolve_by_name(conn, short, file, call.is_method_style);
    }

    // ── Simple name: scope-map lookup → HIGH ─────────────────────────────────
    if let Some(qualified) = scope_map.direct.get(call_text) {
        // Try qualified_name exact match first.
        if let Ok(id) = conn.query_row(
            "SELECT id FROM nodes WHERE qualified_name = ?1 AND kind = 'fn' LIMIT 1",
            rusqlite::params![qualified],
            |r| r.get::<_, i64>(0),
        ) {
            return Some((id, HIGH, "high"));
        }
        // The scope map had an entry but qualified_name didn't match; try the
        // short name in case the DB only has unqualified names.
        let short = qualified.split("::").last().unwrap_or(call_text);
        if let Ok(id) = conn.query_row(
            "SELECT id FROM nodes WHERE name = ?1 AND kind = 'fn' LIMIT 1",
            rusqlite::params![short],
            |r| r.get::<_, i64>(0),
        ) {
            return Some((id, HIGH, "high"));
        }
    }

    // Wildcard imports: test each imported module prefix as `prefix::name`.
    for prefix in &scope_map.wildcards {
        let candidate = format!("{}::{}", prefix, call_text);
        if let Ok(id) = conn.query_row(
            "SELECT id FROM nodes WHERE qualified_name = ?1 AND kind = 'fn' LIMIT 1",
            rusqlite::params![candidate],
            |r| r.get::<_, i64>(0),
        ) {
            return Some((id, HIGH, "high"));
        }
    }

    // ── No scope-map hit: fall back to name tiers ─────────────────────────────
    resolve_by_name(conn, call_text, file, call.is_method_style)
}

/// MEDIUM: unique match by name in the same file.
/// LOW: unique match by name across all files.
/// Returns `None` if ambiguous (> 1 match).
fn resolve_by_name(
    conn: &Connection,
    name: &str,
    file: &str,
    keep_ambiguous_as_low: bool,
) -> Option<(i64, f64, &'static str)> {
    // MEDIUM — same file, unique
    let same_file: Vec<i64> = {
        let mut stmt = conn
            .prepare_cached(
                "SELECT id FROM nodes WHERE name = ?1 AND kind = 'fn' AND file = ?2",
            )
            .ok()?;
        let rows: Vec<i64> = stmt
            .query_map(rusqlite::params![name, file], |r| r.get(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    if same_file.len() == 1 {
        return Some((same_file[0], MEDIUM, "medium"));
    }

    // LOW — globally unique, non-private only.
    // Private functions cannot be called from other modules, so a cross-file
    // match on a private function is definitively a false positive.
    let global: Vec<i64> = {
        let mut stmt = conn
            .prepare_cached(
                "SELECT id FROM nodes WHERE name = ?1 AND kind = 'fn' \
                 AND visibility NOT IN ('private', 'default')",
            )
            .ok()?;
        let rows: Vec<i64> = stmt
            .query_map(rusqlite::params![name], |r| r.get(0))
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    if global.len() == 1 {
        return Some((global[0], LOW, "low"));
    }

    if keep_ambiguous_as_low && !global.is_empty() {
        return Some((global[0], LOW, "low"));
    }

    None
}

fn infer_impl_type(qualified_name: &str) -> Option<String> {
    let mut parts: Vec<&str> = qualified_name.split("::").collect();
    if parts.len() < 2 {
        return None;
    }
    parts.pop();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("::"))
    }
}

struct ScopeMap {
    direct: HashMap<String, String>,
    wildcards: Vec<String>,
}

struct CallSite {
    line: u32,
    text: String,
    is_method_style: bool,
}

struct FileNode {
    id: i64,
    range_start: i32,
    range_end: i32,
    kind: String,
    qualified_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn normalize_path_handles_crate_self_super_prefixes() {
        assert_eq!(normalize_path("crate::query::open_db", "discover"), "query::open_db");
        assert_eq!(
            normalize_path("self::resolve", "resolver"),
            "resolver::resolve"
        );
        assert_eq!(
            normalize_path("super::config::load", "workspace::discover"),
            "workspace::config::load"
        );
    }

    #[test]
    fn module_prefix_is_derived_from_src_layout() {
        assert_eq!(module_prefix_from_file("./src/lib.rs"), "");
        assert_eq!(module_prefix_from_file("./src/query.rs"), "query");
        assert_eq!(
            module_prefix_from_file("./crates/core/src/foo/bar.rs"),
            "foo::bar"
        );
        assert_eq!(
            module_prefix_from_file("./crates/core/src/foo/mod.rs"),
            "foo"
        );
    }

    #[test]
    fn ambiguous_method_calls_are_kept_as_low_confidence() {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch(
            "CREATE TABLE nodes (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                visibility TEXT NOT NULL,
                qualified_name TEXT NOT NULL
             );",
        )
        .expect("schema");

        conn.execute(
            "INSERT INTO nodes (id, name, kind, file, visibility, qualified_name)
             VALUES (1, 'run', 'fn', './src/a.rs', 'public', 'a::run')",
            [],
        )
        .expect("insert a");
        conn.execute(
            "INSERT INTO nodes (id, name, kind, file, visibility, qualified_name)
             VALUES (2, 'run', 'fn', './src/b.rs', 'public', 'b::run')",
            [],
        )
        .expect("insert b");

        let dropped = resolve_by_name(&conn, "run", "./src/c.rs", false);
        assert!(dropped.is_none(), "non-method ambiguity should be dropped");

        let kept = resolve_by_name(&conn, "run", "./src/c.rs", true).expect("low edge");
        assert_eq!(kept.2, "low");
    }
}
