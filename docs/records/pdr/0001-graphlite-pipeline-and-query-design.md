---
id: "PDR-0001"
title: "GraphLite pipeline and query design"
record-type: pdr
status: approved
revision: 3
date: 2026-08-21
slug: graphlite-pipeline-and-query-design
tags: []
relationships: {}
---

# PDR-0001: GraphLite pipeline and query design

## Problem

An LLM agent working in a codebase needs to answer structural questions (who calls this, what would change if I edit that, what's the architecture) without either reading whole files defensively (expensive, imprecise, doesn't scale with codebase size) or lacking any structural ground truth at all (hallucinated call graphs, missed call sites).

## Requirements

Index Rust, TypeScript, JavaScript, Svelte, HTML, and CSS source into symbols and call edges. Provide token-metered query commands (map, symbols, graph, context, blast-radius, trace-path) that return XML with an explicit token/line budget the caller controls. Re-indexing must be incremental, keyed on file content hash, so repeated queries against a large codebase stay cheap. For Rust specifically, enrich beyond syntactic tree-sitter facts with real semantic data: qualified names, visibility, trait implementations.

## Constraints

Must work without requiring the target project's build to succeed (tree-sitter parses syntax, not semantics, so it tolerates broken code). Semantic enrichment for Rust is necessarily best-effort on top of that, since it depends on a working compiler toolchain. Output must carry its own token cost so an agent can budget context spend without guessing.

## Proposed Design

A three-stage pipeline writes into one SQLite database (.graphlite/codegraph.db): (1) tree-sitter parses all supported languages in parallel (rayon) into symbols and syntactic CALLS edges, confidence=0.8; (2) for Rust, `cargo +nightly rustdoc --output-format json` is run per affected crate and the resulting JSON is parsed to attach qualified_name, visibility, and IMPL_TRAIT edges, source="rustdoc"; (3) a use-statement resolver produces CALLS_RESOLVED edges with a high/medium/low confidence tier, source="resolver". A separate role-inference pass classifies each symbol (entrypoint, orchestrator, infra, domain, utility, leaf, model) from graph topology (fan-in/fan-out shape), independent of any per-language logic. Every query command renders XML via a shared writer and reports its own token count in the output header.

## Components

tree-sitter parser + per-language .scm query files (queries/*.scm) for symbol/call extraction; rustdoc_enricher.rs (cargo invocation, JSON parsing, node reconciliation); resolver.rs (use-statement resolution to CALLS_RESOLVED edges); roles.rs (topology-based role classification); query.rs (all read-path commands and XML rendering); schema.rs (SQLite schema, incremental hashing); watch.rs (filesystem watch + re-index); annotate.rs (persistent human/LLM notes on symbols, with staleness tracking against re-indexed content); policy.rs + violations.rs (architecture layer/context coupling rules, built-in packs, custom overrides).

## Interfaces

A single `graphlite` CLI binary (clap-derived subcommands), documented in README.md and in skills/graphlite.md (a Claude Code skill file distributed inside this repo, describing the recommended query-escalation workflow and per-command token budgets). `.graphlite/config.toml` (TOML) configures ignore patterns, workspace crate-to-layer mapping, and policy overrides. No network interface; output is XML (or Markdown via --md) to stdout.

## Data Model

SQLite tables include nodes (symbol id, stable_id, kind, file, range, visibility, role, fan_in/fan_out), edges (from/to node id, edge_type, source, confidence_tier), annotations (intent, behavior, tags, source, confidence, staleness), node_diagnostics (clippy findings), and crate-risk tables (cargo-audit ingestion). stable_id (e.g. "src/policy.rs::fn::effective_policy") is the durable cross-query identifier; numeric node ids are session-local.

## Failure Modes

Missing grammar or unparseable file: skipped with a logged warning, not a hard failure. Rustdoc enrichment failing for one crate (nightly not installed, cargo error) is caught per-crate in enrich_crates and logged as a warning, not propagated -- the rest of the index still completes. A rustdoc JSON format_version mismatch against EXPECTED_FORMAT_VERSION is a hard, loud failure (bail!) rather than a silent partial parse. Invalid FTS5 query syntax passed to `symbols` currently surfaces as a raw anyhow backtrace rather than a clean error -- an observed gap, not yet fixed.

## Alternatives Considered

A single unified language-server-protocol backend (rust-analyzer, tsserver, etc.) instead of tree-sitter+rustdoc: rejected as the primary mechanism because LSP servers require a working build and per-language server management; tree-sitter's syntax-only parsing tolerates broken/mid-edit code and covers six languages uniformly. An LSP client (lsp_client.rs) exists in this codebase for one specific feature (semantic rename) rather than as the core indexing mechanism -- README.md now states rename is deprecated in favor of editor-native LSP rename, though the CLI (`graphlite rename --help`, main.rs Commands::Rename) still dispatches it; this inconsistency is unresolved as of this record.

## Evidence

Verified directly, in a real session working against both this codebase and a second real Rust workspace (github.com Strata project, 4 crates / 211 symbols): `graphlite deps` correctly derived crate dependency direction with zero manual configuration; `graphlite context`/`blast-radius`/`trace-path` reproduced call-structure facts (e.g. cli/src/publish.rs::fn::run -> stage::publish -> stage::replace_target in 2 hops) that would otherwise require reading multiple full files; policy layering (kernel/records=domain, store=infra, strata=adapter) matched the target project's own documented architecture with zero manual dependency-direction violations found.

## Experiments

None formally recorded prior to this backfill. The verification described under `evidence` was exploratory, ad hoc, and not captured as a repeatable experiment at the time it was run.

## Risks

rustdoc enrichment requires the nightly toolchain and consumes unstable, only-loosely-versioned JSON output (mitigated by the format_version assertion, but every nightly rustdoc-json schema change still requires a manual constant bump and re-verification of every raw serde_json::Value access path, since rustdoc-types is not used). rustdoc enrichment runs strictly serially per crate (enrich_crates is a plain for loop) despite rayon already being a dependency used elsewhere in this same pipeline (discover.rs) -- on a multi-crate workspace this is very likely the dominant cost of a full re-index, and is the concrete, felt performance problem motivating RFC-0002. query.rs has grown to 2945 lines mixing orchestration and XML rendering for every command, a maintainability risk independent of correctness.

## Open Questions

Should rustdoc-based enrichment continue to exist as a separate mechanism from the LSP client already present in this codebase, or should one subsume the other -- and does that decision depend on resolving the rename-deprecation inconsistency noted under `alternatives` first? Tracked forward in RFC-0002.

## Resulting Decisions

This record captures already-implemented, working design as of the session that backfilled it; it does not itself constitute a new decision. RFC-0002 (rustdoc/cargo enrichment redesign) is the forward-looking record that references this PDR as the description of the system it proposes to change.
