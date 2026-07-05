# Dashboard & observability

This document describes the metrics/observability subsystem added in v1.1, built
test-first from [`DASHBOARD-SPEC.md`](../DASHBOARD-SPEC.md). It gives an LLM user
**observability** into whether using CCE is *improving or degrading their
experience over time*, from persisted data, through two north-star lenses:
**token & cost savings** and **retrieval quality**.

The base engine (chunking, embedding, retrieval) is unchanged and stays
byte-for-byte conformant to [`SPEC.md`](../SPEC.md); `conformance.json` is
identical to the 1.0.0 release.

## The pipeline

```
cce search / index / feedback
        │  append one JSON line (best-effort, fail-open)
        ▼
.cce/metrics.jsonl              ← the persisted event log (src/metrics.rs)
        │  read + skip malformed/blank lines, tolerate unknown fields
        ▼
aggregate(events, now, price)   ← pure function (src/aggregator.rs)
        │  totals · north-stars · daily series · recent searches
        ▼
GET /api/metrics                ← the aggregate + a wall-clock generated_ts
        │
        ▼
GET /  (self-contained HTML)    ← inline CSS/JS, hand-drawn SVG (src/dashboard.rs)
```

**Wall-clock time lives in exactly one place.** Unlike the deterministic core
engine, the metrics subsystem uses real timestamps and unique event IDs. To keep
tests deterministic, the clock (`Clock`) and the id source (`IdSource`) are
**injected**: production uses `SystemClock` + `HexIdSource`; tests pin fixed
values. The aggregator itself takes `now` as a parameter and is a pure function
of `(events, now, price)` — no ambient time, so both language implementations
produce identical numbers from the same log.

## The event log

Location: `<store-dir>/metrics.jsonl` (beside the index; default
`<indexed-dir>/.cce/`). Append-only, one JSON object per line, UTF-8. Writes are
**best-effort**: on any I/O or serialization error, `cce` prints a warning and
continues — a metrics failure never changes a search result or the exit code.

Every event carries `schema` (`cce.metrics/v1`), `event`, `ts` (ISO-8601 UTC,
second precision), and `id` (12 lowercase-hex chars, unique per event).

### `search` (appended by `cce search`)

```json
{"schema":"cce.metrics/v1","event":"search","ts":"...","id":"...",
 "query":"...","top_k":5,"graph_enabled":true,"embedder":"hash",
 "result_count":3,
 "baseline_tokens":40000,   // Σ whole-file token_count over the DISTINCT result files
 "served_tokens":8000,      // Σ token_count(content) of the returned chunks
 "tokens_saved":32000,      // max(0, baseline_tokens - served_tokens)
 "savings_ratio":0.8,       // tokens_saved / baseline_tokens (0.0 if baseline == 0)
 "top_score":0.9,           // final score of rank-1 result (0.0 if empty)
 "mean_score":0.7,          // mean final score of returned results (0.0 if empty)
 "empty":false,             // result_count == 0
 "low_confidence":false,    // result_count > 0 AND top_score < 0.30
 "latency_ms":5.0,          // captured; no dedicated panel
 "source":"cli"}            // v2.4.1: "cli" (cce search) or "mcp" (agent context_search)
```

`source` (added additively in v2.4.1) powers the agent-vs-human panel. Older logs
without it read back as `"cli"`.

`baseline_tokens` is the "read the whole file" counterfactual. To make it
accurate, `cce index` now persists each indexed file's **whole-file token count**
(`token_count(entire file content)`) in the store; at search time the baseline is
the sum of those counts over the *distinct* files appearing in the results (a
missing entry contributes 0).

### `index` (appended by `cce index`)

```json
{"schema":"cce.metrics/v1","event":"index","ts":"...","id":"...",
 "files_indexed":231,"chunks":1728,"index_bytes":123456,"duration_ms":740.0,
 "embedder":"hash","full":true,
 "sha":"25bd0098…",        // v2.4.1: indexed commit sha (null on non-git)
 "source":"local",          // v2.4.1: "local" (cce index) | "sync-pull" (cce sync pull)
 "sensitive_skipped":1}     // v2.4.1: sensitive files skipped (secret-safety panel)
```

`sha` / `source` / `sensitive_skipped` were added additively in v2.4.1; older logs
without them read back as `null` / `"local"` / `0`.

### `feedback` (appended by `cce feedback`)

```json
{"schema":"cce.metrics/v1","event":"feedback","ts":"...","id":"...",
 "target_id":"<a search event id>","helpful":true,"note":""}
```

### Robustness

The reader skips malformed/blank lines (counted as `skipped`, never a crash) and
tolerates unknown future fields. An absent log is an empty dataset — the dashboard
renders a friendly "no data yet" state, not an error. (The `.jsonl` format is also
excluded from indexing, so a metrics log never becomes part of the corpus.)

## The aggregator (formulas)

`aggregate(events, now, price)` produces the `/api/metrics` body (minus the
wall-clock `generated_ts`, which the server adds at the edge). Output rounding:
ratios/scores → **6 decimals**, cost → **2 decimals**, both
**round-half-away-from-zero**; counts and token sums are integers.

Windows (with `TREND_WINDOW_DAYS = 7`):

- **Current** = events with `now − 7d ≤ ts < now`.
- **Prior** = events with `now − 14d ≤ ts < now − 7d`.

Definitions:

- `direction(delta)` = `up` if `delta > 1e-9`, `down` if `delta < −1e-9`, else
  `flat`. For savings and quality (higher is better), `up` = improving.
- `helpful_rate` over a feedback set = `helpful / (helpful + not_helpful)`, or
  **null** when there is no feedback in that set.
- `mean_top_score` over a window = mean of `top_score` across **non-empty**
  searches (`result_count > 0`); `0.0` if none.
- `empty_rate` = `empty_searches / total_searches` (`0.0` if none);
  `low_conf_rate` = `low_confidence_searches / total_searches` (`0.0` if none).
- `mean_savings_ratio` over a set = mean of `savings_ratio` (empties contribute
  `0.0`); `0.0` if no searches.
- **North-star deltas** are computed from the 6-decimal-rounded current and prior
  means (`delta_ratio` from `mean_savings_ratio`, `delta_top_score` from
  `mean_top_score`), so both languages reach an identical value and direction.

Output shape (keys, some nested objects elided):

```
schema, totals{searches, indexes, feedback, tokens_saved, cost_saved_usd,
               mean_savings_ratio, mean_top_score, helpful, not_helpful, helpful_rate},
north_star{ savings{current, prior, delta_ratio, direction},
            quality{current, prior, delta_top_score, direction} },
by_source{ cli{searches, tokens_saved, mean_savings_ratio, mean_top_score},
           mcp{searches, tokens_saved, mean_savings_ratio, mean_top_score} },
secret_safety{ sensitive_skipped, index_runs },
index_freshness{ indexes, source, sha, indexed_ts },     // PURELY log-derived — no network call
series{ daily:[ {date, searches, tokens_saved, mean_savings_ratio,
                 mean_top_score, empty_rate, low_conf_rate, helpful,
                 not_helpful} ] },        // one per UTC date with ANY search/feedback, ascending
recent_searches:[ {ts, id, query, result_count, tokens_saved, savings_ratio,
                   top_score, empty, feedback} ]   // ≤20, newest first
by_package:[ {package, searches, tokens_saved, mean_savings_ratio, mean_top_score} ]
                                          // workspace dashboard only (SPEC-V2.2 §7)
```

### v2.4.1 dashboard refresh — the panels

The dashboard was built at v1.1 (savings + quality only). v2.4.1 adds four panels for
the capabilities that landed since, all fed from the additive schema above:

- **Agent vs human usage** (`by_source`) — CLI searches vs MCP/agent searches: how much
  your agent leans on CCE. A search's `source` other than `"mcp"` counts as `cli`.
- **Per-package breakdown** (`by_package`, workspace only) — an **array of objects**,
  each `{package, searches, tokens_saved, mean_savings_ratio, mean_top_score}`, sorted by
  `package`: savings, searches, and **quality** per member, i.e. where in the ecosystem
  CCE helps most.
- **Index freshness** (`index_freshness`) — the indexed `sha`, the source (`local` for
  `cce index`, `sync-pull` for a `cce sync pull` install), `indexed_ts`, and the count of
  index runs. **Purely log-derived**, so the dashboard makes **zero network calls** and
  works fully offline. It deliberately carries **no** `remote_latest`/`behind_remote` — a
  live behind-remote comparison lives in `cce sync status` and MCP `index_status`, which
  are allowed to consult the remote.
- **Secret-safety** (`secret_safety`) — the sensitive-files-skipped count across index
  runs: reassurance that secure-by-default redaction is working.

`by_source`, `secret_safety`, `index_freshness`, and `totals.mean_top_score` are all
**pure functions of the log**, so — like the §4.1 anchor — the Ruby and Rust engines
produce identical numbers from the same log, and the dashboard request path touches no
network.

`recent_searches[*].feedback` is resolved by matching `feedback.target_id` to the
search `id` (latest wins) → `helpful` / `not_helpful` / `none`. A day with
feedback but no searches still appears (its search-derived numbers are 0);
index-only days do not create a series entry.

### The conformance anchor (§4.1)

The bundled fixture [`test/fixture/metrics_sample.jsonl`](../test/fixture/metrics_sample.jsonl)
(7 events) aggregated with `now = 2026-07-05T00:00:00Z` and `price = 3.00`
reproduces exactly: totals `searches 4, tokens_saved 53000, cost_saved_usd 0.16,
mean_savings_ratio 0.525000, helpful_rate 0.500000`; savings current
`mean_savings_ratio 0.533333` vs prior `0.500000` (`delta 0.033333`, `up`);
quality current `mean_top_score 0.750000, empty_rate 0.333333` vs prior
`0.400000` (`delta 0.350000`, `up`); and a four-day series. This is the
**cross-language equivalence gate** for the dashboard — the Ruby sibling must
produce the same numbers. It is asserted in `src/aggregator.rs` tests.

## The dashboard server

`cce dashboard` binds **`127.0.0.1` only** (a hand-rolled HTTP/1.1 server on
`std::net::TcpListener` — no new dependency) and is **read-only**: nothing it
serves mutates any file. Endpoints:

- `GET /` — the HTML page (self-contained: inline CSS/JS, hand-drawn SVG charts,
  **no external network, CDN, or fonts**).
- `GET /api/metrics` — the §4 aggregate, computed fresh from the live log per
  request (so a refresh reflects new events), plus a `generated_ts` stamp.
- `GET /api/health` — `{"status":"ok","events":<int>,"skipped":<int>}`.
- Any other path — `404` with a small JSON body.

The page foregrounds the two north-stars with ↑ improving / ↓ degrading / → flat
indicators, KPI cards (tokens saved, est. $ saved, searches, helpful-rate), the
v2.4.1 refresh panels (index-freshness & secret-safety cards, an agent-vs-human table,
and — in workspace mode — a by-package table), daily SVG charts, and a recent-searches
table, with a friendly empty state. See
[`SECURITY.md`](../SECURITY.md) for the server's place in the threat model
(loopback-only, read-only, self-contained; a token would be required if a future
version ever bound a non-loopback address).

## Where this would strain

The v1.1 design deliberately keeps things simple. Known strain points:

- **The log grows unbounded.** `metrics.jsonl` is append-only with **no rotation,
  compaction, or retention** in v1.1. On a busy project it grows forever, and
  every dashboard request re-reads and re-parses the whole file. This is fine for
  a local dev tool with thousands of events; at millions it would want rotation
  (e.g. daily files) and/or an incremental/cached aggregate.
- **Whole-file re-aggregation per request.** `GET /api/metrics` recomputes the
  entire aggregate on each hit. That keeps the view live and the code trivially
  correct, but it is O(events) per request — a caching layer keyed on file mtime
  would be the first optimization.
- **Second-precision timestamps + daily buckets.** The series is bucketed by UTC
  calendar date; there is no hour/minute granularity and no timezone control.
  Two events in the same second are ordered by file position for tie-breaks.
- **Single-writer assumption.** Appends are best-effort with no file locking.
  Concurrent `cce` processes writing the same log could in principle interleave a
  line under pathological conditions; the reader's skip-malformed rule keeps that
  from ever crashing a read, but a truly concurrent writer story is out of scope.
- **Latency and usage-volume are captured but unvisualized.** `latency_ms` and
  raw counts are in the log; dedicated panels are explicitly backlog for v1.1.
- **The `$` estimate is a single flat price.** `cost_saved_usd` uses one
  `--price` (USD per 1M input tokens); it does not model per-model pricing,
  output tokens, or caching.
