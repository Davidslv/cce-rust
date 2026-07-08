# Measuring retrieval quality — `cce relevance`

CCE's measurement story has three legs:

| Command | Question it answers |
|---|---|
| `cce conformance` | Is the output **stable**? (byte-identity across runs and implementations) |
| `cce bench` / `cce eval` | Is it **fast and cheap**? (latency, token/cost savings) |
| `cce relevance` | Is the **ranking any good**? (precision@k, recall, MRR, F1 vs labeled fixtures) |

`cce relevance` runs labeled queries through the **real retrieval pipeline** at a
named backend configuration and scores each against expected-result anchors with
standard IR metrics. It is a *harness*: it never changes ranking behavior, it
measures it — so a proposed ranking change can show exactly which queries it
helps and which it hurts, before it merges.

## Quick start

```bash
$ cce relevance eval/relevance/code.jsonl
CCE relevance — ranking quality vs labeled fixtures (cce.relevance/v1)
  corpus  : eval/relevance/../../test/fixture/samples
  embedder: hash
  queries : 6

  backend            P@k      recall         MRR          F1
  bm25          0.500000    1.000000    0.888889    0.623016
  vector        0.533333    1.000000    1.000000    0.646164
  hybrid        0.500000    1.000000    0.916667    0.623016
```

Two starter fixture sets ship with the repo:

- [`eval/relevance/code.jsonl`](../eval/relevance/code.jsonl) — code queries over
  the seven-file, six-language conformance sample corpus
  (`test/fixture/samples`).
- [`eval/relevance/docs.jsonl`](../eval/relevance/docs.jsonl) — knowledge-style
  queries over a small synthetic markdown corpus
  (`eval/relevance/docs-corpus/`).

Both are offline, deterministic (hash embedder), and gated in CI: the code set's
`--json` report is **byte-pinned** against
`test/fixture/relevance/code.golden.json`, the same golden discipline as
`conformance.json`.

## The fixture contract (`cce.relevance/v1`)

A fixture set is NDJSON — one JSON object per line — and a **documented,
stable contract**, like the `cce.knowledge/v1` feed: teams can point the harness
at their own private fixture sets over their own corpora.

**Header line** (optional, first non-blank line):

```json
{"schema":"cce.relevance/v1","corpus":"../../test/fixture/samples"}
```

- `schema` — must be `cce.relevance/v1`.
- `corpus` — default corpus directory, resolved **relative to the fixture
  file**. `--dir` on the command line overrides it.

**Case lines** (one labeled query each):

```json
{"id":"code-read-config","query":"where is the read_config function","expected":["python.py"],"k":5}
```

| Field | Required | Meaning |
|---|---|---|
| `query` | yes | The search query, run verbatim through the pipeline. |
| `expected` | yes | Non-empty array of **anchors** (below): what a good top-k should surface. |
| `id` | no | Stable case name for reports/deltas (default: `q<line-number>`). |
| `k` | no | The cut-off the metrics are computed at (default: 10, the search default). |

Unknown fields are ignored, so a fixture set may carry its own annotations.

**Anchors** name an expected result at file or chunk-kind granularity:

| Form | Example | Matches a result when… |
|---|---|---|
| `path` | `"auth.py"` | its `file_path` equals the path |
| `path#kind` | `"auth.py#function_definition"` | file **and** tree-sitter `kind` both match |
| `#kind` | `"#interface_declaration"` | its `kind` matches, in any file |

Paths are the store-relative `file_path` the index records (what `cce search`
prints). Kinds are the exact tree-sitter node types `conformance.json` lists.

## Metrics

For each case, only the **top-k** returned results are considered. A result is
*relevant* if it matches any anchor; an anchor is *matched* if any considered
result matches it.

- **precision@k** — relevant results in the top-k, divided by `k` (by `k` even
  when fewer results return: returning less than asked is a ranking outcome).
- **recall** — matched anchors over total anchors.
- **MRR** — 1/rank of the first relevant result (0 when none).
- **F1** — harmonic mean of precision@k and recall.

Per-backend aggregates are macro-averages (mean over queries, each query
weighted equally). Note that with file-level anchors, precision@k is naturally
capped below 1.0 when a corpus has fewer relevant chunks than `k` — compare
scores against each other and over time, not against an absolute 1.0.

## Backends

Every backend is an **existing pipeline entry point** — the harness measures the
real thing, never a reimplementation:

| `--backend` | What runs |
|---|---|
| `bm25` | `retriever::bm25_only_search` — keyword-only ranking (the explicit issue-#30 degraded mode). |
| `vector` | `vector_store::rank_by_cosine` — pure cosine order (the SPEC §6.2 candidate list, before fusion). |
| `hybrid` | `retriever::search` — the full SPEC §6 pipeline `cce search` serves: RRF fusion, confidence blend, path penalty, diversity cap, graph expansion. |

Default is all three. `--store <path>` evaluates an existing persisted store
instead of indexing a corpus directory (an ollama-built store requires a
reachable Ollama, same refusal as `cce search`).

## Comparison mode — gating a ranking change

`--compare A,B` scores exactly two backends and prints per-query deltas, so a
proposed change shows exactly which queries it helps or hurts:

```bash
$ cce relevance eval/relevance/code.jsonl --compare bm25,hybrid
...
  per-query deltas (bm25 → hybrid; positive = hybrid wins)
  query                           dP@k     drecall        dMRR         dF1
  code-read-config           +0.000000   +0.000000   +0.166667   +0.000000
  ...
  mean                       +0.000000   +0.000000   +0.027778   +0.000000
```

The intended workflow for any ranking change (RRF weights, boosts, fusion
parameters):

1. Run `cce relevance <fixtures> --json` at the current configuration and save it.
2. Apply the change; run again; diff — or express the change as a backend and
   use `--compare`.
3. Put the per-query delta table in the PR. A regression on a labeled query is a
   conscious, visible trade-off, not a surprise.

## JSON report (`--json`)

The stable `cce.relevance.report/v1` shape: pretty-printed, alphabetical keys,
scores as fixed 6-decimal strings (the same grammar discipline as
`cce search --json`), one trailing newline. Deterministic for deterministic
backends, hence byte-pinnable:

```json
{
  "backends": [
    {
      "backend": "bm25",
      "f1": "0.623016",
      "mrr": "0.888889",
      "per_query": [ { "f1": "0.571429", "first_relevant_rank": 3, "id": "code-read-config", "k": 5, "mrr": "0.333333", "precision_at_k": "0.400000", "recall": "1.000000" } ],
      "precision_at_k": "0.500000",
      "recall": "1.000000"
    }
  ],
  "corpus": "eval/relevance/../../test/fixture/samples",
  "embedder": "hash",
  "queries": 6,
  "schema": "cce.relevance.report/v1"
}
```

If an intended ranking or report change moves the golden, regenerate it from the
repo root and review the diff like any other golden:

```bash
cargo run -- relevance eval/relevance/code.jsonl --json > test/fixture/relevance/code.golden.json
```

## Writing your own fixture set

1. Pick 10–30 queries your team actually asks (`metrics.jsonl` and the dashboard
   are a good mine — real queries beat invented ones).
2. For each, record the file(s) — or `file#kind` chunks — a good answer must
   surface, as `expected` anchors.
3. Add a header line pointing `corpus` at the repo (or pass `--dir .`), commit
   the fixture set beside your code, and run `cce relevance` in CI.

The fixture format is a stable contract; private fixture sets over private
corpora work exactly like the starter sets.
