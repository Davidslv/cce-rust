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
  queries : 7

  backend            P@k      recall         MRR          F1
  bm25          0.457143    1.000000    0.904762    0.581633
  vector        0.485714    1.000000    1.000000    0.601474
  hybrid        0.457143    1.000000    0.928571    0.581633

  token-level span metrics (1 of 7 queries carry ranged anchors)
  backend          tok-P  tok-recall     tok-IoU
  bm25          0.294118    1.000000    0.294118
  vector        0.238095    1.000000    0.238095
  hybrid        0.223881    1.000000    0.223881
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

**Anchors** name an expected result at file, chunk-kind, or line granularity:

| Form | Example | Matches a result when… |
|---|---|---|
| `path` | `"auth.py"` | its `file_path` equals the path |
| `path#kind` | `"auth.py#function_definition"` | file **and** tree-sitter `kind` both match |
| `#kind` | `"#interface_declaration"` | its `kind` matches, in any file |
| `path@a-b` | `"auth.py@10-42"` | file matches **and** the result's line span overlaps lines `a`–`b` (1-based, inclusive) |
| `path#kind@a-b` | `"auth.py#function_definition@10-42"` | file, kind, **and** span overlap all match |

Paths are the store-relative `file_path` the index records (what `cce search`
prints). Kinds are the exact tree-sitter node types `conformance.json` lists.

The `@a-b` **line-range facet** is additive: text after the last `@` is only
treated as a range when it consists solely of digits and `-` (then it must be a
valid `a-b` with `1 ≤ a ≤ b`); any other `@` — say `user@host.py` — stays
literal path text, so pre-range fixture sets parse unchanged. A range always
needs a file path (`#kind@a-b` alone is rejected).

**When to use ranges.** Chunk-level anchors answer "did the right file/chunk
surface?" — they cannot distinguish retrieving the right 40 lines from
retrieving the right 400-line file. Add a range when the *boundary* is the
question: chunking experiments, context-window budgeting, any change whose
success criterion is "returned the span, not the neighborhood". Cases with
ranged anchors are additionally scored with the token-level metrics below;
keep anchors unranged where any chunk of the file is genuinely a good answer,
since a range that merely mirrors today's chunk boundaries will spuriously
punish a better future chunking.

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

### Token-level metrics (ranged anchors)

For each case that carries at least one `@a-b` anchor, the harness also scores
the **overlap between the expected spans and the retrieved spans**, weighted
with the ONE `cce.tokens/v1` estimator:

- **tok-recall** — overlap tokens over expected-span tokens ("how much of the
  exact span came back?").
- **tok-P** — overlap tokens over all retrieved tokens in the top-k ("how much
  of what came back was inside the span?" — retrieving a whole 400-line file
  for a 40-line span costs precision here even though the chunk-level anchor
  counts it as a hit).
- **tok-IoU** — overlap over the union of both (Jaccard), the single number to
  gate chunking experiments on.

The mechanics, all deterministic: expected lines are the set union of the
case's ranged anchor spans (multi-anchor overlaps count once); retrieved lines
are the union of the top-k results' `start_line`–`end_line` spans across all
files. Each line weighs `estimate_tokens(line_text)` — line texts come from
the indexed chunks (identical for `--dir` and `--store`; where chunks nest,
the outer chunk's text wins), and a line no chunk covers (a gap between
definitions, or a range past end-of-file) weighs the estimator floor of 1,
the same as an empty line. Unranged anchors of a mixed case do not contribute
to token metrics; unranged cases and fixture sets score exactly as before.

Backend aggregates are macro-averages over the ranged cases only, with the
count shown (`token-level span metrics (1 of 7 queries …)` above, `"queries"`
in the JSON `tokens` object).

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

`--compare A,B` scores exactly two backends and prints per-query deltas plus a
**paired significance block** per metric, so a proposed change shows exactly
which queries it helps or hurts — and how much evidence the mean delta carries:

```bash
$ cce relevance eval/relevance/code.jsonl --compare bm25,hybrid
...
  per-query deltas (bm25 → hybrid; positive = hybrid wins)
  query                           dP@k     drecall        dMRR         dF1
  code-read-config           +0.000000   +0.000000   +0.166667   +0.000000
  ...
  mean                       +0.000000   +0.000000   +0.023810   +0.000000

  paired t-test on the per-query deltas (two-sided, n=7, df=6)
  metric        mean-delta           t           p                    95% CI
  P@k            +0.000000         n/a    1.000000    [+0.000000, +0.000000]
  recall         +0.000000         n/a    1.000000    [+0.000000, +0.000000]
  MRR            +0.023810   +1.000000    0.355918    [-0.034450, +0.082069]
  F1             +0.000000         n/a    1.000000    [+0.000000, +0.000000]
```

Per metric: the paired t-statistic over the per-query deltas, the two-sided
p-value at `n−1` degrees of freedom, and a 95% confidence interval on the mean
delta. The MRR row above is the caution the block exists for: the mean says
"+0.024, hybrid wins", the CI says the data are equally consistent with hybrid
*losing* 0.034 — one query moved, six didn't. `t` reads `n/a` when the deltas
have zero variance (then `p` is 1 when they are all zero, 0 when they are all
the same non-zero value) and everything but `n` and the mean is `n/a` at
`n < 2`. The math is a closed-form t-distribution CDF (regularized incomplete
beta) — deterministic, dependency-free, no seed — so compare reports stay
byte-pinnable.

**Minimum detectable effect — read this before trusting a starter-set p.**
Significance at small n is brutal: a paired t-test at n=6–7 with 80% power
only detects a mean delta of roughly **1.4× the per-query delta standard
deviation** (`(t_{α/2,ν} + t_{β,ν}) · s/√n ≈ (2.571 + 0.920)·s/√6`). Deltas
smaller than that will look "not significant" no matter how real they are; the
shipped starter sets gate against *regressions on labeled queries*, not subtle
mean shifts. Size private fixture sets the standard IR way (Sakai's
topic-set-size / power-analysis methodology; see also Urbano et al. on test
behavior at small n): run a pilot over ~10 queries, take the delta variance
`s²` from the block above, and solve `n ≥ ((t_{α/2} + t_β) · s / δ)²` for the
smallest delta `δ` you still care about — for typical per-query IR variances,
detecting a 0.05 mean delta needs on the order of 25–50 labeled queries.

The intended workflow for any ranking change (RRF weights, boosts, fusion
parameters):

1. Run `cce relevance <fixtures> --json` at the current configuration and save it.
2. Apply the change; run again; diff — or express the change as a backend and
   use `--compare`.
3. Put the per-query delta table AND the significance block in the PR. A
   regression on a labeled query is a conscious, visible trade-off, not a
   surprise — and a mean improvement whose CI spans zero is labeled as the
   hope it is, not the evidence it isn't.

## JSON report (`--json`)

The stable `cce.relevance.report/v2` shape: pretty-printed, alphabetical keys,
scores as fixed 6-decimal strings (the same grammar discipline as
`cce search --json`), one trailing newline. Deterministic for deterministic
backends, hence byte-pinnable:

```json
{
  "backends": [
    {
      "backend": "bm25",
      "f1": "0.581633",
      "mrr": "0.904762",
      "per_query": [
        { "f1": "0.571429", "first_relevant_rank": 3, "id": "code-read-config", "k": 5, "mrr": "0.333333", "precision_at_k": "0.400000", "recall": "1.000000" },
        { "f1": "0.333333", "first_relevant_rank": 1, "id": "code-read-config-span", "k": 5, "mrr": "1.000000", "precision_at_k": "0.200000", "recall": "1.000000",
          "tokens": { "iou": "0.294118", "precision": "0.294118", "recall": "1.000000" } }
      ],
      "precision_at_k": "0.457143",
      "recall": "1.000000",
      "tokens": { "iou": "0.294118", "precision": "0.294118", "queries": 1, "recall": "1.000000" }
    }
  ],
  "corpus": "eval/relevance/../../test/fixture/samples",
  "embedder": "hash",
  "queries": 7,
  "schema": "cce.relevance.report/v2"
}
```

v2 (issues #84/#85) carries every v1 field unchanged and adds:

- **`tokens`** — per query (only on ranged cases) and per backend (only when
  the set has ranged cases; `queries` is the macro-average denominator). An
  unranged fixture set emits the exact v1 shape, `schema` string aside.
- **`compare`** — only in `--compare` mode: `a`, `b`, and per metric
  (`precision_at_k`, `recall`, `mrr`, `f1`) the paired-test block
  `{"n", "mean_delta", "t", "p", "ci95_low", "ci95_high"}`. Deltas and bounds
  are signed 6-decimal strings; a statistic that is undefined at the input
  (`t` at zero variance; everything past the mean at `n < 2`) is `null`.

```json
"compare": {
  "a": "bm25",
  "b": "hybrid",
  "metrics": {
    "mrr": { "ci95_high": "+0.082069", "ci95_low": "-0.034450", "mean_delta": "+0.023810", "n": 7, "p": "0.355918", "t": "+1.000000" }
  }
}
```

If an intended ranking or report change moves the golden, regenerate it from the
repo root and review the diff like any other golden:

```bash
cargo run -- relevance eval/relevance/code.jsonl --json > test/fixture/relevance/code.golden.json
```

## Writing your own fixture set

1. Pick 10–30 queries your team actually asks (`metrics.jsonl` and the dashboard
   are a good mine — real queries beat invented ones). If the set will gate
   changes through `--compare` p-values, size it from a pilot variance estimate
   instead (see the minimum-detectable-effect note above).
2. For each, record the file(s) — or `file#kind` chunks — a good answer must
   surface, as `expected` anchors. Add `@a-b` line ranges where the exact span
   is the success criterion.
3. Add a header line pointing `corpus` at the repo (or pass `--dir .`), commit
   the fixture set beside your code, and run `cce relevance` in CI.

The fixture format is a stable contract; private fixture sets over private
corpora work exactly like the starter sets.
