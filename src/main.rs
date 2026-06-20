mod annotate;
mod arch;
mod audit;
mod capabilities;
mod clippy_enricher;
mod config;
mod discover;
mod init_cmd;
mod insert;
mod ipc;
mod language;
mod lsp_client;
mod parser;
mod policy;
mod query;
mod refactor;
mod resolver;
mod roles;
mod rustdoc_enricher;
mod schema;
mod violations;
mod workspace;
mod watch;
mod xml;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "graphlite",
    version,
    about = "Build and query a SQLite code graph",
    long_about = "Build and query a SQLite code graph.\n\nRecommended workflow:\n  1. graphlite deps        — understand the crate architecture and layer assignments first\n  2. graphlite map         — orient across the full symbol set within a crate or the workspace\n  3. graphlite context     — deep-dive into the 1-2 symbols that matter\n\nFor single-crate projects, start at step 2."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Set up .graphlite/ directory, write default config, and run the first index
    Init {
        /// Project root to initialise
        #[arg(default_value = ".")]
        root: String,
    },
    /// Walk a source tree and index all symbols into codegraph.db
    Discover {
        /// Root directory to index
        #[arg(default_value = ".")]
        root: String,
    },
    /// Show a symbol and its graph neighborhood as XML
    ///
    /// Cheaper than `context` -- no blast radius. Use when you need to understand what a symbol
    /// calls without needing to know what depends on it. `--no-snippets` gives pure structural
    /// output (signatures + roles) at significantly lower token cost.
    ///
    /// See also: `symbols` command to find stable_id values by name or qualified name (Type::method).
    Graph {
        /// Symbol id (integer) or name (sym:Name); accepts multiple
        #[arg(required = true)]
        symbols: Vec<String>,
        /// Traversal depth (overrides config depth; default when unset: config depth or 1)
        #[arg(short, long)]
        depth: Option<usize>,
        /// Output format (xml)
        #[arg(long, default_value = "xml")]
        format: String,
        /// Split output by edge trust level (trusted vs syntax)
        #[arg(long)]
        show_trust: bool,
        /// Suppress source snippets from output
        #[arg(long)]
        no_snippets: bool,
        /// Truncate each snippet to at most N lines (appends a comment with remaining count)
        #[arg(long, value_name = "N")]
        max_snippet_lines: Option<usize>,
        /// Maximum rows to emit from ordered neighbors/dependents
        #[arg(long, value_name = "N")]
        budget_lines: Option<usize>,
        /// Approximate token budget over emitted rows (4 chars/token estimate)
        #[arg(long, value_name = "N")]
        budget_tokens: Option<usize>,
        /// Resume window at this row offset
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Compact output: suppress doc/snippet/annotation payloads
        #[arg(long)]
        compact: bool,
    },
    /// Full-text search for symbols -- the recommended first step when you know a name.
    ///
    /// FTS5 query language (porter stemming enabled):
    ///   require_permission          exact word (stemmed)
    ///   require*                    prefix match
    ///   "require permission"        phrase
    ///   auth* AND handler           boolean AND
    ///   SqlitePermissionChecker::*  qualified name prefix (Type::method form)
    ///
    /// Output: XML `symbol` elements with id, name, qualified_name, kind, role, file,
    /// signature, and stable_id. Use the stable_id value directly with graph/context/blast-radius.
    ///
    /// Tip: qualified names (Type::method) are resolved directly -- no need to know the file.
    /// Tip: role attribute signals architectural layer; orchestrator/infra carry the most
    /// structural signal per token.
    ///
    /// Next steps after symbols:
    ///   graphlite context sym:STABLE_ID   -- deep dive on one symbol
    ///   graphlite graph sym:STABLE_ID     -- cheaper: neighborhood only, no blast-radius
    Symbols {
        /// FTS5 query string (supports *, AND, OR, etc.)
        query: String,
        /// Filter results by language
        #[arg(long, value_name = "LANG")]
        language: Option<String>,
        /// Restrict results to files containing this substring
        #[arg(long, value_name = "PATTERN")]
        file: Option<String>,
        /// Restrict results to a bounded context name (derived from file path)
        #[arg(long, value_name = "CTX")]
        context: Option<String>,
        /// Restrict results to a workspace crate name
        #[arg(long, value_name = "CRATE")]
        crate_name: Option<String>,
        /// Exclude test symbols/files from results
        #[arg(long)]
        exclude_tests: bool,
        /// Output as markdown table instead of XML
        #[arg(long)]
        md: bool,
    },
    /// Show workspace crate dependency graph (cargo metadata derived)
    ///
    /// Start here for any multi-crate workspace. Shows each crate's layer assignment,
    /// fan-in (how many other crates depend on it), and crate-level doc summary.
    /// This gives you the architectural skeleton before descending into symbols with `map`.
    Deps {
        /// Output as markdown instead of XML
        #[arg(long)]
        md: bool,
        /// Expand each crate to list its top-level modules with symbol count,
        /// dominant role, and the single highest-fan-in symbol (hotspot)
        #[arg(long)]
        modules: bool,
    },
    /// Resolve an ambiguous symbol query to a deterministic top candidate
    Resolve {
        /// Symbol selector (id, sym:stable_id, qualified_name, or plain name)
        query: String,
        /// Filter candidates by language
        #[arg(long, value_name = "LANG")]
        language: Option<String>,
        /// Prefer candidates in this inferred role (e.g. entrypoint, orchestrator)
        #[arg(long, value_name = "ROLE")]
        prefer_role: Option<String>,
        /// Prefer candidates whose file path contains this substring
        #[arg(long, value_name = "PATTERN")]
        prefer_file: Option<String>,
        /// Output as markdown instead of XML
        #[arg(long)]
        md: bool,
    },
    /// Trace execution paths from an entrypoint to completion boundaries
    TracePath {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
        /// Optional target selector (id, sym:stable_id, qualified/name)
        #[arg(long)]
        to: Option<String>,
        /// Traversal direction: outgoing|incoming|both (aliases: write|read)
        #[arg(long, default_value = "outgoing")]
        direction: String,
        /// Maximum hop depth
        #[arg(long, default_value = "6")]
        max_depth: usize,
        /// Maximum number of paths to emit
        #[arg(long, default_value = "20")]
        max_paths: usize,
        /// Annotate async boundaries in path hops
        #[arg(long)]
        with_async_boundaries: bool,
        /// Annotate channel/actor boundaries in path hops
        #[arg(long)]
        with_channels: bool,
        /// Collapse low-signal utility/leaf hops to emphasize endpoint-to-core flow
        #[arg(long)]
        high_level: bool,
    },
    /// Re-run role classification over the current graph after indexing/annotations
    Reclassify {
        /// Project root
        #[arg(default_value = ".")]
        root: String,
    },
    /// Find all symbols that (transitively) depend on a given symbol
    ///
    /// Emits call-site snippets (2 lines around each call) rather than full function bodies —
    /// the intent is to show how each caller uses the target, not what the caller does overall.
    /// Use `--no-snippets` if you only need the list of dependent symbol names.
    BlastRadius {
        /// Symbol id (integer) or name (sym:Name); accepts multiple
        #[arg(required = true)]
        symbols: Vec<String>,
        /// Traversal depth limit, 0 = unlimited (overrides config depth; default when unset: config depth or 5)
        #[arg(short, long)]
        depth: Option<usize>,
        /// Suppress source snippets from output
        #[arg(long)]
        no_snippets: bool,
        /// Truncate each snippet to at most N lines (appends a comment with remaining count)
        #[arg(long, value_name = "N")]
        max_snippet_lines: Option<usize>,
        /// Maximum rows to emit from ordered neighbors/dependents
        #[arg(long, value_name = "N")]
        budget_lines: Option<usize>,
        /// Approximate token budget over emitted rows (4 chars/token estimate)
        #[arg(long, value_name = "N")]
        budget_tokens: Option<usize>,
        /// Resume window at this row offset
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Compact output: suppress doc/snippet/annotation payloads
        #[arg(long)]
        compact: bool,
    },
    /// Show full context for a symbol: graph neighborhood + blast radius in one document
    ///
    /// Combines graph neighborhood and blast radius in one document. Snippets are included by
    /// default and are the primary driver of token count — use selectively after `map` has
    /// identified the specific symbol you need to understand deeply.
    ///
    /// Use `--no-snippets` when you need call structure only (who this calls, who depends on it)
    /// without reading implementations. The `role` attribute on the focus node signals
    /// architectural significance: `orchestrator` and `infra` nodes yield the most structural
    /// insight per token.
    Context {
        /// Symbol id (integer) or name (sym:Name); accepts multiple
        #[arg(required = true)]
        symbols: Vec<String>,
        /// Traversal depth for graph neighborhood (overrides config depth)
        #[arg(long)]
        depth: Option<usize>,
        /// Traversal depth for blast radius callers (default: 1)
        #[arg(long)]
        blast_depth: Option<usize>,
        /// Suppress source snippets from output
        #[arg(long)]
        no_snippets: bool,
        /// Minimal edit surface: signature+doc+annotation only, no snippets, depth 1 each side
        /// (~150–500 tokens). Implies --no-snippets and overrides --depth/--blast-depth.
        #[arg(long)]
        edit: bool,
        /// Truncate each snippet to at most N lines (appends a comment with remaining count)
        #[arg(long, value_name = "N")]
        max_snippet_lines: Option<usize>,
        /// Maximum rows to emit from ordered neighbors/dependents
        #[arg(long, value_name = "N")]
        budget_lines: Option<usize>,
        /// Approximate token budget over emitted rows (4 chars/token estimate)
        #[arg(long, value_name = "N")]
        budget_tokens: Option<usize>,
        /// Resume window at this row offset
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Compact output: suppress doc/snippet/annotation payloads
        #[arg(long)]
        compact: bool,
    },
    /// Semantic rename via rust-analyzer (requires `graphlite watch` to be running)
    ///
    /// Asks the watch daemon to rename a symbol using rust-analyzer's LSP rename,
    /// which handles all call sites, trait impls, re-exports, and macro-generated
    /// references. The resulting WorkspaceEdit is written to `edits.json`.
    ///
    /// Workflow:
    ///   1. graphlite watch .          (start daemon; keeps rust-analyzer warm)
    ///   2. graphlite rename sym:X Y   (produces edits.json)
    ///   3. graphlite diff-rename      (review changes)
    ///   4. graphlite apply-edits      (commit atomically)
    ///
    /// The first rename warms rust-analyzer (~15-60s depending on project size).
    /// Subsequent renames in the same watch session are fast (reuse warm client).
    Rename {
        /// Symbol id (integer), stable_id, sym:name, or plain name
        symbol: String,
        /// New name for the symbol
        new_name: String,
        /// Project root (must match the root passed to `graphlite watch`)
        #[arg(long, default_value = ".")]
        root: String,
    },
    /// Preview a rename diff from edits.json without modifying files
    DiffRename {
        /// Path to the edits JSON file
        #[arg(default_value = "edits.json")]
        edits_file: String,
    },
    /// Apply edits from edits.json to disk atomically
    ApplyEdits {
        /// Path to the edits JSON file
        #[arg(default_value = "edits.json")]
        edits_file: String,
    },
    /// Add or update a semantic annotation on a symbol
    Annotate {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
        /// Short description of what the symbol does
        #[arg(long)]
        intent: Option<String>,
        /// Description of observable behavior and side effects
        #[arg(long)]
        behavior: Option<String>,
        /// Comma-separated domain tags (e.g. "db,io,entrypoint")
        #[arg(long)]
        tags: Option<String>,
        /// Annotation source identifier
        #[arg(long, default_value = "llm")]
        source: String,
        /// Confidence score 0.0–1.0
        #[arg(long, default_value = "0.8")]
        confidence: f64,
    },
    /// List annotations, optionally filtered to stale ones
    Annotations {
        /// Show only annotations whose symbol has changed since annotation was written
        #[arg(long)]
        stale: bool,
    },
    /// Watch a source tree for changes and re-index automatically
    ///
    /// Runs as a long-lived process. Listens on `.graphlite/watcher.sock` for IPC
    /// from `graphlite annotate` and other clients. File changes trigger a
    /// full re-index (parser + enrichers); `reindex` socket messages do the same.
    Watch {
        /// Root directory to watch (default: .)
        #[arg(default_value = ".")]
        root: String,
    },
    /// Show workspace and context violations: forbidden cross-context couplings
    ///
    /// Queries semantic edges (resolver/rustdoc sources). Test nodes are excluded.
    /// Use `[[exceptions]]` in `.graphlite/config.toml` to suppress known-acceptable patterns.
    Violations {
        /// Filter to edges leaving this bounded context
        #[arg(long, value_name = "CTX")]
        from_context: Option<String>,
        /// Filter to edges arriving at this bounded context
        #[arg(long, value_name = "CTX")]
        to_context: Option<String>,
        /// Show violations touching this context (from or to)
        #[arg(long, value_name = "CTX")]
        context: Option<String>,
        /// Max violations to show, 0 = unlimited (default: 50)
        #[arg(long, default_value = "50")]
        top: usize,
        /// Substring filter over symbol/file/context/layer fields
        #[arg(long)]
        pattern: Option<String>,
        /// Group context-coupling findings by owning crate edge
        #[arg(long)]
        by_crate: bool,
        /// Crate-edge selector (e.g. \"from:adapter to:domain\")
        #[arg(long)]
        edge: Option<String>,
        /// Suppress caller/callee signatures from output
        #[arg(long)]
        no_snippets: bool,
        /// Fail when policy has unresolved/ambiguous mappings (e.g. '?' workspace layers)
        #[arg(long)]
        strict_policy: bool,
        /// Include code-quality violations sourced from `graphlite check` diagnostics
        #[arg(long)]
        check: bool,
        /// Include crate advisory findings sourced from `graphlite audit`
        #[arg(long)]
        audit: bool,
    },
    /// Run cargo audit advisories ingestion and store crate risk metadata
    Audit {
        /// Project root
        #[arg(default_value = ".")]
        root: String,
    },
    /// Run cargo clippy diagnostics enrichment and store results in codegraph.db
    Check {
        /// Project root
        #[arg(default_value = ".")]
        root: String,
    },
    /// Show supported command/flag surface (for agent/tooling compatibility)
    Capabilities {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage built-in architecture policy packs and linting
    Policy {
        #[command(subcommand)]
        command: PolicyCommands,
    },
    /// Show a high-level map of public symbols grouped by file, with hotspots by fan-in
    ///
    /// Symbol-level orientation command. In a multi-crate workspace, run `deps` first to
    /// understand crate structure, then use `map` within a specific crate. For single-crate
    /// projects, `map` is your starting point. Emits all public symbols grouped by file with
    /// roles, signatures, fan-in/fan-out, and hotspot ranking. Token count is reported in the
    /// XML header so you know exactly what you consumed.
    ///
    /// Recommended: `map --with-docs --with-file-docs` gives maximum signal in one pass.
    /// From the role and fan-in attributes you can identify which 1-2 symbols warrant a deeper
    /// `context` query. Use `--all` to include private symbols when you need implementation-level
    /// detail without pulling full snippets.
    Map {
        /// Include private symbols (default: public/exported only)
        #[arg(long)]
        all: bool,
        /// Number of hotspot symbols to highlight by fan-in (default: 10)
        #[arg(long, default_value = "10")]
        top: usize,
        /// Include per-symbol doc comments (/// for Rust, JSDoc for TS/JS)
        #[arg(long)]
        with_docs: bool,
        /// Include file-level doc comments (//! for Rust, @component for Svelte, JSDoc header for TS/JS)
        #[arg(long)]
        with_file_docs: bool,
        /// Rank hotspots by all edges.
        /// By default, hotspots are ranked by semantic trusted edges (resolver/rustdoc).
        #[arg(long)]
        all_edges: bool,
        /// Filter by inferred role (orchestrator, entrypoint, leaf, infra, domain, model)
        #[arg(long, value_name = "ROLE")]
        role: Option<String>,
        /// Filter by bounded context name (derived from file path)
        #[arg(long, value_name = "CTX")]
        context: Option<String>,
        /// Group output by file path instead of module name (reverts default module grouping)
        #[arg(long)]
        by_file: bool,
        /// Output as markdown tables instead of XML
        #[arg(long)]
        md: bool,
    },
}

#[derive(Subcommand)]
enum PolicyCommands {
    /// Initialize a built-in policy pack in .graphlite/config.toml
    InitPack {
        /// Pack name: ddd-hexagonal-rust | cqrs-event-sourced-rust
        pack: String,
        /// Project root
        #[arg(long, default_value = ".")]
        root: String,
        /// Replace existing [policy].pack value
        #[arg(long)]
        force: bool,
    },
    /// Lint policy rules for conflicts and ineffective/dead entries
    Lint {
        /// Project root
        #[arg(long, default_value = ".")]
        root: String,
        /// Show only stale suppression findings
        #[arg(long)]
        stale: bool,
        /// Exit non-zero when stale suppressions are found
        #[arg(long)]
        fail_on_stale: bool,
        /// Exit non-zero when overbroad suppressions are found
        #[arg(long)]
        fail_on_broad: bool,
        /// Match-count threshold for broad suppression detection
        #[arg(long, default_value = "25")]
        broad_threshold: usize,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("graphlite=info".parse().unwrap()),
        )
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new("%H:%M:%S".to_string()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { root } => init_cmd::run(&root)?,
        Commands::Discover { root } => discover::run(&root)?,
        Commands::Graph {
            symbols,
            depth,
            format,
            show_trust,
            no_snippets,
            max_snippet_lines,
            budget_lines,
            budget_tokens,
            offset,
            compact,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            query::graph(
                &symbols,
                depth,
                &format,
                show_trust,
                !no_snippets,
                max_snippet_lines,
                query::OutputControl {
                    budget_lines,
                    budget_tokens,
                    offset,
                    compact,
                },
            )?
        }
        Commands::Symbols {
            query,
            language,
            file,
            context,
            crate_name,
            exclude_tests,
            md,
        } => query::symbols(
            &query,
            language.as_deref(),
            file.as_deref(),
            context.as_deref(),
            crate_name.as_deref(),
            exclude_tests,
            md,
        )?,
        Commands::Deps { md, modules } => query::deps(md, modules)?,
        Commands::Resolve {
            query,
            language,
            prefer_role,
            prefer_file,
            md,
        } => query::resolve(
            &query,
            language.as_deref(),
            prefer_role.as_deref(),
            prefer_file.as_deref(),
            md,
        )?,
        Commands::TracePath {
            symbol,
            to,
            direction,
            max_depth,
            max_paths,
            with_async_boundaries,
            with_channels,
            high_level,
        } => query::trace_path(
            &symbol,
            to.as_deref(),
            &direction,
            max_depth,
            max_paths,
            with_async_boundaries,
            with_channels,
            high_level,
        )?,
        Commands::Reclassify { root } => roles::run(&root)?,
        Commands::BlastRadius {
            symbols,
            depth,
            no_snippets,
            max_snippet_lines,
            budget_lines,
            budget_tokens,
            offset,
            compact,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            query::blast_radius(
                &symbols,
                depth,
                !no_snippets,
                max_snippet_lines,
                query::OutputControl {
                    budget_lines,
                    budget_tokens,
                    offset,
                    compact,
                },
            )?
        }
        Commands::Context {
            symbols,
            depth,
            blast_depth,
            no_snippets,
            edit,
            max_snippet_lines,
            budget_lines,
            budget_tokens,
            offset,
            compact,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            let blast_depth = blast_depth.unwrap_or(1);
            query::context(
                &symbols,
                depth,
                blast_depth,
                !no_snippets,
                edit,
                max_snippet_lines,
                query::OutputControl {
                    budget_lines,
                    budget_tokens,
                    offset,
                    compact,
                },
            )?
        }
        Commands::Rename {
            symbol,
            new_name,
            root,
        } => refactor::rename(&symbol, &new_name, &root)?,
        Commands::DiffRename { edits_file } => refactor::diff_rename(&edits_file)?,
        Commands::ApplyEdits { edits_file } => refactor::apply_edits(&edits_file)?,
        Commands::Annotate {
            symbol,
            intent,
            behavior,
            tags,
            source,
            confidence,
        } => annotate::annotate(
            &symbol,
            intent.as_deref(),
            behavior.as_deref(),
            tags.as_deref(),
            &source,
            confidence,
        )?,
        Commands::Annotations { stale } => annotate::list_annotations(stale)?,
        Commands::Watch { root } => watch::run(&root)?,
        Commands::Violations {
            from_context,
            to_context,
            context,
            top,
            pattern,
            by_crate,
            edge,
            no_snippets,
            strict_policy,
            check,
            audit,
        } => violations::run(&violations::Params {
            from_context: from_context.as_deref(),
            to_context: to_context.as_deref(),
            context: context.as_deref(),
            top,
            pattern: pattern.as_deref(),
            by_crate,
            edge: edge.as_deref(),
            no_snippets,
            strict_policy,
            check,
            audit,
        })?,
        Commands::Check { root } => clippy_enricher::run(&root)?,
        Commands::Audit { root } => audit::run(&root)?,
        Commands::Capabilities { json } => capabilities::run(json)?,
        Commands::Policy { command } => match command {
            PolicyCommands::InitPack { root, pack, force } => {
                policy::init_pack(&root, &pack, force)?
            }
            PolicyCommands::Lint {
                root,
                stale,
                fail_on_stale,
                fail_on_broad,
                broad_threshold,
            } => policy::lint(
                &root,
                policy::LintOptions {
                    stale_only: stale,
                    fail_on_stale,
                    fail_on_broad,
                    broad_threshold,
                },
            )?,
        },
        Commands::Map {
            all,
            top,
            with_docs,
            with_file_docs,
            all_edges,
            role,
            context,
            by_file,
            md,
        } => query::map(
            all,
            top,
            with_docs,
            with_file_docs,
            all_edges,
            role.as_deref(),
            context.as_deref(),
            by_file,
            md,
        )?,
    }

    Ok(())
}
