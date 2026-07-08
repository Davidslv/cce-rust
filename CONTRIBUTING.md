# Contributing to cce-rust

Thank you for your interest in **cce-rust**, the Rust clean-room implementation
of the Code Context Engine. Contributions are welcome. This is a solo,
best-effort project (see [SUPPORT.md](SUPPORT.md) and [GOVERNANCE.md](GOVERNANCE.md)),
so a little process up front keeps things smooth for everyone.

The sibling Ruby implementation lives at
[davidslv/cce-ruby](https://github.com/davidslv/cce-ruby) and follows the same
conventions — if you contribute to both, they will feel familiar.

## Open an issue first for anything non-trivial

For bug fixes with an obvious cause, a PR is fine. For **anything larger** — a
new feature, a behaviour change, a new dependency, or a change that could affect
[conformance](conformance.json) — please **open an issue first** so we can agree
on the approach before you invest time. This project is spec-driven: changes that
alter observable behaviour must be justified against [`SPEC.md`](SPEC.md).

## Development setup

You need a stable Rust toolchain and a C compiler for the tree-sitter grammars.
See the [Installation](README.md#installation--environment-setup) section of the
README for macOS and Ubuntu/Debian steps. In short:

```bash
git clone https://github.com/davidslv/cce-rust
cd cce-rust
cargo build
cargo test
```

No database or other system libraries are required — the store is JSON on disk.

## The three quality gates

Every change must keep all three green. CI runs exactly these:

```bash
cargo test                                                  # 1. tests pass
cargo clippy --all-targets --all-features -- -D warnings    # 2. zero lint warnings
cargo fmt --check                                           # 3. formatting matches
```

- **`rustfmt.toml` is the house style.** It is load-bearing and deliberately
  compact (`use_small_heuristics = "Max"`, imports/modules not reordered). Run
  `cargo fmt` before committing; do not hand-format around it.
- **Clippy runs with `-D warnings`.** A warning is a failure. Fix the cause
  rather than sprinkling `#[allow(...)]`; if an allow is genuinely warranted,
  scope it as narrowly as possible and say why in a comment.

## Test-driven development

This codebase was built test-first (SPEC §12) and expects to stay that way — see
[`docs/TDD.md`](docs/TDD.md) for the red → green log.

- Write a **failing test first**, then the minimum code to pass it, then refactor
  with tests green.
- Tests must be **deterministic and hermetic**: no network, no reliance on wall
  clock or ambient filesystem. The single Ollama integration test is marked
  `#[ignore]` for exactly this reason.
- Keep or improve coverage. The suite is **660 tests at ~94% line coverage**
  (`cargo llvm-cov`); a change that drops coverage should add tests, not lower
  the bar.

## Spec conformance must not drift

`cce conformance test/fixture/samples` produces a byte-stable [`conformance.json`](conformance.json)
that is designed to match the Ruby sibling on the same fixture. If your change
alters that output, that is a significant, deliberate act: call it out
explicitly in the PR, explain why it is correct against `SPEC.md`, and expect it
to be discussed. Do not change conformance output as an incidental side effect.

## Commit and PR conventions

- Keep commits focused and logically scoped; write imperative subject lines
  (e.g. `fix: guard against empty query in retriever`). Conventional-commit
  prefixes (`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`) are
  encouraged but not enforced.
- Reference the issue a PR closes.
- Fill in the [pull request template](.github/PULL_REQUEST_TEMPLATE.md) — it is a
  checklist of the real gates above.
- One concern per PR. Small, reviewable PRs merge faster.

## Code of Conduct

This project follows a Code of Conduct (see `CODE_OF_CONDUCT.md`). By
participating you agree to uphold it.
