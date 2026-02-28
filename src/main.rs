mod discover;
mod insert;
mod language;
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
    },
    /// Full-text search for symbols
    Symbols {
        /// FTS5 query string (supports *, AND, OR, etc.)
        query: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Discover { root } => discover::run(&root)?,
        Commands::Graph {
            symbol,
            depth,
            format,
        } => query::graph(&symbol, depth, &format)?,
        Commands::Symbols { query } => query::symbols(&query)?,
    }

    Ok(())
}
