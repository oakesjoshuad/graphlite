mod discover;
mod insert;
mod language;
mod lsp;
mod parser;
mod query;
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
    },
    /// Full-text search for symbols
    Symbols {
        /// FTS5 query string (supports *, AND, OR, etc.)
        query: String,
        /// Filter results by language
        #[arg(long, value_name = "LANG")]
        language: Option<String>,
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
        } => query::graph(&symbol, depth, &format, show_trust)?,
        Commands::Symbols { query, language } => query::symbols(&query, language.as_deref())?,
    }

    Ok(())
}
