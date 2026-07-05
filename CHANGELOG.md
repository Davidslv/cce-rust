# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.4.1] - 2026-07-05

The **closing consolidation of the v2.4 milestone**: a refreshed dashboard that
surfaces the capabilities landed since v1.1, plus a verified, gapless, offline-first
documentation sweep. **Additive patch release** — the metrics schema grows only by
adding fields (older logs still parse), the base engine and single-repo
`conformance.json` are byte-for-byte unchanged, and `SYNC_FORMAT_VERSION` stays
`"2.3"` so the shared golden checksum
`581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` is untouched.

### Added

- **Dashboard refresh (`src/dashboard.rs`, `src/aggregator.rs`)** — four new panels:
  **agent-vs-human usage** (CLI vs MCP searches), **per-package breakdown**
  (savings/searches/quality per workspace member — now with `mean_top_score`),
  **index freshness / sync status** (indexed `sha`, local-vs-pulled source,
  behind-remote), and **secret-safety** (sensitive-files-skipped count). The page
  stays loopback-only, read-only, and self-contained (inline CSS/JS, hand-drawn SVG,
  no external network).
- **Metrics schema — additive fields.** `search` events carry
  `source: "cli" | "mcp"` (the CLI `search` path tags `"cli"`; the MCP
  `context_search` path tags `"mcp"`). `index` events carry `sha`, `source`, and
  `sensitive_skipped`. Absent/unknown fields degrade gracefully (a pre-v2.4.1 search
  reads back as `"cli"`; an index event as `"local"`).
- **Aggregator sections.** `/api/metrics` gains `usage_by_source`, `secret_safety`,
  and `index_freshness` (log-derived, pure, cross-language-identical), and
  `totals.mean_top_score`. The dashboard edge layers the live, offline-safe
  `remote_latest`/`behind_remote` onto `index_freshness`.
- **Documentation sweep** — a dedicated, **verified offline-first** section proving
  `index` / `search` / `stats` / `dashboard` / `workspace` / `cce mcp` all run with
  no network and no remote; macOS **and** Ubuntu setup with explicit prerequisites
  (toolchain, C compiler, git, git-LFS); a Sync + MCP best-practices section; and
  both an online and an offline cold-start transcript in
  [`docs/VERIFIED.md`](docs/VERIFIED.md).

### Changed

- `retriever::build_search_record` takes a `source` argument so the CLI and MCP
  search paths tag their metrics events.
- `dashboard::run` / `serve` / `route` take the project `root` so the freshness
  panel can read the sync marker (offline-safe: no remote ⇒ no network).
- Version bumped to **2.4.1** (`Cargo.toml`, `CITATION.cff`). `SYNC_FORMAT_VERSION`
  deliberately **unchanged** at `"2.3"`.

## [2.4.0] - 2026-07-05

**CCE MCP** — a [Model Context Protocol](https://modelcontextprotocol.io) server
(`cce mcp`) so an agent (Claude Code) uses CCE as a **first-class tool it
auto-invokes** — running `context_search` instead of reading and grepping whole
files — plus `cce init` to wire an editor up plug-and-play. This closes the last
gap between the clean-room CCE and the original Python implementation: the agent
integration. Built test-first from [`SPEC-MCP.md`](SPEC-MCP.md). **Additive minor
release**: the CLI and single-repo `conformance.json` are untouched, and MCP is
read-only, offline, and does not require CCE Sync.

### Added

- **`cce mcp`** (`src/mcp/`) — an MCP server over stdio (JSON-RPC 2.0), pinning
  protocol version `2025-06-18`. Handles `initialize` (advertising
  `serverInfo { name: "cce", version }` and `capabilities { tools: {} }`),
  `notifications/initialized`, `tools/list`, `tools/call`, and `ping`. Resolves the
  store exactly like the CLI (`--dir` / `--store` / cwd, `--workspace`), is
  read-only, and answers a missing/empty index with a friendly "run `cce index`"
  message rather than crashing. The dispatch loop is transport-generic, so it is
  driven hermetically in tests by piping JSON-RPC to stdin.
- **Three tools** with schemas identical to the Ruby engine (the cross-language
  contract): `context_search` (ranked chunks for a query — the "PREFERRED over
  Read/Grep" tool — logging a `search` metrics event and returning a `query_id`),
  `index_status` (counts + sync freshness), and `record_feedback` (a `feedback`
  event closing the dashboard's quality loop).
- **`cce init [<dir>] [--agent claude] [--remote <sync-url>] [--force]`** — ensures
  an index (`cce sync pull --latest` when a remote is configured/passed, else a
  local `cce index` / workspace index), then merges an idempotent `cce` entry into
  `.mcp.json` and a marker-bounded block into `CLAUDE.md`, and prints next steps.
- **CCE MCP × CCE Sync (soft dependency)** — on startup, if a sync remote is
  configured and `sync.auto_pull` is on, `cce mcp` best-effort pulls the latest
  CI-built index (offline-safe; never blocks or errors). `index_status` reports the
  index source (local vs pulled), its sha, and whether it is behind the remote. MCP
  works fully with no Sync configured. New public `sync::commands::freshness`.
- **Docs** — a README "Use it with Claude Code (MCP)" section, [`docs/mcp.md`](docs/mcp.md),
  and a cold-start MCP transcript added to [`docs/VERIFIED.md`](docs/VERIFIED.md).

### Changed

- **Sync artifact format version decoupled from the app version** — introduced
  `sync::SYNC_FORMAT_VERSION` (`"2.3"`), which names the *artifact format* rather than
  the release, replacing the old `cce_version_minor()` that derived it from the crate
  version. CCE MCP is additive and does not change the artifact format, so the format
  version stays `2.3`: the content address stays `hash/2.3/…`, the manifest
  `cce_version` stays `"2.3"`, and the shared golden checksum on `test/fixture/samples`
  stays `581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` — so a v2.4
  release does **not** invalidate existing caches or diverge from the Ruby engine's
  artifacts. `SYNC_FORMAT_VERSION` moves only when the artifact bytes actually change.
- `retriever::build_search_record` was lifted out of `main.rs` into the library so
  the CLI `search` and the MCP `context_search` log a byte-identical metrics event.

## [2.3.0] - 2026-07-05

**CCE Sync** — a distributed, offline-first cache for the index: *git remotes for
the index*. Your local `.cce/` stays authoritative; an optional git-backed remote
is a **content-addressed cache** you push to and pull from. Because the index is
deterministic (hash embedder), a cache for `repo@sha` is byte-identical no matter
who — or which language engine — built it. Built test-first from
[`SPEC-SYNC.md`](SPEC-SYNC.md). **Additive minor release**: absent a configured
remote, every command behaves exactly as before and single-repo `conformance.json`
remains byte-identical.

### Added

- **Portable interchange artifact** (`src/sync/artifact.rs`) — a canonical,
  byte-exact, cross-language format (reconciled to the single spec in
  [`SPEC-SYNC-RECONCILE.md`](SPEC-SYNC-RECONCILE.md)): a UTF-8 stream with an LF
  after every line — the manifest line, one sorted-key compact-JSON object per chunk
  (sorted by `(file_path, start_line, id)`), then the graph line
  `{"edges":[…],"nodes":[…]}`. Embeddings are **standard base64 (with padding) of
  256 little-endian IEEE-754 `f64` bytes** (not decimals), so the bytes match across
  Ruby and Rust. **No provenance** (`built_at`/`built_by` removed) so the artifact is
  reproducible; `file_tokens` lives in the manifest; `pack_set_id` is the literal
  `c,javascript,python,ruby,rust,typescript`. `checksum` = lowercase-hex SHA-256
  over the whole stream serialized with `checksum` set to `""`. A committed **shared
  golden checksum** on `test/fixture/samples` anchors the format cross-language.
- **Content address** (`src/sync/mod.rs`) —
  `<embedder>/<cce_ver>/<repo_id>/<sha>.cce`; `repo_id` = normalized git origin
  (`host__org__repo`) or a `sync.repo_id` override. Only the `hash` embedder is
  shareable.
- **Git remote backend** (`src/sync/remote.rs`) — a `SyncRemote` trait with a
  `GitRemote` impl: a local working clone under `~/.cce/sync/<remote-id>/`,
  `put` = write at the content path + commit + push (fetch-rebase-retry on a ref
  race), `get` = fetch + read. `*.cce` blobs use **git-LFS** by default; the core
  path works over plain git (no `git-lfs` binary required for the tests).
- **CLI** (`src/sync/commands.rs`, `src/main.rs`) — `cce sync init`, `push`,
  `pull`, `status`, `verify`. `push` refuses a dirty tree or a non-hash index;
  `pull` installs the artifact into `.cce/` and never overwrites a different sha
  without `--force`; `pull --latest` follows a per-branch ref pointer; `verify`
  re-indexes locally and confirms the pulled checksum. All are **workspace-aware**
  (`--workspace`), each member keyed by its own `repo_id@sha`.
- **Config** (`src/sync/config.rs`) — `sync.remote`, `sync.lfs` (default true),
  `sync.repo_id`, `sync.auto_pull`, `sync.retention` under `<root>/.cce/config`
  (global `~/.cce/config.yml` fallback). Absent ⇒ pure local CCE.
- **Docs** — a README "CCE Sync" section with a verified end-to-end walkthrough,
  macOS/Ubuntu install incl. `git lfs install`, a ready-to-copy CI workflow
  ([`docs/ci/cce-sync.yml`](docs/ci/cce-sync.yml)), [`docs/sync.md`](docs/sync.md)
  (model, artifact format, content address, permissions, troubleshooting), and
  [`docs/VERIFIED.md`](docs/VERIFIED.md) (the cold-start transcript).

### Guarantees

- **Offline-first (normative).** No remote ⇒ every command behaves as today. A
  configured-but-unreachable remote ⇒ `sync` fails gracefully; all non-sync
  commands are unaffected. A failed push/pull never breaks local indexing or search.

## [2.2.0] - 2026-07-05

**Workspace mode** — CCE now understands an *ecosystem* of related codebases (e.g.
a Rails app + engines + a frontend under one root) as a single searchable whole,
while **each member stays isolated in its own store**. Built test-first from
[`SPEC-V2.2.md`](SPEC-V2.2.md). This is an **additive minor release**: absent
`--workspace`, every command behaves exactly as before and single-repo
`conformance.json` remains byte-identical.

### Added

- **Auto-detection + manifest** (`src/workspace.rs`). `cce workspace init [<dir>]
  [--force]` walks the root under the standard ignore rules and detects members by
  §3 markers — `*.gemspec` ⇒ Ruby (`ruby-engine` when an `app/`, `config/routes.rb`
  or `lib/**/engine.rb` marker is present, else `ruby-gem`); `Gemfile` +
  `config/application.rb` ⇒ `rails-app`; `package.json` ⇒ `typescript` (with
  `tsconfig.json`) or `javascript`. Members do **not** nest. Writes a deterministic
  `<dir>/.cce/workspace.yml` (members sorted by path, names collision-suffixed).
  Hand-written manifests are honoured. `cce workspace list` prints members + edges.
- **Federated indexing** — `cce index --workspace [<dir>]` indexes each member into
  its **own** `<member>/.cce/index.json` via the normal pipeline (language packs +
  secret scrubbing inherited). A member's store is **byte-identical to indexing that
  member standalone** (asserted). Then builds `<dir>/.cce/workspace-graph.json`.
- **Cross-member dependency edges (Level 1)** (SPEC-V2.2 §5). Declared deps are
  extracted from `*.gemspec` (`add_dependency`/`add_runtime_dependency`/
  `add_development_dependency`), `Gemfile` (`gem "name"`), and `package.json`
  (`dependencies`/`devDependencies`/`peerDependencies`); an edge `A → B` is recorded
  (with its `via`) when a dep `A` declares matches member `B`'s `package` or `name`.
  Deterministic: edges sorted by `(from, to, via)`.
- **Federated search** — `cce search "q" --workspace [<dir>] [--package a,b]
  [--top-k N] [--no-graph] [--json]`. Defined to equal the standard §6 retrieval run
  over the **union** of in-scope members' chunks (BM25 stats over the union;
  diversity key `(member, file_path)`). Each result is tagged with its `package` and
  member-relative `file_path`. Graph expansion adds the union of members' intra-store
  import graphs **plus** cross-member edges (a top result in `A` expands into a
  dependency target `B`). `--package` scopes to named members (errors on an unknown
  name).
- **Workspace stats & dashboard** — `cce stats --workspace` (per-member + totals +
  edges) and `cce dashboard --workspace` (a roll-up over every member's
  `metrics.jsonl` plus a `by_package` breakdown; loopback-only, read-only,
  self-contained, unchanged posture).
- Fixture ecosystem `test/fixture/workspace/` (`app` / `billing` / `web`) plus 10
  end-to-end CLI tests and unit tests covering detection, each dependency extractor,
  per-member byte-identical isolation, federation-equals-union, `--package` scoping
  (+ unknown-name error), the cross-member graph hop, stats and dashboard roll-up,
  and a re-assert that single-repo `conformance.json` is byte-identical.

### Changed

- `retriever` is refactored to expose `rank_core` (the §6 ranking without graph
  expansion) so federated search runs the **identical** pipeline over the union
  corpus. `store::Index::from_parts` and `graph_store::Graph::{out_pairs,from_pairs}`
  support building the combined corpus. Single-repo behaviour is unchanged.
- New pinned dependency `serde_yaml = "=0.9.34"` (parsing hand-written manifests;
  the manifest is emitted by a byte-deterministic hand-rolled writer).
- Version bumped to **2.2.0** (`Cargo.toml`, `CITATION.cff`).

## [2.1.0] - 2026-07-05

**Secret & sensitive-file protection**, built test-first from
[`SPEC-V2.1.md`](SPEC-V2.1.md). Indexing becomes **secret-safe by default** in two
layers, with an explicit opt-out. This is an **additive minor release**: the base
engine is untouched and `conformance.json` remains byte-identical.

### Added

- **Layer 1 — sensitive files are never read** (`src/sensitive.rs`). Before the
  walker reads a file, its basename is tested against a fixed policy: sensitive
  extensions (`pem`, `key`, `p12`, `pfx`, `keystore`, `jks`, `ppk`, `der`, `asc`),
  exact basenames (`credentials.*`, `secrets.*`, `.netrc`, `.pgpass`, `.htpasswd`,
  `.dockercfg`, `kubeconfig`, `id_rsa`/`id_dsa`/`id_ecdsa`/`id_ed25519`), and the
  **dotenv rule** (`.env` / `.env.*` are sensitive **except** safe templates ending
  `.example`/`.sample`/`.template`/`.dist`). Skipped files are counted separately
  as **`sensitive skipped`** in the `index` summary and never read into memory.
- **Layer 2 — secrets are redacted before chunking** (`src/redactor.rs`). Each
  indexed file's content is scrubbed for high-confidence secrets — private-key
  blocks, AWS/GitHub/Slack/Stripe/OpenAI/Anthropic/Google keys, JWTs, and a
  guarded generic `key = value` assignment — replaced with `[REDACTED:<LABEL>]`
  **before** it is chunked, embedded, or stored, so the store never contains the
  raw value and `chunk_id`/`token_count` derive from the redacted text. A
  placeholder guard leaves documentation examples (`API_KEY="your-api-key-here"`),
  interpolations, and literals untouched. Redaction is deterministic, so the
  cross-language equivalence guarantee still holds.
- **`--allow-secrets`** flag on `cce index` (default off ⇒ protection **on**)
  disables both layers for a run and prints a warning; content is then indexed
  verbatim.
- Fixture corpus `test/fixture/secrets/` (`.env`, `.env.example`, `id_rsa`,
  `config.rb`) plus an end-to-end acceptance test of the skip/redact/opt-out
  behaviour.
- Test suite grows to 154 hermetic tests (+1 `#[ignore]` Ollama) at 95.08% line
  coverage (`cargo llvm-cov`).

### Changed

- `cce index` summary adds a `sensitive skipped : N` line (and widens the label
  column). No change to the store schema or to `conformance.json`.
- New pinned dependency: `regex = "=1.12.4"` (redaction patterns).

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

[Unreleased]: https://github.com/davidslv/cce-rust/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/davidslv/cce-rust/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/davidslv/cce-rust/compare/v1.1.0...v2.0.0
[1.1.0]: https://github.com/davidslv/cce-rust/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/davidslv/cce-rust/releases/tag/v1.0.0
