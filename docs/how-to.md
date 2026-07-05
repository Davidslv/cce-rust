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
- `--json` — emit a JSON array of `{rank, chunk_id, file_path, start_line,
  end_line, chunk_type, score}`.

Scores are rounded to 6 decimals and ties break by `chunk_id` ascending, so
output is stable across runs (SPEC §5.3).

## Inspect a store

```bash
cce stats --dir ./my-project        # or --store <path>
```

Reports chunk count, file count, average tokens per chunk, on-disk size, and a
per-language breakdown.

## Benchmark on a real repository

```bash
cce bench /path/to/flask --name "pallets/flask@3.0.3"
```

- Runs the pipeline over a checked-out repo and writes
  [`BENCHMARKS.md`](BENCHMARKS.md).
- Records the corpus commit; by default it reads git `HEAD` of the repo, or pass
  `--commit <sha>`.
- Uses the deterministic hash embedder, so recall and token-savings numbers are
  reproducible and comparable to the Ruby sibling; latency is language-specific.

## Regenerate the conformance file

```bash
cce conformance test/fixture -o conformance.json
```

- Emits a byte-stable JSON of every chunk (path, lines, type, `chunk_id`,
  `token_count`) for the fixture.
- Designed to match the [Ruby sibling](https://github.com/davidslv/cce-ruby) on
  the same fixture. If your change alters this output, that is a deliberate,
  spec-level change — call it out (see [`../CONTRIBUTING.md`](../CONTRIBUTING.md)).

## Switch to semantic embeddings (Ollama)

```bash
# One-time: install Ollama (https://ollama.com) and pull the model
ollama pull nomic-embed-text

# Index with the semantic embedder
cce index ./my-project --embedder ollama
```

- Talks to a local Ollama server over `localhost` HTTP only. No other command
  makes network calls.
- If Ollama is unreachable, `cce` prints a warning and falls back to the hash
  embedder — indexing still succeeds.
- Ollama vectors are model-dependent, so an Ollama-built index is **not** covered
  by the conformance guarantee.

## Run the quality gates locally

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

These are exactly what CI enforces. `rustfmt.toml` is the house style.
