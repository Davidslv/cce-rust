# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.0.0] - 2026-07-05

Pluggable **language packs**, built test-first from [`SPEC-V2.md`](SPEC-V2.md).
Language support is factored out of the core into self-contained packs, four new
languages ship, and every chunk gains a `kind` field. **This is a breaking
release**: the conformance output shape changes and the supported-language set
changes.

### Added

- **Language-pack architecture** — a `LanguagePack` trait (`src/packs/`) plus a
  registry resolve files to packs by extension. The core chunker/importer
  (`src/chunker.rs`) references **no language by name**; a guard test enforces it.
  Adding a language is one pack file + registration + validation — no core edits.
- **Four new languages**: **Ruby**, **Rust**, **TypeScript**, and **C** packs,
  joining the converted **Python** and **JavaScript** packs (six total). New
  grammar crates pinned in `Cargo.toml` (`tree-sitter-ruby`, `-rust`,
  `-typescript`, `-c`), ABI-compatible with the pinned `tree-sitter` core.
- **`kind` field on every chunk** — the exact tree-sitter node type (e.g.
  `struct_specifier`, `trait_item`, `interface_declaration`, `method`), carried
  through persistence, `search` (human + `--json`), `stats` (a by-kind
  breakdown), and conformance. `kind` is not part of `chunk_id`.
- **Three-layer pack validators** (`src/packs/validators.rs`): structural lint,
  grammar-binding lint with "did you mean" node-kind suggestions, and a
  behavioural self-test (min function/class counts, kinds present, and
  `extract_imports == expected` exactly). Surfaced by **`cce packs`** /
  **`cce packs --validate`**, a CI test gate over every pack, and cheap fail-fast
  startup checks.
- **Sample corpus** at `test/fixture/samples/` (seven files) — both the pack
  self-tests and the cross-language conformance corpus.
- **Per-language benchmarks** — `cce bench --lang ruby|rust|typescript|c` with the
  labeled query sets from SPEC-V2 §8; measured numbers for Ruby (sinatra), Rust
  (hyperfine), TypeScript (zustand), and C (jq) in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).
- New guide [`docs/adding-a-language.md`](docs/adding-a-language.md); README,
  architecture, how-to, getting-started, `llms.txt`, and `AGENTS.md` swept of the
  Python/JavaScript-only framing.
- Test suite grows to 129 tests at 94.76% line coverage (`cargo llvm-cov`).

### Changed (breaking)

- **Conformance output shape** — `cce conformance` now targets
  `test/fixture/samples`, tags `spec_version` `"2.0"`, adds `kind` to every chunk
  object, and drops the query section (the chunk array is the equivalence gate).
- **Supported-language set** — six AST-aware packs instead of two.
- **Module-fallback line count** — the fallback chunk's `end_line` is now
  `(number of "\n" bytes) + 1` (a trailing newline counts its line), closing the
  one v1 cross-language divergence. This changes fallback `chunk_id`s.
- The base v1 fixture moved to `test/fixture/base/` so the samples corpus is
  independent.

## [1.1.0] - 2026-07-05

Dashboard & observability, built test-first from
[`DASHBOARD-SPEC.md`](DASHBOARD-SPEC.md) (SPEC v1.1). The base engine (chunking,
embedding, retrieval) is unchanged and stays byte-for-byte conformant —
`conformance.json` is identical to the 1.0.0 release.

### Added

- Persisted metrics event log (`.cce/metrics.jsonl`): `cce search`, `cce index`,
  and the new `cce feedback` each append one best-effort/fail-open JSON line. The
  metrics subsystem is the one place real wall-clock time and unique IDs are used;
  the clock and id source are injected so tests stay deterministic.
- `cce feedback <query-id> --helpful|--not-helpful [--note ...]` — rate a past
  search result. `cce search` now prints a `query-id` (and adds `query_id` to
  `--json`, which is now an object wrapping the `results` array).
- Whole-file token counts persisted per indexed file so a search's
  `baseline_tokens` (the "read the whole file" counterfactual) is accurate.
- Pure aggregator (`aggregator.rs`): totals, two north-stars (token/cost SAVINGS
  and retrieval QUALITY) with current-vs-prior windowed deltas and an
  improving/degrading/flat direction, a daily series, and a recent-searches view.
  Reproduces the DASHBOARD-SPEC §4.1 anchor exactly.
- `cce dashboard [--dir DIR|--store PATH] [--port N] [--metrics PATH] [--no-open]`
  — a loopback-only (`127.0.0.1`), read-only, fully self-contained web server
  (inline CSS/JS, hand-drawn SVG charts, no external network/CDN) serving
  `GET /`, `GET /api/metrics`, and `GET /api/health`. Hand-rolled on
  `std::net::TcpListener` — no new dependency.
- `--no-metrics` flag on `index`/`search`; the metrics log format (`.jsonl`) is
  excluded from indexing so it never pollutes the corpus.
- Docs: new [`docs/dashboard.md`](docs/dashboard.md) (pipeline, schema, formulas,
  "where this would strain"); README, `docs/how-to.md`, `SECURITY.md`,
  `llms.txt`, and `AGENTS.md` updated.
- Test suite grows to 113 tests (112 hermetic + 1 `#[ignore]` Ollama) at 95.44%
  line coverage (`cargo llvm-cov`).

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

[Unreleased]: https://github.com/davidslv/cce-rust/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/davidslv/cce-rust/compare/v1.1.0...v2.0.0
[1.1.0]: https://github.com/davidslv/cce-rust/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/davidslv/cce-rust/releases/tag/v1.0.0
