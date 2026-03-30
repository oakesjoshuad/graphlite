//! Shared architectural analysis helpers used by both `viz` and `violations`.

use crate::config::{ViolationException, ViolationRule};

/// Derive a bounded context name from a file path.
/// `./infrastructure/src/foo.rs` → `"infrastructure"`
/// `./src/foo.rs`                → `"root"`
pub fn file_to_context(path: &str) -> String {
    let normalized = path.trim_start_matches("./");
    let first = normalized.split('/').next().unwrap_or(normalized);
    if first == "src" {
        "root".to_string()
    } else {
        first.to_string()
    }
}

/// Returns true when a file path belongs to test code.
/// Matches: `tests/*`, `*/tests/*`, `*_test.rs`, `*_tests.rs`.
pub fn is_test_file(path: &str) -> bool {
    let p = path.trim_start_matches("./");
    p.starts_with("tests/")
        || p.contains("/tests/")
        || p.ends_with("_test.rs")
        || p.ends_with("_tests.rs")
}

/// Config-defined forbidden context coupling. Applied to all edges (context
/// coupling is file-level, which tree-sitter reliably captures).
pub fn context_violation_reason(
    from_ctx: &str,
    to_ctx: &str,
    rules: &[ViolationRule],
) -> Option<String> {
    if from_ctx == to_ctx {
        return None;
    }
    for rule in rules {
        if rule.from_context == from_ctx && rule.to_context == to_ctx {
            let r = rule.reason.as_deref().unwrap_or("context coupling");
            return Some(format!("{from_ctx} \u{2192} {to_ctx} ({r})"));
        }
    }
    None
}

/// Check whether a violation edge is covered by a suppression exception.
/// All non-`None` fields of the exception must match for it to fire.
/// Returns the exception reason string if suppressed.
pub fn exception_reason<'a>(
    from_ctx: &str,
    to_ctx: &str,
    from_role: &str,
    to_role: &str,
    exceptions: &'a [ViolationException],
) -> Option<&'a str> {
    for exc in exceptions {
        let fc = exc.from_context.as_deref().is_none_or(|v| v == from_ctx);
        let tc = exc.to_context.as_deref().is_none_or(|v| v == to_ctx);
        let fr = exc.from_role.as_deref().is_none_or(|v| v == from_role);
        let tr = exc.to_role.as_deref().is_none_or(|v| v == to_role);
        if fc && tc && fr && tr {
            return Some(exc.reason.as_deref().unwrap_or("suppressed"));
        }
    }
    None
}
