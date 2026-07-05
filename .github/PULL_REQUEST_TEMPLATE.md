# Pull Request

## Summary

<!-- What does this change do, and why? Link the issue it closes. -->

Closes #

## Type of change

- [ ] Bug fix (behaviour now matches `SPEC.md` / documented behaviour)
- [ ] Feature
- [ ] Documentation
- [ ] Refactor / internal (no observable behaviour change)
- [ ] Other:

## Checklist — the quality gates

All three must be green (CI runs exactly these):

- [ ] `cargo test` passes (added/updated tests for the change)
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` is clean
- [ ] `cargo fmt --check` passes (`rustfmt.toml` is the house style)
- [ ] Coverage maintained or improved (`cargo llvm-cov`; baseline 95.33%)
- [ ] Docs updated where behaviour or usage changed (README / `docs/`)
- [ ] TDD: a failing test was written first for new behaviour

## Conformance

- [ ] `cce conformance test/fixture` output is **unchanged**, **or**
- [ ] It changes intentionally — explained below and justified against `SPEC.md`
      (note: this may also require a matching change in the Ruby sibling)

<!-- If conformance changes, explain here. -->

## Notes for the reviewer

<!-- Anything else worth knowing: trade-offs, follow-ups, screenshots of output. -->
