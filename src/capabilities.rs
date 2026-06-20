use anyhow::Result;
use serde::Serialize;

#[derive(Serialize, Clone)]
struct FlagCap {
    name: &'static str,
    takes_value: bool,
}

#[derive(Serialize)]
struct CommandCap {
    name: &'static str,
    summary: &'static str,
    flags: Vec<FlagCap>,
}

#[derive(Serialize)]
struct CapabilitySurface {
    name: &'static str,
    version: &'static str,
    commands: Vec<CommandCap>,
}

pub fn run(json: bool) -> Result<()> {
    let surface = CapabilitySurface {
        name: "graphlite",
        version: env!("CARGO_PKG_VERSION"),
        commands: vec![
            cmd("init", "Initialize graphlite config and index", &[]),
            cmd("discover", "Index symbols and edges into codegraph.db", &[]),
            cmd(
                "symbols",
                "Search symbols by FTS query",
                &[
                    flag("language", true),
                    flag("file", true),
                    flag("context", true),
                    flag("crate-name", true),
                    flag("exclude-tests", false),
                    flag("md", false),
                ],
            ),
            cmd("deps", "Show workspace crate dependency graph", &[flag("md", false), flag("modules", false)]),
            cmd(
                "resolve",
                "Resolve ambiguous symbol selectors to deterministic top candidates",
                &[
                    flag("language", true),
                    flag("prefer-role", true),
                    flag("prefer-file", true),
                    flag("md", false),
                ],
            ),
            cmd(
                "trace-path",
                "Trace function-hop paths between entrypoints and completion boundaries",
                &[
                    flag("to", true),
                    flag("direction", true),
                    flag("max-depth", true),
                    flag("max-paths", true),
                    flag("with-async-boundaries", false),
                    flag("with-channels", false),
                    flag("high-level", false),
                ],
            ),
            cmd(
                "graph",
                "Graph neighborhood for one or more symbols",
                &[
                    flag("depth", true),
                    flag("format", true),
                    flag("show-trust", false),
                    flag("no-snippets", false),
                    flag("max-snippet-lines", true),
                    flag("budget-lines", true),
                    flag("budget-tokens", true),
                    flag("offset", true),
                    flag("compact", false),
                ],
            ),
            cmd(
                "blast-radius",
                "Caller impact for one or more symbols",
                &[
                    flag("depth", true),
                    flag("no-snippets", false),
                    flag("max-snippet-lines", true),
                    flag("budget-lines", true),
                    flag("budget-tokens", true),
                    flag("offset", true),
                    flag("compact", false),
                ],
            ),
            cmd(
                "context",
                "Combined graph + blast-radius view for one or more symbols",
                &[
                    flag("depth", true),
                    flag("blast-depth", true),
                    flag("max-depth", true),
                    flag("no-snippets", false),
                    flag("edit", false),
                    flag("max-snippet-lines", true),
                    flag("budget-lines", true),
                    flag("budget-tokens", true),
                    flag("offset", true),
                    flag("compact", false),
                ],
            ),
            cmd(
                "watch",
                "Watch source files and re-index on change",
                &[],
            ),
            cmd(
                "reclassify",
                "Re-run role classification over the current graph",
                &[],
            ),
            cmd(
                "violations",
                "Context and workspace layer policy checks",
                &[
                    flag("from-context", true),
                    flag("to-context", true),
                    flag("context", true),
                    flag("top", true),
                    flag("pattern", true),
                    flag("by-crate", false),
                    flag("edge", true),
                    flag("no-snippets", false),
                    flag("strict-policy", false),
                    flag("check", false),
                    flag("audit", false),
                ],
            ),
            cmd(
                "check",
                "Run cargo clippy diagnostics and store node quality signals",
                &[],
            ),
            cmd(
                "audit",
                "Run cargo-audit advisories and store crate risk signals",
                &[],
            ),
            cmd(
                "map",
                "High-level symbol map and hotspots",
                &[
                    flag("all", false),
                    flag("top", true),
                    flag("with-docs", false),
                    flag("with-file-docs", false),
                    flag("all-edges", false),
                    flag("role", true),
                    flag("context", true),
                    flag("by-file", false),
                    flag("md", false),
                ],
            ),
            cmd(
                "annotate",
                "Add or update a semantic annotation on a symbol",
                &[
                    flag("intent", true),
                    flag("behavior", true),
                    flag("tags", true),
                    flag("source", true),
                    flag("confidence", true),
                ],
            ),
            cmd("annotations", "List annotations", &[flag("stale", false)]),
            cmd(
                "rename",
                "Semantic rename via rust-analyzer; requires graphlite watch to be running",
                &[flag("root", true)],
            ),
            cmd(
                "diff-rename",
                "Preview edits.json as a human-readable diff (accepts optional [EDITS_FILE] arg)",
                &[],
            ),
            cmd(
                "apply-edits",
                "Apply edits.json to disk atomically (accepts optional [EDITS_FILE] arg)",
                &[],
            ),
            cmd(
                "capabilities",
                "Show supported command and flag surface",
                &[flag("json", false)],
            ),
            cmd(
                "policy",
                "Manage built-in architecture policy packs (subcommands: init-pack, lint)",
                &[],
            ),
            cmd(
                "policy init-pack",
                "Initialize a built-in policy pack in .graphlite/config.toml",
                &[
                    flag("root", true),
                    flag("force", false),
                ],
            ),
            cmd(
                "policy lint",
                "Lint policy rules for stale/broad suppressions; supports CI gate flags",
                &[
                    flag("root", true),
                    flag("stale", false),
                    flag("fail-on-stale", false),
                    flag("fail-on-broad", false),
                    flag("broad-threshold", true),
                ],
            ),
        ],
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&surface)?);
    } else {
        println!("graphlite {} capabilities", surface.version);
        println!("commands:");
        for c in &surface.commands {
            let flags = if c.flags.is_empty() {
                String::from("-")
            } else {
                c.flags
                    .iter()
                    .map(|f| {
                        if f.takes_value {
                            format!("--{} <value>", f.name)
                        } else {
                            format!("--{}", f.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!("  {:<14} {}", c.name, c.summary);
            println!("  {:<14} {}", "flags:", flags);
        }
    }

    Ok(())
}

fn cmd(name: &'static str, summary: &'static str, flags: &[FlagCap]) -> CommandCap {
    CommandCap {
        name,
        summary,
        flags: flags.to_vec(),
    }
}

fn flag(name: &'static str, takes_value: bool) -> FlagCap {
    FlagCap { name, takes_value }
}
