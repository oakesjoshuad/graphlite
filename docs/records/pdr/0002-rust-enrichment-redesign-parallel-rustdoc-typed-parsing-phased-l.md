---
id: "PDR-0002"
title: "Rust enrichment redesign: parallel rustdoc, typed parsing, phased LSP evaluation"
record-type: pdr
status: approved
revision: 3
date: 2026-08-21
slug: rust-enrichment-redesign-parallel-rustdoc-typed-parsing-phased-l
tags: []
relationships:
  derived-from:
    - "RFC-0002"
  implemented-by:
    - "EDR-0002"
  produces:
    - "EDR-0002"
---

# PDR-0002: Rust enrichment redesign: parallel rustdoc, typed parsing, phased LSP evaluation

## Problem

cli/src/rustdoc_enricher.rs's enrich_crates runs `cargo +nightly rustdoc --output-format json` once per affected crate, strictly serially (a plain for loop), even though this codebase already depends on rayon and uses it for the tree-sitter parse stage in discover.rs -- felt directly and repeatedly by this project's own maintainer on multi-crate workspaces. Separately, apply_json parses rustdoc JSON as raw serde_json::Value rather than through a typed schema, so a future nightly rustdoc-json format change surfaces as a silently-None .get()/.as_str() chain rather than a compiler error. A feasibility spike (RFC-0002-EV-001, 2026-08-21) additionally found that the codebase's existing lsp_client.rs (used today only for interactive `rename`) can plausibly source two of rustdoc's three signals (qualified_name, trait_impl) with comparable fidelity, but rust-analyzer's readiness signal (`experimental/serverStatus{quiescent:true}`) did not fire within 170s in that spike, and the third signal (visibility) turns out to already be available for free from tree-sitter's existing extract_visibility (parser.rs:184), which rustdoc_enricher.rs currently overwrites unconditionally with no NULL-guard.

## Requirements

enrich_crates wall-clock time must improve on multi-crate workspaces, without changing per-crate failure semantics (a single crate's rustdoc/parse failure must still degrade to a warn+skip, not a hard stop, per PDR-0001's documented failure_modes). rustdoc JSON parsing must become resilient to schema drift at compile time rather than failing silently at runtime. qualified_name/visibility/trait_impl fidelity must not regress. The visibility overwrite bug found in the spike must be fixed regardless of which enrichment source is used going forward. Any LSP-based sourcing must not be adopted until it is demonstrably as reliable as the current rustdoc-JSON path, not merely as fast in one favorable spike run -- efficiency gains are only acceptable where correctness is verified first, not assumed.

## Constraints

rusqlite::Connection is !Send -- a parallel enrichment stage cannot hold or share a Connection across worker threads; any parallel stage must be pure computation only, with all DB writes serialized afterward. Cargo's own target-directory locking behavior when multiple `cargo +nightly rustdoc` invocations run concurrently against one shared --target-dir is unverified in this session (RFC-0002's own open question 2, still unresolved) -- Phase 1 implementation must not assume parallel invocations actually run concurrently against a shared target dir without first measuring it. LSP-based sourcing (Phase 2) is constrained by the unresolved quiescent-signal reliability gap surfaced in RFC-0002-EV-001 and must not ship until that gap is understood. Nightly toolchain dependency and rustdoc JSON format-version fragility are pre-existing and out of this PDR's scope to remove.

## Proposed Design

Two phases, deliberately not committed to in the same implementation pass, per RFC-0002's alternatives (a)/(b)/(c) being independently valuable rather than a single bundled change.

PHASE 1 -- parallel rustdoc + typed parsing (ready to implement once the two open experiments below are run):
1. Hoist build_node_map out of the per-crate loop: build it once from a single `SELECT id, file, range_start FROM nodes` query before enrichment starts, wrapped in an Arc for shared read-only access across worker threads (it is currently rebuilt, wastefully, inside every per-crate apply_json call).
2. Split run_and_apply/apply_json into a pure Send-safe stage (run_cargo_rustdoc + JSON parse + node_map lookup, producing an in-memory CrateEnrichment{node_updates, impl_trait_edges} value per crate, no DB access) and a DB-application stage. Run the pure stage via `crates.par_iter().map(...).collect::<Vec<_>>()`, mirroring discover.rs's existing `.par_iter()` tree-sitter parse stage (src/discover.rs:154). A crate whose cargo invocation or JSON parse fails produces an Err, degraded to a warn+skip exactly as today's for-loop does -- this preserves PDR-0001's documented failure_modes exactly, just via rayon's map/collect instead of a for loop's match arm.
3. Apply all collected CrateEnrichments to the database serially, in one transaction, on the calling thread -- the same parallel-compute/serial-single-transaction-commit pattern already established elsewhere in this codebase.
4. Adopt the rustdoc-types crate: replace apply_json's raw serde_json::Value navigation with typed deserialization into rustdoc_types::Crate. Field access becomes compiler-checked; an upstream schema break becomes a compile error instead of a silently-None access chain. Keep the existing EXPECTED_FORMAT_VERSION runtime assertion as a belt-and-suspenders check -- rustdoc-types targets a version range, not an exact pin, so the runtime assertion still catches drift within that range that the type definitions alone would not.
5. Fix the visibility overwrite bug found in the spike: stop letting rustdoc_enricher.rs unconditionally clobber a value tree-sitter's extract_visibility (parser.rs:184) already computed for free on every discover. Decide during implementation whether rustdoc's finer-grained `restricted::path` form is worth retaining as a secondary enrichment only when tree-sitter's raw modifier text can't already express the same distinction (it usually can, since tree-sitter captures the exact `pub(...)` text verbatim from source).

PHASE 2 -- LSP-based qualified_name/trait_impl sourcing, explicitly deferred and not committed to by this PDR:
Not to be implemented until the RFC-0002-EV-001 quiescent-signal gap is understood -- either the actual readiness condition rust-analyzer needs is found (a different notification, a validated settle-window heuristic, or evidence that per-file documentSymbol/hover requests are safe pre-quiescence for this specific use case), or the gap is shown not to matter at production scale. Visibility sourcing does not carry over to this phase regardless of outcome -- tree-sitter already covers it per Phase 1 step 5. Requires a materially different lsp_client.rs than exists today: the current client is single-in-flight and blocking, built for one interactive rename call; bulk enrichment across many symbols needs request pipelining or must accept O(symbols) sequential round-trips, and either approach needs its own timing validation against rustdoc JSON's per-crate subprocess cost before it can be justified as an improvement rather than a regression. This phase is the eventual path to collapsing tree-sitter+rustdoc+LSP down to tree-sitter+LSP as graphlite's two code-understanding mechanisms, but is not this PDR's committed scope.

## Components

src/rustdoc_enricher.rs (restructured per Phase 1: build_node_map hoisted, run_and_apply split into pure-compute + serial-apply stages, serde_json::Value replaced with rustdoc_types::Crate). Cargo.toml (+rustdoc-types dependency, version verified against this project's pinned nightly's format_version before adoption). src/parser.rs (extract_visibility, line 184 -- referenced, not modified, as the pre-existing source of truth Phase 1 step 5 defers to). src/lsp_client.rs (untouched by Phase 1; Phase 2, if pursued, needs a request-pipelining redesign that is explicitly out of this PDR's scope).

## Interfaces

No CLI-facing interface changes. `graphlite discover`/`graphlite check` behavior is unchanged from the caller's perspective except: faster wall-clock time on multi-crate workspaces (Phase 1, pending the target-dir locking experiment), and more consistent visibility values for private/default-visibility items once the Phase 1 overwrite fix lands.

## Data Model

No schema changes. nodes.qualified_name / nodes.visibility / nodes.trait_impl columns and IMPL_TRAIT edges keep their existing meaning and are populated by the same source="rustdoc" provenance in Phase 1; only the code path producing them changes internally.

## Failure Modes

Preserved from PDR-0001's documented failure_modes: a single crate's cargo/parse failure still degrades to a per-crate warn+skip, not a hard stop -- in Phase 1 this becomes a rayon-mapped Result collected and filtered the same way today's for-loop's `match ... Err(e) => warn!` already does. rustdoc-types deserialization failure on data the current raw-Value parser would have silently tolerated is a new possible failure point; it should degrade the same way (warn+skip that one crate) unless it is a full format_version mismatch affecting every crate, which remains the existing loud bail!.

## Alternatives Considered

Do nothing / accept serial enrich_crates -- rejected, this is the maintainer's own directly-felt problem (RFC-0002's motivation). Ship only Phase 1's parallelization (a) without typed parsing (b) -- leaves the schema-drift maintenance risk unaddressed for no real benefit, since (a) and (b) touch the same function and are cheaper to do together than sequentially. Commit to Phase 2 (LSP sourcing) in the same implementation pass as Phase 1 -- rejected by this PDR specifically: the spike (RFC-0002-EV-001) found a real, unresolved reliability gap (the quiescent readiness signal never fired in a 170s wait) that makes Phase 2 a correctness risk, not just a speed opportunity, if rushed into the same pass as two independently low-risk changes. Phasing it separately lets Phase 1 ship without waiting on Phase 2's resolution, and keeps "efficient" from being pursued at the expense of "correct."

## Evidence

RFC-0002-EV-001 (this repo's own feasibility spike, 2026-08-21, evidence attached to RFC-0002) -- hover/documentSymbol fidelity comparison against cached rustdoc JSON, and the visibility-overwrite finding. Confirmed by direct code reading: discover.rs already uses rayon::prelude::* and .par_iter() (src/discover.rs:154) for its tree-sitter parse stage, establishing the parallel-compute/serial-commit pattern Phase 1 reuses. Confirmed enrich_crates (src/rustdoc_enricher.rs:127-134) is currently a plain serial for loop. Confirmed src/parser.rs:184's extract_visibility already extracts a raw visibility_modifier string from the tree-sitter AST on every discover, independent of rustdoc.

## Experiments

Two experiments are required before/during Phase 1 implementation and have not yet been run: (1) time two or more concurrent `cargo +nightly rustdoc` invocations against one shared --target-dir on a real multi-crate workspace, to determine whether cargo's own build-lock serializes them anyway -- this determines whether Phase 1's parallelization needs per-crate target dirs instead, trading away the compiled-dependency reuse the shared dir was originally chosen for (RFC-0002's open question 2, still unresolved by this PDR). (2) time enrich_crates end-to-end on a workspace of the size that originally motivated RFC-0002, to get a real before/after wall-clock comparison -- this repo's own single small crate (1.9s warm-cache rustdoc run, measured during the RFC-0002 spike) is not representative of the felt multi-crate problem this PDR exists to address.

## Risks

Phase 1's split of run_and_apply into pure-compute and serial-apply stages is a real refactor, not a trivial wrapper -- risk of subtly changing behavior if any ordering dependency exists between crates in the current per-crate flow (e.g. if a later crate's rustdoc run were ever found to depend on an earlier crate's node having just been inserted by build_node_map -- hoisting node_map to a single upfront snapshot would break that; this has not been verified absent in the current code and should be checked before implementation, not assumed). rustdoc-types adoption risk: must verify the crate's supported format_version range actually covers this project's EXPECTED_FORMAT_VERSION=57 before starting the migration, not discover a mismatch mid-implementation. Phase 2's risks are covered under alternatives/constraints and are the reason it is not in this PDR's committed scope.

## Open Questions

1. Cargo target-dir locking behavior under concurrent rustdoc invocations -- unresolved, needs experiment (see `experiments`) before Phase 1 lands. 2. Whether rustdoc-types' supported format_version range covers this project's pinned nightly toolchain's actual output -- needs verification before adoption. 3. The quiescent-signal reliability gap from RFC-0002-EV-001 -- needs investigation before Phase 2 can be considered at all. 4. Whether to retain rustdoc's finer-grained restricted::path visibility form as a secondary enrichment once tree-sitter's raw modifier text becomes the primary source, or drop rustdoc from the visibility signal entirely.

## Resulting Decisions

This PDR proposes Phase 1 (parallelize enrich_crates + adopt rustdoc-types + fix the visibility overwrite bug) as the concrete next implementation step, contingent on first resolving the two open experiments listed above -- not yet approved for implementation as of this draft. Phase 2 (LSP-based qualified_name/trait_impl sourcing) is deliberately deferred, not rejected -- to be revisited as its own follow-up record once the quiescent-signal reliability gap is understood, rather than implemented speculatively alongside Phase 1's lower-risk changes.
