use std::process::Command;
use std::sync::OnceLock;

static DB_READY: OnceLock<()> = OnceLock::new();

fn bin() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("target/release/graphlite");
    p
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

fn lsp_enabled() -> bool {
    std::env::var("GRAPHLITE_LSP_TESTS").is_ok()
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

// --- LSP-gated ---

#[test]
fn lsp_rename_produces_edits_json() {
    if !lsp_enabled() {
        eprintln!("skip: GRAPHLITE_LSP_TESTS not set");
        return;
    }
    ensure_db();
    let status = Command::new(bin())
        .args(["rename", "sym:xml_escape", "xml_encode", "--root", "."])
        .current_dir(root())
        .status()
        .unwrap();
    assert!(status.success());
    assert!(
        std::path::Path::new(root()).join("edits.json").exists(),
        "edits.json should be created after rename"
    );
}

#[test]
fn lsp_diff_rename_shows_old_and_new() {
    if !lsp_enabled() {
        eprintln!("skip: GRAPHLITE_LSP_TESTS not set");
        return;
    }
    let out = Command::new(bin())
        .args(["diff-rename"])
        .current_dir(root())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("xml_escape") && text.contains("xml_encode"),
        "diff should show both old and new names"
    );
}

#[test]
fn lsp_apply_edits_and_cargo_builds() {
    if !lsp_enabled() {
        eprintln!("skip: GRAPHLITE_LSP_TESTS not set");
        return;
    }
    let status = Command::new(bin())
        .args(["apply-edits"])
        .current_dir(root())
        .status()
        .unwrap();
    assert!(status.success());
    let build = Command::new("cargo")
        .args(["build"])
        .current_dir(root())
        .status()
        .unwrap();
    assert!(
        build.success(),
        "cargo build must succeed after apply-edits"
    );
    // Restore changes
    Command::new("git")
        .args(["checkout", "--", "."])
        .current_dir(root())
        .status()
        .unwrap();
    let _ = std::fs::remove_file(std::path::Path::new(root()).join("edits.json"));
}
