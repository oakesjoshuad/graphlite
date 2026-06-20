use std::{collections::BTreeMap, fs, path::Path};

use serde::Deserialize;

/// A forbidden context coupling rule. Edges from `from_context` to `to_context`
/// are flagged as violations in `graphlite violations`, regardless of edge source.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ViolationRule {
    pub from_context: String,
    pub to_context: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A suppression rule for known-acceptable violation patterns.
/// All specified fields must match for suppression to fire; omitted fields are wildcards.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ViolationException {
    #[serde(default)]
    pub from_context: Option<String>,
    #[serde(default)]
    pub to_context: Option<String>,
    #[serde(default)]
    pub from_role: Option<String>,
    #[serde(default)]
    pub to_role: Option<String>,
    #[serde(default)]
    pub stable_id: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Layer visibility policy.
/// Symbols in `layer` must not exceed `max_visibility`.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct VisibilityRule {
    pub layer: String,
    pub max_visibility: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A layer-to-layer violation rule for workspace crates.
/// Any `Cargo.toml` dependency edge from a crate assigned `from_layer` to a
/// crate assigned `to_layer` is reported as a violation.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct WorkspaceViolationRule {
    pub from_layer: String,
    pub to_layer: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Workspace layer assignments and violation rules.
/// Each key in `layers` is a crate name; the value is a free-form layer name
/// (`shared`, `domain`, `port`, `application`, `infra`, `adapter`,
/// `composition`, etc.) or `"?"` while still being assigned.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct WorkspaceConfig {
    #[serde(default)]
    pub layers: BTreeMap<String, String>,
    #[serde(default)]
    pub violations: Vec<WorkspaceViolationRule>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct PolicyConfig {
    #[serde(default)]
    pub pack: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Forbidden context coupling rules for `graphlite violations`.
    #[serde(default)]
    pub violations: Vec<ViolationRule>,
    /// Suppression rules for known-acceptable violation patterns.
    #[serde(default)]
    pub exceptions: Vec<ViolationException>,
    /// Visibility policy by architectural layer.
    #[serde(default)]
    pub visibility_rules: Vec<VisibilityRule>,
    /// Workspace layer mapping (present only in Cargo workspace projects).
    pub workspace: Option<WorkspaceConfig>,
    /// Optional built-in policy pack selection.
    #[serde(default)]
    pub policy: Option<PolicyConfig>,
}

fn default_depth() -> usize {
    2
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ignore: vec![],
            depth: default_depth(),
            violations: vec![],
            exceptions: vec![],
            visibility_rules: vec![],
            workspace: None,
            policy: None,
        }
    }
}

/// Walk up from `start` looking for a `.graphlite/` directory.
/// Returns the directory that contains `.graphlite/` as a string, or an error
/// if none is found up to the filesystem root.
pub fn find_root(start: &str) -> anyhow::Result<String> {
    use anyhow::anyhow;
    let mut dir = Path::new(start).canonicalize()?;
    loop {
        if dir.join(".graphlite").is_dir() {
            return Ok(dir.to_string_lossy().into_owned());
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => return Err(anyhow!("no .graphlite/ directory found from {}", start)),
        }
    }
}

/// Load config from `{root}/.graphlite/config.toml`.
/// Returns `Config::default()` silently if the file is absent or unparseable —
/// this keeps `discover` idempotent without requiring `init` first.
pub fn load(root: &str) -> Config {
    let path = Path::new(root).join(".graphlite/config.toml");
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}
