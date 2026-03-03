use std::{fs, path::Path};

use serde::Deserialize;

pub const DEFAULT_CONFIG_TOML: &str = r#"# graphlite configuration

# Additional directories to ignore during indexing.
# Extends built-in defaults: node_modules, target, build, dist, .svelte-kit, .git, .cache, .next, .nuxt, __pycache__
ignore = []

# LSP languages for semantic enrichment (auto-detected on init).
# lsp = ["rust"]

# Default traversal depth for graph and blast-radius commands.
depth = 2

# Forbidden context coupling rules for `graphlite viz`.
# Each rule flags edges where a node in `from_context` calls into `to_context`.
# Complements the built-in role-layer heuristic.
# Example:
# [[violations]]
# from_context = "presentation"
# to_context   = "persistence"
# reason       = "presentation must not access persistence directly"

# Suppression rules for known-acceptable violation patterns.
# All specified fields must match; omitted fields are wildcards.
# Example:
# [[exceptions]]
# from_context = "runtime"
# to_role      = "utility"
# reason       = "runtime infrastructure may use shared utilities"
"#;

/// A forbidden context coupling rule. Edges from `from_context` to `to_context`
/// are flagged as violations in `graphlite viz`, regardless of edge source.
#[derive(Debug, Deserialize, Default)]
pub struct ViolationRule {
    pub from_context: String,
    pub to_context: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A suppression rule for known-acceptable violation patterns.
/// All specified fields must match for suppression to fire; omitted fields are wildcards.
#[derive(Debug, Deserialize, Default)]
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
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub lsp: Vec<String>,
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Forbidden context coupling rules for `graphlite viz`.
    #[serde(default)]
    pub violations: Vec<ViolationRule>,
    /// Suppression rules for known-acceptable violation patterns.
    #[serde(default)]
    pub exceptions: Vec<ViolationException>,
}

fn default_depth() -> usize {
    2
}

impl Default for Config {
    fn default() -> Self {
        Config {
            ignore: vec![],
            lsp: vec![],
            depth: default_depth(),
            violations: vec![],
            exceptions: vec![],
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
