mod annotate;
mod config;
mod discover;
mod init_cmd;
mod insert;
mod language;
mod lsp;
mod parser;
mod query;
mod refactor;
mod schema;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "graphlite", about = "Build and query a SQLite code graph")]
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
        /// LSP language for semantic enrichment (overrides config)
        #[arg(long, value_name = "LANG")]
        lsp: Option<String>,
    },
    /// Walk a source tree and index all symbols into codegraph.db
    Discover {
        /// Root directory to index
        #[arg(default_value = ".")]
        root: String,
        /// LSP language to use for semantic enrichment (e.g. "rust")
        #[arg(long, value_name = "LANG")]
        lsp: Option<String>,
    },
    /// Show a symbol and its graph neighborhood as XML
    Graph {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
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
    },
    /// Full-text search for symbols
    Symbols {
        /// FTS5 query string (supports *, AND, OR, etc.)
        query: String,
        /// Filter results by language
        #[arg(long, value_name = "LANG")]
        language: Option<String>,
    },
    /// Find all symbols that (transitively) depend on a given symbol
    BlastRadius {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
        /// Traversal depth limit, 0 = unlimited (overrides config depth; default when unset: config depth or 5)
        #[arg(short, long)]
        depth: Option<usize>,
        /// Suppress source snippets from output
        #[arg(long)]
        no_snippets: bool,
    },
    /// Show full context for a symbol: graph neighborhood + blast radius in one document
    Context {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
        /// Traversal depth for graph neighborhood (overrides config depth)
        #[arg(long)]
        depth: Option<usize>,
        /// Traversal depth for blast radius callers (default: 1)
        #[arg(long)]
        blast_depth: Option<usize>,
        /// Suppress source snippets from output
        #[arg(long)]
        no_snippets: bool,
    },
    /// Rename a symbol via rust-analyzer and write edits to edits.json
    Rename {
        /// Symbol id (integer) or name (sym:Name)
        symbol: String,
        /// New name for the symbol
        new_name: String,
        /// Project root for LSP
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
    /// Show a high-level map of public symbols grouped by file, with hotspots by fan-in
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
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { root, lsp } => init_cmd::run(&root, lsp.as_deref())?,
        Commands::Discover { root, lsp } => discover::run(&root, lsp.as_deref())?,
        Commands::Graph {
            symbol,
            depth,
            format,
            show_trust,
            no_snippets,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            query::graph(&symbol, depth, &format, show_trust, !no_snippets)?
        }
        Commands::Symbols { query, language } => query::symbols(&query, language.as_deref())?,
        Commands::BlastRadius {
            symbol,
            depth,
            no_snippets,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            query::blast_radius(&symbol, depth, !no_snippets)?
        }
        Commands::Context {
            symbol,
            depth,
            blast_depth,
            no_snippets,
        } => {
            let depth = depth.unwrap_or_else(|| config::load(".").depth);
            let blast_depth = blast_depth.unwrap_or(1);
            query::context(&symbol, depth, blast_depth, !no_snippets)?
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
        Commands::Map {
            all,
            top,
            with_docs,
            with_file_docs,
        } => query::map(all, top, with_docs, with_file_docs)?,
    }

    Ok(())
}
