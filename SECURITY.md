# Security Policy

## Supported versions

cce-rust follows [Semantic Versioning](https://semver.org). Security fixes are
provided for the current `1.0.x` line only.

| Version | Supported |
|---|---|
| 1.0.x | ✅ |
| < 1.0 | ❌ |

## Threat model

cce-rust is a **local command-line tool**. Understanding what it does — and does
not — do makes the real attack surface clear.

- **Trust boundary: the local filesystem.** `cce` reads and parses source files
  under a directory you point it at, and writes a JSON index to a store
  directory (default `<dir>/.cce/index.json`). The **indexed source is untrusted
  data**: it is fed to the tree-sitter parser but is **never executed**. The main
  robustness concern is that the parser and chunker handle hostile or malformed
  input (huge files, invalid UTF-8, pathological syntax) without crashing or
  misbehaving — files that fail these checks are skipped, not run.
- **No network by default.** In its default configuration `cce` makes **no
  network calls whatsoever**. The only optional network path is the opt-in Ollama
  embedder (`--embedder ollama`), which sends chunk text over **localhost HTTP**
  (via the `ureq` client) to a local Ollama server you run yourself. If that
  server is unreachable, `cce` warns and falls back to the offline hash embedder.
- **No code execution of indexed content.** `cce` does not evaluate, import, or
  run any of the code it indexes. `cce bench` shells out to `git rev-parse` to
  record a commit for its report; that is the only external process it invokes.
- **Output is data on disk.** The store is plain JSON written under a directory
  you control. Treat a store as containing verbatim snippets of whatever you
  indexed — do not share a store built from a private repository.

Because there is no server, no daemon, no authentication, and no
attacker-reachable network surface by default, the practical risk is limited to
parser/robustness bugs on untrusted input and to whatever trust you place in a
local Ollama server you opt into.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** — do not open a public
issue for a security report.

- Preferred: open a private advisory via
  [GitHub Security Advisories](https://github.com/davidslv/cce-rust/security/advisories/new).
- Alternatively, email the maintainer: **davidslv.london@gmail.com**.

Please include a description, reproduction steps, and the impact you foresee.
This is a solo, best-effort project with **no formal SLA**; you will get an
honest acknowledgement as soon as the maintainer is able, and fixes are
prioritised by severity. Coordinated disclosure is appreciated — please give a
reasonable window before publicising details.
