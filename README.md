# graphlite

A CLI tool that builds a SQLite code graph from tree-sitter symbol extraction, designed to give LLMs surgical XML context instead of raw source files.

Instead of dumping 18k tokens of raw code into a prompt, `graphlite context sym:run` emits ~2.4k tokens of structured XML describing exactly what a symbol does, what it calls, and what depends on it.

## How it works

1. **Parse** — tree-sitter extracts symbols and edges from source files in parallel
2. **Store** — symbols and edges are inserted into a SQLite database (FTS5 for search)
3. **Enrich** — rust-analyzer provides trusted call edges beyond what static parsing sees
4. **Classify** — graph topology analysis assigns each node a role (entrypoint, orchestrator, infra, leaf, etc.)
5. **Query** — XML renderers surface symbols, neighborhoods, blast radii, and high-level maps on demand

## Supported languages

Rust, TypeScript, JavaScript, Svelte, HTML, CSS

LSP enrichment is currently supported for Rust (via `rust-analyzer`).

## Installation

```
cargo build --release
```

The binary is at `./target/release/graphlite`. Add it to your `PATH` or invoke it directly.

## Quick start

```bash
# First-time setup: create .graphlite/, detect LSP, index the project
graphlite init .

# Re-index after changes (incremental, only changed files)
graphlite discover .

# Find symbols by name (FTS5 full-text search)
graphlite symbols "parse*"

# Get a symbol's graph neighborhood as XML
graphlite graph sym:run

# Find everything that (transitively) depends on a symbol
graphlite blast-radius sym:init_db

# Graph neighborhood + blast radius in a single document
graphlite context sym:run

# High-level map of public symbols grouped by file, with hotspot ranking
graphlite map
graphlite map --all --top 20
```

## Commands

### `init`

Sets up `.graphlite/` in the project root, writes `config.toml` with auto-detected LSP servers, and runs the first full index.

```bash
graphlite init .
graphlite init . --lsp rust
```

### `discover`

Incrementally re-indexes the source tree. Only files whose content hash has changed are re-parsed and re-inserted.

```bash
graphlite discover .
```

### `symbols`

Full-text search over symbol names. Returns XML.

```bash
graphlite symbols "auth*"
graphlite symbols "parse" --language rust
```

### `graph`

Shows a symbol and its immediate call graph neighborhood as XML. Includes signatures and source snippets by default.

```bash
graphlite graph sym:run
graphlite graph sym:run --depth 2
graphlite graph sym:run --no-snippets
```

Symbols can be addressed by name (`sym:run`) or integer node ID.

### `blast-radius`

Finds all symbols that transitively depend on a given symbol. Useful for understanding the impact of a change.

```bash
graphlite blast-radius sym:init_db
graphlite blast-radius sym:Config --depth 3
```

### `context`

Combines `graph` and `blast-radius` into a single `<context>` document. The primary command for generating LLM context.

```bash
graphlite context sym:parse_file
```

### `map`

Emits a high-level map of public symbols grouped by file, plus a hotspot ranking by fan-in count. Useful for understanding the overall shape of a codebase at a glance.

```bash
graphlite map
graphlite map --all          # include private symbols
graphlite map --top 20       # show top 20 hotspots (default: 10)
graphlite map --with-docs    # include doc comments
```

### `annotate` / `annotations`

Attach semantic annotations (intent, behavior, tags) to a symbol. Annotations persist in the database and can be queried. Stale annotations (where the symbol's content has changed since annotation) are flagged.

```bash
graphlite annotate sym:enrich \
  --intent "Enrich graph with LSP-verified call edges" \
  --behavior "Spawns rust-analyzer, walks all fn nodes, inserts CALLS_TRUSTED edges"

graphlite annotations           # list all
graphlite annotations --stale   # list only stale ones
```

### `rename` / `diff-rename` / `apply-edits`

LSP-backed rename workflow. Uses rust-analyzer to compute all edit locations, writes them to `edits.json`, and optionally applies them.

```bash
graphlite rename sym:old_name new_name --root .
graphlite diff-rename           # preview diff without applying
graphlite apply-edits           # apply edits.json to disk atomically
```

## Configuration

`graphlite init` creates `.graphlite/config.toml`:

```toml
ignore = []
lsp = ["rust"]    # auto-detected; accepts multiple values
depth = 2
```

- `ignore` — glob patterns for paths to exclude from indexing
- `lsp` — list of language servers to use for enrichment (currently only `"rust"` is active)
- `depth` — default graph traversal depth

## Database schema

The SQLite database at `.graphlite/codegraph.db` contains:

- `nodes` — one row per symbol: name, kind, file, line range, signature, visibility, content hash, role
- `edges` — directed edges between nodes with an edge type (`CALLS`, `CALLS_TRUSTED`, `DEFINES`, etc.)
- `files` — per-file content hashes for incremental indexing
- `annotations` — semantic annotations keyed by node ID
- `fts_symbols` — FTS5 virtual table over node names for full-text search

## Role inference

After indexing, graphlite runs a second pass that classifies each node using graph topology metrics (fan-in, fan-out, IO proximity, caller spread):

| Role | Meaning |
|---|---|
| `entrypoint` | High fan-out from a zero-fan-in node; likely a top-level command handler |
| `orchestrator` | High fan-out with some callers; coordinates other work |
| `infra` | High IO proximity or broad caller spread; infrastructure/glue |
| `domain` | Mid-range metrics; core business logic |
| `utility` | Moderate fan-in, low fan-out; general-purpose helper |
| `leaf` | Low fan-out; worker called by others |
| `model` | Struct or enum; data container |

Roles appear in all XML output and can guide an LLM toward the most structurally significant symbols.

## Output format

All query commands emit XML to stdout. Diagnostic output (file counts, edge counts) goes to stderr.

```xml
<context symbol="run" total_tokens="2400">
  <graph root_id="72" nodes="8" edges="10">
    <focus>
      <node id="72" name="run" kind="fn" file="./src/discover.rs"
            role="orchestrator" fan_in="2" fan_out="8">
        <signature>pub fn run(root: &amp;str, lsp_lang: Option&lt;&amp;str&gt;) -&gt; Result&lt;()&gt;</signature>
        <snippet>...</snippet>
      </node>
    </focus>
    <neighbors depth="1">
      ...
    </neighbors>
  </graph>
  <blast_radius dependent_count="0">
    ...
  </blast_radius>
</context>
```

## Development

```bash
cargo build --release
cargo test --test e2e              # runs non-LSP integration tests
GRAPHLITE_LSP_TESTS=1 cargo test --test e2e -- --test-threads=1  # include LSP tests
```

E2E tests invoke the release binary directly, so build first.

## Logging

```bash
RUST_LOG=warn graphlite discover .    # default: warnings only
RUST_LOG=info graphlite discover .    # include progress info
RUST_LOG=debug graphlite discover .   # include LSP message traffic
```
