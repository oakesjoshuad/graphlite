mod discover;
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
        /// Traversal depth
        #[arg(short, long, default_value = "1")]
        depth: usize,
        /// Output format (xml)
        #[arg(long, default_value = "xml")]
        format: String,
        /// Split output by edge trust level (trusted vs syntax)
        #[arg(long)]
        show_trust: bool,
        /// Include full body snippets for neighbor nodes
        #[arg(long)]
        snippets: bool,
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
        /// Traversal depth limit (0 = unlimited)
        #[arg(short, long, default_value = "5")]
        depth: usize,
        /// Include call-site source snippets for each dependent
        #[arg(long)]
        snippets: bool,
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
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Discover { root, lsp } => discover::run(&root, lsp.as_deref())?,
        Commands::Graph {
            symbol,
            depth,
            format,
            show_trust,
            snippets,
        } => query::graph(&symbol, depth, &format, show_trust, snippets)?,
        Commands::Symbols { query, language } => query::symbols(&query, language.as_deref())?,
        Commands::BlastRadius { symbol, depth, snippets } => {
            query::blast_radius(&symbol, depth, snippets)?
        }
        Commands::Rename {
            symbol,
            new_name,
            root,
        } => refactor::rename(&symbol, &new_name, &root)?,
        Commands::DiffRename { edits_file } => refactor::diff_rename(&edits_file)?,
        Commands::ApplyEdits { edits_file } => refactor::apply_edits(&edits_file)?,
    }

    Ok(())
}
