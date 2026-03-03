use std::collections::BTreeMap;
use std::f64::consts::PI;

use anyhow::Result;
use serde::Serialize;

use crate::arch::{
    context_violation_reason, exception_reason, file_to_context, role_violation_reason,
};
use crate::query::open_db;

const VIZ_TEMPLATE: &str = include_str!("viz_template.html");

#[derive(Serialize)]
struct VizOutput {
    meta: Meta,
    contexts: Vec<ContextInfo>,
    nodes: Vec<VizNode>,
    edges: Vec<VizEdge>,
}

#[derive(Serialize)]
struct Meta {
    total_nodes: usize,
    total_edges: usize,
    /// Trusted edges (rust-analyzer) where the caller is at a lower abstraction
    /// layer than the callee — i.e. crate::arch::role_z(from) < crate::arch::role_z(to).
    /// Structural (tree-sitter) edges are excluded to prevent false positives
    /// from unresolved common names (new, execute, send, etc.).
    violation_count: usize,
    /// Violations suppressed by [[exceptions]] rules in config.toml.
    suppressed_violation_count: usize,
}

#[derive(Serialize)]
struct ContextInfo {
    name: String,
    node_count: usize,
}

#[derive(Serialize)]
struct VizNode {
    id: i64,
    name: String,
    kind: String,
    stable_id: String,
    file: String,
    language: String,
    visibility: String,
    role: String,
    role_confidence: f64,
    fan_in: i64,
    fan_in_trusted: i64,
    fan_out: i64,
    fan_out_trusted: i64,
    context: String,
    signature: Option<String>,
    pos: Pos,
}

#[derive(Serialize)]
struct Pos {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Serialize)]
struct VizEdge {
    from: i64,
    to: i64,
    edge_type: String,
    source: String,
    /// Present only when the edge is a layer-inversion violation.
    /// Format: "from_role → to_role" for UI display.
    #[serde(skip_serializing_if = "Option::is_none")]
    violation: Option<String>,
}

fn fnv1a_u64(data: &[u8]) -> u64 {
    let mut hash: u64 = 14695981039346656037u64;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211u64);
    }
    hash
}

fn build_output(conn: &rusqlite::Connection) -> Result<VizOutput> {
    let mut stmt = conn.prepare(
        "SELECT
          n.id, n.name, n.kind, n.stable_id, n.file, n.language,
          n.visibility, n.role, n.role_confidence, n.signature,
          (SELECT COUNT(*) FROM edges WHERE to_id   = n.id)                              AS fan_in,
          (SELECT COUNT(*) FROM edges WHERE to_id   = n.id AND source = 'rust-analyzer') AS fan_in_trusted,
          (SELECT COUNT(*) FROM edges WHERE from_id = n.id)                              AS fan_out,
          (SELECT COUNT(*) FROM edges WHERE from_id = n.id AND source = 'rust-analyzer') AS fan_out_trusted
        FROM nodes n
        ORDER BY n.id",
    )?;

    struct RawNode {
        id: i64,
        name: String,
        kind: String,
        stable_id: String,
        file: String,
        language: String,
        visibility: String,
        role: String,
        role_confidence: f64,
        signature: Option<String>,
        fan_in: i64,
        fan_in_trusted: i64,
        fan_out: i64,
        fan_out_trusted: i64,
    }

    let raw_nodes: Vec<RawNode> = stmt
        .query_map([], |r| {
            Ok(RawNode {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                stable_id: r.get(3)?,
                file: r.get(4)?,
                language: r.get(5)?,
                visibility: r.get(6)?,
                role: r.get(7)?,
                role_confidence: r.get(8)?,
                signature: r.get(9)?,
                fan_in: r.get(10)?,
                fan_in_trusted: r.get(11)?,
                fan_out: r.get(12)?,
                fan_out_trusted: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Count nodes per context (BTreeMap for stable alphabetical ordering)
    let mut context_counts: BTreeMap<String, usize> = BTreeMap::new();
    for node in &raw_nodes {
        let ctx = file_to_context(&node.file);
        *context_counts.entry(ctx).or_insert(0) += 1;
    }

    // Compute context centroids:
    // "root" (single-crate projects) always sits at the origin.
    // All other contexts are arranged radially outward from it.
    let context_names: Vec<String> = context_counts.keys().cloned().collect();
    let non_root: Vec<&String> = context_names
        .iter()
        .filter(|n| n.as_str() != "root")
        .collect();
    let n_radial = non_root.len().max(1);
    let r_global = 20.0 * (n_radial as f64).sqrt();

    let mut context_centroid: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    if context_counts.contains_key("root") {
        context_centroid.insert("root".to_string(), (0.0, 0.0));
    }
    for (i, name) in non_root.iter().enumerate() {
        let angle = (i as f64 / n_radial as f64) * 2.0 * PI;
        context_centroid.insert(
            (*name).clone(),
            (r_global * angle.cos(), r_global * angle.sin()),
        );
    }

    // Build positioned nodes
    let mut viz_nodes: Vec<VizNode> = Vec::with_capacity(raw_nodes.len());
    for node in &raw_nodes {
        let ctx = file_to_context(&node.file);
        let node_count = *context_counts.get(&ctx).unwrap_or(&1);
        // Radius grows slowly with count but is capped — keeps clusters tight,
        // density within the cluster communicates node count
        let context_radius = (6.0 + (node_count as f64 / 100.0).sqrt()).min(10.0);

        let hash = fnv1a_u64(node.stable_id.as_bytes());
        let jitter_angle = (hash & 0xFFFF) as f64 / 65536.0 * 2.0 * PI;
        let jitter_r = ((hash >> 16) & 0xFFFF) as f64 / 65536.0 * context_radius;
        let (cx, cy) = context_centroid.get(&ctx).copied().unwrap_or((0.0, 0.0));

        let layer = crate::arch::role_z(&node.role);
        let z_jitter = ((hash >> 32) & 0xFFFF) as f64 / 65536.0 * 4.0 - 2.0;

        viz_nodes.push(VizNode {
            id: node.id,
            name: node.name.clone(),
            kind: node.kind.clone(),
            stable_id: node.stable_id.clone(),
            file: node.file.clone(),
            language: node.language.clone(),
            visibility: node.visibility.clone(),
            role: node.role.clone(),
            role_confidence: node.role_confidence,
            fan_in: node.fan_in,
            fan_in_trusted: node.fan_in_trusted,
            fan_out: node.fan_out,
            fan_out_trusted: node.fan_out_trusted,
            context: ctx,
            signature: node.signature.clone(),
            pos: Pos {
                x: cx + jitter_r * jitter_angle.cos(),
                y: cy + jitter_r * jitter_angle.sin(),
                z: layer + z_jitter,
            },
        });
    }

    // Load config violation rules (empty when no config file exists).
    let cfg = crate::config::load(".");

    // Query edges — JOIN both endpoints to classify violations inline.
    // Include file columns to derive contexts for config rules.
    let mut edge_stmt = conn.prepare(
        "SELECT e.from_id, e.to_id, e.edge_type, e.source,
                nf.role AS from_role, nt.role AS to_role,
                nf.file AS from_file, nt.file AS to_file
         FROM edges e
         JOIN nodes nf ON nf.id = e.from_id
         JOIN nodes nt ON nt.id = e.to_id",
    )?;

    struct RawEdge {
        from_id: i64,
        to_id: i64,
        edge_type: String,
        source: String,
        from_role: String,
        to_role: String,
        from_file: String,
        to_file: String,
    }

    let raw_edges: Vec<RawEdge> = edge_stmt
        .query_map([], |r| {
            Ok(RawEdge {
                from_id: r.get(0)?,
                to_id: r.get(1)?,
                edge_type: r.get(2)?,
                source: r.get(3)?,
                from_role: r.get(4)?,
                to_role: r.get(5)?,
                from_file: r.get(6)?,
                to_file: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut violation_count = 0usize;
    let mut suppressed_violation_count = 0usize;

    let viz_edges: Vec<VizEdge> = raw_edges
        .iter()
        .map(|e| {
            let from_ctx = file_to_context(&e.from_file);
            let to_ctx = file_to_context(&e.to_file);

            // Config context rules apply to all edges — context coupling is
            // file-level, which tree-sitter reliably captures.
            let candidate = context_violation_reason(&from_ctx, &to_ctx, &cfg.violations)
                // Role-layer heuristic applies only to trusted edges to prevent
                // false positives from unresolved common names.
                .or_else(|| {
                    if e.source == "rust-analyzer" {
                        role_violation_reason(&e.from_role, &e.to_role)
                    } else {
                        None
                    }
                });

            let violation = if candidate.is_some() {
                if exception_reason(
                    &from_ctx,
                    &to_ctx,
                    &e.from_role,
                    &e.to_role,
                    &cfg.exceptions,
                )
                .is_some()
                {
                    suppressed_violation_count += 1;
                    None
                } else {
                    violation_count += 1;
                    candidate
                }
            } else {
                None
            };

            VizEdge {
                from: e.from_id,
                to: e.to_id,
                edge_type: e.edge_type.clone(),
                source: e.source.clone(),
                violation,
            }
        })
        .collect();

    let contexts: Vec<ContextInfo> = context_counts
        .into_iter()
        .map(|(name, node_count)| ContextInfo { name, node_count })
        .collect();

    let output = VizOutput {
        meta: Meta {
            total_nodes: viz_nodes.len(),
            total_edges: viz_edges.len(),
            violation_count,
            suppressed_violation_count,
        },
        contexts,
        nodes: viz_nodes,
        edges: viz_edges,
    };

    Ok(output)
}

fn output_to_html(output: &VizOutput) -> Result<String> {
    let json = serde_json::to_string(output)?;
    // Escape </script> so embedded JSON cannot break the script tag
    let safe_json = json.replace("</script>", r"<\/script>");
    Ok(VIZ_TEMPLATE.replace("__GRAPH_DATA__", &safe_json))
}

/// Generate the self-contained HTML visualization as a string.
/// Reads from the database at the current working directory.
pub fn generate_html() -> Result<String> {
    let conn = open_db()?;
    let output = build_output(&conn)?;
    output_to_html(&output)
}

pub fn run(html: bool) -> Result<()> {
    let conn = open_db()?;
    let output = build_output(&conn)?;
    if html {
        println!("{}", output_to_html(&output)?);
    } else {
        println!("{}", serde_json::to_string(&output)?);
    }
    Ok(())
}
