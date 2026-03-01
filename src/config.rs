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
"#;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub lsp: Vec<String>,
    #[serde(default = "default_depth")]
    pub depth: usize,
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
