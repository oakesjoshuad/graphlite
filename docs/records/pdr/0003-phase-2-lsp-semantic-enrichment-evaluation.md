---
id: "PDR-0003"
title: "Phase 2 LSP semantic-enrichment evaluation"
record-type: pdr
status: approved
revision: 3
date: 2026-08-21
slug: phase-2-lsp-semantic-enrichment-evaluation
tags: []
relationships:
  derived-from:
    - "PDR-0002"
  produces:
    - "EDR-0003"
---

# PDR-0003: Phase 2 LSP semantic-enrichment evaluation

## Problem

PDR-0002 Phase 1 keeps rustdoc JSON as the production source for Rust qualified names and trait-implementation edges, while the existing rust-analyzer client could potentially reduce nightly/toolchain and rustdoc-schema fragility. The prior spike found that the current quiescent readiness signal did not fire within 170 seconds, so adopting LSP sourcing without a stronger validation plan would risk incomplete graph data.

## Requirements

Evaluate, without changing production behavior, whether rust-analyzer can provide qualified_name and IMPL_TRAIT fidelity equal to or better than rustdoc on representative single- and multi-crate workspaces. Establish a repeatable readiness protocol, preserve per-crate warn-and-skip semantics, measure cold and warm wall time, and define explicit fallback behavior. Visibility remains tree-sitter-owned and is not part of this phase.

## Constraints

Do not modify src/lsp_client.rs or production enrichment in this evaluation record. Phase 2 is not considered started by the record itself. The current rustdoc path remains authoritative until parity and reliability are demonstrated. LSP serverStatus is experimental and its quiescent field is documented as user-facing status, not a guaranteed correctness barrier. No schema or CLI changes are authorized by this record.

## Proposed Design

Build a standalone prototype or test harness around the existing LSP framing/client concepts, without wiring it into discover. Start rust-analyzer with serverStatusNotification enabled, collect serverStatus health/quiescent and $/progress events, then issue bounded probes after readiness. For qualified names, compare per-file textDocument/documentSymbol hierarchy and reconstructed module paths against rustdoc/tree-sitter ground truth; do not use workspace/symbol as the complete inventory because rust-analyzer documents fuzzy search, type-oriented defaults, workspace/dependency scopes, and a result limit. For trait edges, compare textDocument/implementation or equivalent implementation responses for representative traits, types, blanket impls, associated methods, and cross-crate cases. Run repeated cold-cache and warm-cache trials on a representative multi-crate workspace. A production proposal may only proceed if all signals meet parity thresholds, readiness is bounded and repeatable, and an LSP failure falls back to the existing rustdoc result or per-crate skip without corrupting committed data.

## Components

Future prototype/evaluation harness; existing src/lsp_client.rs as an inspected but currently unchanged transport; src/rustdoc_enricher.rs and parser.rs as the ground-truth comparison paths; representative Rust workspace fixtures; benchmark reports and Strata evidence. Production integration, if later approved, would require a separate implementation record.

## Interfaces

No current CLI or database interface changes. The eventual prototype may emit a machine-readable comparison report, but graphlite discover/check behavior remains unchanged. Any future production adapter must preserve nodes.qualified_name, nodes.trait_impl, and rustdoc-sourced IMPL_TRAIT edge meaning.

## Data Model

No schema changes. Existing rustdoc-enriched nodes and edges remain authoritative during evaluation. Comparison output is external evidence only and must not be written into the graph database as production enrichment.

## Failure Modes

A missing or unhealthy rust-analyzer, failed Cargo workspace load, timeout, malformed response, incomplete symbol inventory, or parity mismatch causes the prototype to report failure for that workspace/crate and retain rustdoc as the production source. A serverStatus health=warning/error or quiescent timeout is not silently treated as ready. No whole-index hard stop is introduced by the prototype.

## Alternatives Considered

Retain rustdoc permanently: lowest migration risk but keeps nightly JSON and schema fragility. Use a hybrid LSP/rustdoc path: likely safest if LSP can replace only qualified names or trait edges selectively, at the cost of two semantic mechanisms. Replace rustdoc wholesale: potentially simplifies the stack but is rejected until readiness, coverage, failure semantics, and timing are proven. Use workspace/symbol as the inventory: rejected because official rust-analyzer documentation describes it as fuzzy, configurable by scope/kind, and limited by default.

## Evidence

PDR-0002 and RFC-0002 document the 170-second quiescent-signal gap and require Phase 2 deferral. Current code inspection shows LspClient is single-in-flight and currently rename-specific. External primary sources reviewed 2026-08-21: rust-analyzer LSP Extensions documents experimental/serverStatus health/quiescent and says it is primarily user-facing status: https://rust-analyzer.github.io/book/contributing/lsp-extensions.html; rust-analyzer Features documents workspace symbol search behavior and implementation navigation: https://rust-analyzer.github.io/book/features.html; rust-analyzer Configuration documents workspace symbol default kind, scope, and result limit: https://rust-analyzer.github.io/book/configuration; the LSP specification defines the standardized request/response protocol and current version: https://microsoft.github.io/language-server-protocol/.

## Experiments

1. Readiness repeatability: at least 10 cold and warm launches across representative workspaces; record time to initialize, first quiescent=true, last progress end, health, and whether bounded documentSymbol/implementation probes return complete results. 2. Qualified-name parity: compare all indexed workspace symbols or a statistically justified complete sample, including modules, re-exports, private items, macros where supported, and cross-crate paths. 3. Trait-edge parity: compare trait definitions, direct impls, blanket impls, associated methods, and cross-crate implementations against rustdoc. 4. Failure injection: missing binary, invalid Cargo metadata, timeout, unhealthy server, and malformed/partial responses; verify fallback and per-crate isolation. 5. Performance: cold/warm wall time and request counts against current rustdoc on the same workspace.

## Risks

The serverStatus signal may be emitted before all useful semantic queries are stable, or may remain quiescent=false indefinitely on valid projects. LSP inventory APIs may omit private or dependency symbols, impose limits, or return locations without enough path context. Implementation requests may require one request per trait/type and become slower than rustdoc. Rust-analyzer version drift can change experimental behavior. A prototype that reports only favorable hover/documentSymbol examples would produce false confidence, so acceptance must be corpus-wide and failure-injected.

## Open Questions

Which LSP request combination can produce a complete, bounded inventory for private workspace items? Can documentSymbol hierarchy reconstruct rustdoc-equivalent qualified names across out-of-line modules and re-exports? Can implementation responses enumerate all relevant IMPL_TRAIT edges, including blanket and cross-crate cases? What readiness condition is both bounded and correlated with probe completeness when quiescent never fires? What parity threshold and fallback policy justify a later production PDR?

## Resulting Decisions

Create an evaluation-only Phase 2 record. Do not modify src/lsp_client.rs, do not change discover/check, and do not imply that Phase 2 production implementation has begun. The next authorized step is a standalone prototype and benchmark evidence; production integration requires a follow-up implementation record after the exit criteria are met.
