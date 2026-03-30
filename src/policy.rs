use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use rusqlite::Connection;
use toml::Value;

use crate::arch;
use crate::config::{
    self, Config, ViolationException, ViolationRule, VisibilityRule, WorkspaceConfig,
    WorkspaceViolationRule,
};
use crate::xml as vxml;

const PACK_DDD: &str = "ddd-hexagonal-rust";
const PACK_CQRS: &str = "cqrs-event-sourced-rust";

#[derive(Clone)]
pub struct EffectivePolicy {
    pub pack: Option<String>,
    pub violations: Vec<ViolationRule>,
    pub exceptions: Vec<ViolationException>,
    pub visibility_rules: Vec<VisibilityRule>,
    pub workspace: Option<WorkspaceConfig>,
}

struct PolicyPack {
    violations: Vec<ViolationRule>,
    visibility_rules: Vec<VisibilityRule>,
    workspace_violations: Vec<WorkspaceViolationRule>,
}

struct Finding {
    severity: &'static str,
    code: &'static str,
    message: String,
}

#[derive(Clone, Copy)]
pub struct LintOptions {
    pub stale_only: bool,
    pub fail_on_stale: bool,
    pub fail_on_broad: bool,
    pub broad_threshold: usize,
}

pub fn supported_packs() -> &'static [&'static str] {
    &[PACK_DDD, PACK_CQRS]
}

pub fn effective_policy(cfg: &Config) -> Result<EffectivePolicy> {
    let pack_name = cfg.policy.as_ref().and_then(|p| p.pack.clone());
    let mut violations = Vec::new();
    let mut visibility_rules = Vec::new();
    let mut workspace_violations = Vec::new();

    if let Some(name) = &pack_name {
        let pack = pack_for(name)
            .ok_or_else(|| anyhow!("unknown policy pack '{}'", name))?;
        violations.extend(pack.violations);
        visibility_rules.extend(pack.visibility_rules);
        workspace_violations.extend(pack.workspace_violations);
    }

    violations.extend(cfg.violations.clone());
    visibility_rules.extend(cfg.visibility_rules.clone());

    let mut workspace = cfg.workspace.clone();
    if !workspace_violations.is_empty() {
        if let Some(ws) = &mut workspace {
            ws.violations.extend(workspace_violations);
        } else {
            workspace = Some(WorkspaceConfig {
                layers: BTreeMap::new(),
                violations: workspace_violations,
            });
        }
    }

    Ok(EffectivePolicy {
        pack: pack_name,
        violations,
        exceptions: cfg.exceptions.clone(),
        visibility_rules,
        workspace,
    })
}

pub fn init_pack(root: &str, pack: &str, force: bool) -> Result<()> {
    if !supported_packs().contains(&pack) {
        anyhow::bail!(
            "unknown pack '{}'; supported: {}",
            pack,
            supported_packs().join(", ")
        );
    }

    let graphlite_dir = Path::new(root).join(".graphlite");
    fs::create_dir_all(&graphlite_dir)?;
    let config_path = graphlite_dir.join("config.toml");

    let mut doc = if config_path.exists() {
        let text = fs::read_to_string(&config_path)?;
        let table = toml::from_str::<toml::Table>(&text)
            .map_err(|e| anyhow!("failed to parse {}: {}", config_path.display(), e))?;
        Value::Table(table)
    } else {
        Value::Table(Default::default())
    };

    let top = doc
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root must be a TOML table"))?;
    let policy_entry = top
        .entry("policy")
        .or_insert_with(|| Value::Table(Default::default()));
    let policy_tbl = policy_entry
        .as_table_mut()
        .ok_or_else(|| anyhow!("[policy] must be a TOML table"))?;

    if !force
        && policy_tbl
            .get("pack")
            .and_then(Value::as_str)
            .is_some_and(|v| v != pack)
    {
        anyhow::bail!(
            "policy pack already set to '{}'; use --force to replace",
            policy_tbl.get("pack").and_then(Value::as_str).unwrap_or("")
        );
    }
    policy_tbl.insert("pack".to_string(), Value::String(pack.to_string()));

    let rendered = toml::to_string_pretty(&doc)?;
    fs::write(&config_path, rendered)?;

    println!(
        "policy pack '{}' configured in {}",
        pack,
        config_path.display()
    );
    println!(
        "local overrides remain supported via [[violations]], [[exceptions]], [[visibility_rules]], and [workspace]."
    );
    Ok(())
}

pub fn lint(root: &str, opts: LintOptions) -> Result<()> {
    let cfg = config::load(root);
    let effective = effective_policy(&cfg)?;
    let mut findings = Vec::<Finding>::new();

    lint_context_rules(&effective.violations, &mut findings);
    lint_visibility_rules(&effective.visibility_rules, &mut findings);
    lint_workspace_rules(effective.workspace.as_ref(), &mut findings);
    lint_exceptions(root, &effective.exceptions, opts.broad_threshold, &mut findings)?;
    lint_dead_rules_against_repo(root, &effective, &mut findings)?;
    if opts.stale_only {
        findings.retain(|f| f.code.starts_with("dead_exception_"));
    }

    let mut warnings = 0usize;
    let mut errors = 0usize;
    let mut conflicts = 0usize;
    let mut dead_rules = 0usize;
    for f in &findings {
        match f.severity {
            "error" => errors += 1,
            _ => warnings += 1,
        }
        if f.code.starts_with("conflict_") {
            conflicts += 1;
        }
        if f.code.starts_with("dead_") {
            dead_rules += 1;
        }
    }

    let stale_count = findings
        .iter()
        .filter(|f| f.code.starts_with("dead_exception_"))
        .count();
    let broad_count = findings
        .iter()
        .filter(|f| f.code == "broad_exception_rule")
        .count();

    let mut w = vxml::new_stream_writer();
    let findings_s = findings.len().to_string();
    let warnings_s = warnings.to_string();
    let errors_s = errors.to_string();
    let conflicts_s = conflicts.to_string();
    let dead_s = dead_rules.to_string();
    let stale_s = stale_count.to_string();
    let broad_s = broad_count.to_string();
    let pack = effective.pack.as_deref().unwrap_or("none");
    vxml::open_attrs(
        &mut w,
        "policy_lint",
        &[
            ("pack", pack),
            ("findings", &findings_s),
            ("warnings", &warnings_s),
            ("errors", &errors_s),
            ("conflicts", &conflicts_s),
            ("dead_rules", &dead_s),
            ("stale_exceptions", &stale_s),
            ("broad_exceptions", &broad_s),
            ("tokens", "streaming"),
        ],
    )?;
    for f in &findings {
        vxml::empty(
            &mut w,
            "finding",
            &[
                ("severity", f.severity),
                ("code", f.code),
                ("message", &f.message),
            ],
        )?;
    }
    vxml::close(&mut w, "policy_lint")?;
    vxml::finish_stream(w)?;
    if opts.fail_on_stale && stale_count > 0 {
        anyhow::bail!("stale suppressions detected: {}", stale_count);
    }
    if opts.fail_on_broad && broad_count > 0 {
        anyhow::bail!("overbroad suppressions detected: {}", broad_count);
    }
    Ok(())
}

fn lint_context_rules(rules: &[ViolationRule], findings: &mut Vec<Finding>) {
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    for rule in rules {
        if rule.from_context == rule.to_context {
            findings.push(Finding {
                severity: "warning",
                code: "dead_context_self_rule",
                message: format!(
                    "context rule {} -> {} is ineffective (self-edge is ignored)",
                    rule.from_context, rule.to_context
                ),
            });
        }
        let reason = rule.reason.clone().unwrap_or_default();
        let key = (rule.from_context.clone(), rule.to_context.clone());
        if let Some(prev) = seen.get(&key) {
            if prev == &reason {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_context_duplicate_rule",
                    message: format!(
                        "duplicate context rule {} -> {}",
                        key.0, key.1
                    ),
                });
            } else {
                findings.push(Finding {
                    severity: "error",
                    code: "conflict_context_rule",
                    message: format!(
                        "conflicting reasons for context rule {} -> {}",
                        key.0, key.1
                    ),
                });
            }
        } else {
            seen.insert(key, reason);
        }
    }
}

fn lint_visibility_rules(rules: &[VisibilityRule], findings: &mut Vec<Finding>) {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for rule in rules {
        if let Some(prev) = seen.get(&rule.layer) {
            if prev == &rule.max_visibility {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_visibility_duplicate_rule",
                    message: format!("duplicate visibility rule for layer '{}'", rule.layer),
                });
            } else {
                findings.push(Finding {
                    severity: "error",
                    code: "conflict_visibility_rule",
                    message: format!(
                        "layer '{}' has conflicting max_visibility values ('{}' vs '{}')",
                        rule.layer, prev, rule.max_visibility
                    ),
                });
            }
        } else {
            seen.insert(rule.layer.clone(), rule.max_visibility.clone());
        }
    }
}

fn lint_workspace_rules(workspace: Option<&WorkspaceConfig>, findings: &mut Vec<Finding>) {
    let Some(ws) = workspace else {
        return;
    };
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    for rule in &ws.violations {
        let reason = rule.reason.clone().unwrap_or_default();
        let key = (rule.from_layer.clone(), rule.to_layer.clone());
        if let Some(prev) = seen.get(&key) {
            if prev == &reason {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_workspace_duplicate_rule",
                    message: format!(
                        "duplicate workspace layer rule {} -> {}",
                        key.0, key.1
                    ),
                });
            } else {
                findings.push(Finding {
                    severity: "error",
                    code: "conflict_workspace_rule",
                    message: format!(
                        "conflicting reasons for workspace layer rule {} -> {}",
                        key.0, key.1
                    ),
                });
            }
        } else {
            seen.insert(key, reason);
        }
    }
}

fn lint_exceptions(
    root: &str,
    exceptions: &[ViolationException],
    broad_threshold: usize,
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let db = Path::new(root).join(".graphlite/codegraph.db");
    if !db.exists() {
        return Ok(());
    }
    let conn = Connection::open(db)?;
    let mut edge_stmt = conn.prepare(
        "SELECT nf.file, nt.file, nf.role, nt.role, nf.stable_id, nt.stable_id
         FROM edges e
         JOIN nodes nf ON nf.id = e.from_id
         JOIN nodes nt ON nt.id = e.to_id
         WHERE e.source IN ('resolver', 'rustdoc')
           AND nf.role != 'test'
           AND nt.role != 'test'
         GROUP BY e.from_id, e.to_id",
    )?;
    let edges: Vec<(String, String, String, String, String, String)> = edge_stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    for (idx, exc) in exceptions.iter().enumerate() {
        if let Some(stable_id) = exc.stable_id.as_deref() {
            let exists: i64 = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE stable_id = ?1)",
                [stable_id],
                |r| r.get(0),
            )?;
            if exists == 0 {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_exception_stable_id",
                    message: format!(
                        "exception #{} stable_id '{}' does not match any symbol",
                        idx + 1,
                        stable_id
                    ),
                });
            }
        }

        let mut hits = 0usize;
        let mut ctx_pairs = BTreeSet::<(String, String)>::new();
        for (from_file, to_file, from_role, to_role, from_sid, to_sid) in &edges {
            let from_ctx = arch::file_to_context(from_file);
            let to_ctx = arch::file_to_context(to_file);
            if exception_matches(exc, &from_ctx, &to_ctx, from_role, to_role, from_sid, to_sid) {
                hits += 1;
                ctx_pairs.insert((from_ctx, to_ctx));
            }
        }
        if hits == 0 {
            findings.push(Finding {
                severity: "warning",
                code: "dead_exception_unused_rule",
                message: format!("exception #{} is unused in current graph", idx + 1),
            });
        }

        let wildcard_fields = [
            exc.from_context.is_none(),
            exc.to_context.is_none(),
            exc.from_role.is_none(),
            exc.to_role.is_none(),
            exc.stable_id.is_none(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        if hits >= broad_threshold && wildcard_fields >= 2 {
            findings.push(Finding {
                severity: "warning",
                code: "broad_exception_rule",
                message: format!(
                    "exception #{} is broad (hits={}, contexts={})",
                    idx + 1,
                    hits,
                    ctx_pairs.len()
                ),
            });
        }
    }
    Ok(())
}

fn exception_matches(
    exc: &ViolationException,
    from_ctx: &str,
    to_ctx: &str,
    from_role: &str,
    to_role: &str,
    from_stable_id: &str,
    to_stable_id: &str,
) -> bool {
    let fc = exc.from_context.as_deref().is_none_or(|v| v == from_ctx);
    let tc = exc.to_context.as_deref().is_none_or(|v| v == to_ctx);
    let fr = exc.from_role.as_deref().is_none_or(|v| v == from_role);
    let tr = exc.to_role.as_deref().is_none_or(|v| v == to_role);
    let sid = exc
        .stable_id
        .as_deref()
        .is_none_or(|v| v == from_stable_id || v == to_stable_id);
    fc && tc && fr && tr && sid
}

fn lint_dead_rules_against_repo(root: &str, effective: &EffectivePolicy, findings: &mut Vec<Finding>) -> Result<()> {
    let db = Path::new(root).join(".graphlite/codegraph.db");
    if !db.exists() {
        return Ok(());
    }
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT DISTINCT file FROM nodes")?;
    let files: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut contexts = BTreeSet::<String>::new();
    for f in &files {
        contexts.insert(arch::file_to_context(f));
    }

    for rule in &effective.violations {
        if !contexts.is_empty()
            && (!contexts.contains(&rule.from_context) || !contexts.contains(&rule.to_context))
        {
            findings.push(Finding {
                severity: "warning",
                code: "dead_context_rule_no_match",
                message: format!(
                    "context rule {} -> {} has no matching contexts in current graph",
                    rule.from_context, rule.to_context
                ),
            });
        }
    }

    if let Some(ws) = &effective.workspace {
        let assigned_layers: BTreeSet<String> = ws
            .layers
            .values()
            .filter(|v| v.as_str() != "?")
            .cloned()
            .collect();
        for rule in &ws.violations {
            if !assigned_layers.is_empty()
                && (!assigned_layers.contains(&rule.from_layer)
                    || !assigned_layers.contains(&rule.to_layer))
            {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_workspace_rule_no_layer",
                    message: format!(
                        "workspace layer rule {} -> {} has no matching assigned layers",
                        rule.from_layer, rule.to_layer
                    ),
                });
            }
        }
        for (crate_name, layer) in &ws.layers {
            if layer == "?" {
                findings.push(Finding {
                    severity: "warning",
                    code: "dead_workspace_unassigned_layer",
                    message: format!("crate '{}' has unresolved workspace layer '?'", crate_name),
                });
            }
        }
    }

    Ok(())
}

fn pack_for(name: &str) -> Option<PolicyPack> {
    match name {
        PACK_DDD => Some(PolicyPack {
            violations: vec![
                rule_ctx("adapter", "infra", "adapter should call application ports/use-cases, not infra directly"),
                rule_ctx("adapter", "domain", "adapter should enter through application boundary"),
                rule_ctx("infra", "adapter", "infra should not depend on driving adapters"),
                rule_ctx("domain", "infra", "domain must remain persistence-agnostic"),
                rule_ctx("domain", "adapter", "domain must remain transport-agnostic"),
            ],
            visibility_rules: vec![
                rule_vis("domain", "crate", "domain symbols should remain crate-internal"),
                rule_vis("infra", "crate", "infra implementation details should remain internal"),
            ],
            workspace_violations: vec![
                rule_ws("domain", "infra", "domain must not depend on infra"),
                rule_ws("domain", "adapter", "domain must not depend on adapter"),
                rule_ws("port", "infra", "ports define contracts; infra implements them"),
                rule_ws("port", "adapter", "ports should not depend on adapters"),
                rule_ws("application", "adapter", "application should not depend on adapters"),
            ],
        }),
        PACK_CQRS => Some(PolicyPack {
            violations: vec![
                rule_ctx("query", "command", "read side must not depend on write side"),
                rule_ctx("command", "query", "write side should not couple to read projections"),
                rule_ctx("projection", "command", "projections should not drive command execution"),
                rule_ctx("infra", "adapter", "infra should not depend on delivery adapters"),
            ],
            visibility_rules: vec![
                rule_vis("domain", "crate", "domain internals should not leak via public API"),
                rule_vis("application", "crate", "use-case internals should be crate scoped"),
            ],
            workspace_violations: vec![
                rule_ws("query", "command", "query crates must not depend on command crates"),
                rule_ws("projection", "command", "projection crates must not depend on command crates"),
                rule_ws("domain", "infra", "domain must not depend on infra"),
            ],
        }),
        _ => None,
    }
}

fn rule_ctx(from: &str, to: &str, reason: &str) -> ViolationRule {
    ViolationRule {
        from_context: from.to_string(),
        to_context: to.to_string(),
        reason: Some(reason.to_string()),
    }
}

fn rule_vis(layer: &str, max_visibility: &str, reason: &str) -> VisibilityRule {
    VisibilityRule {
        layer: layer.to_string(),
        max_visibility: max_visibility.to_string(),
        reason: Some(reason.to_string()),
    }
}

fn rule_ws(from_layer: &str, to_layer: &str, reason: &str) -> WorkspaceViolationRule {
    WorkspaceViolationRule {
        from_layer: from_layer.to_string(),
        to_layer: to_layer.to_string(),
        reason: Some(reason.to_string()),
    }
}
