---
id: "EDR-0002"
title: "Cross-crate rustdoc path resolution for IMPL_TRAIT edges"
record-type: edr
status: proposed
revision: 2
date: 2026-08-21
slug: cross-crate-rustdoc-path-resolution-for-impl-trait-edges
tags: []
relationships:
  derived-from:
    - "PDR-0002"
    - "PDR-0003"
---

# EDR-0002: Cross-crate rustdoc path resolution for IMPL_TRAIT edges

## Context

The Phase 2 evaluation proved that LSP can observe a dependent crate implementation (AppValue -> Render), while the current rustdoc-backed graph omits that edge. Inspection of phase2_app.json shows the impl points to an external path entry phase2_core::Render whose crate_id is not present in the app crate local index. The production gap is in cross-crate rustdoc ID resolution, not LSP transport or location mapping.

## Decision

Implement cross-crate rustdoc path resolution in a follow-up production change, while retaining rustdoc as the authoritative source. Resolve impl target types to local graph nodes by span, resolve external trait IDs through rustdoc paths and workspace crate qualified-name maps, and emit IMPL_TRAIT edges only when both endpoints resolve unambiguously. Preserve per-crate warn-and-skip behavior and the loud format-version mismatch. Keep the Phase 2 LSP client unchanged; use the standalone LSP harness only as an independent parity oracle.

## Considered Options

Leave the gap unresolved, which loses valid cross-crate trait edges. Use LSP as the production supplement, which adds readiness and version risks already demonstrated by PDR-0003. Add schema fields for crate-qualified IDs, which is unnecessary if existing qualified_name and node span mappings resolve endpoints. Fail the whole workspace when an external path is unresolved, which violates current per-crate failure semantics.

## Consequences

Cross-crate local trait implementations can be represented using existing IMPL_TRAIT edges without a schema or CLI change. The resolver must maintain a workspace-wide qualified-name index, handle name collisions and dependency versus workspace crates, and skip ambiguous endpoints with diagnostics. Additional rustdoc JSON fixtures and multi-crate parity tests are required. Rustdoc remains nightly and format-version pinned; Phase 2 remains deferred.

## Evidence

PDR-0002 and PDR-0003-EV-001 through EV-008. The evaluation fixture demonstrated LSP mapped {Document -> Render, Number -> Render, AppValue -> Render}, while the graph contained only the first two. The typed rustdoc JSON extractor recovered all three, including the external phase2_core::Render path. The current production collect_impl_trait_edges implementation indexes only item IDs from the current crate, explaining the omission. Acceptance requires exact parity for local and cross-crate trait edges on representative fixtures, no regression in per-crate failure handling, and cargo build, clippy, and e2e validation.
