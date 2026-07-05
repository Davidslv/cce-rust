# AGENTS.md

Instructions for an AI agent working in the **cce-rust** repository. Follow these
alongside [`CONTRIBUTING.md`](CONTRIBUTING.md); this file is the agent-oriented
summary.

## What this project is

`cce` is a local CLI that indexes a code repository (tree-sitter AST chunking →
embedding → JSON vector + BM25 index) and answers queries with hybrid retrieval.
It is a **test-first implementation of [`SPEC.md`](SPEC.md) v1.0**, the **Dashboard
addendum [`DASHBOARD-SPEC.md`](DASHBOARD-SPEC.md) (v1.1)**, and the **v2.0
language-packs evolution [`SPEC-V2.md`](SPEC-V2.md)**. All three specs are the
single source of truth for behaviour — treat them as the constitution. A sibling
Ruby implementation ([davidslv/cce-ruby](https://github.com/davidslv/cce-ruby)) is
built from the identical specs, and the two must stay conformance-compatible —
including the dashboard aggregator's §4.1 anchor and the v2 chunk conformance over
`test/fixture/samples`.

**Language support is pluggable packs (SPEC-V2).** The core chunker/importer
(`src/chunker.rs`) references **no language by name** — a guard test enforces this
(`tests/language_packs.rs`). Each language is one file under `src/packs/`
implementing the `LanguagePack` trait, registered in `default_registry()`, and
guarded by three validator layers (`cce packs --validate`). To add a language,
follow [`docs/adding-a-language.md`](docs/adding-a-language.md); do **not** add
language-specific code or comments to the core. Every chunk carries a `kind` (the
exact tree-sitter node type) alongside the coarse `chunk_type`.

**The metrics/dashboard subsystem is the one place wall-clock time and unique
IDs are allowed** (`src/metrics.rs`, `src/aggregator.rs`, `src/dashboard.rs`).
Everywhere else stays deterministic. In metrics, the clock and id source are
**injected** so tests pin them; the aggregator is a **pure function** of
`(events, now, price)` with no ambient time. Keep it that way.

## The gates that must stay green

Before you consider any change done, all three must pass — CI runs exactly these:

```bash
cargo test                                                  # tests pass
cargo clippy --all-targets --all-features -- -D warnings    # zero warnings
cargo fmt --check                                           # formatting matches
```

- Do **not** disable, weaken, or `#[allow(...)]`-around a clippy warning to make
  the gate pass; fix the underlying cause. A narrowly-scoped allow with a written
  reason is acceptable only when genuinely warranted.
- `rustfmt.toml` is the **house style** (compact: `use_small_heuristics = "Max"`,
  imports and modules not reordered). Run `cargo fmt`; never hand-format around
  it.

## TDD discipline

This codebase is built test-first (see [`docs/TDD.md`](docs/TDD.md)):

1. Write a **failing test** for the new behaviour first.
2. Add the minimum code to make it pass.
3. Refactor with tests green.

Tests must be **deterministic and hermetic** — no network, no wall-clock, no
ambient filesystem state. The only network-touching test (Ollama) is `#[ignore]`.
The metrics tests inject a fixed clock/id source, and the dashboard's socket test
binds an **ephemeral loopback port** and serves a bounded number of connections.
Keep coverage at or above the baseline (**129 tests, 94.76% line coverage** via
`cargo llvm-cov`); a change that lowers coverage should add tests. The CI test
gate also runs the three-layer validators over every language pack.

## Spec conformance must not drift

`cce conformance test/fixture/samples` produces a byte-stable
[`conformance.json`](conformance.json) (v2 shape: each chunk carries `kind`, no
queries section) that is designed to match the Ruby sibling on the byte-identical
samples. **Do not change this output as a side effect** — and do not edit the
`test/fixture/samples/` files (the cross-language gate depends on byte equality). If a change legitimately
alters it, that is a deliberate, spec-level act: justify it against `SPEC.md`,
call it out explicitly in the PR, and note that the Ruby sibling may need a
matching change. Preserve the determinism rules (6-decimal
round-half-away-from-zero, `chunk_id` ascending tie-break; SPEC §5.3) everywhere
scores are compared, sorted, or emitted.

## Where things live (docs map)

- [`SPEC.md`](SPEC.md) — normative base-engine behaviour reference (authoritative).
- [`DASHBOARD-SPEC.md`](DASHBOARD-SPEC.md) — normative dashboard/observability
  addendum (v1.1); wins over `SPEC.md` for the metrics feature only.
- [`SPEC-V2.md`](SPEC-V2.md) — the v2.0 language-packs evolution (packs, registry,
  validators, `kind`, conformance shape); wins over `SPEC.md` for chunking/packs.
- [`SPEC-V2.1.md`](SPEC-V2.1.md) · [`SPEC-V2.2.md`](SPEC-V2.2.md) ·
  [`SPEC-SYNC.md`](SPEC-SYNC.md) · [`SPEC-MCP.md`](SPEC-MCP.md) — the secret-protection
  (v2.1), workspace (v2.2), CCE Sync (v2.3), and CCE MCP (v2.4) evolution specs; each
  wins over `SPEC.md` for its feature. `cce mcp`/`cce init` and the three MCP tools
  (`context_search`, `index_status`, `record_feedback`) live in `src/mcp/`; their
  names/schemas/output are a **cross-language contract** with the Ruby engine — do not
  drift them.
- [`docs/mcp.md`](docs/mcp.md) · [`docs/sync.md`](docs/sync.md) — the MCP and Sync
  user docs; [`docs/VERIFIED.md`](docs/VERIFIED.md) is the cold-start transcript.
- [`docs/adding-a-language.md`](docs/adding-a-language.md) — how to add a pack.
- [`docs/architecture.md`](docs/architecture.md) — module map, pipeline, design
  rationale, and where the design strains.
- [`docs/dashboard.md`](docs/dashboard.md) — metrics pipeline, event schema, and
  the aggregation formulas.
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — how each spec ambiguity was resolved.
- [`docs/getting-started.md`](docs/getting-started.md) · [`docs/how-to.md`](docs/how-to.md) — user paths.
- [`docs/TDD.md`](docs/TDD.md) — red → green log and coverage.
- `src/*.rs` — one concern per file, each with a why/what/responsibilities header
  (keep that header convention when adding a module).

## Commit and PR conventions

- Focused commits, imperative subject lines; conventional-commit prefixes
  (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) are encouraged.
- One concern per PR; reference the issue it closes.
- Fill in [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) —
  it is the checklist of the gates above, including the conformance question.
- For anything beyond a small, obvious fix, open an issue first.

## Do not

- Do not commit or push unless explicitly asked.
- Do not add dependencies without discussion (they are pinned in `Cargo.toml`);
  the only ecosystems here are cargo and github-actions.
- Do not introduce network calls into the default code path — offline-by-default
  is a design invariant. The Ollama embedder is the sole, opt-in exception.
- Do not read or copy from any other implementation of this spec (clean-room).
