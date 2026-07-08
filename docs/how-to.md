# How-to recipes

Short, task-oriented recipes for `cce`. New here? Start with
[`getting-started.md`](getting-started.md). The authoritative reference for every
behaviour and constant is [`SPEC.md`](../SPEC.md) — when in doubt, the spec wins.

Examples assume `cce` is on your PATH; otherwise use `./target/release/cce`.

## Index a directory

```bash
cce index ./my-project
```

- Writes the store to `<dir>/.cce/index.json` by default. Override with
  `--store <path>`.
- Re-running is a full, idempotent rebuild — it replaces data for changed and
  removed files (chunk IDs are content-derived). See [`DECISIONS.md`](DECISIONS.md) D3.
- Files that are binary, non-UTF-8, or larger than 2 MB are skipped (reported as
  `files skipped`).
- The walk honors the repo's **committed `.gitignore`** (since v2.6.3) but
  deliberately not machine-local excludes (`.git/info/exclude`, the global
  `core.excludesfile`) or a `.gitignore` above the walk root — so the same commit
  indexes identically on every machine. `.git/` and `.cce/` are always skipped.

## Search a store

```bash
# By directory (resolves <dir>/.cce)
cce search "how does auth work" --dir ./my-project --top-k 5

# By explicit store path
cce search "process payment" --store /tmp/fix.cce --top-k 3

# Machine-readable
cce search "process payment" --dir ./my-project --json
```

Flags:

- `--top-k N` — number of results (default 10).
- `--no-graph` — skip import-graph expansion (step §6.7); results come only from
  direct vector + BM25 + RRF ranking.
- `--json` — emit an object `{query_id, results: [{rank, chunk_id, file_path,
  start_line, end_line, chunk_type, kind, score}, ...]}`. `kind` (the exact
  tree-sitter node type) is new in v2; human output shows it as `chunk_type/kind`.
- `--no-metrics` — do not append a search event to the metrics log (then
  `query_id` is null and no `query-id:` line is printed).

Scores are rounded to 6 decimals and ties break by `chunk_id` ascending, so
output is stable across runs (SPEC §5.3). Human output ends with a `query-id:`
line you can pass to `cce feedback` (see below).

## Rate a search result (feedback)

```bash
# Take the query-id printed by `cce search`, then:
cce feedback 3f9a1c0b7e21 --helpful --dir ./my-project
cce feedback 3f9a1c0b7e21 --not-helpful --note "wrong file" --dir ./my-project
```

- Exactly one of `--helpful` / `--not-helpful` is required.
- Locate the metrics log with `--dir DIR` / `--store PATH` / `--metrics PATH`
  (same log the search wrote to).
- If no search event with that id is found, `cce` warns but still records the
  feedback (see [`DECISIONS.md`](DECISIONS.md) D15).
- Feedback powers the retrieval-quality north-star and the recent-searches table
  in the dashboard.

## View the metrics dashboard

```bash
cce dashboard --dir ./my-project           # serves http://127.0.0.1:8787/
cce dashboard --store /tmp/fix.cce --port 9000
cce dashboard --metrics ./path/to/metrics.jsonl --price 5.00
```

- Serves a **loopback-only** (`127.0.0.1`), **read-only**, fully self-contained
  web page: token/cost savings and retrieval quality, each trended current-vs-prior
  with an improving/degrading indicator, plus a recent-searches table and a
  friendly empty state when there is no data yet.
- Endpoints: `GET /` (page), `GET /api/metrics` (the aggregate JSON, recomputed
  per request so it is live on refresh), `GET /api/health`.
- Flags: `--port N` (default 8787), `--price N` (USD per 1M input tokens for the
  $-saved estimate, default 3.00), `--no-open` (this build only prints the URL —
  it never auto-opens a browser). Stop with Ctrl-C.
- Nothing it serves mutates any file; it draws its own SVG charts and makes no
  external network requests. See [`dashboard.md`](dashboard.md) for the schema and
  formulas.

## See who used CCE — agent vs human (`cce usage`)

```bash
cce usage --dir ./my-project                 # all time, human block
cce usage --since 24h                        # "how much since yesterday?"
cce usage --workspace shop --since 7d        # federated: members + the root log
cce usage --source mcp                       # lead with the agent split only
cce usage --json | jq .by_source.mcp         # the versioned cce.usage/v1 projection
```

- The one-shot, CI-friendly terminal answer to *"how many times did the agent
  call CCE, and how many tokens did that save?"* — the agent (`mcp`) vs human
  (`cli`) split, savings, quality, latency, and the recent queries.
- A **pure projection** of the same aggregate `cce dashboard` serves, so the
  numbers are identical to the dashboard's for the same window; `--workspace`
  folds in the workspace-root log exactly like `cce dashboard --workspace`.
- `--since` takes a relative duration (`90m`, `24h`, `7d`, `4w`) or an ISO UTC
  instant/date; `--source` narrows the display only (the JSON always carries
  both splits). Offline, read-only, exit 0 on an empty window.
- To see savings **inside the conversation**, opt in to the MCP result footer
  (`mcp.result_footer: "on"` in `.cce/config`) — see [`mcp.md`](mcp.md).

## Inspect a store

```bash
cce stats --dir ./my-project        # or --store <path>
```

Reports chunk count, file count, average tokens per chunk, on-disk size, a
per-language breakdown, and a per-`kind` breakdown (the exact node types).

## Troubleshoot a store (doctor)

```bash
cce doctor                      # current directory (workspace-aware)
cce doctor --dir ./my-project   # a specific root
cce doctor --store <path>       # one explicit store file
```

Every `cce index` (and every `cce sync pull` install) writes a small **build
fingerprint** (`.cce/fingerprint.json`, schema `cce.fingerprint/v1`) beside the
store: the engine version, embedder id + dimensions, the chunker identity
(language-pack set, markdown split budget, nesting limit), the tokenizer rule
id, and whether redaction was on — plus a SHA-256 self-checksum and a SHA-256
of the exact store bytes it describes. `cce doctor` is the read-only check
that catches **config drift before it degrades retrieval** — "this index was
built by a different configuration than the binary reading it":

- **fingerprint vs this binary** — every mismatch is explained: a changed pack
  set means chunk_ids may not be reproducible (re-index to realign); a
  different embedder or dimension count means vector scores would compare
  across embedding spaces (the #30 failure mode); a changed tokenizer rule
  drifts token counts and the savings ledger.
- **store parse health** — chunk/file counts, plus the #30 tripwire: any chunk
  with an empty embedding is a definite failure.
- **installed-bytes check** — for pulled stores, the same re-hash
  `cce sync verify --checksum-only` performs (`installed_sha256` in
  `.cce/synced.json`): corruption or local modification since the pull.
- **knowledge store** — the contract version (`cce.knowledge/v1`), snapshot
  id, record/chunk counts, and the data's as-of date.
- **workspace mode** — with a workspace manifest at the root, every member is
  checked (store-only consumer members included) and summarized.

Doctor never mutates anything and needs no network. It exits **non-zero only
on definite corruption or config mismatch**; soft findings render as distinct
`advisory` lines and keep exit 0. A store built before fingerprints existed
gets a graceful notice — `no fingerprint recorded (store built before cce
v2.8) — re-index to enable drift detection` — and exit 0.

## List or validate the language packs

```bash
cce packs               # list the six registered packs
cce packs --validate    # run the three validator layers; non-zero exit on failure
```

- `cce packs` prints each pack's name, extensions, function/class type counts, and
  grammar node-kind count.
- `cce packs --validate` runs the structural, grammar-binding, and behavioural
  self-test layers over every pack and prints any diagnostics. Use it after adding
  or editing a pack — see [`adding-a-language.md`](adding-a-language.md).

## Benchmark on a real repository

```bash
cce bench /path/to/sinatra --lang ruby --name "sinatra/sinatra@v4.1.1"
```

- Indexes a checked-out repo **whole** (exactly as `cce index`) and runs one
  language's labeled query set (`--lang ruby | rust | typescript | c`, default
  `python`), writing [`BENCHMARKS.md`](BENCHMARKS.md). `--lang` selects only the
  query set and label — the file set is the whole repo either way, so recall and
  token-savings match the Ruby sibling exactly.
- Records the corpus commit; by default it reads git `HEAD` of the repo, or pass
  `--commit <sha>`.
- Uses the deterministic hash embedder, so recall and token-savings numbers are
  reproducible and comparable to the Ruby sibling; latency is language-specific.
- The four benchmarked corpora are Ruby (sinatra), Rust (hyperfine), TypeScript
  (zustand), and C (jq); Python/JavaScript stay validated packs but ship no
  labeled corpus.

## Read the savings ledger

```bash
cce savings --dir ./my-project          # the seven-bucket ledger + $ estimate
cce savings --dir ./my-project --json   # the same shape as /api/metrics.savings_by_layer
```

- Aggregates the per-layer token deltas recorded on every `search` event into the
  seven buckets (`retrieval`, `chunk_compression`, `grammar`, `output`, `memory`,
  `turn_summarization`, `progressive_disclosure`) plus a `total`. Purely log-derived,
  **offline** (embedded pricing in `src/pricing.json`; edit it to change the rate).
- The figures are **"vs full-file baseline — not your real end-to-end agent cost."**
  For the real delta, run the A/B eval harness below. See [`savings.md`](savings.md).
- Note: `cce search` (CLI) serves full bodies, so its `chunk_compression` bucket is
  zero; the compact chunks (and that bucket) come from the agent-facing
  `context_search` MCP tool. See [`mcp.md`](mcp.md).

## Pull the team's CI-built index (CCE Sync)

```bash
cce sync init --remote git@github.com:acme/cce-cache.git   # one-time, per project
cce sync pull --latest                                     # main@sha index, instantly
cce sync status                                            # remote, local sha, tree match
```

- The remote is a **content-addressed git cache**: because the hash-embedder index
  is deterministic, a cache for `repo@sha` is byte-identical no matter who built
  it. Let CI push on every merge ([`ci/cce-sync.yml`](ci/cce-sync.yml)); you only
  pull. Offline or no remote is never fatal — local commands are unaffected.
- `cce sync verify` re-indexes locally and confirms the pulled checksum when you
  want proof. Full model, permissions, and troubleshooting: [`sync.md`](sync.md).

## Consume a team cache — no source checkout (consumer mode)

Turn a whole team cache into a searchable, agent-ready workspace on a machine
with **zero source checkouts** — only the `cce` binary and git read access:

```bash
# What does the cache hold? (repo-less: a bare directory + --remote is enough)
cce sync list --remote git@github.com:acme/cce-cache.git

# Pull EVERY repo's latest index and synthesize a ready-to-search workspace
cce sync pull --all --into ctx --remote git@github.com:acme/cce-cache.git

# Search / serve it immediately — federated, member-tagged
cce search "charge invoice" ctx --workspace
cce mcp --workspace --dir ctx

# Integrity-check the pulled stores (no source, no rebuild, no network)
cce sync verify --checksum-only --dir ctx
```

- Re-running `pull --all` is an **idempotent refresh**: only members whose latest
  pointer moved are re-pulled; new repos join, vanished ones are warned about,
  never deleted.
- If the source side pushes with `cce sync push --workspace`, the cache is
  **self-describing** — consumers also get the real member types/packages and the
  cross-member dependency graph, so graph expansion works exactly as at the source.
- `verify --checksum-only` detects corruption, not a malicious build — the full
  rebuild-and-compare `cce sync verify` stays with whoever has the source (CI).
- The whole flow, naming/refresh rules, and output examples: [`sync.md`](sync.md) §7.

## Ingest a knowledge feed (issues, epics, policy docs)

```bash
cce knowledge index curated.jsonl --dir ./my-project
```

- Ingests a **`cce.knowledge/v1`** NDJSON feed (one record per line — any adapter
  that emits the contract) into a separate, snapshot-keyed store under
  `<dir>/.cce/knowledge/`. Each record is redacted, then split by markdown heading
  (`markdown.max_section_tokens`, default 400). A newer ingest supersedes the old.
- Search it through the MCP `context_search` tool's `source: code|knowledge|both`
  argument (default `both` once a store exists); hits carry a
  `[knowledge] <title> — <state> · …` provenance header and staleness weighting.
  The CLI `cce search` stays code-only.
- Fully offline; the code index and Sync artifact are untouched. See
  [`knowledge.md`](knowledge.md).

## Run the real-world A/B eval harness

```bash
cce eval eval/runs.example.jsonl --questions eval/questions.jsonl        # canned demo
cce eval eval/runs.example.jsonl --questions eval/questions.jsonl --json
```

- Aggregates recorded agent runs (off vs on) into a **correctness-gated,
  cost-primary, paired** report — the honest counterpart to `cce savings`. It does
  **not** call a model; drive a live agent with `eval/run.sh`. See
  [`eval/README.md`](../eval/README.md).

## Regenerate the conformance file

```bash
cce conformance test/fixture/samples -o conformance.json
```

- Emits a byte-stable JSON of every chunk (path, lines, `chunk_type`, `kind`,
  `chunk_id`, `token_count`) over the seven-file sample corpus (v2 shape).
- Designed to match the [Ruby sibling](https://github.com/davidslv/cce-ruby) on
  the same byte-identical samples. If your change alters this output, that is a
  deliberate, spec-level change — call it out (see
  [`../CONTRIBUTING.md`](../CONTRIBUTING.md)).

## Switch to semantic embeddings (Ollama)

```bash
# One-time: install Ollama (https://ollama.com) and pull the model
ollama pull nomic-embed-text

# Index with the semantic embedder
cce index ./my-project --embedder ollama
```

- Talks to a local Ollama server over `localhost` HTTP only (override with
  `CCE_OLLAMA_URL` / `CCE_OLLAMA_MODEL`). No other command makes network calls.
- Failures are loud, never silent (#30): if Ollama is unreachable — or fails
  mid-index — `cce index --embedder ollama` aborts with a clear error and writes
  no store. Searching an ollama-built index while Ollama is down errors with
  guidance (start Ollama, or re-index with the default hash embedder); the MCP
  `context_search` tool degrades to BM25-only results under an explicit
  `NOTICE:` line instead.
- Ollama vectors are model-dependent, so an Ollama-built index is **not** covered
  by the conformance guarantee.

## Update the installed binary (self-update)

```bash
cce update                    # → latest release: download, verify, atomic in-place swap
cce update --check            # no download; exit 0 = up to date, exit 10 = update available
cce update --version v2.6.9   # pin a release — the rollback path (downgrades warn but proceed)
cce upgrade                   # alias for all of the above
```

- Downloads the platform tarball from the project's GitHub Releases with `curl`
  and verifies it against the release's `SHA256SUMS` **before** replacing the
  running binary (an atomic rename — any failure leaves the current install
  untouched). This is the only cce command that talks HTTP, and only when
  invoked; there is no auto-check.
- `--check` is built for scripts and cron: one line of output and pinned exit
  codes (`0` up to date, `10` update available, `1` error).
- After an update, the CHANGELOG sections between your old and new version are
  printed (newest first, capped at 5, then a link to the releases page).
- Long-lived `cce mcp` / `cce dashboard` processes keep the old binary until
  restarted.
- Releases cover macOS (Apple Silicon + Intel) and Linux (x86_64 + arm64); on
  anything else, or with no `curl` on PATH, the command errors with a pointer to
  the manual install (README → Installation). If the install location isn't
  writable, re-run with `sudo` or install manually — cce never escalates
  privileges itself.

## Run the quality gates locally

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

These are exactly what CI enforces. `rustfmt.toml` is the house style.
