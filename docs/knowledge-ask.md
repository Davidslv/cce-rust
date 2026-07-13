# Proving the knowledge corpus — `cce knowledge ask`

`cce relevance` measures whether *code* ranking is any good. `cce knowledge ask`
answers the question a **knowledge host** exists to answer: are the things an
operator actually asks answered *from the curated corpus* — and do they stay
answered after every rebuild?

It is the knowledge corpus's standing regression check. A committed suite of real
questions, each pinned to the record a good answer must surface, is run through the
**exact** retrieval MCP `context_search` serves for `source: knowledge`
(`search_knowledge` in `src/knowledge/retrieval.rs`) — no reimplementation, it
measures the real thing. A query is **proven** when every expected record surfaces
in its top-k. The command exits non-zero the moment a query stops proving, so it
gates a corpus rebuild the way `conformance` gates chunk output.

## Quick start

```bash
# Against the fixture corpus the suite header names (the committed, CI-gated proof):
$ cce knowledge ask eval/knowledge/ask.jsonl
CCE knowledge-ask — answers vs the curated corpus (cce.knowledge.ask/v1)
  corpus   : corpus.knowledge.jsonl
  min_score: 0.300000
  proven   : 7/7

  query                           P@k    recall       MRR    rank  proven
  ask-login-lockout           0.200000  1.000000  1.000000       1  yes
  ...
  mean                        0.200000  1.000000  0.928571

# Against the INSTALLED production store (the parked-tail re-run):
$ cce knowledge ask ask.jsonl --dir /path/to/project
```

`--dir <root>` runs the suite against the store installed at
`<root>/.cce/knowledge/` (what a real `cce knowledge pull`/`index` produced)
instead of the header's fixture feed. This is the *same suite, real corpus* path:
it is how U5.4's parked evidence tail closes — the golden suite re-run against the
production instance once the corpus exists.

## The suite contract (`cce.knowledge.ask/v1`)

NDJSON, one JSON object per line — a documented, stable contract like
`cce.knowledge/v1` and `cce.relevance/v1`.

**Header line** (optional, first non-blank line):

```json
{"schema":"cce.knowledge.ask/v1","corpus":"corpus.knowledge.jsonl"}
```

- `schema` — must be `cce.knowledge.ask/v1`.
- `corpus` — a `cce.knowledge/v1` feed, resolved **relative to the suite file**.
  Ingested in memory at run time. `--dir` (an installed store) overrides it.

**Case lines** (one labeled query each):

```json
{"id":"ask-login-lockout","query":"how many failed login attempts before an account is locked","expect":["gh:acme/shop#101"],"k":5}
```

| Field | Required | Meaning |
|---|---|---|
| `query` | yes | The question, run verbatim through `search_knowledge`. |
| `expect` | yes | Non-empty array of `record_id`s (the feed's `id`) a good top-k must surface. |
| `id` | no | Stable case name for reports (default: `q<line-number>`). |
| `k` | no | The top-k cut-off scored (default: 10). |

Anchors are **record ids**, not file paths: the question is "did the right curated
record come back?", which is robust to how the record is heading-chunked. Unknown
fields are ignored, so a suite may carry its own annotations.

## Metrics

The same IR family as `cce relevance`, macro-averaged over the suite:

- **precision@k** — relevant hits in the top-k, over `k`. With one expected record
  per question, this is naturally capped at `1/k` — compare it over time, not
  against 1.0.
- **recall** — expected records that surfaced, over the number expected. `recall
  == 1.0` is what **proven** means.
- **MRR** — 1/rank of the first expected record (0 when none surfaced).
- **F1** — harmonic mean of precision@k and recall.

`min_score` is the recall precision floor `search_knowledge` applies (a hit must
clear it *and* share a query token). It defaults to `0.30`, the
`knowledge.min_score` a fresh instance uses — the exact value MCP applies — and is
overridable with `--min-score`.

Records dropped by the corpus itself never surface here either: a `not_planned` /
`wontfix` record (a decision *not* to act, SPEC-V2.6 §5) is filtered by
`search_knowledge`, so a suite cannot pin an answer to one.

## JSON report (`--json`) and the golden

`--json` emits the stable `cce.knowledge.ask.report/v1` shape: pretty-printed,
alphabetical keys, scores as fixed 6-decimal strings, one trailing newline — the
same grammar discipline as `cce relevance --json`, hence byte-pinnable. The shipped
suite's report is pinned against
[`test/fixture/knowledge/ask.golden.json`](../test/fixture/knowledge/ask.golden.json)
and gated in CI (`tests/knowledge_ask_cli.rs`): a retrieval-behavior change, a
corpus edit, or a report-grammar change must fail there first.

If an intended change moves the golden, regenerate it from the repo root and review
the diff like any other golden:

```bash
cargo run -- knowledge ask eval/knowledge/ask.jsonl --json > test/fixture/knowledge/ask.golden.json
```

## Writing your own suite

1. Mine the questions operators actually ask (a triage tool's query log is a good
   source — real questions beat invented ones). Five is the floor; more is better.
2. For each, record the `id` of the curated record a good answer must surface, as
   `expect`.
3. Point the header `corpus` at a `cce.knowledge/v1` feed (or run `--dir` against
   an installed store), commit the suite beside your corpus, and run
   `cce knowledge ask` in CI after every corpus rebuild.
