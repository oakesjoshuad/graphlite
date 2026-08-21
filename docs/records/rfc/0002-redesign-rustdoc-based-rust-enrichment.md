---
id: "RFC-0002"
title: "Redesign rustdoc-based Rust enrichment"
record-type: rfc
status: draft
revision: 2
date: 2026-08-21
slug: redesign-rustdoc-based-rust-enrichment
tags: []
relationships:
  relates-to:
    - "PDR-0001"
---

# RFC-0002: Redesign rustdoc-based Rust enrichment

## Motivation

Rustdoc-based enrichment (qualified names, visibility, trait-impl edges) is the one stage of the indexing pipeline that is felt, directly and repeatedly, as slow -- reported independently by this project's own maintainer, not just observed as a theoretical concern. It is also the pipeline's most fragile stage: it depends on the nightly toolchain and an explicitly unstable JSON format, and reconciles its own identity space (rustdoc spans) against tree-sitter's (parsed node ranges) via best-effort, multi-form file-path string matching.

## Problem

cli/src/rustdoc_enricher.rs's enrich_crates runs `cargo +nightly rustdoc --output-format json` once per affected crate, strictly serially (a plain for loop), even though this codebase already depends on rayon and uses it for the tree-sitter parse stage in discover.rs. Each invocation is a real subprocess: toolchain startup, analysis, and JSON serialization, not just a cache hit, even with a shared --target-dir. On a multi-crate workspace this is very likely the dominant cost of a full re-index. Separately, apply_json parses the rustdoc JSON as raw serde_json::Value rather than through the rustdoc-types crate (not a dependency here) -- it does correctly assert format_version against an EXPECTED_FORMAT_VERSION constant and fails loudly on mismatch, which is the right defensive move, but every future nightly rustdoc-json schema change still requires manually re-verifying each ad hoc .get()/.as_str()/.as_object() access chain by hand rather than getting a compiler error from a typed struct.

## Scope

cli/src/rustdoc_enricher.rs and its call sites in discover.rs. Three concrete questions to resolve: (1) should enrich_crates parallelize its per-crate cargo invocations the way discover.rs already parallelizes tree-sitter parsing; (2) should apply_json's raw serde_json::Value parsing be replaced with the rustdoc-types crate for typed, compiler-checked access; (3) should rustdoc JSON remain a separate enrichment mechanism at all, or could the LSP client already present in this codebase (lsp_client.rs, currently used for the `rename` command) absorb what rustdoc JSON currently provides -- collapsing three code-understanding mechanisms (tree-sitter, rustdoc JSON, LSP) to two.

## Non-Goals

This RFC does not propose changing the tree-sitter parsing stage, the use-statement resolver, role inference, or any query-command output format. It does not extend language support. It does not itself correct the stale README.md line calling `rename` deprecated (see `alternatives` -- resolved 2026-08-21: the line is factually wrong and should be fixed, but that is a one-line documentation correction independent of this RFC's scope, not a blocker for it).

## Constraints

Rustdoc-derived data (qualified_name, visibility, trait_impl/IMPL_TRAIT edges) must remain available at the same fidelity for Rust projects; this is a performance/robustness/architecture redesign, not a feature reduction. Whatever replaces or restructures the current mechanism must still degrade to a per-crate warning-and-skip on failure, matching the existing failure_modes documented in PDR-0001, not a hard stop for the whole index.

## Proposal

Not yet settled -- this RFC exists to get the three scope questions reviewed before a PDR commits to one answer. The evidence and one plausible reading are laid out under `alternatives`.

## Alternatives Considered

(a) Parallelize enrich_crates as-is (rayon .par_iter() over crates, mirroring discover.rs) -- lowest-risk, addresses the concretely-felt slowness directly, does not touch the JSON-parsing or dual-identity-space concerns. (b) Adopt the rustdoc-types crate for typed parsing -- reduces schema-drift maintenance burden, does not address serial execution or the tree-sitter/rustdoc identity-reconciliation fragility (span_file_match_keys' four match-form normalization, four dedicated tests). (c) Investigate replacing rustdoc JSON with the existing LSP client (lsp_client.rs) as the single semantic-enrichment source -- would remove one of three code-understanding mechanisms entirely and might sidestep the identity-reconciliation problem if the LSP path can address symbols the same way tree-sitter does. RESOLVED 2026-08-21: the blocking question over whether lsp_client.rs is actively maintained or scheduled for removal is settled -- it is actively maintained. `rename` is a live, working command (main.rs Commands::Rename, dispatched through `graphlite watch`'s IPC to a persistent rust-analyzer client in lsp_client.rs), rebuilt by commit 12695cc ('feat: rename via rust-analyzer LSP through watch daemon IPC', 2026-04-05) -- eight commits and one week after 61ec423 (2026-03-29) added the README.md line stating rename 'is no longer implemented in CLI.' That README line is stale documentation debt, not the accurate forward direction; it was never updated after rename was rebuilt on the watch-daemon IPC path. Alternative (c) is therefore not foreclosed and is worth evaluating on its technical merits -- the open question is now purely technical, not organizational: can rustdoc JSON's three signals (qualified_name, visibility, trait_impl/IMPL_TRAIT edges) be obtained through the same persistent rust-analyzer LSP client rename already uses, without regressing fidelity or reintroducing the identity-reconciliation fragility in a different form. These three alternatives are not mutually exclusive -- (a) and (b) are compatible with either doing or not doing (c).

## Open Questions

1. [RESOLVED 2026-08-21] Is lsp_client.rs actively maintained, or is README.md's deprecation note the accurate forward direction? Actively maintained: rebuilt in 12695cc (2026-04-05) to back the live `rename` command via `graphlite watch`'s IPC, a week after the README.md line (added in 61ec423, 2026-03-29) that calls it deprecated -- that line is stale, not descriptive of current intent. This unblocks alternative (c) for consideration. 2. Is parallelizing enrich_crates (alternative a) safe given the shared --target-dir across crates -- does cargo's own locking make concurrent `cargo +nightly rustdoc` invocations against one target dir safe, or would parallelizing require per-crate target dirs (losing the current compiled-dependency reuse the shared dir was chosen for)? 3. Would adopting rustdoc-types (alternative b) meaningfully reduce the four-form path-matching fragility, or is that fragility inherent to reconciling any two independently-computed identity spaces regardless of how the JSON is parsed? 4. If alternative (c) is pursued: can the LSP client obtain qualified_name/visibility/trait_impl signals at the same fidelity rustdoc JSON provides today, and would it still support the per-crate warning-and-skip failure mode required by `constraints`?

## Outcome

Draft. Written directly from a real, verified investigation of this codebase in the same session that backfilled PDR-0001 -- not yet reviewed by a maintainer. No implementation has started.
