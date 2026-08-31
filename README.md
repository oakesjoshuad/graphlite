# graphlite

Token-metered, role-annotated code context for LLMs — a SQLite-backed graph that serves focused XML/Markdown snapshots instead of raw source files.

## Unreleased

- Rustdoc-enriched named types emitted by macro invocations are now indexed at their invocation source span, so they can be resolved by their source-level name. Their local trait implementations can now produce `IMPL_TRAIT` edges after the generated type is stored. When rustdoc provides no source span, discovery now emits an actionable warning instead of silently omitting the type.

---

## Pipeline

```
Source files
    │
    ├── tree-sitter (parallel parse) ──► symbols + syntactic CALLS edges
    │                                    source="tree-sitter", confidence=0.8
    │
    ├── rustdoc JSON (cargo +nightly rustdoc) ──► qualified_name,
    │                                              trait_impl + IMPL_TRAIT
    │                                              source="rustdoc"
    │                                              (visibility is tree-sitter-owned)
    │
    └── use-statement resolver ─────────► CALLS_RESOLVED edges
                                         source="resolver", confidence_tier=high|medium|low
                                                    │
                                         SQLite (FTS5 search, graph queries)
                                                    │
                                         Role inference (topology analysis)
                                                    │
                                         XML/Markdown output with token counts
```

Re-indexing is incremental: only files whose content hash changed are re-parsed.

---

## Supported languages

| Language   | Symbols | Call edges | Semantic enrichment |
|------------|---------|------------|---------------------|
| Rust       | yes     | yes        | rustdoc + resolver  |
| TypeScript | yes     | yes        | —                   |
| JavaScript | yes     | yes        | —                   |
| Svelte     | yes     | yes        | —                   |
| HTML       | yes     | —          | —                   |
| CSS        | yes     | —          | —                   |

---

## Installation

```bash
cargo build --release
# binary: ./target/release/graphlite
```

---

## Commands

```bash
graphlite init .
graphlite discover .
graphlite map
graphlite symbols "parse*"
graphlite symbols "*segment*" --context sales --file opportunities
graphlite deps
graphlite graph sym:run
graphlite graph sym:run --budget-lines 25 --offset 0
graphlite trace-path sym:src/discover.rs::fn::run --direction outgoing
graphlite trace-path sym:src/discover.rs::fn::run --direction write
graphlite blast-radius sym:open_db
graphlite blast-radius sym:open_db --budget-tokens 600 --compact
graphlite context sym:parse_file
graphlite context sym:parse_file --budget-lines 20 --offset 20
graphlite check .
graphlite audit .
graphlite violations --strict-policy
graphlite violations --by-crate
graphlite violations --edge "from:adapter to:domain"
graphlite violations --check
graphlite violations --audit
graphlite policy init-pack ddd-hexagonal-rust
graphlite policy lint
graphlite policy lint --stale
graphlite policy lint --fail-on-stale --fail-on-broad
graphlite reclassify .
graphlite capabilities --json
```

Notes:
- `map`, `symbols`, `deps`, and `resolve` support `--md`; `graph`, `blast-radius`, and `context` do not (XML only).
- `trace-path --direction` accepts `outgoing|incoming|both` plus aliases `write|read`.
- `graph`, `blast-radius`, and `context` support deterministic output windows via `--budget-lines`, `--budget-tokens`, and `--offset`; truncated output includes `next_offset` for exact resume.
- `--compact` on `graph`/`blast-radius`/`context` defers heavy payloads (docs/snippets/annotations) for lower-token passes.
- `symbols` supports scope narrowing via `--file`, `--context`, and `--crate-name`; infix wildcard patterns like `*segment*` use non-FTS fallback.
- `resolve` supports `--prefer-role` and `--prefer-file` for colliding names.
- `trace-path --high-level` collapses utility/leaf-heavy hops for endpoint-to-core flow review.
- `discover` now runs clippy (`check`) and advisory (`audit`) enrichment automatically in best-effort mode.
- `check` and `audit` remain available for on-demand refreshes between discovers.
- `policy init-pack` sets `[policy].pack` in `.graphlite/config.toml`; custom rules in `[[violations]]`, `[[exceptions]]`, `[[visibility_rules]]`, and `[workspace]` still apply as local overrides.
- `policy lint --stale` reports unused suppression rules; `--fail-on-stale` / `--fail-on-broad` support CI gating.
- `rename` performs semantic rename via rust-analyzer, routed through a running `graphlite watch` daemon (`graphlite watch .` first, then `graphlite rename sym:X new_name`, then `graphlite diff-rename` to preview and `graphlite apply-edits` to apply).

---

## Policy Pack Migration

If you already maintain custom policy blocks in `.graphlite/config.toml`, migrate incrementally:

1. Set a baseline pack: `graphlite policy init-pack ddd-hexagonal-rust` (or `cqrs-event-sourced-rust`).
2. Keep existing custom `[[violations]]`, `[[exceptions]]`, `[[visibility_rules]]`, and `[workspace]` sections; these are treated as local overrides/extensions.
3. Run `graphlite policy lint` to detect duplicate/conflicting or ineffective rules.

---

## Configuration

`.graphlite/config.toml`:

```toml
ignore = []
depth = 2
```

Workspace mode also supports:

```toml
[workspace.layers]
crate_a = "application"
crate_b = "domain"

[[workspace.violations]]
from_layer = "adapter"
to_layer = "domain"
reason = "adapter must not depend on domain directly"
```

---

## Output semantics

Key edge/source meanings:

| Field | Meaning |
|---|---|
| `edge_type="CALLS"` + `source="tree-sitter"` | Syntactic call edge |
| `edge_type="CALLS_RESOLVED"` + `source="resolver"` | DB-resolved semantic edge |
| `edge_type="IMPL_TRAIT"` + `source="rustdoc"` | Trait implementation link |
| `confidence_tier` | `high`, `medium`, `low` on resolver edges |

All query output includes token estimates so agents can control context budget.

`graphlite check` stores clippy diagnostics in `node_diagnostics` and updates `nodes.complexity` from `clippy::cognitive_complexity` when present.

---

## Development

```bash
cargo check
cargo clippy -- -D warnings
cargo test --test e2e
```

Optional logging:

```bash
RUST_LOG=warn  graphlite discover .
RUST_LOG=info  graphlite discover .
RUST_LOG=debug graphlite discover .
```
