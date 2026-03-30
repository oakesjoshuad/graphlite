use std::process::Command;
use std::sync::OnceLock;

static DB_READY: OnceLock<()> = OnceLock::new();

fn bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_graphlite")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.push("target/release/graphlite");
            p
        })
}

fn root() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

fn ensure_db() {
    DB_READY.get_or_init(|| {
        let status = Command::new(bin())
            .args(["init", "."])
            .current_dir(root())
            .status()
            .expect("failed to spawn graphlite init");
        assert!(status.success(), "graphlite init failed");
    });
}

// --- correctness ---

#[test]
fn blast_radius_open_db_has_known_dependents() {
    ensure_db();
    let out = Command::new(bin())
        .args(["blast-radius", "--depth", "2", "sym:open_db"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("name=\"symbols\""),
        "expected 'symbols' in blast radius"
    );
    assert!(
        text.contains("name=\"graph\""),
        "expected 'graph' in blast radius"
    );
}

#[test]
fn symbols_fts_finds_xml_escape() {
    ensure_db();
    let out = Command::new(bin())
        .args(["symbols", "xml_escape"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("xml_escape"),
        "FTS should find xml_escape symbol"
    );
}

#[test]
fn blast_radius_snippets_show_call_sites() {
    ensure_db();
    let out = Command::new(bin())
        .args(["blast-radius", "--depth", "1", "sym:open_db"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<snippet>"),
        "expected <snippet> elements (snippets on by default)"
    );
    assert!(
        text.contains("open_db"),
        "snippet should contain 'open_db' call"
    );
}

#[test]
fn graph_with_snippets_emits_snippets_for_neighbors() {
    ensure_db();
    let out = Command::new(bin())
        .args(["graph", "--depth", "1", "sym:open_db"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<snippet>"),
        "expected <snippet> elements (snippets on by default)"
    );
}

#[test]
fn capabilities_json_exposes_surface_and_unavailable() {
    let out = Command::new(bin())
        .args(["capabilities", "--json"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "capabilities --json should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"commands\""), "expected commands list");
    assert!(text.contains("\"violations\""), "expected violations command");
    assert!(text.contains("\"reclassify\""), "expected reclassify command");
    assert!(
        !text.contains("\"unavailable\""),
        "did not expect unavailable section for dropped features"
    );
}

#[test]
fn violations_zero_findings_still_emits_diagnostics() {
    ensure_db();
    let out = Command::new(bin())
        .args(["violations", "--strict-policy", "--top", "1", "--no-snippets"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<diagnostics "),
        "expected diagnostics block even with 0 findings"
    );
}

#[test]
fn violations_strict_policy_fails_unresolved_workspace_layers() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();
    std::fs::write(
        p.join("Cargo.toml"),
        r#"[package]
name = "tmp_graphlite_policy_test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("src/main.rs"), "fn main() {}\n").unwrap();

    // Ensure DB exists.
    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success(), "init should succeed in temp project");

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
tmp_graphlite_policy_test = "?"
"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["violations", "--strict-policy"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "strict policy should fail unresolved workspace layer"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("workspace layer unresolved"),
        "expected actionable strict-policy error, got: {stderr}"
    );
}

// --- token reduction measurement ---

#[test]
fn token_reduction_blast_radius_vs_raw_files() {
    ensure_db();
    let out = Command::new(bin())
        .args(["blast-radius", "--depth", "3", "sym:open_db"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let graphlite_tokens = out.stdout.len() / 4;

    let src = std::path::Path::new(root()).join("src");
    let mut raw_tokens = 0usize;
    for entry in walkdir::WalkDir::new(&src).into_iter().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if content.contains("open_db") {
                    raw_tokens += content.len() / 4;
                }
            }
        }
    }

    eprintln!(
        "token reduction: graphlite={} raw_files={} ratio={:.1}x",
        graphlite_tokens,
        raw_tokens,
        raw_tokens as f64 / graphlite_tokens as f64
    );
    assert!(graphlite_tokens < raw_tokens);
}

#[test]
fn rename_reports_external_lsp_requirement() {
    ensure_db();
    let out = Command::new(bin())
        .args(["rename", "sym:open_db", "open_db_new", "--root", "."])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "rename should fail in-CLI since rust-analyzer integration was removed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("removed from graphlite") || stderr.contains("editor's LSP"),
        "expected migration guidance in stderr, got: {stderr}"
    );
}

#[test]
fn workspace_discover_emits_crate_nodes_and_crate_dep_edges() {
    use rusqlite::Connection;
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[workspace]
members = ["a", "b"]
resolver = "2"
"#,
    )
    .unwrap();

    std::fs::create_dir_all(p.join("a/src")).unwrap();
    std::fs::write(
        p.join("a/Cargo.toml"),
        r#"[package]
name = "a"
version = "0.1.0"
edition = "2021"

[dependencies]
b = { path = "../b" }
"#,
    )
    .unwrap();
    std::fs::write(p.join("a/src/lib.rs"), "pub fn a() { b::b(); }\n").unwrap();

    std::fs::create_dir_all(p.join("b/src")).unwrap();
    std::fs::write(
        p.join("b/Cargo.toml"),
        r#"[package]
name = "b"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("b/src/lib.rs"), "pub fn b() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success(), "init should succeed in workspace");

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
a = "application"
b = "domain"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success(), "discover should succeed");

    let conn = Connection::open(p.join(".graphlite/codegraph.db")).unwrap();
    let crate_nodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE kind = 'crate' AND language = 'workspace'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let crate_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges WHERE edge_type = 'CRATE_DEP' AND source = 'cargo-metadata'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(crate_nodes >= 2, "expected crate nodes to be inserted");
    assert!(crate_edges >= 1, "expected crate dependency edges");
}

#[test]
fn workspace_violations_include_crate_level_output() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[workspace]
members = ["adapter", "domain"]
resolver = "2"
"#,
    )
    .unwrap();

    std::fs::create_dir_all(p.join("adapter/src")).unwrap();
    std::fs::write(
        p.join("adapter/Cargo.toml"),
        r#"[package]
name = "adapter"
version = "0.1.0"
edition = "2021"

[dependencies]
domain = { path = "../domain" }
"#,
    )
    .unwrap();
    std::fs::write(p.join("adapter/src/lib.rs"), "pub fn adapter() { domain::domain(); }\n").unwrap();

    std::fs::create_dir_all(p.join("domain/src")).unwrap();
    std::fs::write(
        p.join("domain/Cargo.toml"),
        r#"[package]
name = "domain"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("domain/src/lib.rs"), "pub fn domain() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success());

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
adapter = "adapter"
domain = "domain"

[[workspace.violations]]
from_layer = "adapter"
to_layer = "domain"
reason = "adapter must not depend on domain directly"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success());

    let out = Command::new(bin())
        .args(["violations", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<workspace_violations"),
        "expected workspace_violations block in output"
    );
    assert!(
        text.contains("from_crate=\"adapter\"") && text.contains("to_crate=\"domain\""),
        "expected crate-level violation dep entry, got: {text}"
    );
}

#[test]
fn deps_command_emits_workspace_crate_graph() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[workspace]
members = ["a", "b"]
resolver = "2"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(p.join("a/src")).unwrap();
    std::fs::write(
        p.join("a/Cargo.toml"),
        r#"[package]
name = "a"
version = "0.1.0"
edition = "2021"

[dependencies]
b = { path = "../b" }
"#,
    )
    .unwrap();
    std::fs::write(p.join("a/src/lib.rs"), "pub fn a() { b::b(); }\n").unwrap();

    std::fs::create_dir_all(p.join("b/src")).unwrap();
    std::fs::write(
        p.join("b/Cargo.toml"),
        r#"[package]
name = "b"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("b/src/lib.rs"), "pub fn b() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success());

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1
[workspace.layers]
a = "application"
b = "domain"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success());

    let out = Command::new(bin())
        .args(["deps"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "deps should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("<deps "), "expected deps root");
    assert!(
        text.contains("from_crate=\"a\"") && text.contains("to_crate=\"b\""),
        "expected a->b edge in deps output: {text}"
    );
}

#[test]
fn violations_by_crate_and_edge_filter_work() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[workspace]
members = ["adapter", "domain"]
resolver = "2"
"#,
    )
    .unwrap();

    std::fs::create_dir_all(p.join("adapter/src")).unwrap();
    std::fs::write(
        p.join("adapter/Cargo.toml"),
        r#"[package]
name = "adapter"
version = "0.1.0"
edition = "2021"
[dependencies]
domain = { path = "../domain" }
"#,
    )
    .unwrap();
    std::fs::write(p.join("adapter/src/lib.rs"), "pub fn adapter() { domain::domain(); }\n").unwrap();

    std::fs::create_dir_all(p.join("domain/src")).unwrap();
    std::fs::write(
        p.join("domain/Cargo.toml"),
        r#"[package]
name = "domain"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("domain/src/lib.rs"), "pub fn domain() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success());

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1
[workspace.layers]
adapter = "adapter"
domain = "domain"
[[violations]]
from_context = "adapter"
to_context = "domain"
reason = "adapter must not call domain directly"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success());

    let out = Command::new(bin())
        .args(["violations", "--by-crate", "--edge", "from:adapter to:domain", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "violations should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("<crate_summary>"), "expected crate summary block");
    assert!(
        text.contains("from_crate=\"adapter\"") && text.contains("to_crate=\"domain\""),
        "expected adapter->domain crate edge in output: {text}"
    );
    assert!(
        text.contains("crate=\"adapter\"") && text.contains("crate=\"domain\""),
        "expected symbol findings to link caller/callee crate ownership"
    );
}

#[test]
fn map_md_includes_complexity_column() {
    ensure_db();
    let out = Command::new(bin())
        .args(["map", "--md"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "map --md should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("| complexity |"),
        "expected complexity column in markdown output"
    );
}

#[test]
fn violations_check_surfaces_dead_code_after_check_run() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[package]
name = "tmp_graphlite_check_test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(
        p.join("src/main.rs"),
        "fn dead_fn() {}\nfn main() {}\n",
    )
    .unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success(), "init should succeed in temp project");

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
tmp_graphlite_check_test = "application"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success(), "discover should succeed");

    let check = Command::new(bin())
        .args(["check", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(check.status.success(), "check should succeed");

    let out = Command::new(bin())
        .args(["violations", "--check", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "violations --check should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<check_violations"),
        "expected check_violations block in output"
    );
    assert!(
        text.contains("code=\"dead_code\""),
        "expected dead_code lint in check violations output"
    );
}

#[test]
fn visibility_rules_emit_visibility_kind_and_respect_pattern_and_exception() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[workspace]
members = ["adapter", "domain"]
resolver = "2"
"#,
    )
    .unwrap();

    std::fs::create_dir_all(p.join("adapter/src")).unwrap();
    std::fs::write(
        p.join("adapter/Cargo.toml"),
        r#"[package]
name = "adapter"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("adapter/src/lib.rs"), "pub fn adapter_api() {}\n").unwrap();

    std::fs::create_dir_all(p.join("domain/src")).unwrap();
    std::fs::write(
        p.join("domain/Cargo.toml"),
        r#"[package]
name = "domain"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::write(p.join("domain/src/lib.rs"), "pub(crate) fn domain_api() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success(), "init should succeed in workspace");

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
adapter = "adapter"
domain = "domain"

[[visibility_rules]]
layer = "adapter"
max_visibility = "crate"
reason = "adapters should be crate-visible"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success(), "discover should succeed");

    let out = Command::new(bin())
        .args(["violations", "--pattern", "adapter_api", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "violations should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<visibility_violations"),
        "expected visibility violations block"
    );
    assert!(
        text.contains("kind=\"visibility\""),
        "expected kind=visibility on violation output"
    );
    assert!(
        text.contains("symbol=\"adapter_api\""),
        "expected adapter_api visibility finding"
    );

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
adapter = "adapter"
domain = "domain"

[[visibility_rules]]
layer = "adapter"
max_visibility = "crate"
reason = "adapters should be crate-visible"

[[exceptions]]
stable_id = "adapter/src/lib.rs::fn::adapter_api"
reason = "intentional export for integration boundary"
"#,
    )
    .unwrap();

    let suppressed = Command::new(bin())
        .args(["violations", "--pattern", "adapter_api", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(suppressed.status.success());
    let suppressed_text = String::from_utf8_lossy(&suppressed.stdout);
    assert!(
        !suppressed_text.contains("symbol=\"adapter_api\""),
        "stable_id exception should suppress visibility violation"
    );
}

#[test]
fn violations_audit_surfaces_advisory_rows() {
    use rusqlite::Connection;
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();

    std::fs::write(
        p.join("Cargo.toml"),
        r#"[package]
name = "tmp_graphlite_audit_test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(p.join("src")).unwrap();
    std::fs::write(p.join("src/main.rs"), "fn main() {}\n").unwrap();

    let init = Command::new(bin())
        .args(["init", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(init.status.success(), "init should succeed");

    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[workspace.layers]
tmp_graphlite_audit_test = "application"
"#,
    )
    .unwrap();

    let discover = Command::new(bin())
        .args(["discover", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(discover.status.success(), "discover should succeed");

    let db = Connection::open(p.join(".graphlite/codegraph.db")).unwrap();
    db.execute(
        "INSERT INTO crate_advisories
         (package_name, version, advisory_id, title, cvss, severity, category, kind, date)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            "demo-dep",
            "1.2.3",
            "RUSTSEC-2026-0001",
            "Demo advisory",
            "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N",
            "high",
            "memory-exposure",
            "vulnerability",
            "2026-01-01"
        ],
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["violations", "--audit", "--top", "20", "--no-snippets"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "violations --audit should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<advisory_violations"),
        "expected advisory_violations block"
    );
    assert!(
        text.contains("kind=\"audit\""),
        "expected kind=audit entries"
    );
    assert!(
        text.contains("advisory=\"RUSTSEC-2026-0001\""),
        "expected inserted advisory id in output"
    );
}

#[test]
fn audit_command_handles_missing_cargo_audit_gracefully() {
    let out = Command::new(bin())
        .args(["audit", "."])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "audit command should exit cleanly");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("audit:") || text.contains("not installed"),
        "expected summary or install guidance, got: {text}"
    );
}

#[test]
fn symbols_namespace_query_uses_non_fts_fallback() {
    ensure_db();
    let out = Command::new(bin())
        .args(["symbols", "discover::run", "--language", "rust", "--md"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "symbols namespace query should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("src/discover.rs::fn::run"),
        "expected discover run stable_id in symbols output"
    );
}

#[test]
fn symbols_infix_wildcard_and_scope_filters_work() {
    ensure_db();
    let out = Command::new(bin())
        .args([
            "symbols",
            "*discover*",
            "--language",
            "rust",
            "--file",
            "src/discover.rs",
            "--context",
            "root",
            "--md",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "symbols infix wildcard should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("src/discover.rs"),
        "expected filtered discover file results"
    );
}

#[test]
fn resolve_returns_deterministic_top_candidate_with_strategy() {
    ensure_db();
    let out = Command::new(bin())
        .args(["resolve", "run"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "resolve should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<resolution ") && text.contains("strategy="),
        "expected resolution root with strategy"
    );
    assert!(
        text.contains("selected_id="),
        "expected selected_id on resolution output"
    );
}

#[test]
fn resolve_prefer_role_biases_selection() {
    ensure_db();
    let out = Command::new(bin())
        .args(["resolve", "run", "--prefer-role", "orchestrator"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "resolve with prefer-role should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("role=\"orchestrator\""),
        "expected orchestrator-preferred resolution, got: {text}"
    );
}

#[test]
fn trace_path_emits_paths_and_hops() {
    ensure_db();
    let out = Command::new(bin())
        .args([
            "trace-path",
            "sym:src/discover.rs::fn::run",
            "--direction",
            "outgoing",
            "--max-depth",
            "4",
            "--max-paths",
            "5",
            "--with-async-boundaries",
            "--with-channels",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "trace-path should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("<trace_path "), "expected trace_path root");
    assert!(text.contains("<path "), "expected at least one path");
    assert!(text.contains("<hop "), "expected hop entries");
    assert!(
        text.contains("found=\"5\""),
        "expected max_paths to cap emitted path count"
    );
}

#[test]
fn trace_path_accepts_read_write_direction_aliases() {
    ensure_db();
    let write_out = Command::new(bin())
        .args([
            "trace-path",
            "sym:src/discover.rs::fn::run",
            "--direction",
            "write",
            "--max-depth",
            "2",
            "--max-paths",
            "3",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(write_out.status.success(), "trace-path write alias should succeed");
    let write_text = String::from_utf8_lossy(&write_out.stdout);
    assert!(
        write_text.contains("direction=\"outgoing\""),
        "write alias should map to outgoing"
    );

    let read_out = Command::new(bin())
        .args([
            "trace-path",
            "sym:src/discover.rs::fn::run",
            "--direction",
            "read",
            "--max-depth",
            "2",
            "--max-paths",
            "3",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(read_out.status.success(), "trace-path read alias should succeed");
    let read_text = String::from_utf8_lossy(&read_out.stdout);
    assert!(
        read_text.contains("direction=\"incoming\""),
        "read alias should map to incoming"
    );
}

#[test]
fn trace_path_high_level_mode_marks_output() {
    ensure_db();
    let out = Command::new(bin())
        .args([
            "trace-path",
            "sym:src/discover.rs::fn::run",
            "--high-level",
            "--max-depth",
            "3",
            "--max-paths",
            "3",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "trace-path --high-level should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("high_level=\"true\""),
        "expected high_level marker on trace_path root"
    );
}

#[test]
fn policy_init_pack_writes_pack_to_config() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();
    std::fs::create_dir_all(p.join(".graphlite")).unwrap();
    std::fs::write(p.join(".graphlite/config.toml"), "depth = 2\n").unwrap();

    let out = Command::new(bin())
        .args(["policy", "init-pack", "ddd-hexagonal-rust", "--root", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "policy init-pack should succeed");

    let cfg = std::fs::read_to_string(p.join(".graphlite/config.toml")).unwrap();
    assert!(
        cfg.contains("[policy]") && cfg.contains("pack = \"ddd-hexagonal-rust\""),
        "expected policy pack in config, got: {cfg}"
    );
}

#[test]
fn policy_lint_reports_conflicts_and_dead_rules() {
    use tempfile::tempdir;

    let td = tempdir().unwrap();
    let p = td.path();
    std::fs::create_dir_all(p.join(".graphlite")).unwrap();
    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[[violations]]
from_context = "adapter"
to_context = "infra"
reason = "a"

[[violations]]
from_context = "adapter"
to_context = "infra"
reason = "b"

[[violations]]
from_context = "domain"
to_context = "domain"
reason = "self"
"#,
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["policy", "lint", "--root", "."])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(out.status.success(), "policy lint should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("<policy_lint "),
        "expected policy_lint root output"
    );
    assert!(
        text.contains("code=\"conflict_context_rule\""),
        "expected conflict finding, got: {text}"
    );
    assert!(
        text.contains("code=\"dead_context_self_rule\""),
        "expected dead-rule finding, got: {text}"
    );
}

#[test]
fn graph_budget_lines_emits_truncation_and_resume_handle() {
    ensure_db();
    let first = Command::new(bin())
        .args([
            "graph",
            "sym:src/discover.rs::fn::run",
            "--budget-lines",
            "2",
            "--offset",
            "0",
            "--no-snippets",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(first.status.success(), "graph budget query should succeed");
    let first_text = String::from_utf8_lossy(&first.stdout);
    assert!(first_text.contains("shown_items=\"2\""));
    assert!(first_text.contains("truncated=\"true\""));
    assert!(first_text.contains("next_offset=\"2\""));

    let marker = "stable_id=\"";
    let mut first_neighbor_stable = None::<String>;
    if let Some(nei_pos) = first_text.find("<neighbors") {
        let tail = &first_text[nei_pos..];
        if let Some(pos) = tail.find(marker) {
            let rest = &tail[pos + marker.len()..];
            if let Some(end) = rest.find('"') {
                first_neighbor_stable = Some(rest[..end].to_string());
            }
        }
    }

    let second = Command::new(bin())
        .args([
            "graph",
            "sym:src/discover.rs::fn::run",
            "--budget-lines",
            "2",
            "--offset",
            "2",
            "--no-snippets",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(second.status.success(), "graph resume query should succeed");
    let second_text = String::from_utf8_lossy(&second.stdout);
    assert!(second_text.contains("offset=\"2\""));
    if let Some(stable) = first_neighbor_stable {
        assert!(
            !second_text.contains(&stable),
            "resume chunk should advance beyond first chunk rows"
        );
    }
}

#[test]
fn blast_radius_compact_mode_defers_heavy_payloads() {
    ensure_db();
    let out = Command::new(bin())
        .args([
            "blast-radius",
            "sym:open_db",
            "--depth",
            "2",
            "--budget-lines",
            "5",
            "--compact",
        ])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "compact blast-radius should succeed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("compact=\"true\""),
        "expected compact mode marker on output"
    );
    assert!(
        !text.contains("<snippet>"),
        "compact mode should defer snippets"
    );
    assert!(!text.contains("<annotation"), "compact mode should defer annotations");
}

#[test]
fn reclassify_command_runs_successfully() {
    ensure_db();
    let out = Command::new(bin())
        .args(["reclassify", "."])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success(), "reclassify should succeed");
}

#[test]
fn policy_lint_stale_and_broad_suppressions_support_ci_gates() {
    use tempfile::tempdir;

    ensure_db();
    let td = tempdir().unwrap();
    let p = td.path();
    std::fs::create_dir_all(p.join(".graphlite")).unwrap();
    std::fs::copy(
        std::path::Path::new(root()).join(".graphlite/codegraph.db"),
        p.join(".graphlite/codegraph.db"),
    )
    .unwrap();
    std::fs::write(
        p.join(".graphlite/config.toml"),
        r#"depth = 1

[[exceptions]]
from_context = "definitely_not_a_real_context"
reason = "stale"

[[exceptions]]
reason = "broad"
"#,
    )
    .unwrap();

    let stale_out = Command::new(bin())
        .args(["policy", "lint", "--root", ".", "--stale"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(stale_out.status.success(), "stale lint should succeed");
    let stale_text = String::from_utf8_lossy(&stale_out.stdout);
    assert!(
        stale_text.contains("code=\"dead_exception_unused_rule\""),
        "expected stale suppression finding, got: {stale_text}"
    );

    let fail_stale = Command::new(bin())
        .args(["policy", "lint", "--root", ".", "--fail-on-stale"])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(
        !fail_stale.status.success(),
        "expected non-zero status when stale gate is enabled"
    );

    let fail_broad = Command::new(bin())
        .args([
            "policy",
            "lint",
            "--root",
            ".",
            "--fail-on-broad",
            "--broad-threshold",
            "1",
        ])
        .current_dir(p)
        .output()
        .unwrap();
    assert!(
        !fail_broad.status.success(),
        "expected non-zero status when broad gate is enabled"
    );
}
