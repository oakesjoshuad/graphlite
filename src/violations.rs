use std::collections::BTreeMap;

use anyhow::Result;

use crate::arch::{context_violation_reason, exception_reason, role_violation_reason};
use crate::config;
use crate::query::open_db;

pub struct Params<'a> {
    /// Filter to edges leaving a specific bounded context.
    pub from_context: Option<&'a str>,
    /// Filter to edges arriving at a specific bounded context.
    pub to_context: Option<&'a str>,
    /// Alias: show both sides of a single context (from OR to).
    pub context: Option<&'a str>,
    /// Pattern filter: "from_role:to_role" e.g. "infra:domain".
    pub pattern: Option<&'a str>,
    /// Max violations to show (0 = unlimited).
    pub top: usize,
    /// Suppress callee/caller signatures from output.
    pub no_snippets: bool,
}

struct RawEdge {
    from_id: i64,
    from_name: String,
    from_file: String,
    from_role: String,
    from_fan_in: i64,
    from_fan_out: i64,
    from_sig: Option<String>,
    to_id: i64,
    to_name: String,
    to_file: String,
    to_role: String,
    to_fan_in: i64,
    to_fan_out: i64,
    to_sig: Option<String>,
}

struct Violation {
    from_id: i64,
    from_name: String,
    from_file: String,
    from_ctx: String,
    from_role: String,
    from_fan_in: i64,
    from_fan_out: i64,
    from_sig: Option<String>,
    to_id: i64,
    to_name: String,
    to_file: String,
    to_ctx: String,
    to_role: String,
    to_fan_in: i64,
    to_fan_out: i64,
    to_sig: Option<String>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn run(params: &Params) -> Result<()> {
    let conn = open_db()?;
    let cfg = config::load(".");

    // Parse optional pattern filter.
    let (pat_from_role, pat_to_role): (Option<&str>, Option<&str>) = if let Some(p) = params.pattern
    {
        let parts: Vec<&str> = p.splitn(2, ':').collect();
        if parts.len() == 2 {
            (Some(parts[0]), Some(parts[1]))
        } else {
            (Some(p), None)
        }
    } else {
        (None, None)
    };

    // Load all trusted edges (excluding test nodes) with enough metadata to
    // classify violations in Rust rather than SQL (keeps SQL simple).
    let mut stmt = conn.prepare(
        "SELECT
             nf.id,        nf.name,       nf.file,       nf.role,
             nf.signature, nt.id,         nt.name,       nt.file,
             nt.role,      nt.signature,
             (SELECT COUNT(*) FROM edges WHERE to_id   = nf.id) AS from_fan_in,
             (SELECT COUNT(*) FROM edges WHERE from_id = nf.id) AS from_fan_out,
             (SELECT COUNT(*) FROM edges WHERE to_id   = nt.id) AS to_fan_in,
             (SELECT COUNT(*) FROM edges WHERE from_id = nt.id) AS to_fan_out
         FROM edges e
         JOIN nodes nf ON nf.id = e.from_id
         JOIN nodes nt ON nt.id = e.to_id
         WHERE e.source = 'rust-analyzer'
           AND nf.role  != 'test'
           AND nt.role  != 'test'
         GROUP BY e.from_id, e.to_id",
    )?;

    let raw: Vec<RawEdge> = stmt
        .query_map([], |r| {
            Ok(RawEdge {
                from_id: r.get(0)?,
                from_name: r.get(1)?,
                from_file: r.get(2)?,
                from_role: r.get(3)?,
                from_sig: r.get(4)?,
                to_id: r.get(5)?,
                to_name: r.get(6)?,
                to_file: r.get(7)?,
                to_role: r.get(8)?,
                to_sig: r.get(9)?,
                from_fan_in: r.get(10)?,
                from_fan_out: r.get(11)?,
                to_fan_in: r.get(12)?,
                to_fan_out: r.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Classify each edge as a violation, apply exceptions, apply filters.
    let mut violations: Vec<Violation> = Vec::new();
    let mut suppressed_counts: BTreeMap<String, usize> = BTreeMap::new();

    for e in &raw {
        let from_ctx = crate::arch::file_to_context(&e.from_file);
        let to_ctx = crate::arch::file_to_context(&e.to_file);

        // Check for any violation reason (context rule or role inversion).
        let reason = context_violation_reason(&from_ctx, &to_ctx, &cfg.violations)
            .or_else(|| role_violation_reason(&e.from_role, &e.to_role));

        let reason = match reason {
            Some(r) => r,
            None => continue,
        };

        // Check suppression exceptions.
        if let Some(exc_reason) = exception_reason(
            &from_ctx,
            &to_ctx,
            &e.from_role,
            &e.to_role,
            &cfg.exceptions,
        ) {
            *suppressed_counts.entry(exc_reason.to_string()).or_insert(0) += 1;
            continue;
        }

        // Apply context / pattern filters.
        if let Some(ctx) = params.context {
            if from_ctx != ctx && to_ctx != ctx {
                continue;
            }
        }
        if let Some(fc) = params.from_context {
            if from_ctx != fc {
                continue;
            }
        }
        if let Some(tc) = params.to_context {
            if to_ctx != tc {
                continue;
            }
        }
        if let Some(pfr) = pat_from_role {
            if e.from_role != pfr {
                continue;
            }
        }
        if let Some(ptr) = pat_to_role {
            if e.to_role != ptr {
                continue;
            }
        }

        let _ = reason; // used for suppression check above; group key encodes it
        violations.push(Violation {
            from_id: e.from_id,
            from_name: e.from_name.clone(),
            from_file: e.from_file.clone(),
            from_ctx,
            from_role: e.from_role.clone(),
            from_fan_in: e.from_fan_in,
            from_fan_out: e.from_fan_out,
            from_sig: e.from_sig.clone(),
            to_id: e.to_id,
            to_name: e.to_name.clone(),
            to_file: e.to_file.clone(),
            to_ctx,
            to_role: e.to_role.clone(),
            to_fan_in: e.to_fan_in,
            to_fan_out: e.to_fan_out,
            to_sig: e.to_sig.clone(),
        });
    }

    // Sort by callee fan_in descending (most-depended-on callees first).
    violations.sort_unstable_by(|a, b| b.to_fan_in.cmp(&a.to_fan_in));

    let total = violations.len();
    let suppressed_total: usize = suppressed_counts.values().sum();

    let shown = if params.top > 0 && violations.len() > params.top {
        violations.truncate(params.top);
        params.top
    } else {
        total
    };

    // Group by (from_role, to_role) for summary.
    let mut pattern_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for v in &violations {
        *pattern_counts
            .entry((v.from_role.clone(), v.to_role.clone()))
            .or_insert(0) += 1;
    }

    // Estimate tokens (rough: ~4 chars/token).
    let mut buf = String::new();
    render_xml(
        &violations,
        &pattern_counts,
        &suppressed_counts,
        total,
        suppressed_total,
        shown,
        params.no_snippets,
        &mut buf,
    );
    let token_est = buf.len() / 4;

    // Prepend token estimate to opening tag.
    let output = buf.replacen(
        "<violations ",
        &format!("<violations tokens=\"{token_est}\" "),
        1,
    );
    print!("{output}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_xml(
    violations: &[Violation],
    pattern_counts: &BTreeMap<(String, String), usize>,
    suppressed_counts: &BTreeMap<String, usize>,
    total: usize,
    suppressed_total: usize,
    shown: usize,
    no_snippets: bool,
    buf: &mut String,
) {
    buf.push_str(&format!(
        "<violations total=\"{total}\" suppressed=\"{suppressed_total}\" shown=\"{shown}\" source=\"trusted\">\n"
    ));

    // Suppression summary.
    if !suppressed_counts.is_empty() {
        buf.push_str("  <suppressed_summary>\n");
        for (reason, count) in suppressed_counts {
            buf.push_str(&format!(
                "    <exception count=\"{count}\" reason=\"{}\"/>\n",
                xml_escape(reason)
            ));
        }
        buf.push_str("  </suppressed_summary>\n");
    }

    // Pattern summary.
    buf.push_str("  <summary>\n");
    for ((fr, tr), count) in pattern_counts {
        buf.push_str(&format!(
            "    <pattern from_role=\"{}\" to_role=\"{}\" count=\"{count}\"/>\n",
            xml_escape(fr),
            xml_escape(tr)
        ));
    }
    buf.push_str("  </summary>\n");

    // Group violations by (from_role, to_role).
    let mut groups: BTreeMap<(&str, &str), Vec<&Violation>> = BTreeMap::new();
    for v in violations {
        groups
            .entry((v.from_role.as_str(), v.to_role.as_str()))
            .or_default()
            .push(v);
    }

    for ((fr, tr), group) in &groups {
        buf.push_str(&format!("  <group from_role=\"{fr}\" to_role=\"{tr}\">\n"));
        for v in group.iter() {
            buf.push_str(&format!(
                "    <violation callee_fan_in=\"{}\" caller_fan_out=\"{}\">\n",
                v.to_fan_in, v.from_fan_out
            ));
            buf.push_str(&format!(
                "      <caller id=\"{}\" name=\"{}\" file=\"{}\" context=\"{}\" role=\"{}\" fan_in=\"{}\" fan_out=\"{}\"/>\n",
                v.from_id,
                xml_escape(&v.from_name),
                xml_escape(&v.from_file),
                xml_escape(&v.from_ctx),
                xml_escape(&v.from_role),
                v.from_fan_in,
                v.from_fan_out,
            ));
            buf.push_str(&format!(
                "      <callee id=\"{}\" name=\"{}\" file=\"{}\" context=\"{}\" role=\"{}\" fan_in=\"{}\" fan_out=\"{}\"/>\n",
                v.to_id,
                xml_escape(&v.to_name),
                xml_escape(&v.to_file),
                xml_escape(&v.to_ctx),
                xml_escape(&v.to_role),
                v.to_fan_in,
                v.to_fan_out,
            ));
            if !no_snippets {
                if let Some(sig) = &v.from_sig {
                    buf.push_str(&format!(
                        "      <caller_signature>{}</caller_signature>\n",
                        xml_escape(sig)
                    ));
                }
                if let Some(sig) = &v.to_sig {
                    buf.push_str(&format!(
                        "      <callee_signature>{}</callee_signature>\n",
                        xml_escape(sig)
                    ));
                }
            }
            buf.push_str("    </violation>\n");
        }
        buf.push_str("  </group>\n");
    }

    buf.push_str("</violations>\n");
}
