use std::{fs, path::Path};

use anyhow::Result;

use crate::{config::DEFAULT_CONFIG_TOML, discover};

pub fn run(root: &str, lsp_override: Option<&str>) -> Result<()> {
    let graphlite_dir = Path::new(root).join(".graphlite");

    // 1. Create .graphlite/ directory (idempotent)
    fs::create_dir_all(&graphlite_dir)?;

    // 2. Write default config.toml only if absent, injecting auto-detected LSP
    let config_path = graphlite_dir.join("config.toml");
    if !config_path.exists() {
        let detected: Vec<String> = if let Some(lang) = lsp_override {
            vec![lang.to_string()]
        } else {
            discover::detect_lsp(root)
        };
        let content = if detected.is_empty() {
            DEFAULT_CONFIG_TOML.to_string()
        } else {
            let array = detected
                .iter()
                .map(|l| format!("\"{}\"", l))
                .collect::<Vec<_>>()
                .join(", ");
            DEFAULT_CONFIG_TOML.replace("# lsp = [\"rust\"]", &format!("lsp = [{}]", array))
        };
        fs::write(&config_path, content)?;
        if detected.is_empty() {
            eprintln!("created: {}", config_path.display());
        } else {
            eprintln!(
                "created: {} (auto-detected lsp = {:?})",
                config_path.display(),
                detected
            );
        }
    }

    // 3. Add .graphlite/codegraph.db to .gitignore (not the whole dir — config.toml should be committed)
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
            eprintln!("updated: .gitignore (+{})", db_entry);
        }
    } else {
        eprintln!("note: add {} to your .gitignore", db_entry);
    }

    // 4. Run discover (reads config, creates db, indexes)
    discover::run(root, lsp_override)
}
