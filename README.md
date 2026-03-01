# graphlite

Token-metered, role-annotated code context for LLMs — a SQLite-backed graph that serves precise XML snapshots instead of raw source files.

Feeding a raw source file to an LLM costs 5–15x more tokens than a graphlite query on the same symbol. Every graphlite output carries an explicit `tokens="..."` attribute so you always know exactly what you consumed.

---

## What it does differently

Most code indexers answer "where is this defined?" Graphlite answers:

- **What does this symbol call, and what calls it?** (`context` — call graph neighborhood + blast radius in one document)
- **If I change this, what breaks transitively?** (`blast-radius` — full dependent chain, not just direct callers)
- **Which symbols are architecturally significant?** (`map` with topology-inferred roles)
- **Are these call edges verified or inferred?** (LSP-trusted vs tree-sitter syntactic, distinguished in every output)

The last point matters most. Tree-sitter can resolve a free function call but cannot resolve `object.method()` across files without type information. Rust-analyzer's call hierarchy gives verified, type-resolved edges. Graphlite tracks which is which so you can calibrate confidence in the graph before relying on it.

---

## Pipeline

```
Source files
    │
    ├── tree-sitter (parallel parse) ──► symbols + syntactic CALLS edges
    │                                    source="tree-sitter", confidence=0.8
    │
    └── rust-analyzer (LSP) ──────────► CALLS_TRUSTED edges (type-resolved)
                                         source="rust-analyzer", confidence=1.0
                                                    │
                                         SQLite (FTS5 search, graph queries)
                                                    │
                                         Role inference (topology analysis)
                                                    │
                                         XML output with token counts
```

Re-indexing is incremental: only files whose content hash has changed are re-parsed.

---

## Supported languages

| Language   | Symbols | Call edges | LSP enrichment     |
|------------|---------|------------|--------------------|
| Rust       | yes     | yes        | rust-analyzer      |
| TypeScript | yes     | yes        | —                  |
| JavaScript | yes     | yes        | —                  |
| Svelte     | yes     | yes        | —                  |
| HTML       | yes     | —          | —                  |
| CSS        | yes     | —          | —                  |

---

## Installation

```bash
cargo build --release
# binary: ./target/release/graphlite
```

Add to `$PATH` or invoke with the full path.

---

## Recommended workflow

Follow in order. Escalate only when the previous step is insufficient.

### 1. Orient — always start here

```bash
graphlite map
graphlite map --with-docs --with-file-docs
```

All public symbols grouped by file, with roles, fan-in, fan-out, and signatures. `<hotspots>` surfaces the top 10 symbols by trusted fan-in — the nodes where behaviour concentrates. Typical cost: **800–2000 tokens**.

Narrow by role when you know the architectural layer you're investigating:

```bash
graphlite map --role orchestrator   # coordination logic
graphlite map --role infra          # IO, persistence, external services
graphlite map --role leaf           # worker nodes, terminal callees
graphlite map --all                 # include private symbols
```

### 2. Find — when you have a name fragment

```bash
graphlite symbols "parse*"
graphlite symbols "auth" --language rust
```

FTS5 full-text search. Returns names, kinds, files, node IDs, and signatures. Typical cost: **50–200 tokens**.

### 3. Inspect — 1–2 symbols identified by map or find

```bash
graphlite context sym:NAME
```

Combines the symbol's call graph with its blast radius in one `<context>` document. The `total_tokens` attribute tells you the cost before you've used it for anything. Control cost precisely:

```bash
graphlite context sym:NAME --max-snippet-lines 20    # truncates long bodies; ~1500 tokens
graphlite context sym:NAME --no-snippets             # structure only; ~400 tokens
graphlite context sym:NAME --depth 2 --blast-depth 2 # wider graph
```

For structure-only work (refactoring, renaming, tracing dependencies), `--no-snippets` gives you roles, signatures, fan-in/out, and the full call graph without any implementation bodies.

### 4. Assess impact before modifying

```bash
graphlite blast-radius sym:NAME
graphlite blast-radius sym:NAME --depth 3
```

Transitively walks everything that depends on the target. Emits 2-line call-site snippets (how each caller uses the target, not what the caller does overall). Use before touching a widely-called function or changing a struct that many modules import.

---

## Commands

### `init`

```bash
graphlite init .
graphlite init . --lsp rust
```

Creates `.graphlite/config.toml` with auto-detected LSP servers, then runs the first full index. Safe to re-run.

### `discover`

```bash
graphlite discover .
```

Incremental re-index. Only re-parses files whose content hash changed. Run after saving edits.

### `symbols`

```bash
graphlite symbols "auth*"
graphlite symbols "parse AND file" --language rust
```

FTS5 search. Supports `*` wildcards, `AND`, `OR`, phrase queries. Returns `<symbols>` XML.

### `graph`

```bash
graphlite graph sym:run
graphlite graph sym:run --depth 2
graphlite graph sym:run --no-snippets
graphlite graph sym:run --max-snippet-lines 20
graphlite graph sym:run --show-trust       # split trusted vs syntactic edges
```

Call graph neighborhood only — cheaper than `context` when blast radius is not needed. Focus node gets full snippet (or truncated at `--max-snippet-lines`); neighbors get signatures plus any stored annotations.

### `blast-radius`

```bash
graphlite blast-radius sym:init_db
graphlite blast-radius sym:Config --depth 3
graphlite blast-radius sym:NAME --no-snippets
```

Transitive dependent chain. Depth 0 means unlimited (capped at 50 internally to prevent cycles).

### `context`

```bash
graphlite context sym:parse_file
graphlite context sym:parse_file --max-snippet-lines 20
graphlite context sym:parse_file --no-snippets
graphlite context sym:parse_file --depth 2 --blast-depth 2
```

Primary deep-inspection command. Check `total_tokens` in the output before deciding whether to truncate. Full snippets on a large function can reach 10k tokens; `--max-snippet-lines 20` typically brings this under 2k.

### `map`

```bash
graphlite map
graphlite map --all                     # include private symbols
graphlite map --top 20                  # show top 20 hotspots (default: 10)
graphlite map --with-docs               # per-symbol doc comments
graphlite map --with-file-docs          # file-level module docs
graphlite map --role orchestrator       # filter by inferred role
graphlite map --all-edges               # rank hotspots by all edges, not just trusted
```

Orientation map. Roles and fan-in values tell you where complexity concentrates before you read any implementation. `--all-edges` is useful in projects without LSP enrichment; by default, hotspot ranking uses only trusted edges to suppress false positives from common method names.

### `annotate` / `annotations`

```bash
graphlite annotate sym:enrich \
  --intent "Enrich graph with LSP-verified call edges" \
  --behavior "Spawns rust-analyzer, walks fn nodes, inserts CALLS_TRUSTED edges" \
  --source manual --confidence 0.9

graphlite annotations           # list all
graphlite annotations --stale   # symbols whose content changed after annotation
```

Annotations persist across re-indexing. They surface automatically in `graph`, `blast-radius`, and `context` output on both focus and neighbor nodes. Staleness detection compares the symbol's stored content hash at annotation time against the current hash.

### `rename` / `diff-rename` / `apply-edits`

```bash
graphlite rename sym:old_name new_name --root .
graphlite diff-rename       # preview as diff without writing to disk
graphlite apply-edits       # apply edits.json to disk atomically
```

LSP-backed rename via rust-analyzer. Computes all affected locations, writes to `edits.json`, applies atomically.

---

## Output format

All query commands write XML to stdout. Diagnostic output goes to stderr.

```xml
<context symbol="run" root_id="66" total_tokens="2391">
  <graph root_id="66" tokens="1843" nodes="9" edges="6">
    <focus>
      <node id="66" name="run" kind="fn" file="./src/discover.rs"
            range="L1-L87" visibility="pub"
            fan_in="2" fan_out="8"
            role="orchestrator" role_confidence="0.76">
        <signature>pub fn run(root: &amp;str, lsp_lang: Option&lt;&amp;str&gt;) -&gt; Result&lt;()&gt;</signature>
        <annotation source="manual" confidence="0.9" stale="false">
          <intent>Entry point for incremental indexing</intent>
          <behavior>Walks source tree, diffs hashes, bulk-inserts, optionally runs LSP</behavior>
        </annotation>
        <snippet>...</snippet>
      </node>
    </focus>
    <neighbors depth="1">
      <node id="12" name="load" kind="fn" file="./src/config.rs"
            range="L24-L36" visibility="pub"
            fan_in="4" fan_out="1"
            edge_type="CALLS" role="utility" role_confidence="0.66"
            signature="pub fn load(root: &amp;str) -&gt; Config"/>
    </neighbors>
  </graph>
  <blast_radius root_id="66" tokens="548" dependent_count="2">
    <focus>...</focus>
    <dependents depth="1">
      <node id="2" name="run" kind="fn" file="./src/init_cmd.rs" ...>
        <snippet>discover::run(root, ...)</snippet>  <!-- call-site only, not full body -->
      </node>
    </dependents>
  </blast_radius>
</context>
```

Key attributes:

| Attribute | Meaning |
|---|---|
| `tokens` | `len(xml) / 4`; actual cost within ~10% |
| `role` | Topology-inferred classification |
| `role_confidence` | Score 0–1; below 0.6 adds `role_uncertain="true"` |
| `edge_type="CALLS_TRUSTED"` | LSP-verified; follows generics and dispatch |
| `edge_type="CALLS"` | Syntactic; may have false positives on common names |
| `fan_in` | Callers; `fan_out` = callees |
| `stale="true"` on annotation | Symbol body changed since annotation was written |

---

## Role inference

Assigned automatically after each index from graph topology metrics (fan-in, fan-out, IO proximity, caller spread).

| Role | Signal | When to reach for it |
|---|---|---|
| `entrypoint` | Zero callers, high fan-out | Top-level command handlers |
| `orchestrator` | High fan-out with some callers | Coordination logic; high context value per token |
| `infra` | High IO proximity, broad caller spread | Persistence, networking, external services |
| `domain` | Mid-range metrics | Core business logic |
| `utility` | Moderate fan-in, low fan-out | Shared helpers |
| `leaf` | Low fan-out, called by many | Terminal workers; understand through callers |
| `model` | Struct or enum | Data shape; usually readable from signature alone |

`orchestrator` and `infra` nodes give the most structural insight per token. `leaf` and `model` nodes are often fully described by their signature and rarely need a full `context` query.

---

## Configuration

`.graphlite/config.toml` (created by `init`, safe to commit):

```toml
ignore = []        # glob patterns to exclude
lsp    = ["rust"]  # language servers; auto-detected by init
depth  = 2         # default traversal depth for graph/context
```

`.graphlite/codegraph.db` is SQLite and should be in `.gitignore` (init adds it automatically).

---

## Database

| Table | Contents |
|---|---|
| `nodes` | name, kind, file, line range, signature, visibility, content hash, role |
| `edges` | from_id, to_id, edge_type, source, confidence |
| `files` | per-file content hashes for incremental indexing |
| `annotations` | intent, behavior, tags, source, confidence, hash-at-annotation |
| `fts_symbols` | FTS5 virtual table over node names |

---

## For agents

### CLAUDE.md / agents.md snippet

Paste this block into any project's `CLAUDE.md` (or `agents.md`, `.cursorrules`, or equivalent) after running `graphlite init`:

```markdown
## Code navigation — graphlite is indexed

This project has a `.graphlite/` directory. Use graphlite commands instead of reading
source files. Raw reads cost 5-15x more tokens and lack role, fan-in, trust-tier, and
annotation metadata.

### Workflow — follow in order, escalate only when the step above was insufficient

**Step 1 — Orient (always start here)**
  graphlite map                                      # ~800-2000 tokens
  graphlite map --with-docs --with-file-docs         # include doc comments
  graphlite map --role orchestrator                  # filter by architectural role

The <hotspots> block names the symbols where behaviour concentrates. Start there.
Roles: entrypoint, orchestrator, infra, domain, utility, leaf, model.
orchestrator and infra nodes yield the most structural insight per token.
leaf and model nodes are usually fully described by their signatures.

**Step 2 — Search (when you have a name fragment)**
  graphlite symbols "name*"                          # FTS5; ~50-200 tokens

**Step 3 — Inspect (1-2 symbols from step 1 or 2)**
  graphlite context sym:NAME --max-snippet-lines 20  # ~1500 tokens
  graphlite context sym:NAME --no-snippets           # ~400 tokens, structure only
  graphlite context sym:NAME                         # full; check total_tokens attribute first

**Step 4 — Assess impact (before any modification)**
  graphlite blast-radius sym:NAME                    # transitive dependents

### Token budget reference

  context (full snippets)           2000–12000 tokens
  context --max-snippet-lines 20    ~1500 tokens
  context --no-snippets             ~400 tokens
  map                               800–2000 tokens
  blast-radius                      500–3000 tokens
  symbols search                    50–200 tokens

### When to read source files directly

Only when graphlite cannot answer: generated code, config files, templates, or when
the truncated snippet is demonstrably insufficient and the symbol is critical.
Always prefer graphlite graph sym:NAME over reading the whole file.
```

---

## Development

```bash
cargo build --release
cargo test --test e2e                                             # non-LSP tests (fast)
GRAPHLITE_LSP_TESTS=1 cargo test --test e2e -- --test-threads=1  # include LSP tests
```

E2E tests invoke the release binary directly — build first.

```bash
RUST_LOG=warn  graphlite discover .   # default: warnings only
RUST_LOG=info  graphlite discover .   # indexing progress
RUST_LOG=debug graphlite discover .   # LSP message traffic
```
