use std::process::Command;

pub struct CrateMember {
    pub name: String,
    #[allow(dead_code)]
    pub path: String,
    pub entry_file: String,
}

pub struct WorkspaceGraph {
    pub members: Vec<CrateMember>,
    /// Production (non-dev) dependency edges: (from_crate_name, to_crate_name).
    pub deps: Vec<(String, String)>,
}

/// Detect a Cargo workspace rooted at `root` using `cargo metadata`.
/// Returns `None` if `Cargo.toml` is absent, cargo fails, or no workspace
/// members are found.
/// Only production (`kind: null`) dependencies are included in `deps`.
pub fn detect(root: &str) -> Option<WorkspaceGraph> {
    let manifest = format!("{}/Cargo.toml", root.trim_end_matches('/'));

    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            &manifest,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;

    let workspace_member_ids: std::collections::HashSet<String> = meta["workspace_members"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let packages = meta["packages"].as_array()?;
    let abs_root = std::fs::canonicalize(root).unwrap_or_else(|_| std::path::PathBuf::from(root));

    // Build members list from workspace packages only.
    let mut members: Vec<CrateMember> = packages
        .iter()
        .filter(|pkg| {
            pkg["id"]
                .as_str()
                .map(|id| workspace_member_ids.contains(id))
                .unwrap_or(false)
        })
        .filter_map(|pkg| {
            let name = pkg["name"].as_str()?.to_string();
            let manifest_path = pkg["manifest_path"].as_str()?;
            let crate_dir = std::path::Path::new(manifest_path).parent()?;
            // Express crate dir as path relative to workspace root.
            let rel = crate_dir
                .strip_prefix(&abs_root)
                .unwrap_or(crate_dir)
                .to_string_lossy()
                .into_owned();
            let entry_file = pkg["targets"]
                .as_array()
                .and_then(|targets| {
                    targets
                        .iter()
                        .find(|t| {
                            t["kind"]
                                .as_array()
                                .map(|kinds| {
                                    kinds
                                        .iter()
                                        .any(|k| matches!(k.as_str(), Some("lib" | "bin")))
                                })
                                .unwrap_or(false)
                        })
                        .and_then(|t| t["src_path"].as_str())
                })
                .map(|src| {
                    std::path::Path::new(src)
                        .strip_prefix(&abs_root)
                        .unwrap_or(std::path::Path::new(src))
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| {
                    if rel.is_empty() {
                        "src/lib.rs".to_string()
                    } else {
                        format!("{}/src/lib.rs", rel)
                    }
                });
            Some(CrateMember {
                name,
                path: rel,
                entry_file,
            })
        })
        .collect();

    if members.is_empty() {
        return None;
    }

    members.sort_by(|a, b| a.name.cmp(&b.name));

    let member_names: std::collections::HashSet<&str> =
        members.iter().map(|m| m.name.as_str()).collect();

    // Build a package-id → package-name map for workspace members.
    let id_to_name: std::collections::HashMap<&str, &str> = packages
        .iter()
        .filter_map(|pkg| {
            let id = pkg["id"].as_str()?;
            let name = pkg["name"].as_str()?;
            if workspace_member_ids.contains(id) {
                Some((id, name))
            } else {
                None
            }
        })
        .collect();

    // Collect production dep edges between workspace members from resolve.nodes.
    let mut deps: Vec<(String, String)> = Vec::new();
    if let Some(nodes) = meta["resolve"]["nodes"].as_array() {
        for node in nodes {
            let node_id = match node["id"].as_str() {
                Some(id) => id,
                None => continue,
            };
            let from_name = match id_to_name.get(node_id) {
                Some(n) => *n,
                None => continue,
            };
            if let Some(node_deps) = node["deps"].as_array() {
                for dep in node_deps {
                    let dep_pkg_id = dep["pkg"].as_str().unwrap_or("");
                    let dep_name = match id_to_name.get(dep_pkg_id) {
                        Some(n) => *n,
                        None => continue,
                    };
                    // Only emit for production deps (dep_kinds contains a null-kind entry).
                    let is_prod = dep["dep_kinds"]
                        .as_array()
                        .map(|kinds| kinds.iter().any(|k| k["kind"].is_null()))
                        .unwrap_or(false);
                    if is_prod && member_names.contains(dep_name) && dep_name != from_name {
                        deps.push((from_name.to_string(), dep_name.to_string()));
                    }
                }
            }
        }
    }

    deps.sort_unstable();
    deps.dedup();

    Some(WorkspaceGraph { members, deps })
}

/// Return the package name for the crate whose manifest directory is the
/// closest ancestor of `abs_file`. Used to identify which crate owns a file.
#[allow(dead_code)]
pub fn crate_for_file(root: &str, abs_file: &std::path::Path) -> Option<String> {
    let manifest = format!("{}/Cargo.toml", root.trim_end_matches('/'));
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            &manifest,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let workspace_member_ids: std::collections::HashSet<String> = meta["workspace_members"]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let packages = meta["packages"].as_array()?;

    // Find the package whose manifest directory is the longest prefix of abs_file.
    let mut best: Option<(usize, String)> = None;
    for pkg in packages {
        let id = pkg["id"].as_str().unwrap_or("");
        if !workspace_member_ids.is_empty() && !workspace_member_ids.contains(id) {
            continue;
        }
        let manifest_path = pkg["manifest_path"].as_str()?;
        let crate_dir = std::path::Path::new(manifest_path).parent()?;
        if abs_file.starts_with(crate_dir) {
            let depth = crate_dir.components().count();
            if best.as_ref().map(|(d, _)| depth > *d).unwrap_or(true) {
                best = Some((depth, pkg["name"].as_str()?.to_string()));
            }
        }
    }
    best.map(|(_, name)| name)
}
