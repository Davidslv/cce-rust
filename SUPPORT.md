# Support

Thanks for using **cce-rust**. Here is how to get help and what to expect.

## Where to ask

- **Bugs** — open a [bug report](https://github.com/davidslv/cce-rust/issues/new?template=bug_report.yml).
- **Feature ideas** — open a [feature request](https://github.com/davidslv/cce-rust/issues/new?template=feature_request.yml).
- **Questions, ideas, "is this a bug?"** — use
  [GitHub Discussions](https://github.com/davidslv/cce-rust/discussions) if
  enabled, otherwise open an issue and label it a question.

Before filing, please skim the docs — many questions are answered there:

- [README](README.md) — install, usage, and worked examples.
- [`docs/getting-started.md`](docs/getting-started.md) — first index + search.
- [`docs/how-to.md`](docs/how-to.md) — task recipes.
- [`SPEC.md`](SPEC.md) — the authoritative description of every behaviour.
- [`docs/DECISIONS.md`](docs/DECISIONS.md) — why ambiguous cases resolve the way
  they do.

## In scope

- Defects where `cce` deviates from [`SPEC.md`](SPEC.md) or its documented
  behaviour.
- Build/setup problems on macOS or Ubuntu/Debian with a stable Rust toolchain.
- Reasonable, spec-aligned feature requests.
- Documentation gaps and errors.

## Out of scope

- Support for toolchains, OSes, or dependency versions other than those pinned in
  `Cargo.toml` and documented in the README.
- The internals or health of a local Ollama server you run (the embedder is
  optional and falls back to the offline hash embedder).
- Changes that would break cross-implementation conformance with the Ruby sibling
  without a corresponding spec change.
- Turning `cce` into a long-running server/daemon — it is a CLI by design.

## What to expect

cce-rust is maintained by one person ([David Silva](https://davidslv.uk)) on a
**best-effort, no-SLA** basis. Issues and PRs are read and triaged as time
allows; there is no guaranteed response time. Clear, reproducible reports with
version (`cce --version`), OS, and exact commands get the fastest and best help.

The sibling Ruby implementation is at
[davidslv/cce-ruby](https://github.com/davidslv/cce-ruby).

For security issues, do **not** use public channels — see [SECURITY.md](SECURITY.md).
