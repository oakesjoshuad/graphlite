---
id: "EDR-0004"
title: "Invalidate cached file symbols on parser-version change, not just content-hash change"
record-type: edr
status: draft
revision: 1
date: 2026-08-30
slug: invalidate-cached-file-symbols-on-parser-version-change-not-just
tags: []
relationships: {}
---

# EDR-0004: Invalidate cached file symbols on parser-version change, not just content-hash change

## Context

Confirmed directly (2026-08-30) via an adjacent consumer, Strata: `discover`'s reindex decision (src/discover.rs, comparing `compute_file_hash(path)` against the `files.file_hash` column) is keyed only to file content. When graphlite's own extraction logic changes -- e.g. today's fix adding const_item/static_item recognition -- a file whose content has not changed keeps its old, incomplete cached symbols indefinitely, even after `graphlite discover` is re-run with the upgraded binary. The natural, expected remedy ("just run discover again") silently does nothing; the only way to pick up the new capability is to delete `.graphlite/codegraph.db` and rebuild from scratch, which is undocumented and non-obvious. This was discovered the hard way: after installing the const/static-indexing fix and re-running `graphlite discover` on Strata's own repository, `PUBLISH_RENDERER_DEFAULT` still failed to resolve until the database was deleted and rebuilt -- the fix was correct, but the cache didn't know to stop trusting its old results.

`open_or_init_db` (src/schema.rs) already has a precedent for this class of problem: a series of best-effort `ALTER TABLE ... ADD COLUMN` statements (each wrapped in `let _ = conn.execute(...)`, tolerating the already-exists error) that evolve an existing database's schema in place without a formal migration framework, since `codegraph.db` is a fully regenerable index rather than canonical data.

## Decision

Add a `parser_version INTEGER NOT NULL DEFAULT 0` column to the `files` table, following the exact idempotent `ALTER TABLE ADD COLUMN` pattern already used in `open_or_init_db` for `doc`, `role`, `role_confidence`, and `complexity`. Define a single `const PARSER_VERSION: i64` (co-located with `compute_file_hash` in src/discover.rs, or in src/parser.rs next to `kind_from_node`), with a doc comment instructing maintainers to bump it whenever a change to queries/*.scm or kind_from_node could change what symbols get extracted from unchanged file content. Set it to `1` at introduction.

Extend the reindex-decision query in discover.rs (currently `SELECT file_hash FROM files WHERE path = ?1`, compared only against the freshly computed content hash) to also select `parser_version`, and change the filter to mark a file as changed when the stored `file_hash` differs from the new hash OR the stored `parser_version` differs from the current `PARSER_VERSION` constant. Extend `upsert_file_hash` (src/insert.rs) to also write the current `PARSER_VERSION` alongside `file_hash` and `doc` on every upsert.

Because existing databases' pre-existing rows get `parser_version = 0` from the column default, and the introduced constant starts at `1`, every file is correctly treated as changed and gets one full, automatic reprocess the first time `discover` runs after this change ships -- with no separate migration step, no manual database deletion, and no special-casing for "first run after upgrade" versus any other run.

## Considered Options

Tie invalidation to the crate's own `CARGO_PKG_VERSION` instead of a dedicated constant -- rejected: checked graphlite's own git history and confirmed version bumps are not disciplined relative to parser/query changes (several commits changing extraction-relevant code, including today's, did not bump Cargo.toml's version); a dedicated constant used for exactly one purpose is a clearer, more reliable signal than overloading semver, which changes for reasons unrelated to parsing (CLI UX, dependency bumps, unrelated features).

Always perform a full reindex on every `discover` invocation, dropping the content-hash shortcut entirely -- rejected: this throws away the real, valuable efficiency win of incremental indexing for the overwhelmingly common case (nothing changed), to fix a rare event (the graphlite binary itself was upgraded). The fix should cost nothing on ordinary runs and only trigger extra work exactly when the tool's own capabilities changed.

Document the manual `rm .graphlite/codegraph.db && graphlite discover` workaround instead of fixing it in code -- rejected: this is the status quo that caused today's confusion, is easy to forget, and burdens every future graphlite upgrade with a manual step a tool should handle for its own users automatically.

Store the version as a single global row (a `schema_meta` table) rather than per-file -- considered, and simpler in one sense (one comparison instead of one per file), but rejected because it forces an all-or-nothing full reindex on any version bump, even one that only affects a subset of node kinds or one language's query file; a per-file column allows (in principle, not required by this decision) a future refinement where only affected languages or files are invalidated, without revisiting the schema again.

## Consequences

The very next `graphlite discover` run after installing an upgraded graphlite binary will transparently and fully reprocess the entire indexed tree exactly once, then return to normal incremental behavior -- no documentation to remember, no manual database deletion. Ordinary runs (unchanged binary, unchanged files) pay one extra integer comparison per file, negligible relative to the existing hash computation and query execution already happening. `files.parser_version` becomes a second piece of state a future contributor must remember to consider alongside `file_hash` in `upsert_file_hash` and the reindex-decision query -- both single, well-known call sites, so the risk of forgetting is low but not zero; the doc comment on `PARSER_VERSION` is the primary safeguard against forgetting to bump it. This does not address the separate, distinct integration gap already found the same day in Strata's own `symbol_locator.rs` (it calls `graphlite symbols`, a raw full-text search, where `graphlite resolve --prefer-file` would already rank and disambiguate correctly) -- that fix belongs in Strata, not here, and is out of scope for this record.

## Evidence

Direct, dated incident (2026-08-30): after implementing and installing graphlite's const/static-item indexing fix (queries/rust.scm, src/parser.rs), `graphlite resolve PUBLISH_RENDERER_DEFAULT` against Strata's own repository continued to return zero candidates until `.graphlite/codegraph.db` was deleted and `graphlite discover` re-run from scratch, at which point it resolved cleanly (candidates="1", kind="const"). Confirmed the root cause directly: `graphlite symbols PUBLISH_RENDERER_DEFAULT --file cli/src/config.rs` returned zero matches against the stale, incrementally-reindexed database despite the file's content and the installed binary both being correct; a fully fresh index of the same file with the same binary found it immediately. src/schema.rs's `open_or_init_db` (lines ~123-145) confirms the existing idempotent-ALTER-TABLE precedent this decision extends. src/discover.rs's reindex filter (~lines 105-120) and src/insert.rs's `upsert_file_hash` (~lines 52-64) are the two exact call sites this decision modifies.
