use anyhow::Result;
use rusqlite::Connection;
use tracing::info;

struct NodeMetrics {
    id: i64,
    kind: String,
    visibility: String,
    file: String,
    name: String,
    fan_in: i64,
    fan_out: i64,
    io_call_count: i64,
    caller_file_spread: i64,
    callee_file_spread: i64,
}

/// Percentile rank: fraction of sorted values <= value.
/// Returns 0.0 for empty slices.
fn percentile_rank(value: i64, sorted: &[i64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.partition_point(|&x| x <= value) as f64 / sorted.len() as f64
}

/// Second-pass role inference over all nodes using graph math.
///
/// Runs after symbols, edges, and rustdoc/resolver enrichment are committed so it sees
/// the full graph. Pure SQL reads + a single batched UPDATE — no file I/O.
pub fn infer_roles(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT
             n.id,
             n.kind,
             n.visibility,
             n.file,
             n.name,
             (SELECT COUNT(*) FROM edges WHERE from_id = n.id)           AS fan_out,
             (SELECT COUNT(*) FROM edges WHERE to_id   = n.id)           AS fan_in,
             -- IO proximity: callee names matching known IO patterns
             (SELECT COUNT(*) FROM edges e
              JOIN nodes c ON e.to_id = c.id
              WHERE e.from_id = n.id
                AND (   c.name LIKE 'open%'
                     OR c.name LIKE 'read%'
                     OR c.name LIKE 'write%'
                     OR c.name LIKE 'query%'
                     OR c.name LIKE 'execute%'
                     OR c.name LIKE 'connect%'
                     OR c.name LIKE 'send%'
                     OR c.name LIKE 'recv%'
                     OR c.name LIKE 'fetch%'
                     OR c.name IN ('spawn', 'init_db', 'open_db')))     AS io_call_count,
             -- Caller spread: how many distinct source files call this node
             (SELECT COUNT(DISTINCT caller.file)
              FROM edges e
              JOIN nodes caller ON e.from_id = caller.id
              WHERE e.to_id = n.id)                                      AS caller_file_spread,
             -- Callee spread: how many distinct files this node calls into
             (SELECT COUNT(DISTINCT callee.file)
              FROM edges e
              JOIN nodes callee ON e.to_id = callee.id
              WHERE e.from_id = n.id)                                    AS callee_file_spread
         FROM nodes n
         WHERE n.role_confidence < 0.99",
    )?;

    let nodes: Vec<NodeMetrics> = stmt
        .query_map([], |r| {
            Ok(NodeMetrics {
                id: r.get(0)?,
                kind: r.get(1)?,
                visibility: r.get(2)?,
                file: r.get(3)?,
                name: r.get(4)?,
                fan_out: r.get(5)?,
                fan_in: r.get(6)?,
                io_call_count: r.get(7)?,
                caller_file_spread: r.get(8)?,
                callee_file_spread: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if nodes.is_empty() {
        return Ok(());
    }

    // Build sorted distributions for percentile ranking
    let mut fan_out_sorted: Vec<i64> = nodes.iter().map(|n| n.fan_out).collect();
    let mut fan_in_sorted: Vec<i64> = nodes.iter().map(|n| n.fan_in).collect();
    fan_out_sorted.sort_unstable();
    fan_in_sorted.sort_unstable();

    // Spread penalty: small codebases with low absolute variance get lower confidence.
    // A codebase where max fan_out=3 is much less certain than one where max=30+.
    let max_connectivity = fan_out_sorted
        .last()
        .copied()
        .unwrap_or(0)
        .max(fan_in_sorted.last().copied().unwrap_or(0));
    let spread_factor = (max_connectivity as f64 / 10.0).min(1.0);
    let spread_penalty = 0.5 + 0.5 * spread_factor;

    let tx = conn.unchecked_transaction()?;
    {
        let mut update =
            tx.prepare_cached("UPDATE nodes SET role = ?2, role_confidence = ?3 WHERE id = ?1")?;

        for node in &nodes {
            let (role, confidence) =
                classify(node, &fan_out_sorted, &fan_in_sorted, spread_penalty);
            update.execute(rusqlite::params![node.id, role, confidence])?;
        }
    }
    tx.commit()?;

    info!(count = nodes.len(), "roles inferred");
    Ok(())
}

pub fn run(root: &str) -> Result<()> {
    let graphlite_dir = format!("{}/.graphlite", root.trim_end_matches('/'));
    std::fs::create_dir_all(&graphlite_dir)?;
    let db_path = format!("{}/codegraph.db", graphlite_dir);
    let conn = crate::schema::open_or_init_db(&db_path)?;
    infer_roles(&conn)
}

fn classify(
    node: &NodeMetrics,
    fan_out_sorted: &[i64],
    fan_in_sorted: &[i64],
    spread_penalty: f64,
) -> (&'static str, f64) {
    // Test files are classified structurally — skip graph-metric classification.
    if crate::arch::is_test_file(&node.file) {
        return ("test", 0.95);
    }

    // Name heuristic: private fns with no callers whose name starts with `test_`.
    // Catches inline test helpers that aren't decorated with `#[test]`.
    if node.fan_in == 0
        && node.kind == "fn"
        && node.visibility != "pub"
        && node.name.starts_with("test_")
    {
        return ("test", 0.75);
    }

    let fo_rank = percentile_rank(node.fan_out, fan_out_sorted);
    let fi_rank = percentile_rank(node.fan_in, fan_in_sorted);

    // --- Structural signals (high confidence, spread-independent) ---

    // Data / type definitions
    if matches!(
        node.kind.as_str(),
        "struct" | "enum" | "type" | "interface" | "const"
    ) {
        let conf = if node.fan_out == 0 { 0.95 } else { 0.75 };
        return ("model", conf);
    }

    // No outgoing calls at all → terminal node
    if node.fan_out == 0 {
        return ("leaf", 0.90);
    }

    // Called by nothing AND public → top of call chain
    if node.fan_in == 0 && node.visibility == "pub" {
        return ("entrypoint", 0.85);
    }

    // --- IO proximity (pattern-matched, moderate confidence) ---
    if node.io_call_count > 0 {
        return ("infra", 0.80 * spread_penalty);
    }

    // --- Distribution-based (relative, spread-penalised) ---

    // Top 20% by fan_out AND calls across multiple files → true coordinator.
    // Requiring callee_file_spread >= 2 prevents high-fan-out helpers that only call
    // within their own file (e.g. apply_cors_headers) from being misclassified.
    if fo_rank >= 0.80 && node.callee_file_spread >= 2 {
        // Interpolate from 0.70 at the 80th percentile to 0.80 at the 100th
        let scaled = 0.70 + 0.10 * (fo_rank - 0.80) / 0.20;
        return ("orchestrator", scaled * spread_penalty);
    }

    // Top 20% by fan_in → many callers
    if fi_rank >= 0.80 {
        if node.caller_file_spread >= 2 {
            // Called from multiple files → reusable helper
            let scaled = 0.65 + 0.10 * (fi_rank - 0.80) / 0.20;
            return ("utility", scaled * spread_penalty);
        }
        // Called heavily but from one place → domain logic
        let scaled = 0.55 + 0.10 * (fi_rank - 0.80) / 0.20;
        return ("domain", scaled * spread_penalty);
    }

    // Residual: moderate fan_in signals domain, everything else is uncertain
    if fi_rank >= 0.50 {
        return ("domain", 0.45 * spread_penalty);
    }

    ("domain", 0.30 * spread_penalty)
}
