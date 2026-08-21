use std::{fs, path::Path};

use anyhow::{anyhow, Result};
use tracing::info;
use rusqlite::Connection;

use crate::xml as vxml;

pub(crate) fn open_db() -> Result<Connection> {
    let conn = Connection::open(".graphlite/codegraph.db")
        .map_err(|_| anyhow!(".graphlite/codegraph.db not found - run `graphlite init` first"))?;
    Ok(conn)
}

fn is_testish(file: &str, role: &str) -> bool {
    if role == "test" {
        return true;
    }
    let p = file.trim_start_matches("./").to_ascii_lowercase();
    p.contains("/tests/")
        || p.starts_with("tests/")
        || p.ends_with("_test.rs")
        || p.ends_with(".test.ts")
        || p.ends_with(".test.js")
        || p.ends_with(".spec.ts")
        || p.ends_with(".spec.js")
}

fn is_trusted_source(source: &str) -> bool {
    matches!(source, "resolver" | "rustdoc")
}

fn write_wrapper_start(tag: &'static str, attrs: &[(&str, &str)]) -> Result<()> {
    let mut w = vxml::new_stream_writer();
    vxml::open_attrs(&mut w, tag, attrs)?;
    vxml::finish_stream(w)?;
    Ok(())
}

fn write_wrapper_end(tag: &'static str) -> Result<()> {
    let mut w = vxml::new_stream_writer();
    vxml::close(&mut w, tag)?;
    vxml::finish_stream(w)?;
    Ok(())
}

pub(crate) fn resolve_symbol_id(conn: &Connection, arg: &str) -> Result<i64> {
    if let Ok(id) = arg.parse::<i64>() {
        return Ok(id);
    }
    let key = arg.strip_prefix("sym:").unwrap_or(arg);
    // stable_id format: file::kind::name or file::kind::ImplType::name (contains '::')
    if key.contains("::") {
        if let Ok(id) = conn.query_row(
            "SELECT id FROM nodes WHERE stable_id = ?1 LIMIT 1",
            rusqlite::params![key],
            |r| r.get::<_, i64>(0),
        ) {
            return Ok(id);
        }
    }
    let id: i64 = conn
        .query_row(
            "SELECT n.id
             FROM nodes n
             WHERE n.name = ?1
             ORDER BY
                CASE WHEN n.role = 'test' THEN 1 ELSE 0 END,
                (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) DESC,
                n.file ASC,
                n.id ASC
             LIMIT 1",
            rusqlite::params![key],
            |r| r.get(0),
        )
        .map_err(|_| anyhow!("symbol '{}' not found in database", key))?;
    Ok(id)
}

type SymbolSearchRow = (
    i64,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
);

struct ResolveCandidate {
    id: i64,
    name: String,
    qualified_name: String,
    kind: String,
    role: String,
    file: String,
    signature: Option<String>,
    stable_id: String,
    fan_in: i64,
}

pub fn resolve(
    query: &str,
    language: Option<&str>,
    prefer_role: Option<&str>,
    prefer_file: Option<&str>,
    md: bool,
) -> Result<()> {
    let conn = open_db()?;
    let (strategy, mut candidates) = resolve_candidates(&conn, query, language)?;
    if candidates.is_empty() {
        anyhow::bail!("no symbol matched query '{}'", query);
    }

    candidates.sort_by(|a, b| {
        let a_role_pref = prefer_role.is_some_and(|r| a.role == r);
        let b_role_pref = prefer_role.is_some_and(|r| b.role == r);
        let a_file_pref = prefer_file.is_some_and(|p| a.file.contains(p));
        let b_file_pref = prefer_file.is_some_and(|p| b.file.contains(p));
        b_role_pref
            .cmp(&a_role_pref)
            .then_with(|| b_file_pref.cmp(&a_file_pref))
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| (a.role == "test").cmp(&(b.role == "test")))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.id.cmp(&b.id))
    });

    let selected = &candidates[0];

    if md {
        println!(
            "Resolved `{}` using strategy `{}` ({} candidate(s))",
            query,
            strategy,
            candidates.len()
        );
        println!(
            "Selected: `sym:{}` (`{}` in `{}`)",
            selected.stable_id, selected.name, selected.file
        );
        println!("| id | name | qualified_name | kind | role | fan_in | file | stable_id |");
        println!("|---:|---|---|---|---|---:|---|---|");
        for c in candidates.iter().take(10) {
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                c.id,
                c.name.replace('|', "\\|"),
                c.qualified_name.replace('|', "\\|"),
                c.kind.replace('|', "\\|"),
                c.role.replace('|', "\\|"),
                c.fan_in,
                c.file.replace('|', "\\|"),
                c.stable_id.replace('|', "\\|"),
            );
        }
        return Ok(());
    }

    let mut w = vxml::new_stream_writer();
    let candidates_s = candidates.len().to_string();
    let selected_id_s = selected.id.to_string();
    vxml::open_attrs(
        &mut w,
        "resolution",
        &[
            ("query", query),
            ("strategy", &strategy),
            ("candidates", &candidates_s),
            ("selected_id", &selected_id_s),
        ],
    )?;
    append_symbol_xml(
        &mut w,
        selected.id,
        &selected.name,
        &selected.qualified_name,
        &selected.kind,
        &selected.role,
        &selected.file,
        selected.signature.as_deref(),
        &selected.stable_id,
    );
    if candidates.len() > 1 {
        vxml::open(&mut w, "alternatives")?;
        for c in candidates.iter().skip(1).take(9) {
            append_symbol_xml(
                &mut w,
                c.id,
                &c.name,
                &c.qualified_name,
                &c.kind,
                &c.role,
                &c.file,
                c.signature.as_deref(),
                &c.stable_id,
            );
        }
        vxml::close(&mut w, "alternatives")?;
    }
    vxml::close(&mut w, "resolution")?;
    vxml::finish_stream(w)?;
    Ok(())
}

pub fn deps(md: bool, modules: bool) -> Result<()> {
    let conn = open_db()?;
    let config = crate::config::load(".");

    let layers: std::collections::HashMap<&str, &str> = config
        .workspace
        .as_ref()
        .map(|w| {
            w.layers
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect()
        })
        .unwrap_or_default();

    let mut crates_stmt = conn.prepare(
        "SELECT name, doc, file
         FROM nodes
         WHERE kind = 'crate' AND language = 'workspace'
         ORDER BY name",
    )?;
    let crates: Vec<(String, Option<String>, String)> = crates_stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut edges_stmt = conn.prepare(
        "SELECT nf.name, nt.name
         FROM edges e
         JOIN nodes nf ON nf.id = e.from_id
         JOIN nodes nt ON nt.id = e.to_id
         WHERE e.edge_type = 'CRATE_DEP'
           AND e.source = 'cargo-metadata'
         ORDER BY nf.name, nt.name",
    )?;
    let edges: Vec<(String, String)> = edges_stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    // fan_in: count how many workspace crates depend on each crate.
    let mut fan_in: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (_, to) in &edges {
        *fan_in.entry(to.as_str()).or_insert(0) += 1;
    }

    // Per-crate module data (only loaded when --modules is set).
    // Map: crate_name -> Vec<ModuleRow> sorted by symbol count desc.
    let crate_modules: std::collections::HashMap<String, Vec<ModuleRow>> = if modules {
        build_crate_modules(&conn, &crates)?
    } else {
        std::collections::HashMap::new()
    };

    if md {
        println!("Workspace crates: {}", crates.len());
        println!("Crate dependency edges: {}", edges.len());
        if !crates.is_empty() {
            println!("| crate | layer | fan_in | doc |");
            println!("|---|---|---:|---|");
            for (name, doc, _file) in &crates {
                let layer = layers.get(name.as_str()).copied().unwrap_or("-");
                let fi = fan_in.get(name.as_str()).copied().unwrap_or(0);
                let doc_cell = doc
                    .as_deref()
                    .and_then(|d| d.lines().next())
                    .unwrap_or("-");
                println!("| {} | {} | {} | {} |",
                    name.replace('|', "\\|"), layer, fi, doc_cell.replace('|', "\\|"));
            }
        }
        if modules {
            println!();
            println!("| crate | module | symbols | role | hotspot | hotspot_fan_in |");
            println!("|---|---|---:|---|---|---:|");
            for (name, _, _) in &crates {
                if let Some(mods) = crate_modules.get(name.as_str()) {
                    for m in mods {
                        println!("| {} | {} | {} | {} | {} | {} |",
                            name.replace('|', "\\|"),
                            m.module.replace('|', "\\|"),
                            m.symbols,
                            m.role.replace('|', "\\|"),
                            m.hotspot.replace('|', "\\|"),
                            m.hotspot_fan_in,
                        );
                    }
                }
            }
        }
        if !edges.is_empty() {
            println!();
            println!("| from_crate | to_crate |");
            println!("|---|---|");
            for (from, to) in &edges {
                println!("| {} | {} |", from.replace('|', "\\|"), to.replace('|', "\\|"));
            }
        }
        return Ok(());
    }

    let mut w = vxml::new_stream_writer();
    let crates_s = crates.len().to_string();
    let edges_s = edges.len().to_string();
    vxml::open_attrs(
        &mut w,
        "deps",
        &[("crates", &crates_s), ("edges", &edges_s), ("tokens", "streaming")],
    )?;
    if !crates.is_empty() {
        vxml::open(&mut w, "crates")?;
        for (name, doc, _file) in &crates {
            let layer = layers.get(name.as_str()).copied().unwrap_or("-");
            let fi = fan_in.get(name.as_str()).copied().unwrap_or(0);
            let fi_s = fi.to_string();
            let doc_first = doc.as_deref().and_then(|d| d.lines().next()).unwrap_or("");
            let mods = if modules { crate_modules.get(name.as_str()) } else { None };
            if mods.is_none_or(|v| v.is_empty()) {
                if doc_first.is_empty() {
                    vxml::empty(&mut w, "crate", &[("name", name), ("layer", layer), ("fan_in", &fi_s)])?;
                } else {
                    vxml::empty(&mut w, "crate", &[("name", name), ("layer", layer), ("fan_in", &fi_s), ("doc", doc_first)])?;
                }
            } else {
                if doc_first.is_empty() {
                    vxml::open_attrs(&mut w, "crate", &[("name", name), ("layer", layer), ("fan_in", &fi_s)])?;
                } else {
                    vxml::open_attrs(&mut w, "crate", &[("name", name), ("layer", layer), ("fan_in", &fi_s), ("doc", doc_first)])?;
                }
                for m in mods.unwrap() {
                    let sym_s = m.symbols.to_string();
                    let hfi_s = m.hotspot_fan_in.to_string();
                    vxml::empty(&mut w, "module", &[
                        ("name", m.module.as_str()),
                        ("symbols", &sym_s),
                        ("role", m.role.as_str()),
                        ("hotspot", m.hotspot.as_str()),
                        ("hotspot_fan_in", &hfi_s),
                    ])?;
                }
                vxml::close(&mut w, "crate")?;
            }
        }
        vxml::close(&mut w, "crates")?;
    }
    if !edges.is_empty() {
        vxml::open(&mut w, "edges")?;
        for (from, to) in &edges {
            vxml::empty(&mut w, "edge", &[("from_crate", from), ("to_crate", to)])?;
        }
        vxml::close(&mut w, "edges")?;
    }
    vxml::close(&mut w, "deps")?;
    vxml::finish_stream(w)?;
    Ok(())
}

struct ModuleRow {
    module: String,
    symbols: usize,
    role: String,
    hotspot: String,
    hotspot_fan_in: usize,
}

/// Derive the crate root directory prefix from the crate node's entry file.
/// e.g. `./src/main.rs` → `./src/`  (single-crate)
///      `./domain/src/lib.rs` → `./domain/src/`  (workspace member)
fn crate_src_prefix(crate_file: &str) -> String {
    let path = std::path::Path::new(crate_file);
    if let Some(parent) = path.parent() {
        let s = parent.to_string_lossy();
        if s.is_empty() || s == "." {
            return "./src/".to_string();
        }
        return format!("{}/", s);
    }
    "./src/".to_string()
}

/// Extract the top-level module name from a file path relative to the crate src prefix.
/// `./src/query.rs`            → Some("query")
/// `./src/lsp/mod.rs`          → Some("lsp")
/// `./src/lsp/client.rs`       → Some("lsp")
/// `./src/lib.rs`              → None  (crate root)
/// `./src/main.rs`             → None  (crate root)
fn top_level_module(file: &str, src_prefix: &str) -> Option<String> {
    let rel = file.strip_prefix(src_prefix)?;
    let first = rel.split('/').next()?;
    let module = first.strip_suffix(".rs").unwrap_or(first);
    if matches!(module, "lib" | "main" | "mod") {
        return None;
    }
    Some(module.to_string())
}

fn build_crate_modules(
    conn: &Connection,
    crates: &[(String, Option<String>, String)],
) -> Result<std::collections::HashMap<String, Vec<ModuleRow>>> {
    // Load all non-crate symbol nodes with their incoming edge count (fan_in).
    // We include all edge types that represent real usage.
    let mut sym_stmt = conn.prepare(
        "SELECT n.file, n.name, n.role,
                COUNT(e.id) as fan_in
         FROM nodes n
         LEFT JOIN edges e ON e.to_id = n.id
              AND e.edge_type IN ('CALLS_RESOLVED','CALLS_TRUSTED','CALLS_INFERRED')
         WHERE n.kind NOT IN ('crate')
           AND n.language NOT IN ('workspace')
         GROUP BY n.id
         ORDER BY n.file, fan_in DESC",
    )?;

    struct SymRow { file: String, name: String, role: String, fan_in: usize }
    let syms: Vec<SymRow> = sym_stmt
        .query_map([], |r| {
            Ok(SymRow {
                file:   r.get::<_, String>(0)?,
                name:   r.get::<_, String>(1)?,
                role:   r.get::<_, String>(2)?,
                fan_in: r.get::<_, i64>(3)? as usize,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut result = std::collections::HashMap::new();

    for (crate_name, _doc, crate_file) in crates {
        let prefix = crate_src_prefix(crate_file);

        // module -> (symbol_count, role_counts, hotspot_name, hotspot_fan_in)
        let mut modules: std::collections::HashMap<
            String,
            (usize, std::collections::HashMap<String, usize>, String, usize),
        > = std::collections::HashMap::new();

        for sym in &syms {
            let Some(module) = top_level_module(&sym.file, &prefix) else { continue };
            let entry = modules.entry(module).or_insert_with(|| {
                (0, std::collections::HashMap::new(), String::new(), 0)
            });
            entry.0 += 1;
            *entry.1.entry(sym.role.clone()).or_insert(0) += 1;
            // First symbol encountered per module (ordered by fan_in desc) is the hotspot.
            if entry.3 == 0 && entry.2.is_empty() {
                entry.2 = sym.name.clone();
                entry.3 = sym.fan_in;
            }
        }

        let mut rows: Vec<ModuleRow> = modules
            .into_iter()
            .map(|(module, (symbols, role_counts, hotspot, hotspot_fan_in))| {
                let role = role_counts
                    .into_iter()
                    .max_by_key(|(_, c)| *c)
                    .map(|(r, _)| r)
                    .unwrap_or_else(|| "unknown".to_string());
                ModuleRow { module, symbols, role, hotspot, hotspot_fan_in }
            })
            .collect();

        // Sort by symbol count descending, then module name for stability.
        rows.sort_by(|a, b| b.symbols.cmp(&a.symbols).then(a.module.cmp(&b.module)));
        result.insert(crate_name.clone(), rows);
    }

    Ok(result)
}

fn resolve_candidates(
    conn: &Connection,
    query: &str,
    language: Option<&str>,
) -> Result<(String, Vec<ResolveCandidate>)> {
    let key = query.strip_prefix("sym:").unwrap_or(query).trim();
    if key.is_empty() {
        return Ok(("empty".to_string(), Vec::new()));
    }

    if let Ok(id) = key.parse::<i64>() {
        let rows = fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.id = ?1",
            rusqlite::params![id],
        )?;
        return Ok(("id-exact".to_string(), rows));
    }

    if key.contains("::") {
        let stable = fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.stable_id = ?1",
            rusqlite::params![key],
        )?;
        if !stable.is_empty() {
            return Ok(("stable-id-exact".to_string(), stable));
        }
        let qualified = fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.qualified_name = ?1",
            rusqlite::params![key],
        )?;
        if !qualified.is_empty() {
            return Ok(("qualified-name-exact".to_string(), qualified));
        }
        let ns_like = format!("%{}%", key.replace("::", "%"));
        let ns_rows = fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1",
            rusqlite::params![ns_like],
        )?;
        if !ns_rows.is_empty() {
            return Ok(("namespace-shortcut-ranked".to_string(), ns_rows));
        }
    }

    let name_exact = if let Some(lang) = language {
        fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.name = ?1 AND n.language = ?2",
            rusqlite::params![key, lang],
        )?
    } else {
        fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.name = ?1",
            rusqlite::params![key],
        )?
    };
    if !name_exact.is_empty() {
        return Ok(("name-exact-ranked".to_string(), name_exact));
    }

    let like_term = if key.contains("::") {
        format!("%{}%", key.replace("::", "%"))
    } else {
        format!("%{}%", key)
    };
    let like_hits = if let Some(lang) = language {
        fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.language = ?2
               AND (n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1)",
            rusqlite::params![like_term, lang],
        )?
    } else {
        fetch_candidates(
            conn,
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id,
                    (SELECT COUNT(*) FROM edges e WHERE e.to_id = n.id) AS fan_in
             FROM nodes n
             WHERE n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1",
            rusqlite::params![like_term],
        )?
    };
    Ok(("substring-ranked".to_string(), like_hits))
}

fn fetch_candidates<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<ResolveCandidate>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok(ResolveCandidate {
            id: r.get(0)?,
            name: r.get(1)?,
            qualified_name: r.get(2)?,
            kind: r.get(3)?,
            role: r.get(4)?,
            file: r.get(5)?,
            signature: r.get(6)?,
            stable_id: r.get(7)?,
            fan_in: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn symbols(
    fts_query: &str,
    language: Option<&str>,
    file_filter: Option<&str>,
    context_filter: Option<&str>,
    crate_filter: Option<&str>,
    exclude_tests: bool,
    md: bool,
) -> Result<()> {
    let conn = open_db()?;
    let key = fts_query.strip_prefix("sym:").unwrap_or(fts_query);
    let wildcard_infix = fts_query.starts_with('*')
        || fts_query
            .trim_end_matches('*')
            .chars()
            .any(|c| c == '*');
    let use_namespace_fallback = key.contains("::") || fts_query.starts_with("sym:");
    let use_like_wildcard = wildcard_infix && !fts_query.contains(' ');

    let rows_data: Vec<SymbolSearchRow> = if use_namespace_fallback {
        let like_term = if key.contains("::") {
            format!("%{}%", key.replace("::", "%"))
        } else {
            format!("%{}%", key)
        };
        if let Some(lang) = language {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
                 FROM nodes n
                 WHERE n.language = ?2
                   AND (n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1)
                 ORDER BY n.file, n.name, n.id",
            )?;
            let rows = stmt.query_map(rusqlite::params![like_term, lang], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
                 FROM nodes n
                 WHERE n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1
                 ORDER BY n.file, n.name, n.id",
            )?;
            let rows = stmt.query_map(rusqlite::params![like_term], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
    } else if use_like_wildcard {
        let like_term = format!("%{}%", key.replace('*', "%"));
        if let Some(lang) = language {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
                 FROM nodes n
                 WHERE n.language = ?2
                   AND (n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1)
                 ORDER BY n.file, n.name, n.id",
            )?;
            let rows = stmt.query_map(rusqlite::params![like_term, lang], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
                 FROM nodes n
                 WHERE n.stable_id LIKE ?1 OR n.qualified_name LIKE ?1 OR n.name LIKE ?1
                 ORDER BY n.file, n.name, n.id",
            )?;
            let rows = stmt.query_map(rusqlite::params![like_term], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
    } else {
        // FTS can be brittle for free-text with punctuation (e.g. hyphenated terms).
        // Fallback to tokenized LIKE matching when FTS parse fails.
        match fts_symbol_rows(&conn, fts_query, language) {
            Ok(rows) => rows,
            Err(_) => {
                let like = tokenized_like_rows(&conn, fts_query, language)?;
                if like.is_empty() {
                    // Preserve original FTS error if fallback also misses.
                    fts_symbol_rows(&conn, fts_query, language)?
                } else {
                    like
                }
            }
        }
    };

    let ws_graph = if crate_filter.is_some() {
        crate::workspace::detect(".")
    } else {
        None
    };
    let rows_data: Vec<SymbolSearchRow> = rows_data
        .into_iter()
        .filter(|(_, _, _, _, role, file, _, _)| {
            file_filter.is_none_or(|f| file.contains(f))
                && context_filter
                    .is_none_or(|ctx| crate::arch::file_to_context(file) == ctx)
                && crate_filter.is_none_or(|want| {
                    ws_graph.as_ref().is_some_and(|ws| {
                        symbol_crate_for_file(ws, file).is_some_and(|c| c == want)
                    })
                })
                && (!exclude_tests || !is_testish(file, role))
        })
        .collect();

    let count = rows_data.len();
    info!(count, "symbol search matches");
    if md {
        println!("| id | name | qualified_name | kind | role | file | signature | stable_id |");
        println!("|---:|---|---|---|---|---|---|---|");
        for (id, name, qualified_name, kind, role, file, sig, stable_id) in &rows_data {
            let sig = sig.as_deref().unwrap_or("").replace('|', "\\|");
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                id,
                name.replace('|', "\\|"),
                qualified_name.replace('|', "\\|"),
                kind.replace('|', "\\|"),
                role.replace('|', "\\|"),
                file.replace('|', "\\|"),
                sig,
                stable_id.replace('|', "\\|"),
            );
        }
    } else {
        let mut w = vxml::new_stream_writer();
        vxml::open(&mut w, "symbols")?;
        for (id, name, qualified_name, kind, role, file, sig, stable_id) in &rows_data {
            append_symbol_xml(
                &mut w,
                *id,
                name,
                qualified_name,
                kind,
                role,
                file,
                sig.as_deref(),
                stable_id,
            );
        }
        vxml::close(&mut w, "symbols")?;
        vxml::finish_stream(w)?;
    }
    Ok(())
}

fn fts_symbol_rows(
    conn: &Connection,
    fts_query: &str,
    language: Option<&str>,
) -> Result<Vec<SymbolSearchRow>> {
    if let Some(lang) = language {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
             FROM fts_symbols f
             JOIN nodes n ON n.id = f.node_id
             WHERE fts_symbols MATCH ?1 AND n.language = ?2
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(rusqlite::params![fts_query, lang], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?;
        return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
         FROM fts_symbols f
         JOIN nodes n ON n.id = f.node_id
         WHERE fts_symbols MATCH ?1
         ORDER BY rank",
    )?;
    let rows = stmt.query_map(rusqlite::params![fts_query], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn tokenized_like_rows(
    conn: &Connection,
    query: &str,
    language: Option<&str>,
) -> Result<Vec<SymbolSearchRow>> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        "SELECT n.id, n.name, n.qualified_name, n.kind, n.role, n.file, n.signature, n.stable_id
         FROM nodes n WHERE 1=1",
    );
    if language.is_some() {
        sql.push_str(" AND n.language = ?");
    }
    for _ in &tokens {
        sql.push_str(
            " AND (n.name LIKE ? OR n.qualified_name LIKE ? OR n.stable_id LIKE ? OR n.signature LIKE ?)",
        );
    }
    sql.push_str(" ORDER BY n.file, n.name, n.id");

    let mut params: Vec<String> = Vec::new();
    if let Some(lang) = language {
        params.push(lang.to_string());
    }
    for t in &tokens {
        let like = format!("%{}%", t.replace("::", "%"));
        params.push(like.clone());
        params.push(like.clone());
        params.push(like.clone());
        params.push(like);
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[allow(clippy::too_many_arguments)]
fn append_symbol_xml<W: std::io::Write>(
    w: &mut quick_xml::Writer<W>,
    id: i64,
    name: &str,
    qualified_name: &str,
    kind: &str,
    role: &str,
    file: &str,
    sig: Option<&str>,
    stable_id: &str,
) {
    let id_s = id.to_string();
    let mut attrs = vec![
        ("id", id_s.as_str()),
        ("name", name),
        ("qualified_name", qualified_name),
        ("kind", kind),
        ("role", role),
        ("file", file),
        ("stable_id", stable_id),
    ];
    if let Some(s) = sig {
        attrs.push(("signature", s));
    }
    vxml::empty(w, "symbol", &attrs).expect("xml");
}

struct NodeRow {
    id: i64,
    name: String,
    kind: String,
    file: String,
    range_start: i64,
    range_end: i64,
    signature: Option<String>,
    visibility: String,
    doc: Option<String>,
    fan_in: i64,
    fan_out: i64,
    role: String,
    role_confidence: f64,
    stable_id: String,
}

struct NeighborRow {
    id: i64,
    name: String,
    kind: String,
    file: String,
    range_start: i64,
    range_end: i64,
    signature: Option<String>,
    depth: i64,
    edge_type: Option<String>,
    #[allow(dead_code)]
    source: Option<String>,
    #[allow(dead_code)]
    confidence: Option<f64>,
    visibility: String,
    doc: Option<String>,
    fan_in: i64,
    fan_out: i64,
    role: String,
    role_confidence: f64,
    stable_id: String,
}

#[derive(Clone, Copy)]
pub struct OutputControl {
    pub budget_lines: Option<usize>,
    pub budget_tokens: Option<usize>,
    pub offset: usize,
    pub compact: bool,
}

#[derive(Clone, Copy)]
struct WindowMeta {
    total_items: usize,
    offset: usize,
    shown_items: usize,
    truncated: bool,
    next_offset: Option<usize>,
    budget_lines: Option<usize>,
    budget_tokens: Option<usize>,
}

fn apply_window<T>(
    rows: &[T],
    offset: usize,
    budget_lines: Option<usize>,
    budget_tokens: Option<usize>,
    token_cost: impl Fn(&T) -> usize,
) -> (std::ops::Range<usize>, WindowMeta) {
    let total = rows.len();
    let start = offset.min(total);
    let mut end = total;

    if let Some(max_lines) = budget_lines {
        end = end.min(start.saturating_add(max_lines));
    }
    if let Some(max_tokens) = budget_tokens {
        let mut used = 0usize;
        let mut idx = start;
        while idx < end {
            let cost = token_cost(&rows[idx]);
            if idx > start && used.saturating_add(cost) > max_tokens {
                break;
            }
            used = used.saturating_add(cost);
            idx += 1;
        }
        end = idx;
    }

    let shown = end.saturating_sub(start);
    let truncated = end < total;
    let next_offset = if truncated { Some(end) } else { None };
    (
        start..end,
        WindowMeta {
            total_items: total,
            offset: start,
            shown_items: shown,
            truncated,
            next_offset,
            budget_lines,
            budget_tokens,
        },
    )
}

fn estimate_neighbor_tokens(n: &NeighborRow) -> usize {
    // Deterministic approximation (4 chars/token) from stable fields.
    let mut chars = n.name.len()
        + n.kind.len()
        + n.file.len()
        + n.visibility.len()
        + n.role.len()
        + n.stable_id.len()
        + 24;
    if let Some(sig) = &n.signature {
        chars += sig.len();
    }
    if let Some(doc) = &n.doc {
        chars += doc.len().min(256);
    }
    (chars / 4).max(1)
}

fn estimate_node_tokens(n: &NodeRow) -> usize {
    let mut chars = n.name.len()
        + n.kind.len()
        + n.file.len()
        + n.visibility.len()
        + n.role.len()
        + n.stable_id.len()
        + 24;
    if let Some(sig) = &n.signature {
        chars += sig.len();
    }
    if let Some(doc) = &n.doc {
        chars += doc.len().min(256);
    }
    (chars / 4).max(1)
}

struct AnnotationRow {
    intent: Option<String>,
    behavior: Option<String>,
    tags: Option<String>,
    source: String,
    confidence: f64,
    stale: bool,
}

fn write_annotation_xml<W: std::io::Write>(w: &mut quick_xml::Writer<W>, ann: &AnnotationRow) {
    let conf_s = format!("{:.1}", ann.confidence);
    let stale_s = ann.stale.to_string();
    vxml::open_attrs(
        w,
        "annotation",
        &[
            ("source", &ann.source),
            ("confidence", &conf_s),
            ("stale", &stale_s),
        ],
    )
    .expect("xml");
    if let Some(intent) = &ann.intent {
        vxml::text_tag(w, "intent", intent).expect("xml");
    }
    if let Some(behavior) = &ann.behavior {
        vxml::text_tag(w, "behavior", behavior).expect("xml");
    }
    if let Some(tags) = &ann.tags {
        vxml::text_tag(w, "tags", tags).expect("xml");
    }
    vxml::close(w, "annotation").expect("xml");
}

struct EdgeInfo {
    edge_type: String,
    from_id: i64,
    to_id: i64,
    from_name: String,
    to_name: String,
    source: String,
    #[allow(dead_code)]
    confidence: f64,
}

struct NodeDiagnostic {
    code: String,
    level: String,
    message: String,
    suggestion: Option<String>,
}

fn fetch_annotations(
    conn: &Connection,
    ids: &[i64],
) -> std::collections::HashMap<i64, AnnotationRow> {
    if ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let placeholders: String = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT a.node_id, a.intent, a.behavior, a.tags, a.source, a.confidence,
                a.content_hash_at_annotation != n.content_hash AS stale
         FROM annotations a JOIN nodes n ON n.id = a.node_id
         WHERE a.node_id IN ({})",
        placeholders
    );
    let mut map = std::collections::HashMap::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        let params = rusqlite::params_from_iter(ids.iter().copied());
        if let Ok(rows) = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                AnnotationRow {
                    intent: r.get(1)?,
                    behavior: r.get(2)?,
                    tags: r.get(3)?,
                    source: r.get(4)?,
                    confidence: r.get(5)?,
                    stale: r.get::<_, bool>(6).unwrap_or(false),
                },
            ))
        }) {
            for row in rows.flatten() {
                map.insert(row.0, row.1);
            }
        }
    }
    map
}

fn fetch_node_diagnostics(conn: &Connection, node_id: i64) -> Vec<NodeDiagnostic> {
    let mut stmt = match conn.prepare_cached(
        "SELECT code, level, message, suggestion
         FROM node_diagnostics
         WHERE node_id = ?1
         ORDER BY id ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map(rusqlite::params![node_id], |r| {
        Ok(NodeDiagnostic {
            code: r.get(0)?,
            level: r.get(1)?,
            message: r.get(2)?,
            suggestion: r.get(3)?,
        })
    })
    .ok()
    .into_iter()
    .flat_map(|rows| rows.filter_map(|r| r.ok()))
    .collect()
}

fn write_diagnostics_xml<W: std::io::Write>(w: &mut quick_xml::Writer<W>, rows: &[NodeDiagnostic]) {
    if rows.is_empty() {
        return;
    }
    let count_s = rows.len().to_string();
    vxml::open_attrs(w, "diagnostics", &[("count", &count_s)]).expect("xml");
    for d in rows {
        let mut attrs = vec![("code", d.code.as_str()), ("level", d.level.as_str())];
        if let Some(s) = &d.suggestion {
            attrs.push(("suggestion", s.as_str()));
        }
        vxml::open_attrs(w, "diagnostic", &attrs).expect("xml");
        vxml::text_tag(w, "message", &d.message).expect("xml");
        vxml::close(w, "diagnostic").expect("xml");
    }
    vxml::close(w, "diagnostics").expect("xml");
}

fn read_snippet(
    file: &str,
    line_start: i64,
    line_end: i64,
    max_lines: Option<usize>,
) -> Option<String> {
    let content = fs::read_to_string(Path::new(file)).ok()?;
    let all_lines: Vec<&str> = content.lines().collect();
    let start = ((line_start - 1) as usize).min(all_lines.len());
    let end = (line_end as usize).min(all_lines.len());
    let lines = &all_lines[start..end];
    if let Some(max) = max_lines {
        if lines.len() > max {
            let truncated = lines[..max].join("\n");
            return Some(format!(
                "{}\n// \u{2026}{} more lines",
                truncated,
                lines.len() - max
            ));
        }
    }
    Some(lines.join("\n"))
}

fn read_call_site_snippet(
    file: &str,
    range_start: i64,
    range_end: i64,
    target_name: &str,
) -> Option<String> {
    let content = fs::read_to_string(Path::new(file)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = ((range_start - 1) as usize).min(lines.len());
    let end = (range_end as usize).min(lines.len());
    let body = &lines[start..end];

    let hits: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(target_name))
        .map(|(i, _)| i)
        .collect();

    if hits.is_empty() {
        return None;
    }

    let mut out: Vec<&str> = Vec::new();
    let mut last_end = 0usize;
    for i in hits {
        let ctx_start = i.saturating_sub(1);
        let ctx_end = (i + 2).min(body.len());
        if ctx_start > last_end && !out.is_empty() {
            out.push("...");
        }
        let from = ctx_start.max(last_end);
        out.extend_from_slice(&body[from..ctx_end]);
        last_end = ctx_end;
    }
    Some(out.join("\n"))
}

pub fn graph(
    symbols: &[String],
    depth: usize,
    _format: &str,
    show_trust: bool,
    snippets: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }
    if symbols.len() > 1 {
        let count_s = symbols.len().to_string();
        write_wrapper_start("graphs", &[("count", &count_s)])?;
        for symbol in symbols {
            graph_one(symbol, depth, show_trust, snippets, max_snippet_lines, control)?;
        }
        write_wrapper_end("graphs")?;
        return Ok(());
    }
    graph_one(
        &symbols[0],
        depth,
        show_trust,
        snippets,
        max_snippet_lines,
        control,
    )
}

fn graph_one(
    symbol: &str,
    depth: usize,
    show_trust: bool,
    snippets: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    let focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence, n.stable_id
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                    stable_id: r.get(13)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    if show_trust {
        let mut edge_stmt = conn.prepare(
            "SELECT e.edge_type, e.from_id, e.to_id,
                    n_from.name, n_to.name, e.source, e.confidence
             FROM edges e
             JOIN nodes n_from ON n_from.id = e.from_id
             JOIN nodes n_to ON n_to.id = e.to_id
             WHERE e.from_id = ?1 OR e.to_id = ?1",
        )?;
        let edges: Vec<EdgeInfo> = edge_stmt
            .query_map(rusqlite::params![root_id], |r| {
                Ok(EdgeInfo {
                    edge_type: r.get(0)?,
                    from_id: r.get(1)?,
                    to_id: r.get(2)?,
                    from_name: r.get(3)?,
                    to_name: r.get(4)?,
                    source: r.get(5)?,
                    confidence: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        print_xml_trust_split(&focus, &edges)?;
    } else {
        let mut stmt = conn.prepare(
            "WITH RECURSIVE neighborhood(id, depth) AS (
                SELECT ?1, 0
                UNION ALL
                SELECT e.to_id, n.depth + 1
                FROM neighborhood n JOIN edges e ON e.from_id = n.id
                WHERE n.depth < ?2
            )
            SELECT DISTINCT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
                   nd.signature, nh.depth,
                   (SELECT edge_type FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   (SELECT source FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   (SELECT confidence FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
                   nd.visibility, nd.doc,
                   (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
                   (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
                   nd.role, nd.role_confidence, nd.stable_id
            FROM nodes nd JOIN neighborhood nh ON nd.id = nh.id
            WHERE nd.id != ?1
            ORDER BY nh.depth, nd.name",
        )?;

        let neighbors: Vec<NeighborRow> = stmt
            .query_map(rusqlite::params![root_id, depth as i64], |r| {
                Ok(NeighborRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    depth: r.get(7)?,
                    edge_type: r.get(8)?,
                    source: r.get(9)?,
                    confidence: r.get(10)?,
                    visibility: r.get(11)?,
                    doc: r.get(12)?,
                    fan_in: r.get(13)?,
                    fan_out: r.get(14)?,
                    role: r.get(15)?,
                    role_confidence: r.get(16)?,
                    stable_id: r.get(17)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let edge_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
            rusqlite::params![root_id],
            |r| r.get(0),
        )?;

        let mut w = vxml::new_stream_writer();
        let (window, meta) = apply_window(
            &neighbors,
            control.offset,
            control.budget_lines,
            control.budget_tokens,
            estimate_neighbor_tokens,
        );
        write_graph_xml(
            &mut w,
            &conn,
            &focus,
            &neighbors[window],
            edge_count,
            snippets,
            snippets,
            max_snippet_lines,
            Some(meta),
            control.compact,
        )?;
        vxml::finish_stream(w)?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_graph_xml<W: std::io::Write>(
    w: &mut quick_xml::Writer<W>,
    conn: &Connection,
    focus: &NodeRow,
    neighbors: &[NeighborRow],
    edge_count: i64,
    snippets: bool,
    show_focus_snippet: bool,
    max_snippet_lines: Option<usize>,
    meta: Option<WindowMeta>,
    compact: bool,
) -> Result<()> {
    let snippet = if show_focus_snippet && !compact {
        read_snippet(
            &focus.file,
            focus.range_start,
            focus.range_end,
            max_snippet_lines,
        )
    } else {
        None
    };

    let mut all_ids: Vec<i64> = vec![focus.id];
    all_ids.extend(neighbors.iter().map(|n| n.id));
    let annotations = if compact {
        std::collections::HashMap::new()
    } else {
        fetch_annotations(conn, &all_ids)
    };
    let focus_diagnostics = if compact {
        Vec::new()
    } else {
        fetch_node_diagnostics(conn, focus.id)
    };

    let root_id_s = focus.id.to_string();
    let nodes_s = (1 + neighbors.len()).to_string();
    let edges_s = edge_count.to_string();
    let mut root_attrs = vec![
        ("root_id", root_id_s.as_str()),
        ("tokens", "streaming"),
        ("nodes", nodes_s.as_str()),
        ("edges", edges_s.as_str()),
    ];
    let compact_s = compact.to_string();
    if compact {
        root_attrs.push(("compact", compact_s.as_str()));
    }
    let mut total_items_s = None::<String>;
    let mut offset_s = None::<String>;
    let mut shown_items_s = None::<String>;
    let mut truncated_s = None::<String>;
    let mut next_offset_s = None::<String>;
    let mut budget_lines_s = None::<String>;
    let mut budget_tokens_s = None::<String>;
    if let Some(m) = meta {
        total_items_s = Some(m.total_items.to_string());
        offset_s = Some(m.offset.to_string());
        shown_items_s = Some(m.shown_items.to_string());
        truncated_s = Some(m.truncated.to_string());
        if let Some(next) = m.next_offset {
            next_offset_s = Some(next.to_string());
        }
        if let Some(b) = m.budget_lines {
            budget_lines_s = Some(b.to_string());
        }
        if let Some(b) = m.budget_tokens {
            budget_tokens_s = Some(b.to_string());
        }
    }
    if let Some(s) = total_items_s.as_ref() {
        root_attrs.push(("total_items", s.as_str()));
    }
    if let Some(s) = offset_s.as_ref() {
        root_attrs.push(("offset", s.as_str()));
    }
    if let Some(s) = shown_items_s.as_ref() {
        root_attrs.push(("shown_items", s.as_str()));
    }
    if let Some(s) = truncated_s.as_ref() {
        root_attrs.push(("truncated", s.as_str()));
    }
    if let Some(s) = next_offset_s.as_ref() {
        root_attrs.push(("next_offset", s.as_str()));
    }
    if let Some(s) = budget_lines_s.as_ref() {
        root_attrs.push(("budget_lines", s.as_str()));
    }
    if let Some(s) = budget_tokens_s.as_ref() {
        root_attrs.push(("budget_tokens", s.as_str()));
    }
    vxml::open_attrs(w, "graph", &root_attrs).expect("xml");

    vxml::open(w, "focus").expect("xml");
    let id_s = focus.id.to_string();
    let range_s = format!("L{}-L{}", focus.range_start, focus.range_end);
    let fan_in_s = focus.fan_in.to_string();
    let fan_out_s = focus.fan_out.to_string();
    let role_conf_s = format!("{:.2}", focus.role_confidence);
    let role_uncertain_s = (focus.role_confidence < 0.6).to_string();
    let mut attrs = vec![
        ("id", id_s.as_str()),
        ("stable_id", focus.stable_id.as_str()),
        ("name", focus.name.as_str()),
        ("kind", focus.kind.as_str()),
        ("file", focus.file.as_str()),
        ("range", range_s.as_str()),
        ("visibility", focus.visibility.as_str()),
        ("fan_in", fan_in_s.as_str()),
        ("fan_out", fan_out_s.as_str()),
        ("role", focus.role.as_str()),
        ("role_confidence", role_conf_s.as_str()),
    ];
    if focus.role_confidence < 0.6 {
        attrs.push(("role_uncertain", role_uncertain_s.as_str()));
    }
    vxml::open_attrs(w, "node", &attrs).expect("xml");
    if !compact {
        if let Some(sig) = &focus.signature {
            vxml::text_tag(w, "signature", sig).expect("xml");
        }
    }
    if !compact {
        if let Some(doc) = &focus.doc {
            if !doc.is_empty() {
                vxml::text_tag(w, "doc", doc).expect("xml");
            }
        }
    }
    if !compact {
        if let Some(ann) = annotations.get(&focus.id) {
            write_annotation_xml(w, ann);
        }
    }
    if !compact {
        write_diagnostics_xml(w, &focus_diagnostics);
    }
    if !compact {
        if let Some(snip) = snippet {
            vxml::text_tag(w, "snippet", &snip).expect("xml");
        }
    }
    vxml::close(w, "node").expect("xml");
    vxml::close(w, "focus").expect("xml");

    let max_depth = neighbors.iter().map(|n| n.depth).max().unwrap_or(0);
    for d in 1..=max_depth {
        let depth_s = d.to_string();
        vxml::open_attrs(w, "neighbors", &[("depth", &depth_s)]).expect("xml");
        for n in neighbors.iter().filter(|n| n.depth == d) {
            let id_s = n.id.to_string();
            let range_s = format!("L{}-L{}", n.range_start, n.range_end);
            let fan_in_s = n.fan_in.to_string();
            let fan_out_s = n.fan_out.to_string();
            let role_conf_s = format!("{:.2}", n.role_confidence);
            let role_uncertain_s = (n.role_confidence < 0.6).to_string();
            let mut attrs = vec![
                ("id", id_s.as_str()),
                ("stable_id", n.stable_id.as_str()),
                ("name", n.name.as_str()),
                ("kind", n.kind.as_str()),
                ("file", n.file.as_str()),
                ("range", range_s.as_str()),
                ("visibility", n.visibility.as_str()),
                ("fan_in", fan_in_s.as_str()),
                ("fan_out", fan_out_s.as_str()),
                ("role", n.role.as_str()),
                ("role_confidence", role_conf_s.as_str()),
            ];
            if n.role_confidence < 0.6 {
                attrs.push(("role_uncertain", role_uncertain_s.as_str()));
            }
            if let Some(et) = &n.edge_type {
                attrs.push(("edge_type", et.as_str()));
            }
            if !compact {
                if let Some(sig) = &n.signature {
                    attrs.push(("signature", sig.as_str()));
                }
            }
            let has_doc = !compact && n.doc.as_ref().is_some_and(|d| !d.is_empty());
            let neighbor_snippet = if snippets && !compact {
                read_snippet(&n.file, n.range_start, n.range_end, max_snippet_lines)
            } else {
                None
            };
            let neighbor_annotation = if compact { None } else { annotations.get(&n.id) };
            if has_doc || neighbor_snippet.is_some() || neighbor_annotation.is_some() {
                vxml::open_attrs(w, "node", &attrs).expect("xml");
                if let Some(doc) = &n.doc {
                    if !doc.is_empty() {
                        vxml::text_tag(w, "doc", doc).expect("xml");
                    }
                }
                if let Some(ann) = neighbor_annotation {
                    write_annotation_xml(w, ann);
                }
                if let Some(snip) = neighbor_snippet {
                    vxml::text_tag(w, "snippet", &snip).expect("xml");
                }
                vxml::close(w, "node").expect("xml");
            } else {
                vxml::empty(w, "node", &attrs).expect("xml");
            }
        }
        vxml::close(w, "neighbors").expect("xml");
    }
    vxml::close(w, "graph").expect("xml");

    Ok(())
}

fn print_xml_trust_split(focus: &NodeRow, edges: &[EdgeInfo]) -> Result<()> {
    let snippet = read_snippet(&focus.file, focus.range_start, focus.range_end, None);

    let trusted: Vec<&EdgeInfo> = edges
        .iter()
        .filter(|e| is_trusted_source(&e.source))
        .collect();
    let syntax: Vec<&EdgeInfo> = edges.iter().filter(|e| !is_trusted_source(&e.source)).collect();

    let mut w = vxml::new_stream_writer();
    let root_id_s = focus.id.to_string();
    let trusted_len_s = trusted.len().to_string();
    let syntax_len_s = syntax.len().to_string();
    vxml::open_attrs(
        &mut w,
        "graph",
        &[
            ("root_id", &root_id_s),
            ("tokens", "streaming"),
            ("trusted_edges", &trusted_len_s),
            ("syntax_edges", &syntax_len_s),
        ],
    )
    .expect("xml");

    vxml::open(&mut w, "focus").expect("xml");
    let id_s = focus.id.to_string();
    let range_s = format!("L{}-L{}", focus.range_start, focus.range_end);
    vxml::open_attrs(
        &mut w,
        "node",
        &[
            ("id", &id_s),
            ("name", &focus.name),
            ("kind", &focus.kind),
            ("file", &focus.file),
            ("range", &range_s),
        ],
    )
    .expect("xml");
    if let Some(sig) = &focus.signature {
        vxml::text_tag(&mut w, "signature", sig).expect("xml");
    }
    if let Some(snip) = snippet {
        vxml::text_tag(&mut w, "snippet", &snip).expect("xml");
    }
    vxml::close(&mut w, "node").expect("xml");
    vxml::close(&mut w, "focus").expect("xml");

    if !trusted.is_empty() {
        vxml::open_attrs(&mut w, "trusted_edges", &[("confidence", "1.0")]).expect("xml");
        for e in &trusted {
            let from_id_s = e.from_id.to_string();
            let to_id_s = e.to_id.to_string();
            vxml::empty(
                &mut w,
                "edge",
                &[
                    ("type", &e.edge_type),
                    ("from_id", &from_id_s),
                    ("to_id", &to_id_s),
                    ("from_name", &e.from_name),
                    ("to_name", &e.to_name),
                    ("source", &e.source),
                ],
            )
            .expect("xml");
        }
        vxml::close(&mut w, "trusted_edges").expect("xml");
    }

    if !syntax.is_empty() {
        vxml::open_attrs(&mut w, "syntax_edges", &[("confidence", "0.8")]).expect("xml");
        for e in &syntax {
            let from_id_s = e.from_id.to_string();
            let to_id_s = e.to_id.to_string();
            vxml::empty(
                &mut w,
                "edge",
                &[
                    ("type", &e.edge_type),
                    ("from_id", &from_id_s),
                    ("to_id", &to_id_s),
                    ("from_name", &e.from_name),
                    ("to_name", &e.to_name),
                    ("source", &e.source),
                ],
            )
            .expect("xml");
        }
        vxml::close(&mut w, "syntax_edges").expect("xml");
    }
    vxml::close(&mut w, "graph").expect("xml");

    vxml::finish_stream(w)?;
    Ok(())
}

pub fn blast_radius(
    symbols: &[String],
    depth: usize,
    snippets: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }
    if symbols.len() > 1 {
        let count_s = symbols.len().to_string();
        write_wrapper_start("blast_radii", &[("count", &count_s)])?;
        for symbol in symbols {
            blast_radius_one(symbol, depth, snippets, max_snippet_lines, control)?;
        }
        write_wrapper_end("blast_radii")?;
        return Ok(());
    }
    blast_radius_one(&symbols[0], depth, snippets, max_snippet_lines, control)
}

fn blast_radius_one(
    symbol: &str,
    depth: usize,
    snippets: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;
    let mut focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence, n.stable_id
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                    stable_id: r.get(13)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    let depth_limit = if depth == 0 { 50_i64 } else { depth as i64 };
    let include_type_users = matches!(
        focus.kind.as_str(),
        "struct" | "enum" | "type" | "trait" | "union"
    );
    if include_type_users {
        let type_user_count: i64 = conn
            .query_row(
                "SELECT COUNT(*)
                 FROM nodes n
                 WHERE n.id != ?1
                   AND (n.signature LIKE ('%' || ?2 || '%')
                        OR n.doc LIKE ('%' || ?2 || '%'))",
                rusqlite::params![root_id, focus.name],
                |r| r.get(0),
            )
            .unwrap_or(0);
        focus.fan_in += type_user_count;
    }

    let mut stmt = conn.prepare(
        "WITH RECURSIVE type_users(id) AS (
            SELECT n.id
            FROM nodes n
            WHERE ?4 = 1
              AND n.id != ?1
              AND (n.signature LIKE ('%' || ?3 || '%')
                   OR n.doc LIKE ('%' || ?3 || '%'))
        ),
        dependents(id, depth) AS (
            SELECT ?1, 0
            UNION
            SELECT tu.id, 1
            FROM type_users tu
            UNION ALL
            SELECT e.from_id, d.depth + 1
            FROM dependents d JOIN edges e ON e.to_id = d.id
            WHERE d.depth < ?2
        )
        SELECT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, MIN(nh.depth),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence, nd.stable_id
        FROM nodes nd JOIN dependents nh ON nd.id = nh.id
        WHERE nd.id != ?1
        GROUP BY nd.id
        ORDER BY MIN(nh.depth), nd.name",
    )?;

    let rows: Vec<(NodeRow, i64)> = stmt
        .query_map(
            rusqlite::params![root_id, depth_limit, focus.name, include_type_users],
            |r| {
            Ok((
                NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(8)?,
                    doc: r.get(9)?,
                    fan_in: r.get(10)?,
                    fan_out: r.get(11)?,
                    role: r.get(12)?,
                    role_confidence: r.get(13)?,
                    stable_id: r.get(14)?,
                },
                r.get::<_, i64>(7)?,
            ))
        },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut w = vxml::new_stream_writer();
    let (window, meta) = apply_window(
        &rows,
        control.offset,
        control.budget_lines,
        control.budget_tokens,
        |(n, _)| estimate_node_tokens(n),
    );
    write_blast_radius_xml(
        &mut w,
        &conn,
        &focus,
        &rows[window],
        snippets,
        snippets,
        max_snippet_lines,
        Some(meta),
        control.compact,
    )?;
    vxml::finish_stream(w)?;
    Ok(())
}

pub fn context(
    symbols: &[String],
    depth: usize,
    blast_depth: usize,
    snippets: bool,
    edit_mode: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    if symbols.is_empty() {
        return Ok(());
    }
    if symbols.len() > 1 {
        let count_s = symbols.len().to_string();
        write_wrapper_start("contexts", &[("count", &count_s)])?;
        for symbol in symbols {
            context_one(
                symbol,
                depth,
                blast_depth,
                snippets,
                edit_mode,
                max_snippet_lines,
                control,
            )?;
        }
        write_wrapper_end("contexts")?;
        return Ok(());
    }
    context_one(
        &symbols[0],
        depth,
        blast_depth,
        snippets,
        edit_mode,
        max_snippet_lines,
        control,
    )
}

fn context_one(
    symbol: &str,
    depth: usize,
    blast_depth: usize,
    snippets: bool,
    edit_mode: bool,
    max_snippet_lines: Option<usize>,
    control: OutputControl,
) -> Result<()> {
    // edit_mode: signature+doc+annotation only on focus, no neighbor snippets, depth=1 each side
    let (snippets, depth, blast_depth, show_focus_snippet) = if edit_mode {
        (false, 1usize, 1usize, false)
    } else {
        (snippets, depth, blast_depth, snippets)
    };
    let conn = open_db()?;
    let root_id = resolve_symbol_id(&conn, symbol)?;

    // --- graph neighborhood ---
    let focus: NodeRow = conn
        .query_row(
            "SELECT n.id, n.name, n.kind, n.file, n.range_start, n.range_end, n.signature,
                    n.visibility, n.doc,
                    (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                    (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                    n.role, n.role_confidence, n.stable_id
             FROM nodes n WHERE n.id = ?1",
            rusqlite::params![root_id],
            |r| {
                Ok(NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(7)?,
                    doc: r.get(8)?,
                    fan_in: r.get(9)?,
                    fan_out: r.get(10)?,
                    role: r.get(11)?,
                    role_confidence: r.get(12)?,
                    stable_id: r.get(13)?,
                })
            },
        )
        .map_err(|_| anyhow!("node id {} not found", root_id))?;

    let mut neighbor_stmt = conn.prepare(
        "WITH RECURSIVE neighborhood(id, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.to_id, n.depth + 1
            FROM neighborhood n JOIN edges e ON e.from_id = n.id
            WHERE n.depth < ?2
        )
        SELECT DISTINCT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, nh.depth,
               (SELECT edge_type FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               (SELECT source FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               (SELECT confidence FROM edges WHERE from_id = ?1 AND to_id = nd.id LIMIT 1),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence, nd.stable_id
        FROM nodes nd JOIN neighborhood nh ON nd.id = nh.id
        WHERE nd.id != ?1
        ORDER BY nh.depth, nd.name",
    )?;
    let neighbors: Vec<NeighborRow> = neighbor_stmt
        .query_map(rusqlite::params![root_id, depth as i64], |r| {
            Ok(NeighborRow {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                file: r.get(3)?,
                range_start: r.get(4)?,
                range_end: r.get(5)?,
                signature: r.get(6)?,
                depth: r.get(7)?,
                edge_type: r.get(8)?,
                source: r.get(9)?,
                confidence: r.get(10)?,
                visibility: r.get(11)?,
                doc: r.get(12)?,
                fan_in: r.get(13)?,
                fan_out: r.get(14)?,
                role: r.get(15)?,
                role_confidence: r.get(16)?,
                stable_id: r.get(17)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let edge_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM edges WHERE from_id = ?1 OR to_id = ?1",
        rusqlite::params![root_id],
        |r| r.get(0),
    )?;
    // --- blast radius (callers) ---
    let blast_limit = if blast_depth == 0 {
        50_i64
    } else {
        blast_depth as i64
    };
    let mut dep_stmt = conn.prepare(
        "WITH RECURSIVE dependents(id, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.from_id, d.depth + 1
            FROM dependents d JOIN edges e ON e.to_id = d.id
            WHERE d.depth < ?2
        )
        SELECT nd.id, nd.name, nd.kind, nd.file, nd.range_start, nd.range_end,
               nd.signature, MIN(nh.depth),
               nd.visibility, nd.doc,
               (SELECT COUNT(*) FROM edges WHERE to_id = nd.id) AS fan_in,
               (SELECT COUNT(*) FROM edges WHERE from_id = nd.id) AS fan_out,
               nd.role, nd.role_confidence, nd.stable_id
        FROM nodes nd JOIN dependents nh ON nd.id = nh.id
        WHERE nd.id != ?1
        GROUP BY nd.id
        ORDER BY MIN(nh.depth), nd.name",
    )?;
    let dep_rows: Vec<(NodeRow, i64)> = dep_stmt
        .query_map(rusqlite::params![root_id, blast_limit], |r| {
            Ok((
                NodeRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    file: r.get(3)?,
                    range_start: r.get(4)?,
                    range_end: r.get(5)?,
                    signature: r.get(6)?,
                    visibility: r.get(8)?,
                    doc: r.get(9)?,
                    fan_in: r.get(10)?,
                    fan_out: r.get(11)?,
                    role: r.get(12)?,
                    role_confidence: r.get(13)?,
                    stable_id: r.get(14)?,
                },
                r.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // --- unified context document (streaming) ---
    let mut w = vxml::new_stream_writer();
    let root_id_s = root_id.to_string();
    vxml::open_attrs(
        &mut w,
        "context",
        &[
            ("symbol", &focus.name),
            ("root_id", &root_id_s),
            ("total_tokens", "streaming"),
        ],
    )?;
    let (graph_window, graph_meta) = apply_window(
        &neighbors,
        control.offset,
        control.budget_lines,
        control.budget_tokens,
        estimate_neighbor_tokens,
    );
    write_graph_xml(
        &mut w,
        &conn,
        &focus,
        &neighbors[graph_window],
        edge_count,
        snippets,
        show_focus_snippet,
        max_snippet_lines,
        Some(graph_meta),
        control.compact,
    )?;
    let (blast_window, blast_meta) = apply_window(
        &dep_rows,
        control.offset,
        control.budget_lines,
        control.budget_tokens,
        |(n, _)| estimate_node_tokens(n),
    );
    write_blast_radius_xml(
        &mut w,
        &conn,
        &focus,
        &dep_rows[blast_window],
        snippets,
        show_focus_snippet,
        max_snippet_lines,
        Some(blast_meta),
        control.compact,
    )?;
    vxml::close(&mut w, "context")?;
    vxml::finish_stream(w)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_blast_radius_xml<W: std::io::Write>(
    w: &mut quick_xml::Writer<W>,
    conn: &Connection,
    focus: &NodeRow,
    dependents: &[(NodeRow, i64)],
    snippets: bool,
    show_focus_snippet: bool,
    max_snippet_lines: Option<usize>,
    meta: Option<WindowMeta>,
    compact: bool,
) -> Result<()> {
    let snippet = if show_focus_snippet && !compact {
        read_snippet(
            &focus.file,
            focus.range_start,
            focus.range_end,
            max_snippet_lines,
        )
    } else {
        None
    };

    let mut all_ids: Vec<i64> = vec![focus.id];
    all_ids.extend(dependents.iter().map(|(n, _)| n.id));
    let annotations = if compact {
        std::collections::HashMap::new()
    } else {
        fetch_annotations(conn, &all_ids)
    };
    let focus_diagnostics = if compact {
        Vec::new()
    } else {
        fetch_node_diagnostics(conn, focus.id)
    };

    let root_id_s = focus.id.to_string();
    let dep_count_s = dependents.len().to_string();
    let mut root_attrs = vec![
        ("root_id", root_id_s.as_str()),
        ("root_name", focus.name.as_str()),
        ("tokens", "streaming"),
        ("dependent_count", dep_count_s.as_str()),
    ];
    let compact_s = compact.to_string();
    if compact {
        root_attrs.push(("compact", compact_s.as_str()));
    }
    let mut total_items_s = None::<String>;
    let mut offset_s = None::<String>;
    let mut shown_items_s = None::<String>;
    let mut truncated_s = None::<String>;
    let mut next_offset_s = None::<String>;
    let mut budget_lines_s = None::<String>;
    let mut budget_tokens_s = None::<String>;
    if let Some(m) = meta {
        total_items_s = Some(m.total_items.to_string());
        offset_s = Some(m.offset.to_string());
        shown_items_s = Some(m.shown_items.to_string());
        truncated_s = Some(m.truncated.to_string());
        if let Some(next) = m.next_offset {
            next_offset_s = Some(next.to_string());
        }
        if let Some(b) = m.budget_lines {
            budget_lines_s = Some(b.to_string());
        }
        if let Some(b) = m.budget_tokens {
            budget_tokens_s = Some(b.to_string());
        }
    }
    if let Some(s) = total_items_s.as_ref() {
        root_attrs.push(("total_items", s.as_str()));
    }
    if let Some(s) = offset_s.as_ref() {
        root_attrs.push(("offset", s.as_str()));
    }
    if let Some(s) = shown_items_s.as_ref() {
        root_attrs.push(("shown_items", s.as_str()));
    }
    if let Some(s) = truncated_s.as_ref() {
        root_attrs.push(("truncated", s.as_str()));
    }
    if let Some(s) = next_offset_s.as_ref() {
        root_attrs.push(("next_offset", s.as_str()));
    }
    if let Some(s) = budget_lines_s.as_ref() {
        root_attrs.push(("budget_lines", s.as_str()));
    }
    if let Some(s) = budget_tokens_s.as_ref() {
        root_attrs.push(("budget_tokens", s.as_str()));
    }
    vxml::open_attrs(w, "blast_radius", &root_attrs)?;
    // Keep existing structure by reusing graph writer logic on this writer below.
    // Focus
    vxml::open(w, "focus")?;
    let id_s = focus.id.to_string();
    let range_s = format!("L{}-L{}", focus.range_start, focus.range_end);
    let fan_in_s = focus.fan_in.to_string();
    let fan_out_s = focus.fan_out.to_string();
    let role_conf_s = format!("{:.2}", focus.role_confidence);
    let role_uncertain_s = (focus.role_confidence < 0.6).to_string();
    let mut attrs = vec![
        ("id", id_s.as_str()),
        ("stable_id", focus.stable_id.as_str()),
        ("name", focus.name.as_str()),
        ("kind", focus.kind.as_str()),
        ("file", focus.file.as_str()),
        ("range", range_s.as_str()),
        ("visibility", focus.visibility.as_str()),
        ("fan_in", fan_in_s.as_str()),
        ("fan_out", fan_out_s.as_str()),
        ("role", focus.role.as_str()),
        ("role_confidence", role_conf_s.as_str()),
    ];
    if focus.role_confidence < 0.6 {
        attrs.push(("role_uncertain", role_uncertain_s.as_str()));
    }
    vxml::open_attrs(w, "node", &attrs)?;
    if !compact {
        if let Some(sig) = &focus.signature {
            vxml::text_tag(w, "signature", sig)?;
        }
        if let Some(doc) = &focus.doc {
            if !doc.is_empty() {
                vxml::text_tag(w, "doc", doc)?;
            }
        }
        if let Some(ann) = annotations.get(&focus.id) {
            write_annotation_xml(w, ann);
        }
        write_diagnostics_xml(w, &focus_diagnostics);
        if let Some(snip) = snippet {
            vxml::text_tag(w, "snippet", &snip)?;
        }
    }
    vxml::close(w, "node")?;
    vxml::close(w, "focus")?;

    let max_depth = dependents.iter().map(|(_, d)| *d).max().unwrap_or(0);
    for d in 1..=max_depth {
        let depth_s = d.to_string();
        vxml::open_attrs(w, "dependents", &[("depth", &depth_s)])?;
        for (n, _) in dependents.iter().filter(|(_, dep)| *dep == d) {
            let id_s = n.id.to_string();
            let range_s = format!("L{}-L{}", n.range_start, n.range_end);
            let fan_in_s = n.fan_in.to_string();
            let fan_out_s = n.fan_out.to_string();
            let role_conf_s = format!("{:.2}", n.role_confidence);
            let role_uncertain_s = (n.role_confidence < 0.6).to_string();
            let mut attrs = vec![
                ("id", id_s.as_str()),
                ("stable_id", n.stable_id.as_str()),
                ("name", n.name.as_str()),
                ("kind", n.kind.as_str()),
                ("file", n.file.as_str()),
                ("range", range_s.as_str()),
                ("visibility", n.visibility.as_str()),
                ("fan_in", fan_in_s.as_str()),
                ("fan_out", fan_out_s.as_str()),
                ("role", n.role.as_str()),
                ("role_confidence", role_conf_s.as_str()),
            ];
            if n.role_confidence < 0.6 {
                attrs.push(("role_uncertain", role_uncertain_s.as_str()));
            }
            if !compact {
                if let Some(sig) = &n.signature {
                    attrs.push(("signature", sig.as_str()));
                }
            }
            let has_doc = !compact && n.doc.as_ref().is_some_and(|d| !d.is_empty());
            let call_snippet = if snippets && !compact {
                read_call_site_snippet(&n.file, n.range_start, n.range_end, &focus.name)
            } else {
                None
            };
            let dep_annotation = if compact { None } else { annotations.get(&n.id) };
            if has_doc || call_snippet.is_some() || dep_annotation.is_some() {
                vxml::open_attrs(w, "node", &attrs)?;
                if let Some(doc) = &n.doc {
                    if !doc.is_empty() {
                        vxml::text_tag(w, "doc", doc)?;
                    }
                }
                if let Some(ann) = dep_annotation {
                    write_annotation_xml(w, ann);
                }
                if let Some(snip) = call_snippet {
                    vxml::text_tag(w, "snippet", &snip)?;
                }
                vxml::close(w, "node")?;
            } else {
                vxml::empty(w, "node", &attrs)?;
            }
        }
        vxml::close(w, "dependents")?;
    }
    vxml::close(w, "blast_radius")?;
    Ok(())
}

fn file_to_module(path: &str) -> String {
    let normalized = path.trim_start_matches("./");
    std::path::Path::new(normalized)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(normalized)
        .to_string()
}

fn symbol_crate_for_file<'a>(ws: &'a crate::workspace::WorkspaceGraph, file: &str) -> Option<&'a str> {
    let file_norm = file.trim_start_matches("./");
    let mut members: Vec<&crate::workspace::CrateMember> = ws.members.iter().collect();
    members.sort_by_key(|m| std::cmp::Reverse(m.path.len()));
    for m in members {
        if m.path.is_empty() {
            if file_norm.starts_with("src/") {
                return Some(m.name.as_str());
            }
            continue;
        }
        let prefix = format!("{}/", m.path);
        if file_norm.starts_with(&prefix) {
            return Some(m.name.as_str());
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub fn map(
    include_private: bool,
    top: usize,
    with_docs: bool,
    with_file_docs: bool,
    all_edges: bool,
    role: Option<&str>,
    context: Option<&str>,
    by_file: bool,
    md: bool,
) -> Result<()> {
    let conn = open_db()?;

    struct Row {
        file: String,
        name: String,
        kind: String,
        visibility: String,
        signature: Option<String>,
        complexity: Option<i64>,
        hotspot_fan_in: i64,
        fan_in: i64,
        fan_out: i64,
        doc: Option<String>,
        role: String,
        role_confidence: f64,
    }

    let hotspot_subquery = if all_edges {
        "SELECT COUNT(*) FROM edges WHERE to_id = n.id"
    } else {
        "SELECT COUNT(*) FROM edges WHERE to_id = n.id AND source IN ('resolver', 'rustdoc')"
    };

    let visibility_filter = if include_private {
        String::new()
    } else {
        "WHERE n.visibility NOT IN ('private', 'default')".to_string()
    };

    let role_filter = match role {
        Some(r) => format!(" AND n.role = '{}'", r.replace('\'', "''")),
        None => String::new(),
    };

    let where_clause = if visibility_filter.is_empty() {
        if role_filter.is_empty() {
            String::new()
        } else {
            format!("WHERE 1=1{}", role_filter)
        }
    } else {
        format!("{}{}", visibility_filter, role_filter)
    };

    let sql = format!(
        "SELECT n.file, n.name, n.kind, n.visibility, n.signature, n.complexity,
                ({hotspot_subquery}) AS hotspot_fan_in,
                (SELECT COUNT(*) FROM edges WHERE to_id = n.id) AS fan_in,
                (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out,
                n.doc, n.role, n.role_confidence
         FROM nodes n
         {where_clause}
         ORDER BY n.file, hotspot_fan_in DESC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                file: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                visibility: r.get(3)?,
                signature: r.get(4)?,
                complexity: r.get(5)?,
                hotspot_fan_in: r.get(6)?,
                fan_in: r.get(7)?,
                fan_out: r.get(8)?,
                doc: r.get(9)?,
                role: r.get(10)?,
                role_confidence: r.get(11)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if let Some(ctx) = context {
        rows.retain(|r| crate::arch::file_to_context(&r.file) == ctx);
    }

    // File-level docs: path -> doc (only loaded when --with-file-docs)
    let file_doc_map: std::collections::HashMap<String, String> = if with_file_docs {
        let mut fstmt =
            conn.prepare("SELECT path, doc FROM files WHERE doc IS NOT NULL AND doc != ''")?;
        let pairs: Vec<(String, String)> = fstmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        pairs.into_iter().collect()
    } else {
        std::collections::HashMap::new()
    };

    // Group by file (BTreeMap keeps files in alphabetical order)
    let mut by_path: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        by_path.entry(row.file.clone()).or_default().push(i);
    }

    // Hotspots: top N across all files by hotspot_fan_in (trusted by default)
    let mut ranked: Vec<usize> = (0..rows.len()).collect();
    ranked.sort_by(|&a, &b| rows[b].hotspot_fan_in.cmp(&rows[a].hotspot_fan_in));
    ranked.truncate(top);
    // Drop zero-count entries — no trusted edges means no signal
    ranked.retain(|&i| rows[i].hotspot_fan_in > 0);

    if md {
        println!("| file | symbol | kind | visibility | complexity | role | fan_in | fan_out |");
        println!("|---|---|---|---|---:|---|---:|---:|");
        for row in &rows {
            let complexity_s = row
                .complexity
                .map(|v| v.to_string())
                .unwrap_or_else(|| "".to_string());
            println!(
                "| {} | {} | {} | {} | {} | {} ({:.2}) | {} | {} |",
                row.file.replace('|', "\\|"),
                row.name.replace('|', "\\|"),
                row.kind.replace('|', "\\|"),
                row.visibility.replace('|', "\\|"),
                complexity_s,
                row.role.replace('|', "\\|"),
                row.role_confidence,
                row.fan_in,
                row.fan_out
            );
        }
        return Ok(());
    }

    let total_symbols: usize = by_path.values().map(|v| v.len()).sum();
    let mut w = vxml::new_stream_writer();
    let files_s = by_path.len().to_string();
    let symbols_s = total_symbols.to_string();
    vxml::open_attrs(
        &mut w,
        "map",
        &[("files", &files_s), ("symbols", &symbols_s), ("tokens", "streaming")],
    )?;

    let hotspot_source = if all_edges { "all" } else { "trusted" };
    let top_s = ranked.len().to_string();
    vxml::open_attrs(
        &mut w,
        "hotspots",
        &[("top", &top_s), ("fan_in_source", hotspot_source)],
    )?;
    for &i in &ranked {
        let s = &rows[i];
        let fan_in_s = s.hotspot_fan_in.to_string();
        let fan_out_s = s.fan_out.to_string();
        vxml::empty(
            &mut w,
            "symbol",
            &[
                ("name", &s.name),
                ("kind", &s.kind),
                ("file", &s.file),
                ("fan_in", &fan_in_s),
                ("fan_out", &fan_out_s),
            ],
        )?;
    }
    vxml::close(&mut w, "hotspots")?;

    for (file, indices) in &by_path {
        let file_doc = if with_file_docs {
            file_doc_map.get(file.as_str())
        } else {
            None
        };
        let symbols_count_s = indices.len().to_string();
        if by_file {
            vxml::open_attrs(
                &mut w,
                "file",
                &[("path", file), ("symbols", &symbols_count_s)],
            )?;
        } else {
            let module_name = file_to_module(file);
            vxml::open_attrs(
                &mut w,
                "module",
                &[("name", &module_name), ("path", file), ("symbols", &symbols_count_s)],
            )?;
        }

        if let Some(fd) = file_doc {
            vxml::text_tag(&mut w, "doc", fd)?;
        }

        for &i in indices {
            let s = &rows[i];
            let fan_in_s = s.fan_in.to_string();
            let fan_out_s = s.fan_out.to_string();
            let role_conf_s = format!("{:.2}", s.role_confidence);
            let role_uncertain_s = (s.role_confidence < 0.6).to_string();
            let mut attrs = vec![
                ("name", s.name.as_str()),
                ("kind", s.kind.as_str()),
                ("visibility", s.visibility.as_str()),
                ("fan_in", fan_in_s.as_str()),
                ("fan_out", fan_out_s.as_str()),
                ("role", s.role.as_str()),
                ("role_confidence", role_conf_s.as_str()),
            ];
            if s.role_confidence < 0.6 {
                attrs.push(("role_uncertain", role_uncertain_s.as_str()));
            }
            let complexity_s = s.complexity.map(|c| c.to_string());
            if let Some(c) = &complexity_s {
                attrs.push(("complexity", c.as_str()));
            }
            if let Some(sig) = &s.signature {
                attrs.push(("signature", sig.as_str()));
            }
            let sym_doc = if with_docs {
                s.doc.as_deref().filter(|d| !d.is_empty())
            } else {
                None
            };
            if let Some(doc) = sym_doc {
                vxml::open_attrs(&mut w, "symbol", &attrs)?;
                vxml::text_tag(&mut w, "doc", doc)?;
                vxml::close(&mut w, "symbol")?;
            } else {
                vxml::empty(&mut w, "symbol", &attrs)?;
            }
        }
        if by_file {
            vxml::close(&mut w, "file")?;
        } else {
            vxml::close(&mut w, "module")?;
        }
    }
    vxml::close(&mut w, "map")?;
    vxml::finish_stream(w)?;

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn trace_path(
    symbol: &str,
    to: Option<&str>,
    direction: &str,
    max_depth: usize,
    max_paths: usize,
    with_async_boundaries: bool,
    with_channels: bool,
    high_level: bool,
) -> Result<()> {
    let conn = open_db()?;
    let start_id = resolve_symbol_id(&conn, symbol)?;
    let target_id = match to {
        Some(t) => Some(resolve_symbol_id(&conn, t)?),
        None => None,
    };

    #[derive(Clone)]
    struct NodeLite {
        name: String,
        stable_id: String,
        role: String,
        signature: Option<String>,
        fan_out: i64,
    }

    let mut node_stmt = conn.prepare(
        "SELECT name, stable_id, role, signature,
                (SELECT COUNT(*) FROM edges WHERE from_id = n.id) AS fan_out
         FROM nodes n
         WHERE id = ?1",
    )?;
    let load_node = |id: i64, stmt: &mut rusqlite::Statement<'_>| -> Result<NodeLite> {
        let n = stmt.query_row(rusqlite::params![id], |r| {
            Ok(NodeLite {
                name: r.get(0)?,
                stable_id: r.get(1)?,
                role: r.get(2)?,
                signature: r.get(3)?,
                fan_out: r.get(4)?,
            })
        })?;
        Ok(n)
    };
    let start_node = load_node(start_id, &mut node_stmt)?;

    let mut edges_stmt = conn.prepare(
        "SELECT from_id, to_id, edge_type, source
         FROM edges
         WHERE source IN ('resolver', 'rustdoc')",
    )?;
    let edges: Vec<(i64, i64, String, String)> = edges_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let mut out_adj: std::collections::HashMap<i64, Vec<(i64, String, String)>> =
        std::collections::HashMap::new();
    let mut in_adj: std::collections::HashMap<i64, Vec<(i64, String, String)>> =
        std::collections::HashMap::new();
    for (from, to_id, et, src) in &edges {
        out_adj
            .entry(*from)
            .or_default()
            .push((*to_id, et.clone(), src.clone()));
        in_adj
            .entry(*to_id)
            .or_default()
            .push((*from, et.clone(), src.clone()));
    }

    let dir = match direction.to_ascii_lowercase().as_str() {
        "outgoing" | "write" => "outgoing",
        "incoming" | "read" => "incoming",
        "both" => "both",
        _ => {
            anyhow::bail!(
                "invalid direction '{}'; expected outgoing|incoming|both|read|write",
                direction
            );
        }
    };

    #[derive(Clone)]
    struct Hop {
        from: i64,
        to: i64,
        edge_type: String,
        source: String,
    }
    #[derive(Clone)]
    struct PathState {
        current: i64,
        hops: Vec<Hop>,
        visited: std::collections::HashSet<i64>,
    }

    let mut queue = std::collections::VecDeque::new();
    let mut seen_start = std::collections::HashSet::new();
    seen_start.insert(start_id);
    queue.push_back(PathState {
        current: start_id,
        hops: Vec::new(),
        visited: seen_start,
    });

    let mut emitted: Vec<Vec<Hop>> = Vec::new();
    while let Some(state) = queue.pop_front() {
        if emitted.len() >= max_paths {
            break;
        }
        if state.hops.len() >= max_depth {
            emitted.push(state.hops.clone());
            if emitted.len() >= max_paths {
                break;
            }
            continue;
        }

        let outgoing_iter = out_adj.get(&state.current).cloned().unwrap_or_default();
        let incoming_iter = in_adj.get(&state.current).cloned().unwrap_or_default();
        let nexts: Vec<(i64, String, String, bool)> = match dir {
            "incoming" => incoming_iter
                .into_iter()
                .map(|(n, et, src)| (n, et, src, false))
                .collect(),
            "both" => outgoing_iter
                .into_iter()
                .map(|(n, et, src)| (n, et, src, true))
                .chain(
                    incoming_iter
                        .into_iter()
                        .map(|(n, et, src)| (n, et, src, false)),
                )
                .collect(),
            _ => outgoing_iter
                .into_iter()
                .map(|(n, et, src)| (n, et, src, true))
                .collect(),
        };

        if nexts.is_empty() {
            emitted.push(state.hops.clone());
            if emitted.len() >= max_paths {
                break;
            }
            continue;
        }

        for (next, et, src, is_forward) in nexts {
            if emitted.len() >= max_paths {
                break;
            }
            if state.visited.contains(&next) {
                continue;
            }
            let mut next_state = state.clone();
            next_state.visited.insert(next);
            next_state.current = next;
            let (from_id, to_id) = if is_forward {
                (state.current, next)
            } else {
                (next, state.current)
            };
            next_state.hops.push(Hop {
                from: from_id,
                to: to_id,
                edge_type: et,
                source: src,
            });

            if target_id.is_some_and(|tid| tid == next) {
                emitted.push(next_state.hops.clone());
                continue;
            }

            if target_id.is_none() {
                let next_node = load_node(next, &mut node_stmt)?;
                // Completion boundary heuristic: structural leaves and worker boundaries.
                let completion = next_node.fan_out == 0
                    || next_node.role == "leaf"
                    || next_node.role == "infra"
                    || next_node.role == "entrypoint";
                if completion {
                    emitted.push(next_state.hops.clone());
                    continue;
                }
            }
            queue.push_back(next_state);
        }
    }

    let mut w = vxml::new_stream_writer();
    let max_depth_s = max_depth.to_string();
    let max_paths_s = max_paths.to_string();
    let found_s = emitted.len().to_string();
    let high_level_s = high_level.to_string();
    vxml::open_attrs(
        &mut w,
        "trace_path",
        &[
            ("symbol", symbol),
            ("root_id", &start_id.to_string()),
            ("root_name", &start_node.name),
            ("direction", dir),
            ("max_depth", &max_depth_s),
            ("max_paths", &max_paths_s),
            ("found", &found_s),
            ("high_level", &high_level_s),
            ("tokens", "streaming"),
        ],
    )?;

    if let Some(tid) = target_id {
        if let Ok(tnode) = load_node(tid, &mut node_stmt) {
            vxml::empty(
                &mut w,
                "target",
                &[("id", &tid.to_string()), ("name", &tnode.name), ("stable_id", &tnode.stable_id)],
            )?;
        }
    }

    for (idx, path) in emitted.iter().enumerate() {
        let mut rendered: Vec<Hop> = if high_level {
            let mut keep = Vec::new();
            for (i, hop) in path.iter().enumerate() {
                let from = load_node(hop.from, &mut node_stmt)?;
                let to_node = load_node(hop.to, &mut node_stmt)?;
                let low_role = |r: &str| matches!(r, "leaf" | "utility" | "unknown");
                let is_low_signal = low_role(&from.role) && low_role(&to_node.role);
                if i == 0 || i + 1 == path.len() || !is_low_signal {
                    keep.push(hop.clone());
                }
            }
            if keep.is_empty() && !path.is_empty() {
                vec![path[0].clone()]
            } else {
                keep
            }
        } else {
            path.clone()
        };
        if rendered.is_empty() {
            continue;
        }
        let i_s = (idx + 1).to_string();
        let hops_s = rendered.len().to_string();
        vxml::open_attrs(&mut w, "path", &[("index", &i_s), ("hops", &hops_s)])?;
        for hop in rendered.drain(..) {
            let from = load_node(hop.from, &mut node_stmt)?;
            let to_node = load_node(hop.to, &mut node_stmt)?;
            let attrs = vec![
                ("from_id", hop.from.to_string()),
                ("to_id", hop.to.to_string()),
                ("from_name", from.name.clone()),
                ("to_name", to_node.name.clone()),
                ("edge_type", hop.edge_type.clone()),
                ("source", hop.source.clone()),
                ("from_stable_id", from.stable_id.clone()),
                ("to_stable_id", to_node.stable_id.clone()),
            ];
            let mut pairs: Vec<(&str, &str)> = Vec::new();
            for (k, v) in &attrs {
                pairs.push((k, v.as_str()));
            }
            vxml::open_attrs(&mut w, "hop", &pairs)?;

            if with_async_boundaries
                && (from.signature.as_deref().is_some_and(|s| s.contains("async "))
                    || to_node.signature.as_deref().is_some_and(|s| s.contains("async ")))
            {
                vxml::empty(&mut w, "boundary", &[("type", "async")])?;
            }

            if with_channels
                && (is_channelish(from.name.as_str(), from.signature.as_deref())
                    || is_channelish(to_node.name.as_str(), to_node.signature.as_deref()))
            {
                vxml::empty(&mut w, "boundary", &[("type", "channel_or_actor")])?;
            }

            if high_level {
                vxml::empty(&mut w, "boundary", &[("type", "high_level_filter")])?;
            }

            vxml::close(&mut w, "hop")?;
        }
        vxml::close(&mut w, "path")?;
    }

    vxml::close(&mut w, "trace_path")?;
    vxml::finish_stream(w)?;
    Ok(())
}

fn is_channelish(name: &str, signature: Option<&str>) -> bool {
    let name = name.to_ascii_lowercase();
    let sig = signature.unwrap_or("").to_ascii_lowercase();
    let keys = ["channel", "mpsc", "broadcast", "oneshot", "actor", "mailbox"];
    keys.iter().any(|k| name.contains(k) || sig.contains(k))
}
