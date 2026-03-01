---
description: >
  Use this skill when working in a codebase that may have a .graphlite/ index, or when
  the user asks to explore, navigate, understand, or reason about code structure. Trigger
  phrases include: "explore the codebase", "understand this code", "orient in the repo",
  "map the architecture", "what calls X", "what does X do", "find the symbol for",
  "show me what depends on", "assess the impact of changing", "what's the blast radius",
  "how is X used", "where is X defined", "trace the call graph", "understand the structure",
  "what imports X", "show me callers of", "help me navigate this", "get context for".
  Also trigger when the user mentions token budget concerns while exploring code.
argument-hint: [sym:NAME or question about code structure]
---

# graphlite — Code Context Workflow

## Before anything else

Check whether this project has a graphlite index:

```bash
ls .graphlite/codegraph.db 2>/dev/null && echo "indexed" || echo "not indexed"
```

If not indexed and the user wants to work on this project:

```bash
graphlite init .      # first-time setup: detects LSP, indexes everything
graphlite discover .  # re-index after code changes
```

If the project is not indexed and cannot be indexed now, proceed with direct file reads but note the token overhead.

---

## Workflow — follow in order, escalate only when the step above was insufficient

### Step 1: Orient (always start here — run this before any other query)

```bash
graphlite map
graphlite map --with-docs --with-file-docs    # maximum signal, ~2000 tokens
```

Read the output carefully before proceeding:

- `<hotspots>` — top symbols by trusted caller count; where behaviour concentrates
- `fan_in` — how many things call this symbol; high = widely depended on
- `fan_out` — how many things this symbol calls; high = coordinating role
- `role` — topology-inferred classification (see table below)
- `tokens="..."` — your cost for this query, precisely

Narrow with role filters when you know the architectural layer:

```bash
graphlite map --role orchestrator   # coordination logic; high context value per token
graphlite map --role infra          # IO, persistence, external services
graphlite map --role entrypoint     # top-level command handlers
graphlite map --role leaf           # terminal workers; understand through their callers
graphlite map --all                 # include private symbols when you need impl detail
```

Role reference:

| Role         | Signal                             | Query strategy                        |
|--------------|------------------------------------|---------------------------------------|
| entrypoint   | Zero callers, high fan-out         | context to see what it orchestrates   |
| orchestrator | High fan-out, some callers         | context; highest insight per token    |
| infra        | High IO, broad caller spread       | context; often the critical path      |
| domain       | Mid-range metrics                  | context when business logic is needed |
| utility      | Moderate fan-in, low fan-out       | signature usually sufficient          |
| leaf         | Low fan-out, called by many        | blast-radius to see usage patterns    |
| model        | Struct or enum                     | signature usually sufficient          |

### Step 2: Search (when you have a name or fragment)

```bash
graphlite symbols "name*"                   # FTS5; * wildcard, AND, OR supported
graphlite symbols "parse" --language rust   # filter by language
```

Returns names, kinds, files, node IDs, signatures. Cost: 50–200 tokens.

### Step 3: Inspect (1–2 symbols identified in steps 1 or 2)

Check `total_tokens` in the output before deciding on depth:

```bash
# Start here — truncated snippets are usually sufficient
graphlite context sym:NAME --max-snippet-lines 20    # ~1500 tokens

# If structure is all you need (refactoring, renaming, tracing)
graphlite context sym:NAME --no-snippets             # ~400 tokens

# Full detail — only after confirming the symbol warrants it
graphlite context sym:NAME                           # 2000-12000 tokens; check total_tokens

# Wider graph when neighbors are not self-explanatory
graphlite context sym:NAME --depth 2 --blast-depth 2
```

For structural work (tracing call paths, dependency analysis, refactoring), always prefer `--no-snippets` first. Escalate to snippets only when you need to understand implementation logic.

Annotations in the output (when present) are curated human or LLM notes:

```xml
<annotation source="manual" confidence="0.9" stale="false">
  <intent>Entry point for incremental indexing</intent>
  <behavior>Walks source tree, diffs hashes, bulk-inserts, optionally runs LSP</behavior>
</annotation>
```

`stale="true"` means the symbol's body changed since the annotation was written — treat with lower confidence.

### Step 4: Assess impact (before modifying anything)

```bash
graphlite blast-radius sym:NAME             # who depends on this, transitively
graphlite blast-radius sym:NAME --depth 3   # wider search
```

Emits call-site snippets (2 lines around each call) — how callers use the target, not what the callers do. Run this before modifying any symbol with `fan_in > 2`.

---

## Token budget reference

| Command                              | Typical tokens     |
|--------------------------------------|--------------------|
| `map`                                | 800–2000           |
| `map --with-docs --with-file-docs`   | 1500–4000          |
| `symbols "query"`                    | 50–200             |
| `context sym:NAME --no-snippets`     | ~400               |
| `context sym:NAME --max-snippet-lines 20` | ~1500         |
| `context sym:NAME` (full)            | 2000–12000         |
| `blast-radius sym:NAME`              | 500–3000           |
| `graph sym:NAME --no-snippets`       | ~300               |

When total context exceeds your working budget, prefer:
1. `--no-snippets` on context/graph — structure without implementation
2. `--max-snippet-lines 20` on context — capped bodies
3. Multiple focused `graph` queries over one broad `context`

---

## Reading source files directly

Only read source files when:
- `.graphlite/` does not exist and `graphlite init` is not feasible
- The symbol is not indexed (generated code, external dependencies)
- The file is non-code (config, templates, migrations)
- The snippet in graphlite output is truncated and the full implementation is critical

In all other cases, prefer `graphlite graph sym:NAME` over reading the enclosing file.

---

## Adding annotations after understanding a symbol

When you have understood what a symbol does, annotate it for future queries:

```bash
graphlite annotate sym:NAME \
  --intent "One sentence: what this does" \
  --behavior "Observable effects, side effects, error cases" \
  --source llm --confidence 0.8
```

This annotation will surface automatically in all future `graph`, `context`, and `blast-radius` queries on this symbol and its neighbors.
