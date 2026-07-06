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

- `--top-k N` — number of results (default 5).
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

## Inspect a store

```bash
cce stats --dir ./my-project        # or --store <path>
```

Reports chunk count, file count, average tokens per chunk, on-disk size, a
per-language breakdown, and a per-`kind` breakdown (the exact node types).

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

## Run the quality gates locally

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

These are exactly what CI enforces. `rustfmt.toml` is the house style.
