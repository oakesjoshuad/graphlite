---
id: "EDR-0001"
title: "Phase 1 rustdoc enrichment implementation"
record-type: edr
status: draft
revision: 1
date: 2026-08-21
slug: phase-1-rustdoc-enrichment-implementation
tags: []
relationships:
  implements:
    - "PDR-0002"
---

# EDR-0001: Phase 1 rustdoc enrichment implementation

## Context

Implementing PDR-0002 Phase 1 in src/rustdoc_enricher.rs after the required target-locking, timing, rustdoc-types, and visibility experiments.

## Decision

Hoist one SQLite node-map snapshot into Arc, compute typed rustdoc enrichment per crate in rayon workers using per-crate Cargo target directories, then apply all successful results serially in one transaction. Adopt rustdoc-types 0.57.4 for format_version 57 and stop writing visibility from rustdoc so tree-sitter remains authoritative.

## Considered Options

Retain a shared target directory, which Cargo serializes with its build lock; retain serial enrichment; keep raw serde_json navigation; or guard rather than remove rustdoc visibility writes. The experiment showed per-crate target directories are required for actual overlap, and the visibility sample showed rustdoc overwrote private with crate.

## Consequences

The representative three-crate workspace improved from 3.01s before to 1.67s after. Compiled dependency artifacts are less reusable across crates because target directories are isolated. Qualified names, signatures, trait implementations, and IMPL_TRAIT edges are computed without database access and committed serially. Format-version mismatches remain loud; other crate failures warn and skip.

## Evidence

Shared-target concurrent rustdoc: alpha 0.67s and beta 1.15s, with beta blocking on Cargo build-directory lock. Baseline graphlite discover/enrich on the same workspace: 3.01s. Changed implementation: 1.67s with three concurrent rustdoc jobs. Visibility sample after change: private_item=private, public_item=pub, crate_item=pub(crate), restricted_item=pub(in crate::inner). rustdoc-types 0.57.x declares FORMAT_VERSION 57.
