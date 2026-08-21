---
id: "RFC-0001"
title: "Dynamic language discovery via heuristics and parsing"
record-type: rfc
status: draft
revision: 1
date: 2026-08-21
slug: dynamic-language-discovery-via-heuristics-and-parsing
tags: []
relationships:
  relates-to:
    - "PDR-0001"
---

# RFC-0001: Dynamic language discovery via heuristics and parsing

## Motivation

The current language list (Rust, TypeScript, JavaScript, Svelte, HTML, CSS) is fixed at compile time, one tree-sitter grammar crate per language, hardcoded in language.rs. GRAPHLITE-SPEC.xml (committed alongside the v0.2 LSP-enrichment release) proposed replacing this with runtime grammar discovery: no assumption about which languages are supported ahead of time, detection instead driven by a three-tier heuristic (file extension, then shebang line, then actual parse-attempt validation with grammar-family fallback).

## Problem

A fixed, compile-time language list means adding a language requires a new graphlite release. The proposal's premise is that a codebase's actual language mix should be discovered, not pre-declared, and that detection should degrade gracefully (parseable_unknown, unparseable-metadata-only) rather than simply skipping unrecognized extensions.

## Scope

As specified in GRAPHLITE-SPEC.xml: extension-to-language mapping with a shebang-line fallback, a parse-attempt validation tier with grammar-family fallback ordering (e.g. typescript tried before javascript for .js files), a new `languages` SQLite table recording per-language file counts/parse success/confidence, and CLI surface additions (`discover --list-grammars`, `--dry-run`, `--grammar-debug`) plus a `symbols --language` filter over the dynamically discovered set.

## Non-Goals

The spec does not propose changing the indexing/query pipeline itself (tree-sitter parse -> symbols/edges -> SQLite -> XML output) -- only how the set of active languages is determined and reported.

## Constraints

No hard failure on a missing grammar crate for a detected language -- log and skip. Detection must not require the project to build.

## Proposal

Runtime grammar discovery as described under `scope`, with the CLAUDE_DISCOVERY_PROTOCOL section of the spec explicitly written as agent-facing guidance ("never assume languages, wait for discover output first").

## Alternatives Considered

Keep the current fixed compile-time language list (what actually shipped): simpler, fully typed, no runtime grammar-loading machinery, but requires a release to add a language and cannot handle a codebase mixing an unsupported language gracefully beyond skip-with-warning.

## Open Questions

Was this proposal ever formally reviewed, or written and left in draft? -- Left in draft: this record backfills it honestly rather than assuming acceptance. Does the current fixed-list implementation already satisfy the real need well enough that the added runtime-discovery complexity isn't worth it? -- Open; no evidence either way was found in this codebase's history. Is `tier3_parsing_validation`'s try-then-fallback-grammar-family approach (e.g. attempting a rust parse on an unrecognized C-like file) something the tree-sitter query layer can actually support today, or does it require substantial new query-file infrastructure per grammar family?

## Outcome

Not implemented. Verified directly against the current codebase: `graphlite discover --help` / `capabilities --json` show no `--list-grammars`, `--dry-run`, or `--grammar-debug` flags (discover currently takes no flags at all); language.rs implements a fixed extension-to-language match, not runtime grammar scanning; no `languages` table exists in the current schema. `git log --follow` shows this file was added in the same commit as v0.2's LSP enrichment work and has not been touched since -- consistent with a proposal that was written, not pursued, and not since revisited. Recorded here as draft, not accepted or withdrawn, since no explicit review or withdrawal decision is evidenced; a maintainer should either move this forward or formally withdraw it so the record reflects an actual decision rather than an open, silently-abandoned one.
