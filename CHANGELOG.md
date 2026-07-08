# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Knowledge-corpus sync M5.1+M5.2 — the `.cck` artifact and `cce knowledge push` / `pull`
  (#56, per SPEC-SYNC-KNOWLEDGE).** A built knowledge store now travels through the same
  content-addressed cache as code indexes. `sync::knowledge_artifact` owns the canonical,
  byte-exact `.cck` container (manifest + one line per chunk in store order, sorted-keys
  compact JSON, the `.cce` base64 f64-LE embedding codec, zero provenance fields, checksum
  computed with `checksum:""`) — a pure function of `(feed, corpus_id)`, with a committed
  golden checksum for the shared fixture feed and a refusal of embedding-less Phase-A
  stores. `cce knowledge push [--corpus <id>] [--remote <url>]` exports the current local
  store and lands artifact + `current` pointer + published `corpus.json`
  (`cce.knowledgemeta/v1`, carrying `pushed_at` — deliberately outside the reproducible
  artifact; the deterministic `data_as_of` lives inside it) in one commit, then applies
  per-corpus `knowledge.sync.retention` (`keep-last-<n>` prunes oldest by the cache repo's
  commit order; the `current` snapshot is never pruned; prune failures warn, never fail the
  push). `cce knowledge pull [--corpus <id>] [--latest | --snapshot <id>] [--force]
  [--remote <url>]` verifies the manifest checksum (a mismatch is a hard failure naming the
  key) and installs into `.cce/knowledge/` **byte-identical to a local ingest**, recording
  the knowledge sync marker (`synced.json` with `installed_sha256`, the #55 mechanism —
  the `verify --checksum-only` surface wires up in M5.3). Guards per the spec: corpus_id is
  never derived (explicit `--corpus` or `knowledge.sync.corpus_id`, validated
  sanitize-stable); pulling a different corpus refuses without `--force`; the raw feed
  never travels and a planted secret arrives redacted in the artifact (`knowledge index`
  has no bypass flag — asserted). Config: `knowledge.sync.corpus_id` / `remote` (per-corpus
  §4.3 override; default `sync.remote`) / `retention`. `serde_json` gains the
  `float_roundtrip` feature so a loaded store's embeddings parse back to the exact doubles
  that were written (push exports the loaded store; without it the `.cck` drifted a ULP
  from a local ingest). Additive throughout: `SYNC_FORMAT_VERSION`, `conformance.json`,
  code artifacts, and every existing golden are untouched (asserted, not assumed — a
  knowledge corpus beside code artifacts leaves `sync list --json` byte-identical).
- **Knowledge-corpus sync M5.3+M5.4 — the consumer surface and the ingestion reference
  (#56, completing SPEC-SYNC-KNOWLEDGE).** Corpora are now first-class on every consumer
  surface. `cce sync list` grows the §6 knowledge section: a human block after the repos
  table (corpus / current / snapshots / LFS-aware bytes / data as-of) and an OPTIONAL
  `knowledge` array on the unchanged `cce.synclist/v1` JSON — emitted only when the cache
  carries a corpus, so knowledge-free listings stay byte-identical (nullable fields stay
  present as `null`). `cce sync pull --all [--corpus <id>]` installs the cache's corpus
  into the consumer workspace root `.cce/knowledge/` via the `knowledge pull` machinery
  verbatim (store, `current`, and marker byte-identical to a direct pull): an explicit
  `--corpus` wins, a single-corpus cache auto-installs, several corpora warn-and-skip
  naming the ids (one active corpus per root; member pulls never fail because of
  knowledge), and refresh is marker-idempotent — an unmoved remote `current` reports
  `up-to-date` with no fetch, a moved one refreshes exactly the corpus. `cce sync verify
  --checksum-only` gains the knowledge row: re-hash of the installed snapshot against the
  marker's `installed_sha256`, with member semantics (pass row; a mismatch fails loudly
  naming the corpus — plus the honest sharpening that knowledge has NO rebuild-verify
  escalation path at all; a marker without the hash is an explicit notice at exit 0), and
  a knowledge-only root verifies too. MCP `index_status` gains the §4.4 knowledge block
  (corpus or `(local ingest)`, snapshot, records/chunks, data as-of, best-effort
  offline-safe `remote current` / `behind remote` mirroring the code freshness rules);
  reports without a knowledge store are byte-identical. M5.4 ships the reference
  scheduled-adapter workflow `docs/ci/cce-knowledge-sync.yml` (fetch → emit
  `cce.knowledge/v1` → `cce knowledge index` (redacts) → `cce knowledge push`; a builder
  job, never a serving process; the feed is ephemeral and never committed; disjoint
  source-READ vs cache-WRITE secrets) and the documentation pass: docs/knowledge.md M5
  un-deferred with the full sync/consumer/freshness/trust story, docs/sync.md consumer
  mode covers corpora in `list`/`pull --all`/`verify`, docs/mcp.md documents the
  `index_status` knowledge block, README and llms.txt updated. One pinned surface moved
  by design: the #69 additivity test now asserts the M5.3 shape (every pre-existing
  listing field byte-stable beside a corpus; the corpus visible only as the new optional
  key).

### Documentation
- **SPEC-SYNC-KNOWLEDGE.md — the normative build spec for M5, knowledge-corpus sync (#56).**
  The SPEC-SYNC pattern reapplied to the v2.6 knowledge system: a canonical, provenance-free
  `.cck` corpus artifact (the built, redacted store — never the raw feed) under an additive
  `knowledge/<contract_version>/<corpus_id>/` key space in the same git+LFS cache, with a
  `current` pointer and a published `corpus.json` per corpus. Settles the six M5 decisions
  normatively (corpus identity, the honest trust-the-pusher posture with a code-vs-knowledge
  comparison table, access boundary, freshness signals, per-corpus retention, index-time
  redaction), specs `cce knowledge push/pull`, the `cce sync list` knowledge section (still
  `cce.synclist/v1` — additive optional key, knowledge-free listings byte-identical),
  `pull --all` corpus install at the workspace root, and `verify --checksum-only` coverage,
  plus the CI-cron builder reference workflow and milestones M5.1–M5.4. Spec-first: no
  implementation in this change; `SYNC_FORMAT_VERSION`, code artifacts, and all goldens
  untouched. `docs/knowledge.md`'s M5 deferral note now points at the spec.

## [2.6.9] - 2026-07-08

### Added
- **`cce sync list [--remote <url>] [--json]` — enumerate what a sync cache holds (#53).**
  The discovery half of consumer mode: one row per `repo_id` with its **latest sha** (the
  `refs/<branch>` pointer `pull --latest` reads — `-`/`null` when a repo has no pointer yet),
  **artifact count**, and **total artifact bytes** (LFS-aware: an LFS pointer reports its
  recorded artifact size, not the ~130-byte pointer file). Wires up the previously
  CLI-unreachable `SyncRemote::list` (#37/#50), keeping its pinned graceful-skip of
  non-artifact cache entries. Read-only — it never mutates the cache or the local `.cce/` —
  and repo-less: a bare directory plus `--remote <url>` is sufficient. Rows sort by `repo_id`;
  an empty cache is a friendly message (exit 0); an unreachable remote is a clear non-zero
  error. `--json` emits the stable, byte-pinned `cce.synclist/v1` shape.
  `SYNC_FORMAT_VERSION`, `conformance.json`, and every existing golden are untouched.
- **`cce sync pull --all --into <dir> [--remote <url>]` — the one-command repo-less consumer
  workspace (#54).** From a bare directory: enumerates the cache (the #53 `sync list`
  machinery), pulls every `repo_id`'s latest artifact into `<dir>/<member>/.cce/`, and
  synthesizes `<dir>/.cce/workspace.yml` plus the root and per-member `.cce/config`, so
  `cce search --workspace <dir>` and `cce mcp --workspace --dir <dir>` work immediately —
  zero source checkouts, each member federated at its own independent sha. Members are
  short-named from the repo_id's last `__` segment (`-2`/`-3` on collision); the full
  repo_id lives in the member's config so per-member pulls keep working. Repos without a
  latest pointer are warned and skipped, never fatal. Re-runs are idempotent refreshes:
  only members whose latest pointer moved are re-pulled, new repo_ids join the workspace,
  and vanished ones are warned about but never deleted. Synthesized manifests use the new
  neutral `type: store-only` member type (a member with no source to classify); detection
  never emits it and hand-written manifests stay byte-identical. Consumer mode (including
  the repo-less single-member `--latest`/`--commit` pull) is now documented in
  `docs/sync.md`. `SYNC_FORMAT_VERSION`, `conformance.json`, and every existing golden are
  untouched.
- **The self-describing cache — published workspace metadata + `cce sync verify
  --checksum-only` (#55).** Consumer mode 3/3. `cce sync push --workspace` now also publishes
  the canonical `workspace.yml` and the derived cross-member `workspace-graph.json` at
  well-known keys under the workspace's **base** repo_id
  (`hash/<ver>/<base>/workspace.yml` / `…/workspace-graph.json`) — additive by construction
  (neither an artifact nor a `refs/` pointer; SPEC-SYNC §3 now states the additive-keys rule
  normatively). The pull paths consume it: `pull --workspace` installs the published graph,
  merges the real member types/packages into the local manifest (matched by name; the local
  path wins), and can bootstrap a repo-less consumer with no manifest at all; `pull --all`
  discovers every published manifest via the extended `sync list` machinery, enriches exactly
  the members each manifest covers, and installs the merged graphs rewritten to the consumer
  member names (member-name collisions across workspaces: first in repo_id order keeps the
  bare name, later ones stay at their `-2`/`-3` names, warned). Result: a repo-less federated
  search regains **cross-member graph expansion**, byte-identical to the source-side
  workspace. `cce sync verify --checksum-only` gives consumers a real integrity check with
  zero source checkout: `pull` records the SHA-256 of the exact `index.json` bytes it
  installs (an additive `installed_sha256` field in `.cce/synced.json`), and verify re-hashes
  the on-disk file against it — **version-independent** ("has this file changed since
  pull"), so artifacts pushed by older cce versions verify exactly like current ones
  (live-verified against a mixed-version cache; an export-based comparison would false-fail
  them). Failures are loud and name the member; a marker written by an older cce (no
  recorded hash) is an explicit exit-0 re-pull notice, never a false failure. Documented
  caveat: detects corruption, not a malicious build (true `artifact == build(sha)`
  verification stays with source-holders/CI). Also from live review: a `pull --all` refresh
  now **re-adopts** a member directory whose `.cce/config` went missing (matched by name,
  noted in the report) instead of duplicating it as `<name>-2`. Caches without
  published metadata, plain single-member pulls, `SYNC_FORMAT_VERSION`, `conformance.json`,
  and every existing golden are untouched.

### Documentation
- **Consumer-mode documentation sweep (pre-v2.6.9).** The whole doc surface now tells the
  #53/#54/#55 story coherently: a "consume a team cache" recipe in `docs/how-to.md` (the
  flagship repo-less flow), consumer-mode/`store-only` coverage in `docs/workspace.md`, a
  repo-less agent-context note in `docs/mcp.md`, the `list`/`pull --all` CLI surface in
  SPEC-SYNC §5, a "consumer mode over a server" decision entry in `docs/DECISIONS.md`,
  refreshed module-map/`llms.txt`/README index rows, the `Cargo.toml` description, and
  current test counts (605) in README/AGENTS/CONTRIBUTING/getting-started/llms.txt.

## [2.6.8] - 2026-07-08

### Changed
- **Index-time embedding now batches chunks through `try_embed_batch` (#38).** The store build
  path used to embed one chunk per call — one HTTP request per chunk on the Ollama backend, so a
  repo with tens of thousands of chunks cost tens of thousands of sequential round-trips. Chunks
  are now embedded in bounded batches of `EMBED_BATCH_SIZE` (64, pinned in `src/config.rs`), so
  indexing issues `ceil(chunks / 64)` requests instead of one per chunk (measured on a 300-file /
  600-chunk synthetic repo against a 10 ms-latency stub: 601 → 11 requests, ~10.2 s → ~0.2 s).
  The fail-loud policy (#30) holds at batch granularity: a failed or count-mismatched batch aborts
  the index naming the batch's file span, and nothing is persisted — never empty or misaligned
  vectors. The hash embedder is untouched (its default batch impl maps the same pure per-text
  embed over each batch), so all goldens and `conformance.json` are byte-identical.

### Fixed
- **The chunkers survive pathologically nested input — iterative tree walks, no SIGSEGV (#49).**
  A property-suite CI run died with SIGSEGV before proptest could persist the failing seed. Two
  crash classes were reproduced deterministically and fixed. (1) The code and markdown chunkers'
  per-node **recursive** AST walks (`collect_chunks`, `visit_pre`, the heading/inline walks)
  overflowed the thread stack on deeply nested input — measured crash at depth ~219 on a 256 KiB
  stack and ~875–1748 (grammar-dependent) at the 2 MiB Rust test-thread default, while tree-sitter
  itself parses the same input fine at depth 500k. All walks are now **iterative `TreeCursor`
  loops** with identical pre-order emission, so chunk output is byte-identical for every input.
  (2) tree-sitter-md's external scanner serializes its open-block stack into tree-sitter's fixed
  1024-byte buffer **without a bounds check**: ~255 simultaneously open blocks (e.g. one line of
  255 `>` characters) is an assert-abort in debug and a buffer overrun (SIGSEGV) in release,
  independent of stack size and uncatchable from Rust. `chunk_markdown` now computes a conservative
  per-line upper bound on open-block depth **before parsing** and degrades estimated-deeper-than-192
  input to the existing deterministic whole-doc fallback chunk — fail-safe, never crash. A
  deterministic regression suite (`tests/deep_nesting.rs`) chunks nesting just under and far past
  the old thresholds on a 256 KiB thread, and each chunker property case now runs on a 16 MiB
  thread so any future crash becomes a persistable proptest counterexample instead of a process
  kill. All goldens and `conformance.json` are byte-identical.
- **`cce search --workspace --package ""` now errors loudly instead of silently returning no
  results (#45).** An empty-but-present `--package` value (`""`, `","`, whitespace — e.g. an unset
  shell variable in `--package "$PKG"`) used to parse to an empty scope, federate over zero members,
  and print nothing, bypassing the #26 unknown-token error. `parse_scope` now lives in
  `cce::federation` and rejects a scope with no usable token with an actionable message
  (`--package requires at least one member or package name (e.g. --package app,billing)`); the MCP
  `context_search` `package` argument goes through the same parser, so `{"package": ""}` gets the
  same friendly guidance instead of silent no-results. Valid scopes are byte-identical.

### Added
- **Binary-level error-path tests: corrupt store, malformed manifest, garbage remote listing,
  dashboard CLI (#37).** Four real-world corruption scenarios are now pinned by driving the real
  `cce` binary: a truncated-JSON or binary-junk store makes `search`/`stats` exit non-zero with the
  friendly `could not load store …` message (never a panic); a syntactically broken
  `.cce/workspace.yml` surfaces `invalid workspace.yml: …` from `search --workspace` and
  `stats --workspace`; non-artifact entries in a sync remote's ref listing are skipped gracefully
  by `SyncRemote::list` (unit-level — no CLI command reaches the listing parser today); and
  `cce dashboard --port 0 --no-open` (plus the `--workspace` variant) binds an ephemeral loopback
  port, prints the URL, and answers `/api/health` with 200 + valid JSON. Test-only — no behavior
  change.
- **Tests for `src/main.rs` and a byte-pinned `search --json` golden (#32).** The CLI entry point
  (~1,300 lines) previously had zero tests. It now has a unit suite pinning current behavior of the
  pure helpers — `parse_scope` comma/whitespace/empty-segment edges, `resolve_read_store` /
  `resolve_metrics_path` / `metrics_beside_store` precedence (explicit `--metrics` wins, else beside
  the resolved `--store`, else `<root>/.cce/metrics.jsonl`) — plus byte-pinned goldens for the
  script-facing `results_json` / `fed_results_json` shapes (field order, 6-decimal string scores
  incl. round-half-away-from-zero, `query_id: null` when metrics are off, trailing newline), and a
  binary-level `tests/cli.rs` test pinning the parsed `--json` field set. Test-only — no behavior
  change; all existing goldens and `conformance.json` unchanged.
- **Automated, tag-driven releases.** Pushing a `vX.Y.Z` tag now re-runs every CI gate on the tagged
  commit, verifies the tag matches `Cargo.toml` and that this file has a matching section, builds
  release binaries for macOS (arm64/x86_64) and Linux (x86_64/arm64), and publishes a GitHub Release
  with this file's section as the notes plus a `SHA256SUMS`. Process documented in `RELEASING.md`;
  README gains a prebuilt-binary install path. (Repo infrastructure — the `cce` binary is unchanged.)
- **Property-based tests for the chunkers and the pinned token rule (#33).** A new `proptest` suite
  (`tests/property_chunkers.rs`) generates adversarial-but-legal source for all six language packs
  (unicode identifiers, CRLF line endings, trailing whitespace, missing final newline, empty and
  comment-only files, deeply nested definitions, raw printable-unicode garbage) and markdown
  (ATX/setext headings, preambles, fenced code blocks containing `#` lines, varied split budgets),
  and asserts the chunkers' documented invariants on every input: in-bounds ordered line ranges,
  content as an exact byte slice of the input, pre-order nested-or-disjoint emission, determinism,
  `chunk_id` recomputable from the persisted fields, the pinned `max(1, floor(bytes/4))` token rule,
  and markdown section ordering/coverage. Test-only: goldens, `conformance.json`, and the `cce`
  binary are unchanged.

### Documentation
- **v2.6 documentation sweep (#34).** Re-ran the gapless-docs discipline (#11, last executed at
  v2.5.5) over the v2.6.0–v2.6.7 surface. The knowledge track (`cce knowledge index`, the
  `cce.knowledge/v1` contract, the `context_search` `source: code|knowledge|both` blend, provenance +
  staleness weighting, the `knowledge.*` config keys) now appears in `docs/knowledge.md` (M4 section),
  `docs/mcp.md` (the `source` schema property), the README, and the getting-started/how-to/
  how-it-works/architecture cross-references; the v2.6.3 gitignore-aware walker (committed
  `.gitignore` only — builder independence) is documented in the README, guides, architecture, and
  sync's rationale; `docs/sync.md` states that push always rebuilds from source (v2.6.2);
  `docs/workspace.md` + `docs/architecture.md` carry the v2.6.4 `--package` semantics (name or
  `package:` field, loud error with the available list) and the v2.6.7 MCP caching instead of the
  stale "reloaded per query" claim. Stale pins fixed: `cce 2.5.5` / `--tag v2.3.0/v2.4.0` examples,
  the retired `built_at` CI comment, the `--top-k` default (10, not 5), and the 416/500 test counts
  (now 540); the Cargo.toml `description` extends through v2.6 (metadata only). Docs-only — no engine
  change; `conformance.json` and all goldens are byte-identical.

## [2.6.7] - 2026-07-06

### Changed
- **The MCP server caches the single-repo index and the knowledge store across calls (#31).** The
  long-lived `cce mcp` server did O(corpus) work on EVERY tool call: the single-repo path re-read +
  JSON-parsed the whole store and rebuilt the entire BM25 index and import graph per request
  (`Index::load`), and the knowledge path additionally re-ran the embedder over legacy chunks and
  rebuilt a BM25 index per query. Extending the #26 workspace pattern, `McpServer` now caches the
  loaded `Index` and the loaded+embedded knowledge store, keyed by a cheap freshness fingerprint —
  store-file `mtime`+length from one `fs::metadata` call per tool call (for knowledge: the `current`
  pointer plus the snapshot artifact it names). A re-index, a knowledge re-ingest, or a
  `cce sync pull` (startup auto-pull or mid-session) invalidates on the next call; a **deleted store
  drops the cache and serves the friendly missing-index message** — never a stale answer. The #26
  workspace union cache (previously cached forever) now carries the combined fingerprint of its
  in-scope member store files, so a member re-index mid-session is picked up without restarting
  `cce mcp`. Warm calls sit under the #41 per-query embedder choice (BM25-only degradation
  unchanged). Perf only — **ranked results and MCP result text are byte-identical warm vs cold**
  (regression-tested), CLI one-shot paths are untouched, and `conformance.json` + all goldens are
  unchanged. On a synthetic 3.2k-chunk store driven over stdio, a warm MCP `context_search` drops
  from ~23ms to ~2ms per call (~10×); a warm knowledge query (300 records) from ~6ms to ~1ms (~5×) —
  and the win scales with corpus size, since the removed work was O(corpus) per call.

## [2.6.6] - 2026-07-06

### Fixed
- **The Ollama embedder fails loud instead of degrading silently (#30).** Three compounding silent
  failures in the opt-in `--embedder ollama` path are gone. (1) *Index time:* an embedding failure —
  Ollama unreachable at start, or dying mid-index — now **aborts `cce index` with a clear error and
  writes no store** (previously `embed_batch` swallowed errors into empty vectors, which were persisted
  and scored cosine 0 forever, invisible to vector recall). There is deliberately **no fallback to the
  hash embedder at index time** either: that would poison the store's declared embedder space just as
  badly. (2) *Query time, CLI:* `cce search` (and `--workspace`) on an ollama-built store with Ollama
  down now **errors with guidance** (start Ollama, or re-index with the default hash embedder) instead
  of silently embedding the query with the hash backend — cosine across two unrelated vector spaces is
  meaningless. (3) *Query time, MCP:* `context_search` follows the friendly-error pattern — it does not
  crash the session, and now **degrades to keyword-only (BM25) results under a pinned `NOTICE:` line**,
  so the agent keeps getting results while the degradation stays visible. The `Embedder` trait lost its
  silent-empty-vector batch path (`embed_batch` → fallible `try_embed`/`try_embed_batch`), the endpoint
  and model are overridable via `CCE_OLLAMA_URL`/`CCE_OLLAMA_MODEL` (which also keeps the new
  failure-policy tests hermetic — a loopback HTTP stub, never a real server), and the docs that
  described the silent fallback as a feature are rewritten. The default hash-embedder path, the
  knowledge store (hash-only), `conformance.json`, and all goldens are **byte-identical**.

## [2.6.5] - 2026-07-06

### Fixed
- **The workspace dashboard now shows `cce mcp --workspace` (agent) searches (#28).** In workspace mode
  the MCP server writes `search` events to the workspace-root `.cce/metrics.jsonl`, but
  `cce dashboard --workspace` aggregated only the member logs — so agent/MCP searches never appeared in
  `totals`, `recent_searches`, or `by_source`, contradicting `docs/mcp.md`. The workspace dashboard now
  folds the root log into its roll-up (guarded against double-counting a member that points at the root).
  These federated searches span members and stay **out of `by_package`** by design — that panel remains
  per-member. Docs aligned; per-package attribution of agent searches is left as a follow-up option.

### Changed
- **Faster, correcter workspace federation (#26).** Member stores load **without** building per-member
  BM25 (federation scores only the union's BM25), removing redundant work — full-workspace search is
  ~1.3–2× faster (a real 38.6k-chunk workspace: 3.2s→2.4s CLI). **`--package` short-circuits** to load
  only the scoped member(s) (2.08s→1.58s) and now resolves by member name **or** the `package:` field,
  **erroring with the available list** on no match (previously matched member name only and returned
  empty silently). The **MCP server caches the assembled union** per scope, so repeated
  `context_search` no longer re-federates (warm call ≈ CLI). Perf/correctness only — **ranked results
  are byte-identical** (regression-tested); keeps exact brute-force cosine (ANN deferred).

## [2.6.3] - 2026-07-06

### Fixed
- **The indexer now honors committed `.gitignore`** (#24). The walker uses ripgrep's `ignore` crate and
  skips files ignored by the repo's committed `.gitignore`, restoring the sync invariant `artifact ==
  build(sha)`. Machine-local ignore sources (`.git/info/exclude`, global `core.excludesfile`) and
  `.gitignore` above the walk root are deliberately NOT honored, so artifacts stay builder-independent;
  `.git/` and `.cce/` are always skipped. Previously a gitignored-but-present file (e.g. Next's
  `next-env.d.ts`) polluted local indexes, false-failing `cce sync verify`.

### Added
- **`cce init` gitignores the cache** — appends `.cce/*` + `!.cce/workspace.yml` to the repo `.gitignore`
  (git repos only; idempotent): the local index/cache is never committed, the shared `.cce/workspace.yml`
  stays committable.

## [2.6.2] - 2026-07-06

### Fixed
- **`cce sync push` now always rebuilds the index from the working tree** before exporting, instead of
  re-exporting an existing `.cce/index.json`. A just-pulled or otherwise stale/foreign index could be
  republished verbatim under the content-address sha key, violating `artifact == build(sha)` and making
  `cce sync verify` fail. `pull`/`verify` unchanged; the Sync artifact format is byte-identical.

## [2.6.1] - 2026-07-06

### Added
- **Knowledge Sources (v2.6 Phase B)** — knowledge chunks are searchable through the same hybrid
  retrieval as code (hash embedder + BM25 + RRF). `context_search` gains an optional
  `source: code|knowledge|both` (**still 9 tools**; code-only behaviour byte-identical when no
  knowledge store). Knowledge hits carry provenance (`[knowledge] <title> — <state> · <updated_at> ·
  <url>`) with deterministic staleness weighting (recency; drop `not_planned`/`wontfix`; merged-PR
  boost) and a precision-filtered recall floor; `expand_chunk`/`related_context` work on knowledge
  chunks. Fully additive: `conformance.json` + the Sync artifact are byte-identical.

## [2.6.0] - 2026-07-05

### Added
- **Knowledge Sources (v2.6 Phase A)** — a markdown-heading chunker (tree-sitter-markdown; each `##`
  section becomes a content-addressed chunk), the neutral **`cce.knowledge/v1`** ingest contract, and
  **`cce knowledge index <file.jsonl>`** which renders + heading-chunks records into a *separate,
  snapshot-keyed knowledge store* (redacted before write; issue/doc metadata as facets). **Fully
  additive** — the code index, `conformance.json`, and the Sync artifact are byte-identical. Config:
  `markdown.max_section_tokens` (400), `knowledge.enabled`.

## [2.5.5] - 2026-07-05

### Documentation
- **v2.5 documentation sweep** — brought every doc current to the complete Savings
  Layers track (v2.5.0–v2.5.4), verified from a cold start. No engine behaviour
  change; `conformance.json` and the Sync artifact are byte-identical.
  - `README.md`: a "Token savings — honestly" section covering the seven Savings
    Layers, compact-by-default retrieval with expand-on-demand, `cce savings`, and
    the honest "vs full-file baseline — not your real end-to-end agent cost"
    framing; the MCP section now lists all **nine** tools.
  - `docs/savings.md` (new): the seven layers, the ledger, `cce savings`, the
    `cce.tokens/v1` estimator caveat, and the `cce eval` A/B harness.
  - `docs/mcp.md`: documents all **nine** MCP tools with input schemas and the
    find → expand → widen relationships, memory, summarization, and output
    compression.
  - `docs/architecture.md`, `docs/how-it-works.md`, `docs/getting-started.md`,
    `docs/how-to.md`, `docs/dashboard.md`: compact-by-default retrieval,
    `expand_chunk`, memory, and the `savings_by_layer` panel where relevant.
  - `docs/DECISIONS.md`: the key v2.5 decisions (compact-by-default and the
    structural-compact fix, memory anti-pollution, deterministic structured
    digests, grammar self-measurement, `SYNC_FORMAT_VERSION` decoupling, Rust-first
    sequencing).
  - `docs/VERIFIED.md`: a fresh cold-start transcript exercising `cce index` → the
    nine-tool `cce mcp` session (compact `context_search`, `expand_chunk`,
    `record_decision`/`session_recall`, `summarize_context`) → `cce savings`.
  - `llms.txt`, `AGENTS.md`, `CITATION.cff`: the full v2.5 surface.

## [2.5.4] - 2026-07-05

### Added
- **Grammar compression (L3)** — the MCP read-tool result grammars are byte-pinned to a compact,
  filler-free format (`context_search`, `expand_chunk`, `related_context`, `session_recall`,
  `summarize_context`), and the `grammar` savings bucket is self-measured (compact vs a pinned
  verbose baseline, via `cce.tokens/v1`). Completes the **seven-bucket savings ledger**. Additive;
  `conformance.json` and the Sync artifact unchanged. **This completes the v2.5 Savings Layers track.**

## [2.5.3] - 2026-07-05

### Added
- **Turn summarization (L6)** — a `summarize_context(scope?)` MCP tool returning a **deterministic,
  structured** digest of the session so far (files · chunks · queries · decisions touched — deduped,
  sorted, capped with `… (+N more)`) — a structured ledger digest, NOT an LLM summary, so it stays
  byte-deterministic and offline. Backed by an in-memory, wall-clock-free per-session ledger.
  `summarization.auto_tokens` config (default null = manual-only). `tools/list` is now nine tools.
  Additive; `conformance.json` and the Sync artifact unchanged.

## [2.5.2] - 2026-07-05

### Added
- **Memory recall (L5)** — a local-only, secret-scrubbed `.cce/memory.jsonl` of *validated* decisions,
  with two MCP tools: `record_decision(text, tags?, area?)` (deduped by a content-hash id; redacted
  before write) and `session_recall(query, top_k?)` (hybrid search over the memory corpus,
  precision-filtered — score ≥ 0.30 and a shared query token — to avoid context pollution). Reuses the
  retrieval engine; workspace-aware (root + members); **never pushed by Sync** (non-reproducible /
  local). `tools/list` is now eight tools. Additive; `conformance.json` and the Sync artifact unchanged.

## [2.5.1] - 2026-07-05

### Added
- **Output compression (L4)** — `cce init` writes a leveled output-rules block into `CLAUDE.md`
  (`output.level`: `off | lite | standard | max`, default **standard**): terser answers and
  changed-lines-only code edits. New MCP tool **`set_output_compression`** dials the level for the
  running session (in-memory; does not rewrite `CLAUDE.md`). `tools/list` is now six tools.
  Additive; `conformance.json` and the Sync artifact are byte-for-byte unchanged.

## [2.5.0] - 2026-07-05

The first **Savings Layers** — retrieval returns compact chunks by default, with progressive
disclosure to recover detail on demand, and a seven-bucket savings ledger + eval harness that
make token savings measurable and honest.

### Added
- **Chunk compression (L2)** — `context_search` gains `detail: signature | compact | full`
  (default `compact`). AST-driven, per language pack: a container chunk renders as its header +
  doc + the signature lines of its direct members (methods, and Ruby model DSL such as
  `has_many`/`belongs_to`/`validates`); a leaf chunk as its signature + doc. **Retrieval-time
  only** — the index, `conformance.json`, and the Sync artifact are byte-for-byte unchanged.
- **Progressive disclosure (L7)** — new MCP tools `expand_chunk` (recover the full body / file
  slice / graph-neighbours of a chunk by `chunk_id`) and `related_context` (import-graph
  neighbours — imports **and** consumers). `tools/list` is now five tools.
- **Savings ledger** — a seven-bucket `savings` object on search events, a `savings_by_layer`
  panel on `/api/metrics`, and a `cce savings` command with an embedded offline price table.
  Every surface is labelled *"vs full-file baseline — not your real end-to-end agent cost."*
- **Deterministic token counter** `cce.tokens/v1` (`max(1, floor(bytes/4))`) and an in-repo A/B
  **eval harness** (`cce eval`) — correctness-gated, cost-primary.

### Notes
- Tool descriptions carry explicit trigger conditions and steer the agent to `expand_chunk`
  instead of re-searching (measured on a real ecosystem to matter).
- Rust-first: all new formats are byte-pinned so cce-ruby can reconcile to them in a later track.
- `SYNC_FORMAT_VERSION` stays `2.3` (decoupled from the app version); the Sync golden is unchanged.

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
  **index freshness** (indexed `sha`, local-vs-`sync-pull` source), and
  **secret-safety** (sensitive-files-skipped count). Every panel is **purely
  log-derived, so the dashboard makes zero network calls** and stays loopback-only,
  read-only, and self-contained (inline CSS/JS, hand-drawn SVG). Behind-remote lives
  in `cce sync status` / MCP `index_status`, not on the dashboard.
- **Metrics schema — additive fields.** `search` events carry
  `source: "cli" | "mcp"` (the CLI `search` path tags `"cli"`; the MCP
  `context_search` path tags `"mcp"`). `index` events carry `sha`, `source`
  (`"local"` for `cce index`, `"sync-pull"` for a `cce sync pull` install), and
  `sensitive_skipped`. Absent/unknown fields degrade gracefully (a pre-v2.4.1 search
  reads back as `"cli"`; an index event as `"local"`).
- **Aggregator sections.** `/api/metrics` gains `by_source`, `secret_safety`, and
  `index_freshness` (`{indexes, source, sha, indexed_ts}`) — all pure, log-derived,
  cross-language-identical — plus `totals.mean_top_score`. `by_package` (workspace)
  gains `mean_top_score` and is sorted by package. `cce sync pull` records a
  `sync-pull` index event so the pulled provenance is observable with no network call.
- **Documentation sweep** — a dedicated, **verified offline-first** section proving
  `index` / `search` / `stats` / `dashboard` / `workspace` / `cce mcp` all run with
  no network and no remote; macOS **and** Ubuntu setup with explicit prerequisites
  (toolchain, C compiler, git, git-LFS); a Sync + MCP best-practices section; and
  both an online and an offline cold-start transcript in
  [`docs/VERIFIED.md`](docs/VERIFIED.md).

### Changed

- `retriever::build_search_record` takes a `source` argument so the CLI and MCP
  search paths tag their metrics events.
- `cce sync pull` now appends a `sync-pull` `index` event to the metrics log so the
  dashboard's freshness panel is fully log-derived (no request-path network call).
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
