# Security Policy

## Supported versions

cce-rust follows [Semantic Versioning](https://semver.org). Security fixes are
provided for the current `2.1.x` line only.

| Version | Supported |
|---|---|
| 2.1.x | ✅ |
| < 2.1 | ❌ |

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
  indexed — do not share a store built from a private repository. The metrics log
  (`<store-dir>/metrics.jsonl`, since v1.1) sits beside it and likewise contains
  verbatim query strings and derived counts — treat it as equally sensitive.
- **Secrets are kept out of the store by default (since v2.1).** Because the
  store holds verbatim snippets, indexing is **secret-safe by default** in two
  layers. **Layer 1** never reads files whose name marks them as secret material
  (private keys/certs by extension; `credentials.*`/`secrets.*`/`.netrc`/`id_rsa`/
  … by exact name; `.env`/`.env.*` unless a safe-template suffix) — they are
  skipped before being opened and counted as `sensitive skipped`. **Layer 2**
  redacts high-confidence secrets (private-key blocks; AWS/GitHub/Slack/Stripe/
  OpenAI/Anthropic/Google keys; JWTs; guarded `key = value` assignments) to
  `[REDACTED:<LABEL>]` **before** content is chunked, embedded, or stored, so the
  raw value never lands on disk. **Residual risk:** this is a best-effort filter
  over known patterns, not a guarantee — a novel or obfuscated secret can slip
  through, and the store remains local-only data you must still protect. The
  opt-out flag **`--allow-secrets`** disables **both** layers for a run (sensitive
  files are indexed and secrets stored verbatim); `cce` prints a warning when it
  is set, and you own the resulting store's sensitivity.
- **Workspace metadata is non-secret; per-member scrubbing still applies (since
  v2.2).** A workspace adds only two metadata files at the root —
  `.cce/workspace.yml` (the detected member list) and `.cce/workspace-graph.json`
  (cross-member dependency edges) — both derived from directory structure and
  declared manifest dependency *names*; they contain **no source content and no
  secrets**. Every member is still indexed into its own store with the **same
  secret-safe-by-default** Layer 1 + Layer 2 protection described above (a
  member's store is byte-identical to indexing it standalone), and a federated
  search/dashboard only ever reads those per-member stores and logs. `cce
  dashboard --workspace` keeps the loopback-only, read-only, self-contained
  posture and simply federates each member's metrics log.
- **The dashboard server (v1.1) is loopback-only, read-only, and
  self-contained.** `cce dashboard` binds **`127.0.0.1` only**, so it is not
  reachable from other hosts. Every endpoint is **read-only** — nothing it serves
  mutates the index, the metrics log, or any file. The page is **fully
  self-contained**: it inlines all CSS/JS and draws its own SVG charts, making
  **no external network, CDN, font, or script requests**, consistent with CCE's
  offline/local posture. It reads only the metrics log you point it at and, like
  the rest of the tool, does not execute indexed content. Because binding is
  loopback-only, no authentication is required or offered; **if a future version
  ever allowed binding a non-loopback address, it must require a token** before
  doing so. There is no browser auto-open (the command only prints the URL).

Because there is no attacker-reachable network surface by default — the only
server is loopback-only and read-only, and the only outbound path is the opt-in
localhost Ollama embedder — the practical risk is limited to parser/robustness
bugs on untrusted input, to whatever trust you place in a local Ollama server you
opt into, and to the sensitivity of the store/metrics files you keep on disk.

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
