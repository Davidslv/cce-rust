# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-07-05

Initial public release: a clean-room, test-first Rust implementation of the Code
Context Engine, built solely from [`SPEC.md`](SPEC.md) (SPEC v1.0).

### Added

- `cce index` — walk a directory, AST-chunk files with tree-sitter (Python and
  JavaScript, with a whole-file `module` fallback for other languages), embed
  each chunk, and persist a JSON store (vectors + BM25 + import graph).
- `cce search` — hybrid retrieval (exact cosine + Lucene-form BM25) fused with
  Reciprocal Rank Fusion, a confidence blend, a test/doc path penalty, a per-file
  diversity cap, and optional import-graph expansion; human and `--json` output.
- `cce stats` — summary of a persisted store (chunks, files, tokens, languages).
- `cce bench` — benchmark the pipeline on a real repository and write
  `docs/BENCHMARKS.md`.
- `cce conformance` — emit a byte-stable `conformance.json` for cross-language
  verification against the Ruby sibling.
- Deterministic FNV-1a hashing embedder (default, offline) and an optional,
  opt-in local Ollama embedder (`--embedder ollama`) with graceful fallback.
- Determinism guarantees: 6-decimal round-half-away-from-zero and `chunk_id`
  tie-breaking throughout (SPEC §5.3).
- Test suite of 84 tests (83 hermetic + 1 `#[ignore]` Ollama) at 95.33% line
  coverage (`cargo llvm-cov`).
- Project documentation: `SPEC.md`, `docs/architecture.md`, `docs/getting-started.md`,
  `docs/how-to.md`, `docs/DECISIONS.md`, `docs/TDD.md`, `docs/BENCHMARKS.md`.

[Unreleased]: https://github.com/davidslv/cce-rust/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/davidslv/cce-rust/releases/tag/v1.0.0
