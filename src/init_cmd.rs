use std::{collections::BTreeMap, fs, path::Path};

use anyhow::Result;
use tracing::{info, warn};

use crate::{config, discover, workspace};

pub fn run(root: &str) -> Result<()> {
    let graphlite_dir = Path::new(root).join(".graphlite");
    fs::create_dir_all(&graphlite_dir)?;

    // 1. Write config.toml only if absent.
    let config_path = graphlite_dir.join("config.toml");
    if !config_path.exists() {
        let ws = workspace::detect(root);
        let content = match ws.as_ref() {
            Some(ws_info) => build_workspace_config(ws_info),
            None => build_default_config(),
        };
        fs::write(&config_path, &content)?;
        if ws.is_some() {
            info!(path = %config_path.display(), "config created (Cargo workspace detected)");
        } else {
            info!(path = %config_path.display(), "config created");
        }
    }

    // 2. Add codegraph.db to .gitignore (config.toml should be committed).
    let gitignore_path = Path::new(root).join(".gitignore");
    let db_entry = ".graphlite/codegraph.db";
    if gitignore_path.exists() {
        let contents = fs::read_to_string(&gitignore_path).unwrap_or_default();
        if !contents.lines().any(|l| l.trim() == db_entry) {
            let entry = if contents.ends_with('\n') || contents.is_empty() {
                format!("{}\n", db_entry)
            } else {
                format!("\n{}\n", db_entry)
            };
            fs::write(&gitignore_path, format!("{}{}", contents, entry))?;
            info!(entry = db_entry, "updated .gitignore");
        }
    } else {
        warn!(entry = db_entry, "no .gitignore found — add this entry manually");
    }

    // 3. If workspace layers are still unassigned, print instructions and stop.
    //    Otherwise proceed to indexing.
    let cfg = config::load(root);
    if let Some(ws_cfg) = &cfg.workspace {
        let mut unassigned: Vec<&str> = ws_cfg
            .layers
            .iter()
            .filter(|(_, v)| v.as_str() == "?")
            .map(|(k, _)| k.as_str())
            .collect();
        unassigned.sort_unstable();
        if !unassigned.is_empty() {
            warn!(
                config = %config_path.display(),
                unassigned = %unassigned.join(", "),
                "workspace layers not yet assigned — edit config then run: graphlite discover ."
            );
            return Ok(());
        }
    }

    discover::run(root)
}

// ── Config file generation ────────────────────────────────────────────────────

fn build_default_config() -> String {
    r#"# graphlite configuration

# Additional directories to ignore during indexing.
# Extends built-in defaults: node_modules, target, build, dist, .svelte-kit, .git, .cache, .next, .nuxt, __pycache__
ignore = []

# Default traversal depth for graph and blast-radius commands.
depth = 2

# Forbidden context coupling rules for `graphlite violations`.
# Example:
# [[violations]]
# from_context = "presentation"
# to_context   = "persistence"
# reason       = "presentation must not access persistence directly"

# Suppression rules for known-acceptable violation patterns.
# Example:
# [[exceptions]]
# from_context = "runtime"
# to_role      = "utility"
# reason       = "runtime infrastructure may use shared utilities"
#
# Stable-id scoped suppression (works for visibility rules too):
# [[exceptions]]
# stable_id = "src/infra/repo.rs::struct::MyRepo"
# reason    = "exported for integration test crate"
#
# Visibility policy by architectural layer.
# max_visibility: default | crate | restricted | public
# [[visibility_rules]]
# layer = "domain"
# max_visibility = "crate"
# reason = "domain symbols should stay crate-internal"
"#.to_string()
}

fn build_workspace_config(ws: &workspace::WorkspaceGraph) -> String {
    // Build grouped dep graph comment: { from → [to, to, ...] }
    let mut grouped: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in &ws.deps {
        grouped.entry(from.as_str()).or_default().push(to.as_str());
    }
    let dep_lines: String = grouped
        .iter()
        .map(|(from, tos)| format!("#   {from} → {}", tos.join(", ")))
        .collect::<Vec<_>>()
        .join("\n");

    // Alignment: pad crate names to the longest.
    let max_len = ws.members.iter().map(|m| m.name.len()).max().unwrap_or(0);
    let layer_lines: String = ws
        .members
        .iter()
        .map(|m| {
            let pad = " ".repeat(max_len - m.name.len());
            format!("{}{} = \"?\"", m.name, pad)
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"# graphlite configuration

# Additional directories to ignore during indexing.
# Extends built-in defaults: node_modules, target, build, dist, .svelte-kit, .git, .cache, .next, .nuxt, __pycache__
ignore = []

# Default traversal depth for graph and blast-radius commands.
depth = 2

# ── Workspace ──────────────────────────────────────────────────────────────────
# Cargo workspace detected. Assign a layer to each crate below, then run:
#   graphlite discover .
#
# Valid layers (dependencies must only flow from outer layers toward inner):
#   shared      — primitives, IDs, common error types; no workspace deps
#   domain      — aggregates, entities, value objects, domain events, domain services
#   port        — outbound trait definitions (Repository, Gateway, Publisher)
#   application — use cases, command/query handlers, application services
#   infra       — port implementations (Postgres adapters, HTTP clients, message brokers)
#   adapter     — driving-side adapters (HTTP handlers, CLI, gRPC)
#   composition — composition root; wires everything together
#
# Violation: a dependency edge flowing in the wrong direction (inner → outer).
#   domain → infra     domain must not know about adapter implementations
#   port   → infra     ports define the interface; infra implements it
#   domain → adapter
#
# Detected inter-crate dependencies (from Cargo.toml [dependencies]):
{dep_lines}
#
# Example assignments for a hexagonal DDD workspace:
#   primitives     = "shared"
#   identity       = "domain"
#   contract       = "port"
#   infrastructure = "infra"
#   presentation   = "adapter"
#   runtime        = "composition"

[workspace.layers]
{layer_lines}

# Visibility policy by layer (enable after assigning layers above).
# max_visibility: default | crate | restricted | public
# [[visibility_rules]]
# layer = "domain"
# max_visibility = "crate"
# reason = "domain types should not leak as public API"
#
# [[visibility_rules]]
# layer = "infra"
# max_visibility = "crate"
# reason = "infrastructure implementations are internal"
#
# [[visibility_rules]]
# layer = "shared"
# max_visibility = "public"
# reason = "shared primitives may be public"
"#
    )
}
