//! Enrich the symbol graph using `cargo +nightly rustdoc --output-format json`.
//!
//! Replaces rust-analyzer for three signals:
//!   1. Qualified names  (`module::symbol` style module-prefixed names)
//!   2. Visibility       (`public`, `crate`, `default`, `restricted::path`)
//!   3. Trait impl links (`trait_impl` column + `IMPL_TRAIT` edges)
//!
//! The rustdoc JSON format version is asserted at runtime; if the nightly
//! toolchain bumps the format the enricher will fail loudly rather than
//! silently applying corrupt data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Result};
use rayon::prelude::*;
use rusqlite::Connection;
use rustdoc_types::{Crate, Id, ItemEnum, Type};
use tracing::{debug, info, warn};

const EXPECTED_FORMAT_VERSION: u32 = 57;

// ── Public entry point ──────────────────────────────────────────────────────

/// Enrich nodes in `conn` from rustdoc JSON for every crate that contains at
/// least one file in `changed_files`.  `root` is the workspace root (the
/// directory where graphlite was invoked).
pub fn enrich(root: &str, changed_files: &[PathBuf], conn: &Connection) -> Result<usize> {
    if changed_files.is_empty() {
        return Ok(0);
    }

    let abs_root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));

    // Run `cargo metadata` once to discover crates and their manifest paths.
    let crates = discover_crates(root)?;
    if crates.is_empty() {
        return Ok(0);
    }

    // Determine which crates are affected by the changed files.
    let abs_changed: Vec<PathBuf> = changed_files
        .iter()
        .filter_map(|p| {
            std::fs::canonicalize(p).ok().or_else(|| {
                abs_root
                    .join(p.strip_prefix("./").unwrap_or(p))
                    .canonicalize()
                    .ok()
            })
        })
        .collect();

    let affected: Vec<&CrateInfo> = crates
        .iter()
        .filter(|c| abs_changed.iter().any(|f| f.starts_with(&c.abs_dir)))
        .collect();

    if affected.is_empty() {
        // Fall back to running for all crates (e.g. single-crate project where
        // abs path resolution may have failed).
        info!(
            count = crates.len(),
            "rustdoc: enriching all crates (no affected filter matched)"
        );
        return enrich_crates(
            root,
            &crates.iter().collect::<Vec<_>>(),
            &crates,
            conn,
            &abs_root,
        );
    }

    info!(
        count = affected.len(),
        crates = %affected.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "),
        "rustdoc: enriching affected crates"
    );
    enrich_crates(root, &affected, &crates, conn, &abs_root)
}

// ── Crate discovery ─────────────────────────────────────────────────────────

struct CrateInfo {
    name: String,
    manifest_path: PathBuf,
    abs_dir: PathBuf,
}

fn discover_crates(root: &str) -> Result<Vec<CrateInfo>> {
    let manifest = format!("{}/Cargo.toml", root.trim_end_matches('/'));
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            &manifest,
        ])
        .output()?;
    if !out.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let workspace_ids: std::collections::HashSet<String> = meta["workspace_members"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let packages = meta["packages"]
        .as_array()
        .ok_or_else(|| anyhow!("cargo metadata: missing packages array"))?;

    let crates = packages
        .iter()
        .filter(|pkg| {
            workspace_ids.is_empty()
                || pkg["id"]
                    .as_str()
                    .map(|id| workspace_ids.contains(id))
                    .unwrap_or(false)
        })
        .filter_map(|pkg| {
            let name = pkg["name"].as_str()?.to_string();
            let mp = pkg["manifest_path"].as_str()?;
            let manifest_path = PathBuf::from(mp);
            let abs_dir = manifest_path.parent()?.to_path_buf();
            Some(CrateInfo {
                name,
                manifest_path,
                abs_dir,
            })
        })
        .collect();
    Ok(crates)
}

// ── Per-crate enrichment ─────────────────────────────────────────────────────

fn enrich_crates(
    root: &str,
    crates: &[&CrateInfo],
    workspace_crates: &[CrateInfo],
    conn: &Connection,
    abs_root: &Path,
) -> Result<usize> {
    // This snapshot is deliberately taken before any rustdoc work. The compute
    // stage only reads it; all writes are committed below on this connection.
    let node_map = Arc::new(build_node_map(conn, abs_root)?);
    let qualified_node_map = build_qualified_node_map(conn, workspace_crates, abs_root)?;
    let mut results: Vec<(&CrateInfo, Result<CrateEnrichment>)> = crates
        .par_iter()
        .map(|ci| {
            let result = compute_crate_enrichment(root, ci, abs_root, &node_map);
            (*ci, result)
        })
        .collect();

    // Cross-crate trait IDs are represented in rustdoc's `paths` table rather
    // than the current crate's `index`. Resolve them only after every crate's
    // pure parse stage has completed, using the single upfront graph snapshot.
    for (_, result) in &mut results {
        if let Ok(enrichment) = result {
            enrichment.impl_trait_edges = collect_impl_trait_edges(
                &enrichment.doc,
                &node_map,
                abs_root,
                &enrichment.crate_abs_dir,
                &qualified_node_map,
            );
        }
    }

    let tx = conn.unchecked_transaction()?;
    let mut total = 0;
    for (ci, result) in results {
        match result {
            Ok(enrichment) => {
                let count = apply_enrichment(&tx, &enrichment)?;
                debug!(crate_name = %ci.name, count, "rustdoc nodes enriched");
                total += count;
            }
            Err(e) if e.to_string().contains("rustdoc JSON format_version=") => return Err(e),
            Err(e) => warn!(crate_name = %ci.name, error = %e, "rustdoc enrichment skipped"),
        }
    }
    tx.commit()?;
    Ok(total)
}

struct CrateEnrichment {
    doc: Crate,
    crate_abs_dir: PathBuf,
    node_updates: Vec<NodeUpdate>,
    impl_trait_edges: Vec<(i64, i64)>,
}

struct NodeUpdate {
    node_id: i64,
    qualified_name: Option<String>,
    trait_impl: Option<String>,
}

fn compute_crate_enrichment(
    root: &str,
    ci: &CrateInfo,
    abs_root: &Path,
    node_map: &HashMap<(String, i64), i64>,
) -> Result<CrateEnrichment> {
    let json_path = run_cargo_rustdoc(root, &ci.manifest_path, &ci.name)?;
    parse_crate_enrichment(&json_path, abs_root, &ci.abs_dir, node_map)
}

// ── cargo rustdoc invocation ─────────────────────────────────────────────────

fn run_cargo_rustdoc(root: &str, manifest_path: &Path, crate_name: &str) -> Result<PathBuf> {
    let safe_name = crate_name.replace('-', "_");
    // Cargo serializes concurrent builds using a shared target directory. Use
    // one directory per crate so rayon workers can actually overlap.
    let rustdoc_target = Path::new(root)
        .join(".graphlite")
        .join("rustdoc-json")
        .join(&safe_name);
    std::fs::create_dir_all(&rustdoc_target)?;

    let status = Command::new("cargo")
        .args([
            "+nightly",
            "rustdoc",
            "--manifest-path",
            manifest_path.to_str().unwrap_or(""),
            "--target-dir",
            rustdoc_target.to_str().unwrap_or(""),
            "--",
            "-Zunstable-options",
            "--output-format",
            "json",
            "--document-private-items",
        ])
        .status()?;

    if !status.success() {
        bail!("cargo +nightly rustdoc failed for {}", crate_name);
    }

    let doc_dir = rustdoc_target.join("doc");
    let candidates = rustdoc_json_candidates(&doc_dir)?;
    if candidates.is_empty() {
        bail!(
            "rustdoc JSON not found under {} — check that nightly rustdoc is installed",
            doc_dir.display()
        );
    }

    if let Some(path) = pick_rustdoc_json_candidate(&safe_name, &candidates) {
        return Ok(path);
    }

    let names = candidates
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "rustdoc JSON candidates found but none selected for crate '{}': [{}]",
        crate_name,
        names
    )
}

fn rustdoc_json_candidates(doc_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !doc_dir.exists() {
        return Ok(files);
    }
    for ent in std::fs::read_dir(doc_dir)? {
        let ent = match ent {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = ent.path();
        if p.extension().and_then(|e| e.to_str()) == Some("json") {
            files.push(p);
        }
    }
    Ok(files)
}

fn pick_rustdoc_json_candidate(crate_name_safe: &str, candidates: &[PathBuf]) -> Option<PathBuf> {
    // Preferred: exact stem match for expected crate name.
    if let Some(p) = candidates.iter().find(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == crate_name_safe)
    }) {
        return Some(p.clone());
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }

    // Deterministic fallback: newest mtime, then lexical path.
    let mut ranked: Vec<(PathBuf, SystemTime)> = candidates
        .iter()
        .map(|p| {
            let mt = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (p.clone(), mt)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.to_string_lossy().cmp(&b.0.to_string_lossy()))
    });
    ranked.first().map(|(p, _)| p.clone())
}

// ── JSON parsing and DB application ─────────────────────────────────────────

fn parse_crate_enrichment(
    json_path: &Path,
    abs_root: &Path,
    crate_abs_dir: &Path,
    node_map: &HashMap<(String, i64), i64>,
) -> Result<CrateEnrichment> {
    let text = std::fs::read_to_string(json_path)?;
    let doc: Crate = serde_json::from_str(&text)?;

    // Assert format version.
    if doc.format_version != EXPECTED_FORMAT_VERSION {
        bail!(
            "rustdoc JSON format_version={} (expected {}): update graphlite or pin nightly",
            doc.format_version,
            EXPECTED_FORMAT_VERSION
        );
    }

    let method_to_trait = build_method_to_trait(&doc);
    let item_to_qname = build_qualified_names(&doc);
    let mut node_updates = Vec::new();

    for (item_id, item) in &doc.index {
        // Only items from our own crate (crate_id == 0).
        if item.crate_id != 0 {
            continue;
        }
        let span = match item.span.as_ref() {
            Some(s) => s,
            None => continue,
        };
        let range_start = span.begin.0 as i64;

        let node_id = match lookup_node_id(
            node_map,
            &span_file_match_keys(abs_root, crate_abs_dir, &span.filename.to_string_lossy()),
            range_start,
        ) {
            Some(id) => id,
            None => continue,
        };

        let qname = item_to_qname.get(item_id).cloned();
        let trait_impl = method_to_trait.get(item_id).cloned();
        node_updates.push(NodeUpdate {
            node_id,
            qualified_name: qname,
            trait_impl,
        });
    }

    Ok(CrateEnrichment {
        doc,
        crate_abs_dir: crate_abs_dir.to_path_buf(),
        node_updates,
        impl_trait_edges: Vec::new(),
    })
}

fn apply_enrichment(tx: &rusqlite::Transaction<'_>, enrichment: &CrateEnrichment) -> Result<usize> {
    for update in &enrichment.node_updates {
        if let Some(qname) = &update.qualified_name {
            tx.execute(
                "UPDATE nodes SET qualified_name = ?1 WHERE id = ?2 AND (qualified_name = '' OR qualified_name IS NULL)",
                rusqlite::params![qname, update.node_id],
            )?;
            tx.execute(
                "UPDATE fts_symbols SET qualified_name = ?1 WHERE node_id = ?2",
                rusqlite::params![qname, update.node_id],
            )?;
        }
        if let Some(trait_impl) = &update.trait_impl {
            tx.execute(
                "UPDATE nodes SET trait_impl = ?1 WHERE id = ?2",
                rusqlite::params![trait_impl, update.node_id],
            )?;
        }
    }
    for (from_id, to_id) in &enrichment.impl_trait_edges {
        tx.execute(
            "INSERT OR IGNORE INTO edges (from_id, to_id, edge_type, source, confidence) VALUES (?1, ?2, 'IMPL_TRAIT', 'rustdoc', 1.0)",
            rusqlite::params![from_id, to_id],
        )?;
    }
    Ok(enrichment.node_updates.len())
}

// ── Helper: build (db_file, range_start) -> node_id lookup ──────────────────

fn build_node_map(conn: &Connection, abs_root: &Path) -> Result<HashMap<(String, i64), i64>> {
    let mut stmt = conn.prepare_cached("SELECT id, file, range_start FROM nodes")?;
    let mut map: HashMap<(String, i64), i64> = HashMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (id, file, rs) = match row {
            Ok(v) => v,
            Err(_) => continue,
        };
        for key in node_file_match_keys(abs_root, &file) {
            map.entry((key, rs)).or_insert(id);
        }
    }
    Ok(map)
}

fn build_qualified_node_map(
    conn: &Connection,
    crates: &[CrateInfo],
    abs_root: &Path,
) -> Result<HashMap<(String, String), Vec<i64>>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, qualified_name, file FROM nodes WHERE qualified_name IS NOT NULL AND qualified_name != ''",
    )?;
    let mut map: HashMap<(String, String), Vec<i64>> = HashMap::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, qualified_name, file) = match row {
            Ok(v) => v,
            Err(_) => continue,
        };
        let file_path = Path::new(&file);
        let absolute_file = if file_path.is_absolute() {
            file_path.to_path_buf()
        } else {
            abs_root.join(file_path.strip_prefix("./").unwrap_or(file_path))
        };
        let Some(crate_info) = crates
            .iter()
            .find(|ci| absolute_file.starts_with(&ci.abs_dir))
        else {
            continue;
        };
        map.entry((crate_info.name.replace('-', "_"), qualified_name))
            .or_default()
            .push(id);
    }
    Ok(map)
}

fn node_file_match_keys(abs_root: &Path, file: &str) -> Vec<String> {
    let mut out = Vec::new();
    let file_path = Path::new(file);
    push_unique(&mut out, file.to_string());

    if file_path.is_absolute() {
        if let Ok(rel) = file_path.strip_prefix(abs_root) {
            let rel_str = rel.to_string_lossy().to_string();
            push_unique(&mut out, rel_str.clone());
            push_unique(&mut out, format!("./{}", rel_str));
        }
    } else {
        let trimmed = file.strip_prefix("./").unwrap_or(file).to_string();
        push_unique(&mut out, trimmed.clone());
        push_unique(&mut out, format!("./{}", trimmed));
        let abs = abs_root.join(&trimmed);
        push_unique(&mut out, abs.to_string_lossy().to_string());
        if let Ok(canon) = abs.canonicalize() {
            push_unique(&mut out, canon.to_string_lossy().to_string());
        }
    }

    out
}

fn span_file_match_keys(abs_root: &Path, crate_abs_dir: &Path, span_filename: &str) -> Vec<String> {
    let mut out = Vec::new();
    let span_no_dot = span_filename.strip_prefix("./").unwrap_or(span_filename);
    let span_path = Path::new(span_no_dot);

    if span_path.is_absolute() {
        let abs = span_path.to_path_buf();
        push_unique(&mut out, abs.to_string_lossy().to_string());
        if let Ok(rel) = abs.strip_prefix(abs_root) {
            let rel_str = rel.to_string_lossy().to_string();
            push_unique(&mut out, rel_str.clone());
            push_unique(&mut out, format!("./{}", rel_str));
        }
        if let Ok(canon) = abs.canonicalize() {
            push_unique(&mut out, canon.to_string_lossy().to_string());
        }
        return out;
    }

    let rel_crate = crate_abs_dir
        .strip_prefix(abs_root)
        .unwrap_or(crate_abs_dir);
    let mut rel = PathBuf::new();
    if rel_crate != Path::new("")
        && rel_crate != Path::new(".")
        && !span_path.starts_with(rel_crate)
    {
        rel.push(rel_crate);
    }
    rel.push(span_path);

    let rel_str = rel.to_string_lossy().to_string();
    push_unique(&mut out, rel_str.clone());
    push_unique(&mut out, format!("./{}", rel_str));

    let abs_from_root = abs_root.join(&rel);
    push_unique(&mut out, abs_from_root.to_string_lossy().to_string());
    if let Ok(canon) = abs_from_root.canonicalize() {
        push_unique(&mut out, canon.to_string_lossy().to_string());
    }

    let abs_from_crate = crate_abs_dir.join(span_path);
    push_unique(&mut out, abs_from_crate.to_string_lossy().to_string());
    if let Ok(canon) = abs_from_crate.canonicalize() {
        push_unique(&mut out, canon.to_string_lossy().to_string());
    }

    out
}

fn lookup_node_id(
    node_map: &HashMap<(String, i64), i64>,
    file_keys: &[String],
    range_start: i64,
) -> Option<i64> {
    file_keys
        .iter()
        .find_map(|k| node_map.get(&(k.clone(), range_start)).copied())
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !value.is_empty() && !out.contains(&value) {
        out.push(value);
    }
}

// ── Helper: build item_id -> trait_name for methods inside trait impl blocks ─

fn build_method_to_trait(doc: &Crate) -> HashMap<Id, String> {
    let mut map = HashMap::new();
    for item in doc.index.values() {
        if item.crate_id != 0 {
            continue;
        }
        let ItemEnum::Impl(impl_data) = &item.inner else {
            continue;
        };
        let Some(trait_path) = &impl_data.trait_ else {
            continue;
        };
        for method_id in &impl_data.items {
            map.insert(*method_id, trait_path.path.clone());
        }
    }
    map
}

// ── Helper: build item_id -> qualified_name string ───────────────────────────

fn build_qualified_names(doc: &Crate) -> HashMap<Id, String> {
    doc.paths
        .iter()
        .filter_map(|(id, entry)| {
            if entry.crate_id != 0 || entry.path.len() < 2 {
                return None;
            }
            Some((*id, entry.path[1..].join("::")))
        })
        .collect()
}

// ── Helper: emit IMPL_TRAIT edges ────────────────────────────────────────────

fn collect_impl_trait_edges(
    doc: &Crate,
    node_map: &HashMap<(String, i64), i64>,
    abs_root: &Path,
    crate_abs_dir: &Path,
    qualified_node_map: &HashMap<(String, String), Vec<i64>>,
) -> Vec<(i64, i64)> {
    let item_nodes: HashMap<Id, i64> = doc
        .index
        .iter()
        .filter_map(|(id, item)| {
            let span = item.span.as_ref()?;
            let node_id = lookup_node_id(
                node_map,
                &span_file_match_keys(abs_root, crate_abs_dir, &span.filename.to_string_lossy()),
                span.begin.0 as i64,
            )?;
            Some((*id, node_id))
        })
        .collect();

    doc.index
        .values()
        .filter_map(|item| {
            if item.crate_id != 0 {
                return None;
            }
            let ItemEnum::Impl(impl_data) = &item.inner else {
                return None;
            };
            if impl_data.is_synthetic {
                return None;
            }
            let trait_id = impl_data.trait_.as_ref()?.id;
            let type_id = match &impl_data.for_ {
                Type::ResolvedPath(path) => path.id,
                _ => return None,
            };
            let type_label = rustdoc_path_label(doc, type_id);
            let trait_label = rustdoc_path_label(doc, trait_id);
            let Some(from_id) = item_nodes.get(&type_id).copied() else {
                warn!(
                    crate_dir = %crate_abs_dir.display(),
                    type_name = %type_label,
                    trait_name = %trait_label,
                    "rustdoc IMPL_TRAIT target could not be mapped; edge skipped"
                );
                return None;
            };
            let Some(to_id) =
                resolve_rustdoc_node_id(doc, trait_id, &item_nodes, qualified_node_map)
            else {
                if is_workspace_rustdoc_path(doc, trait_id, qualified_node_map) {
                    warn!(
                        crate_dir = %crate_abs_dir.display(),
                        type_name = %type_label,
                        trait_name = %trait_label,
                        "rustdoc IMPL_TRAIT trait could not be mapped; edge skipped"
                    );
                } else {
                    debug!(
                        crate_dir = %crate_abs_dir.display(),
                        type_name = %type_label,
                        trait_name = %trait_label,
                        "rustdoc external IMPL_TRAIT trait is not indexed; edge skipped"
                    );
                }
                return None;
            };
            Some((from_id, to_id))
        })
        .collect()
}

fn rustdoc_path_label(doc: &Crate, item_id: Id) -> String {
    doc.paths
        .get(&item_id)
        .map(|path| path.path.join("::"))
        .unwrap_or_else(|| format!("rustdoc-id-{}", item_id.0))
}

fn is_workspace_rustdoc_path(
    doc: &Crate,
    item_id: Id,
    qualified_node_map: &HashMap<(String, String), Vec<i64>>,
) -> bool {
    let Some(crate_name) = doc.paths.get(&item_id).and_then(|path| path.path.first()) else {
        return false;
    };
    qualified_node_map
        .keys()
        .any(|(workspace_crate, _)| workspace_crate == crate_name)
}

fn resolve_rustdoc_node_id(
    doc: &Crate,
    item_id: Id,
    item_nodes: &HashMap<Id, i64>,
    qualified_node_map: &HashMap<(String, String), Vec<i64>>,
) -> Option<i64> {
    if let Some(node_id) = item_nodes.get(&item_id) {
        return Some(*node_id);
    }
    let path = doc.paths.get(&item_id)?;
    let crate_name = path.path.first()?.replace('-', "_");
    let qualified_name = path.path.get(1..)?.join("::");
    let node_ids = qualified_node_map.get(&(crate_name, qualified_name))?;
    (node_ids.len() == 1).then_some(node_ids[0])
}

#[cfg(test)]
mod tests {
    use super::{pick_rustdoc_json_candidate, span_file_match_keys};
    use std::path::PathBuf;

    #[test]
    fn picks_exact_stem_match_first() {
        let files = vec![
            PathBuf::from("/tmp/doc/other.json"),
            PathBuf::from("/tmp/doc/tools.json"),
        ];
        let picked = pick_rustdoc_json_candidate("tools", &files).expect("candidate");
        assert_eq!(picked, PathBuf::from("/tmp/doc/tools.json"));
    }

    #[test]
    fn picks_single_candidate_when_only_one_exists() {
        let files = vec![PathBuf::from("/tmp/doc/bin_main.json")];
        let picked = pick_rustdoc_json_candidate("tools", &files).expect("candidate");
        assert_eq!(picked, PathBuf::from("/tmp/doc/bin_main.json"));
    }

    #[test]
    fn span_keys_include_relative_and_absolute_forms() {
        let root = PathBuf::from("/repo");
        let crate_dir = PathBuf::from("/repo/tools");
        let keys = span_file_match_keys(&root, &crate_dir, "src/lib.rs");
        assert!(keys.contains(&"./tools/src/lib.rs".to_string()));
        assert!(keys.contains(&"/repo/tools/src/lib.rs".to_string()));
    }

    #[test]
    fn span_keys_do_not_double_prefix_workspace_member_path() {
        let root = PathBuf::from("/repo");
        let crate_dir = PathBuf::from("/repo/sales");
        let keys = span_file_match_keys(&root, &crate_dir, "sales/src/lib.rs");
        assert!(keys.contains(&"./sales/src/lib.rs".to_string()));
        assert!(!keys.contains(&"./sales/sales/src/lib.rs".to_string()));
    }
}
