# AGENTS.md

Instructions for an AI agent working in the **cce-rust** repository. Follow these
alongside [`CONTRIBUTING.md`](CONTRIBUTING.md); this file is the agent-oriented
summary.

## What this project is

`cce` is a local CLI that indexes a code repository (tree-sitter AST chunking →
embedding → JSON vector + BM25 index) and answers queries with hybrid retrieval.
It is a **clean-room, test-first implementation of [`SPEC.md`](SPEC.md) v1.0**.
`SPEC.md` is the single source of truth for behaviour — treat it as the
constitution. A sibling Ruby implementation
([davidslv/cce-ruby](https://github.com/davidslv/cce-ruby)) is built from the
identical spec, and the two must stay conformance-compatible.

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
Keep coverage at or above the baseline (**84 tests, 95.33% line coverage** via
`cargo llvm-cov`); a change that lowers coverage should add tests.

## Spec conformance must not drift

`cce conformance test/fixture` produces a byte-stable
[`conformance.json`](conformance.json) that is designed to match the Ruby
sibling. **Do not change this output as a side effect.** If a change legitimately
alters it, that is a deliberate, spec-level act: justify it against `SPEC.md`,
call it out explicitly in the PR, and note that the Ruby sibling may need a
matching change. Preserve the determinism rules (6-decimal
round-half-away-from-zero, `chunk_id` ascending tie-break; SPEC §5.3) everywhere
scores are compared, sorted, or emitted.

## Where things live (docs map)

- [`SPEC.md`](SPEC.md) — normative behaviour reference (authoritative).
- [`docs/architecture.md`](docs/architecture.md) — module map, pipeline, design
  rationale, and where the design strains.
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
