use std::collections::BTreeMap;

use anyhow::Result;

use crate::arch::{context_violation_reason, exception_reason};
use crate::config;
use crate::config::{ViolationRule, VisibilityRule, WorkspaceConfig};
use crate::query::open_db;
use crate::workspace;
use crate::xml as vxml;

pub struct Params<'a> {
    /// Filter to edges leaving a specific bounded context.
    pub from_context: Option<&'a str>,
    /// Filter to edges arriving at a specific bounded context.
    pub to_context: Option<&'a str>,
    /// Alias: show both sides of a single context (from OR to).
    pub context: Option<&'a str>,
    /// Max violations to show (0 = unlimited).
    pub top: usize,
    /// Optional substring filter over context/symbol/file/layer fields.
    pub pattern: Option<&'a str>,
    /// Group coupling findings by owning crate edge.
    pub by_crate: bool,
    /// Crate-edge selector (e.g. \"from:adapter to:domain\").
    pub edge: Option<&'a str>,
    /// Suppress callee/caller signatures from output.
    pub no_snippets: bool,
    /// Fail when policy has unresolved or ambiguous mappings.
    pub strict_policy: bool,
    /// Include clippy/rustc diagnostic violations from node_diagnostics.
    pub check: bool,
    /// Include crate advisory findings from crate_advisories.
    pub audit: bool,
}

struct RawEdge {
    from_id: i64,
    from_name: String,
    from_file: String,
    from_role: String,
    from_stable_id: String,
    from_visibility: String,
    from_fan_in: i64,
    from_fan_out: i64,
    from_sig: Option<String>,
    to_id: i64,
    to_name: String,
    to_file: String,
    to_role: String,
    to_stable_id: String,
    to_visibility: String,
    to_fan_in: i64,
    to_fan_out: i64,
    to_sig: Option<String>,
}

struct Violation {
    from_id: i64,
    from_name: String,
    from_file: String,
    from_ctx: String,
    from_crate: Option<String>,
    from_role: String,
    from_visibility: String,
    from_fan_in: i64,
    from_fan_out: i64,
    from_sig: Option<String>,
    to_id: i64,
    to_name: String,
    to_file: String,
    to_ctx: String,
    to_crate: Option<String>,
    to_role: String,
    to_visibility: String,
    to_fan_in: i64,
    to_fan_out: i64,
    to_sig: Option<String>,
}

struct VisibilityViolation {
    stable_id: String,
    symbol: String,
    file: String,
    line: i64,
    layer: String,
    actual_visibility: String,
    max_visibility: String,
    reason: String,
}

struct WorkspaceViolation {
    from_crate: String,
    from_layer: String,
    to_crate: String,
    to_layer: String,
    reason: String,
}

struct PolicyDiagnostics {
    policy_source: String,
    strict_policy: bool,
    context_rule_count: usize,
    exception_count: usize,
    workspace_layers_total: usize,
    workspace_layers_assigned: usize,
    workspace_rule_count: usize,
    trusted_edges_loaded: usize,
}

struct CheckViolation {
    node_id: i64,
    name: String,
    file: String,
    role: String,
    visibility: String,
    code: String,
    level: String,
    message: String,
}

struct AdvisoryViolation {
    package_name: String,
    version: String,
    advisory_id: String,
    title: String,
    severity: String,
    kind: String,
}

const VALID_LAYERS: &[&str] = &[
    "shared",
    "domain",
    "port",
    "application",
    "infra",
    "adapter",
    "composition",
];

const VALID_VISIBILITIES: &[&str] = &["default", "crate", "restricted", "public"];

fn check_workspace_violations(
    ws_cfg: Option<&WorkspaceConfig>,
    graph: Option<&workspace::WorkspaceGraph>,
) -> Vec<WorkspaceViolation> {
    let ws_cfg = match ws_cfg {
        Some(ws) if !ws.violations.is_empty() => ws,
        _ => return vec![],
    };

    let graph = match graph {
        Some(g) => g,
        None => return vec![],
    };

    let mut result = Vec::new();
    for (from_crate, to_crate) in &graph.deps {
        let from_layer = match ws_cfg.layers.get(from_crate) {
            Some(l) if l != "?" => l.as_str(),
            _ => continue,
        };
        let to_layer = match ws_cfg.layers.get(to_crate) {
            Some(l) if l != "?" => l.as_str(),
            _ => continue,
        };
        for rule in &ws_cfg.violations {
            if rule.from_layer == from_layer && rule.to_layer == to_layer {
                let reason = rule.reason.clone().unwrap_or_else(|| {
                    format!("{} must not depend on {}", from_layer, to_layer)
                });
                result.push(WorkspaceViolation {
                    from_crate: from_crate.clone(),
                    from_layer: from_layer.to_string(),
                    to_crate: to_crate.clone(),
                    to_layer: to_layer.to_string(),
                    reason,
                });
                break;
            }
        }
    }
    result
}

fn validate_policy(
    workspace: Option<&WorkspaceConfig>,
    violations: &[ViolationRule],
    visibility_rules: &[VisibilityRule],
    strict_policy: bool,
) -> Result<()> {
    let mut errors: Vec<String> = Vec::new();
    if let Some(ws) = workspace {
        for (crate_name, layer) in &ws.layers {
            if layer == "?" && strict_policy {
                errors.push(format!(
                    "workspace layer unresolved for crate '{}' (value '?')",
                    crate_name
                ));
            } else if layer != "?" && !VALID_LAYERS.contains(&layer.as_str()) {
                errors.push(format!(
                    "workspace layer '{}' for crate '{}' is invalid; valid values: {}",
                    layer,
                    crate_name,
                    VALID_LAYERS.join(", ")
                ));
            }
        }
        for rule in &ws.violations {
            if !VALID_LAYERS.contains(&rule.from_layer.as_str()) {
                errors.push(format!(
                    "workspace violation rule has invalid from_layer '{}'",
                    rule.from_layer
                ));
            }
            if !VALID_LAYERS.contains(&rule.to_layer.as_str()) {
                errors.push(format!(
                    "workspace violation rule has invalid to_layer '{}'",
                    rule.to_layer
                ));
            }
        }
    }
    for (i, rule) in violations.iter().enumerate() {
        if rule.from_context.trim().is_empty() || rule.to_context.trim().is_empty() {
            errors.push(format!(
                "context violation rule #{} has empty from_context or to_context",
                i + 1
            ));
        }
    }
    for rule in visibility_rules {
        if !VALID_LAYERS.contains(&rule.layer.as_str()) {
            errors.push(format!(
                "visibility rule has invalid layer '{}'",
                rule.layer
            ));
        }
        if !VALID_VISIBILITIES.contains(&rule.max_visibility.as_str()) {
            errors.push(format!(
                "visibility rule for layer '{}' has invalid max_visibility '{}' (expected one of: {})",
                rule.layer,
                rule.max_visibility,
                VALID_VISIBILITIES.join(", ")
            ));
        }
    }
    if !errors.is_empty() {
        anyhow::bail!("policy validation failed:\n- {}", errors.join("\n- "));
    }
    Ok(())
}

pub fn run(params: &Params) -> Result<()> {
    let conn = open_db()?;
    let cfg = config::load(".");
    let effective = crate::policy::effective_policy(&cfg)?;
    validate_policy(
        effective.workspace.as_ref(),
        &effective.violations,
        &effective.visibility_rules,
        params.strict_policy,
    )?;
    let ws_graph = workspace::detect(".");

    let policy_source = if let Some(pack) = &effective.pack {
        format!(".graphlite/config.toml + pack:{pack}")
    } else if std::path::Path::new(".graphlite/config.toml").exists() {
        ".graphlite/config.toml".to_string()
    } else {
        "default-config".to_string()
    };
    let (workspace_layers_total, workspace_layers_assigned, workspace_rule_count) =
        match &effective.workspace
    {
        Some(ws) => (
            ws.layers.len(),
            ws.layers.values().filter(|v| v.as_str() != "?").count(),
            ws.violations.len(),
        ),
        None => (0, 0, 0),
    };

    // Load all trusted edges (excluding test nodes) with enough metadata to
    // classify violations in Rust rather than SQL (keeps SQL simple).
    let mut stmt = conn.prepare(
        "SELECT
             nf.id,        nf.name,       nf.file,       nf.role,
             nf.stable_id, nf.visibility, nf.signature,  nt.id,         nt.name,
             nt.file,      nt.role,       nt.stable_id, nt.visibility, nt.signature,
             (SELECT COUNT(*) FROM edges WHERE to_id   = nf.id) AS from_fan_in,
             (SELECT COUNT(*) FROM edges WHERE from_id = nf.id) AS from_fan_out,
             (SELECT COUNT(*) FROM edges WHERE to_id   = nt.id) AS to_fan_in,
             (SELECT COUNT(*) FROM edges WHERE from_id = nt.id) AS to_fan_out
         FROM edges e
         JOIN nodes nf ON nf.id = e.from_id
         JOIN nodes nt ON nt.id = e.to_id
         WHERE e.source IN ('resolver', 'rustdoc')
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
                from_stable_id: r.get(4)?,
                from_visibility: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                from_sig: r.get(6)?,
                to_id: r.get(7)?,
                to_name: r.get(8)?,
                to_file: r.get(9)?,
                to_role: r.get(10)?,
                to_stable_id: r.get(11)?,
                to_visibility: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                to_sig: r.get(13)?,
                from_fan_in: r.get(14)?,
                from_fan_out: r.get(15)?,
                to_fan_in: r.get(16)?,
                to_fan_out: r.get(17)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Classify each edge as a violation, apply exceptions, apply filters.
    let mut violations: Vec<Violation> = Vec::new();
    let mut suppressed_counts: BTreeMap<String, usize> = BTreeMap::new();
    let edge_filter = params.edge.and_then(parse_edge_selector);

    for e in &raw {
        let from_ctx = crate::arch::file_to_context(&e.from_file);
        let to_ctx = crate::arch::file_to_context(&e.to_file);
        let from_crate = ws_graph
            .as_ref()
            .and_then(|ws| crate_for_file_path(ws, &e.from_file))
            .map(str::to_string);
        let to_crate = ws_graph
            .as_ref()
            .and_then(|ws| crate_for_file_path(ws, &e.to_file))
            .map(str::to_string);

        // Check for a context-coupling violation (config-defined forbidden paths).
        let reason = context_violation_reason(&from_ctx, &to_ctx, &effective.violations);

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
            &effective.exceptions,
        ) {
            *suppressed_counts.entry(exc_reason.to_string()).or_insert(0) += 1;
            continue;
        }
        if let Some(exc_reason) = stable_id_exception_reason(
            &e.from_stable_id,
            &e.to_stable_id,
            &effective.exceptions,
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
        if let Some(p) = params.pattern {
            if !matches_pattern(
                p,
                &[
                    &from_ctx,
                    &to_ctx,
                    &e.from_name,
                    &e.to_name,
                    &e.from_file,
                    &e.to_file,
                    from_crate.as_deref().unwrap_or(""),
                    to_crate.as_deref().unwrap_or(""),
                ],
            ) {
                continue;
            }
        }
        if let Some((want_from, want_to)) = &edge_filter {
            if want_from
                .as_deref()
                .is_some_and(|v| from_crate.as_deref() != Some(v))
            {
                continue;
            }
            if want_to
                .as_deref()
                .is_some_and(|v| to_crate.as_deref() != Some(v))
            {
                continue;
            }
        }

        let _ = reason;
        violations.push(Violation {
            from_id: e.from_id,
            from_name: e.from_name.clone(),
            from_file: e.from_file.clone(),
            from_ctx,
            from_crate,
            from_role: e.from_role.clone(),
            from_visibility: e.from_visibility.clone(),
            from_fan_in: e.from_fan_in,
            from_fan_out: e.from_fan_out,
            from_sig: e.from_sig.clone(),
            to_id: e.to_id,
            to_name: e.to_name.clone(),
            to_file: e.to_file.clone(),
            to_ctx,
            to_crate,
            to_role: e.to_role.clone(),
            to_visibility: e.to_visibility.clone(),
            to_fan_in: e.to_fan_in,
            to_fan_out: e.to_fan_out,
            to_sig: e.to_sig.clone(),
        });
    }

    // Sort by callee fan_in descending (most-depended-on callees first).
    violations.sort_unstable_by_key(|v| std::cmp::Reverse(v.to_fan_in));

    let total = violations.len();
    let mut visibility_violations = check_visibility_violations(
        &effective,
        ws_graph.as_ref(),
        &conn,
    )?;
    if let Some(p) = params.pattern {
        visibility_violations.retain(|v| {
            matches_pattern(
                p,
                &[&v.symbol, &v.file, &v.layer, &v.actual_visibility, &v.max_visibility, &v.stable_id],
            )
        });
    }
    visibility_violations.retain(|v| {
        if let Some(exc_reason) =
            stable_id_exception_reason(&v.stable_id, &v.stable_id, &effective.exceptions)
        {
            *suppressed_counts.entry(exc_reason.to_string()).or_insert(0) += 1;
            false
        } else {
            true
        }
    });
    visibility_violations.sort_by_key(|v| std::cmp::Reverse(v.line));
    let visibility_total = visibility_violations.len();
    let suppressed_total: usize = suppressed_counts.values().sum();

    let shown = if params.top > 0 && violations.len() > params.top {
        violations.truncate(params.top);
        params.top
    } else {
        total
    };
    let visibility_shown = if params.top > 0 && visibility_violations.len() > params.top {
        visibility_violations.truncate(params.top);
        params.top
    } else {
        visibility_total
    };

    // Group by (from_ctx, to_ctx) for summary.
    let mut pattern_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for v in &violations {
        *pattern_counts
            .entry((v.from_ctx.clone(), v.to_ctx.clone()))
            .or_insert(0) += 1;
    }

    let ws_violations = check_workspace_violations(effective.workspace.as_ref(), ws_graph.as_ref());
    let ws_total = ws_violations.len();
    let check_violations = if params.check {
        load_check_violations(&conn)?
    } else {
        Vec::new()
    };
    let advisory_violations = if params.audit {
        load_advisory_violations(&conn)?
    } else {
        Vec::new()
    };
    let diagnostics = PolicyDiagnostics {
        policy_source,
        strict_policy: params.strict_policy,
        context_rule_count: effective.violations.len(),
        exception_count: effective.exceptions.len(),
        workspace_layers_total,
        workspace_layers_assigned,
        workspace_rule_count,
        trusted_edges_loaded: raw.len(),
    };

    // Build XML output (streaming).
    let mut w = vxml::new_stream_writer();
    render_xml(
        &mut w,
        &diagnostics,
        &violations,
        &ws_violations,
        &pattern_counts,
        &suppressed_counts,
        total,
        suppressed_total,
        shown,
        &visibility_violations,
        visibility_total,
        visibility_shown,
        ws_total,
        &check_violations,
        &advisory_violations,
        params.by_crate,
        params.no_snippets,
    );
    vxml::finish_stream(w)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_xml(
    w: &mut quick_xml::Writer<impl std::io::Write>,
    diagnostics: &PolicyDiagnostics,
    violations: &[Violation],
    ws_violations: &[WorkspaceViolation],
    pattern_counts: &BTreeMap<(String, String), usize>,
    suppressed_counts: &BTreeMap<String, usize>,
    total: usize,
    suppressed_total: usize,
    shown: usize,
    visibility_violations: &[VisibilityViolation],
    visibility_total: usize,
    visibility_shown: usize,
    ws_total: usize,
    check_violations: &[CheckViolation],
    advisory_violations: &[AdvisoryViolation],
    by_crate: bool,
    no_snippets: bool,
) {
    let total_s = total.to_string();
    let supp_s = suppressed_total.to_string();
    let shown_s = shown.to_string();
    let ws_s = ws_total.to_string();
    let vis_total_s = visibility_total.to_string();
    let vis_shown_s = visibility_shown.to_string();

    vxml::open_attrs(
        w,
        "violations",
        &[
            ("total", &total_s),
            ("suppressed", &supp_s),
            ("shown", &shown_s),
            ("visibility_total", &vis_total_s),
            ("visibility_shown", &vis_shown_s),
            ("workspace_total", &ws_s),
            ("tokens", "streaming"),
            ("source", "trusted"),
        ],
    )
    .expect("xml");

    // Policy and analysis diagnostics are always emitted, even with zero findings.
    let strict_s = diagnostics.strict_policy.to_string();
    let ctx_rules_s = diagnostics.context_rule_count.to_string();
    let exc_s = diagnostics.exception_count.to_string();
    let ws_layers_total_s = diagnostics.workspace_layers_total.to_string();
    let ws_layers_assigned_s = diagnostics.workspace_layers_assigned.to_string();
    let ws_rules_s = diagnostics.workspace_rule_count.to_string();
    let edge_count_s = diagnostics.trusted_edges_loaded.to_string();
    vxml::empty(
        w,
        "diagnostics",
        &[
            ("policy_source", &diagnostics.policy_source),
            ("strict_policy", &strict_s),
            ("context_rules", &ctx_rules_s),
            ("exceptions", &exc_s),
            ("workspace_layers_total", &ws_layers_total_s),
            ("workspace_layers_assigned", &ws_layers_assigned_s),
            ("workspace_rules", &ws_rules_s),
            ("trusted_edges_loaded", &edge_count_s),
        ],
    )
    .expect("xml");

    // Suppression summary.
    if !suppressed_counts.is_empty() {
        vxml::open(w, "suppressed_summary").expect("xml");
        for (reason, count) in suppressed_counts {
            let count_s = count.to_string();
            vxml::empty(w, "exception", &[("count", &count_s), ("reason", reason)])
                .expect("xml");
        }
        vxml::close(w, "suppressed_summary").expect("xml");

        let mut ranked: Vec<(&String, &usize)> = suppressed_counts.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        vxml::open(w, "top_suppressions").expect("xml");
        for (reason, count) in ranked.into_iter().take(10) {
            let count_s = count.to_string();
            vxml::empty(w, "exception", &[("count", &count_s), ("reason", reason)])
                .expect("xml");
        }
        vxml::close(w, "top_suppressions").expect("xml");
    }

    // Context-coupling summary.
    vxml::open(w, "summary").expect("xml");
    for ((fc, tc), count) in pattern_counts {
        let count_s = count.to_string();
        vxml::empty(
            w,
            "pattern",
            &[
                ("from_context", fc.as_str()),
                ("to_context", tc.as_str()),
                ("count", &count_s),
            ],
        )
        .expect("xml");
    }
    vxml::close(w, "summary").expect("xml");

    if by_crate {
        let mut crate_counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        for v in violations {
            if let (Some(fc), Some(tc)) = (&v.from_crate, &v.to_crate) {
                *crate_counts.entry((fc.clone(), tc.clone())).or_insert(0) += 1;
            }
        }
        vxml::open(w, "crate_summary").expect("xml");
        for ((fc, tc), count) in &crate_counts {
            let count_s = count.to_string();
            vxml::empty(
                w,
                "edge",
                &[("from_crate", fc), ("to_crate", tc), ("count", &count_s)],
            )
            .expect("xml");
        }
        vxml::close(w, "crate_summary").expect("xml");
    }

    // Group violations by (from_ctx, to_ctx).
    let mut groups: BTreeMap<(&str, &str), Vec<&Violation>> = BTreeMap::new();
    for v in violations {
        groups
            .entry((v.from_ctx.as_str(), v.to_ctx.as_str()))
            .or_default()
            .push(v);
    }

    for ((fc, tc), group) in &groups {
        vxml::open_attrs(w, "group", &[("from_context", fc), ("to_context", tc)])
            .expect("xml");
        for v in group.iter() {
            let to_fi = v.to_fan_in.to_string();
            let from_fo = v.from_fan_out.to_string();
            vxml::open_attrs(
                w,
                "violation",
                &[("kind", "coupling"), ("callee_fan_in", &to_fi), ("caller_fan_out", &from_fo)],
            )
            .expect("xml");

            let from_id_s = v.from_id.to_string();
            let from_fi_s = v.from_fan_in.to_string();
            let from_fo_s = v.from_fan_out.to_string();
            let mut caller_attrs = vec![
                ("id", from_id_s.as_str()),
                ("name", v.from_name.as_str()),
                ("file", v.from_file.as_str()),
                ("context", v.from_ctx.as_str()),
                ("role", v.from_role.as_str()),
                ("visibility", v.from_visibility.as_str()),
                ("fan_in", from_fi_s.as_str()),
                ("fan_out", from_fo_s.as_str()),
            ];
            if let Some(fc) = &v.from_crate {
                caller_attrs.push(("crate", fc.as_str()));
            }
            vxml::empty(w, "caller", &caller_attrs).expect("xml");

            let to_id_s = v.to_id.to_string();
            let to_fi_s = v.to_fan_in.to_string();
            let to_fo_s = v.to_fan_out.to_string();
            let mut callee_attrs = vec![
                ("id", to_id_s.as_str()),
                ("name", v.to_name.as_str()),
                ("file", v.to_file.as_str()),
                ("context", v.to_ctx.as_str()),
                ("role", v.to_role.as_str()),
                ("visibility", v.to_visibility.as_str()),
                ("fan_in", to_fi_s.as_str()),
                ("fan_out", to_fo_s.as_str()),
            ];
            if let Some(tc) = &v.to_crate {
                callee_attrs.push(("crate", tc.as_str()));
            }
            vxml::empty(w, "callee", &callee_attrs).expect("xml");

            if !no_snippets {
                if let Some(sig) = &v.from_sig {
                    vxml::text_tag(w, "caller_signature", sig).expect("xml");
                }
                if let Some(sig) = &v.to_sig {
                    vxml::text_tag(w, "callee_signature", sig).expect("xml");
                }
            }

            vxml::close(w, "violation").expect("xml");
        }
        vxml::close(w, "group").expect("xml");
    }

    if !visibility_violations.is_empty() {
        let total_s = visibility_violations.len().to_string();
        vxml::open_attrs(w, "visibility_violations", &[("total", &total_s)]).expect("xml");
        for v in visibility_violations {
            let line_s = v.line.to_string();
            vxml::empty(
                w,
                "violation",
                &[
                    ("kind", "visibility"),
                    ("stable_id", &v.stable_id),
                    ("symbol", &v.symbol),
                    ("file", &v.file),
                    ("line", &line_s),
                    ("layer", &v.layer),
                    ("actual_visibility", &v.actual_visibility),
                    ("max_visibility", &v.max_visibility),
                    ("reason", &v.reason),
                ],
            )
            .expect("xml");
        }
        vxml::close(w, "visibility_violations").expect("xml");
    }

    // Workspace layer violations (crate-level, from Cargo.toml dep graph).
    if !ws_violations.is_empty() {
        let ws_total_s = ws_total.to_string();
        vxml::open_attrs(w, "workspace_violations", &[("total", &ws_total_s)])
            .expect("xml");

        let mut ws_groups: BTreeMap<(&str, &str), Vec<&WorkspaceViolation>> = BTreeMap::new();
        for v in ws_violations {
            ws_groups
                .entry((v.from_layer.as_str(), v.to_layer.as_str()))
                .or_default()
                .push(v);
        }
        for ((fl, tl), group) in &ws_groups {
            let count_s = group.len().to_string();
            vxml::open_attrs(
                w,
                "layer_violation",
                &[
                    ("from_layer", fl),
                    ("to_layer", tl),
                    ("count", &count_s),
                ],
            )
            .expect("xml");
            for v in group.iter() {
                vxml::empty(
                    w,
                    "dep",
                    &[
                        ("from_crate", &v.from_crate),
                        ("to_crate", &v.to_crate),
                        ("reason", &v.reason),
                    ],
                )
                .expect("xml");
            }
            vxml::close(w, "layer_violation").expect("xml");
        }
        vxml::close(w, "workspace_violations").expect("xml");
    }

    if !check_violations.is_empty() {
        let total_s = check_violations.len().to_string();
        vxml::open_attrs(w, "check_violations", &[("total", &total_s)]).expect("xml");
        for v in check_violations {
            let id_s = v.node_id.to_string();
            vxml::open_attrs(
                w,
                "violation",
                &[
                    ("kind", "check"),
                    ("id", &id_s),
                    ("name", &v.name),
                    ("file", &v.file),
                    ("role", &v.role),
                    ("visibility", &v.visibility),
                    ("code", &v.code),
                    ("level", &v.level),
                ],
            )
            .expect("xml");
            vxml::text_tag(w, "message", &v.message).expect("xml");
            vxml::close(w, "violation").expect("xml");
        }
        vxml::close(w, "check_violations").expect("xml");
    }

    if !advisory_violations.is_empty() {
        let total_s = advisory_violations.len().to_string();
        vxml::open_attrs(w, "advisory_violations", &[("total", &total_s)]).expect("xml");
        for a in advisory_violations {
            vxml::empty(
                w,
                "violation",
                &[
                    ("kind", "audit"),
                    ("package", &a.package_name),
                    ("version", &a.version),
                    ("advisory", &a.advisory_id),
                    ("severity", &a.severity),
                    ("category", &a.kind),
                    ("title", &a.title),
                ],
            )
            .expect("xml");
        }
        vxml::close(w, "advisory_violations").expect("xml");
    }

    vxml::close(w, "violations").expect("xml");
}

fn visibility_rank(v: &str) -> i32 {
    if v == "public" || v == "pub" {
        3
    } else if v.starts_with("restricted") {
        2
    } else if v == "crate" {
        1
    } else {
        0
    }
}

fn stable_id_exception_reason<'a>(
    from_stable_id: &str,
    to_stable_id: &str,
    exceptions: &'a [config::ViolationException],
) -> Option<&'a str> {
    for exc in exceptions {
        if let Some(sid) = &exc.stable_id {
            if sid == from_stable_id || sid == to_stable_id {
                return Some(exc.reason.as_deref().unwrap_or("suppressed"));
            }
        }
    }
    None
}

fn matches_pattern(pattern: &str, fields: &[&str]) -> bool {
    let p = pattern.to_lowercase();
    fields.iter().any(|f| f.to_lowercase().contains(&p))
}

fn parse_edge_selector(edge: &str) -> Option<(Option<String>, Option<String>)> {
    let mut from = None;
    let mut to = None;
    for token in edge.split_whitespace() {
        let t = token.trim_end_matches(',');
        if let Some(v) = t.strip_prefix("from:") {
            if !v.is_empty() {
                from = Some(v.to_string());
            }
        } else if let Some(v) = t.strip_prefix("to:") {
            if !v.is_empty() {
                to = Some(v.to_string());
            }
        }
    }
    if from.is_none() && to.is_none() {
        None
    } else {
        Some((from, to))
    }
}

fn check_visibility_violations(
    effective: &crate::policy::EffectivePolicy,
    ws_graph: Option<&workspace::WorkspaceGraph>,
    conn: &rusqlite::Connection,
) -> Result<Vec<VisibilityViolation>> {
    if effective.visibility_rules.is_empty() {
        return Ok(Vec::new());
    }
    let Some(ws_cfg) = &effective.workspace else {
        return Ok(Vec::new());
    };
    let Some(ws) = ws_graph else {
        return Ok(Vec::new());
    };

    let rule_map: BTreeMap<&str, (&str, &str)> = effective
        .visibility_rules
        .iter()
        .map(|r| {
            (
                r.layer.as_str(),
                (
                    r.max_visibility.as_str(),
                    r.reason
                        .as_deref()
                        .unwrap_or("visibility exceeds layer policy"),
                ),
            )
        })
        .collect();

    if rule_map.is_empty() {
        return Ok(Vec::new());
    }

    let mut stmt = conn.prepare(
        "SELECT stable_id, name, file, range_start, visibility
         FROM nodes
         WHERE kind != 'crate' AND role != 'test'",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "default".to_string()),
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (stable_id, symbol, file, line, visibility) = row?;
        let Some(crate_name) = crate_for_file_path(ws, &file) else {
            continue;
        };
        let Some(layer) = ws_cfg.layers.get(crate_name) else {
            continue;
        };
        if layer == "?" {
            continue;
        }
        let Some((max_v, reason)) = rule_map.get(layer.as_str()) else {
            continue;
        };
        if visibility_rank(&visibility) > visibility_rank(max_v) {
            out.push(VisibilityViolation {
                stable_id,
                symbol,
                file,
                line,
                layer: layer.clone(),
                actual_visibility: visibility,
                max_visibility: (*max_v).to_string(),
                reason: (*reason).to_string(),
            });
        }
    }

    Ok(out)
}

fn crate_for_file_path<'a>(ws: &'a workspace::WorkspaceGraph, file: &str) -> Option<&'a str> {
    let file_norm = file.trim_start_matches("./");
    let mut members: Vec<&workspace::CrateMember> = ws.members.iter().collect();
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

fn load_check_violations(conn: &rusqlite::Connection) -> Result<Vec<CheckViolation>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name, n.file, n.role, n.visibility, d.code, d.level, d.message
         FROM node_diagnostics d
         JOIN nodes n ON n.id = d.node_id
         WHERE d.code = 'dead_code'
         ORDER BY n.file, n.name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CheckViolation {
                node_id: r.get(0)?,
                name: r.get(1)?,
                file: r.get(2)?,
                role: r.get(3)?,
                visibility: r.get(4)?,
                code: r.get(5)?,
                level: r.get(6)?,
                message: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn load_advisory_violations(conn: &rusqlite::Connection) -> Result<Vec<AdvisoryViolation>> {
    let mut stmt = conn.prepare(
        "SELECT package_name, version, advisory_id, title,
                COALESCE(severity, 'informational'), kind
         FROM crate_advisories
         ORDER BY
            CASE COALESCE(severity, 'informational')
                WHEN 'critical' THEN 0
                WHEN 'high' THEN 1
                WHEN 'medium' THEN 2
                WHEN 'low' THEN 3
                ELSE 4
            END,
            package_name",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(AdvisoryViolation {
                package_name: r.get(0)?,
                version: r.get(1)?,
                advisory_id: r.get(2)?,
                title: r.get(3)?,
                severity: r.get(4)?,
                kind: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::validate_policy;
    use crate::config::{Config, WorkspaceConfig, WorkspaceViolationRule};
    use std::collections::BTreeMap;

    fn mk_cfg(layers: &[(&str, &str)], rules: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        let mut map = BTreeMap::new();
        for (k, v) in layers {
            map.insert((*k).to_string(), (*v).to_string());
        }
        let ws_rules = rules
            .iter()
            .map(|(from, to)| WorkspaceViolationRule {
                from_layer: (*from).to_string(),
                to_layer: (*to).to_string(),
                reason: None,
            })
            .collect();
        cfg.workspace = Some(WorkspaceConfig {
            layers: map,
            violations: ws_rules,
        });
        cfg
    }

    #[test]
    fn policy_valid_when_layers_and_rules_are_known() {
        let cfg = mk_cfg(
            &[("core", "domain"), ("adapter_http", "adapter")],
            &[("adapter", "domain")],
        );
        assert!(
            validate_policy(
                cfg.workspace.as_ref(),
                &cfg.violations,
                &cfg.visibility_rules,
                true
            )
            .is_ok()
        );
    }

    #[test]
    fn strict_policy_fails_unresolved_layer() {
        let cfg = mk_cfg(&[("core", "?")], &[]);
        let err = validate_policy(
            cfg.workspace.as_ref(),
            &cfg.violations,
            &cfg.visibility_rules,
            true,
        )
        .expect_err("strict should fail for unresolved layer");
        assert!(
            err.to_string().contains("workspace layer unresolved"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn policy_fails_invalid_layer_name() {
        let cfg = mk_cfg(&[("core", "invalid_layer_name")], &[]);
        let err = validate_policy(
            cfg.workspace.as_ref(),
            &cfg.violations,
            &cfg.visibility_rules,
            false,
        )
        .expect_err("invalid layer should fail");
        assert!(
            err.to_string().contains("is invalid"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn policy_partially_invalid_reports_failure() {
        let cfg = mk_cfg(
            &[("core", "domain"), ("edge", "badlayer")],
            &[("adapter", "domain")],
        );
        let err = validate_policy(
            cfg.workspace.as_ref(),
            &cfg.violations,
            &cfg.visibility_rules,
            false,
        )
        .expect_err("partial invalid config should fail");
        assert!(
            err.to_string().contains("badlayer"),
            "unexpected error: {}",
            err
        );
    }
}
