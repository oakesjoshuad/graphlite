---
id: "EDR-0003"
title: "Retain rustdoc authority after Phase 2 evaluation"
record-type: edr
status: accepted
revision: 3
date: 2026-08-21
slug: retain-rustdoc-authority-after-phase-2-evaluation
tags: []
relationships:
  constrains:
    - "PDR-0002"
  derived-from:
    - "PDR-0003"
---

# EDR-0003: Retain rustdoc authority after Phase 2 evaluation

## Context

PDR-0003 evaluated whether rust-analyzer could replace or supplement the production rustdoc enrichment path for qualified names and IMPL_TRAIT edges. The evaluation now includes repeated readiness/parity runs, expanded generic and cross-crate fixtures, controlled failure injections, and direct rustdoc-versus-LSP timing.

## Decision

Retain parallel cargo rustdoc with typed rustdoc parsing as the authoritative production enrichment source. Keep Phase 2 rust-analyzer work evaluation-only and deferred; do not modify src/lsp_client.rs or wire LSP requests into discover/check. Revisit only with a broader corpus demonstrating complete inventory parity and an explicit production fallback design.

## Considered Options

Adopt LSP as the production source now, which is rejected because tested readiness and parity are bounded but corpus coverage is incomplete and LSP is slower on the representative fixture. Add LSP as a production supplement, which is rejected until fallback, request budgeting, and complete inventory semantics are specified. Close the evaluation without recording a decision, which would leave the authority boundary ambiguous.

## Consequences

No CLI, schema, or production enrichment behavior changes are required. Existing Phase 1 per-crate rustdoc parallelism remains the performance path and measured approximately 1.7-1.95x faster than serial rustdoc on the representative fixture. The standalone evaluator remains useful as a parity and failure oracle. Future LSP work must establish complete workspace inventory coverage, broader trait-edge coverage, bounded readiness, and production fallback semantics before a new implementation record.

## Evidence

PDR-0003-EV-001 through EV-013. EV-011 measured 19/19 graph-indexed qualified-name coverage and 4/4 trait-edge parity across three expanded runs, including generic and cross-crate implementations. EV-012 showed bounded reports for missing server, readiness timeout, and invalid workspace, but explicitly did not prove production fallback. EV-013 measured concurrent rustdoc at 0.668-0.919s versus 5.657-5.828s for the LSP evaluator. Production validation remains clean: release build, clippy with -D warnings, and e2e 36 passed/0 failed/1 ignored.
