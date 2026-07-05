# CCE Dashboard & Observability — Specification v1.1 (addendum to SPEC v1.0)

**Status:** Normative. Extends the base `SPEC.md`. Where this document and
`SPEC.md` disagree, this one wins *for the metrics/dashboard feature only*; the
core engine (chunking, embedding, retrieval) is unchanged and must stay
byte-for-byte conformant to `SPEC.md`.

**Goal:** give an LLM user **observability** into whether using CCE is *improving
or degrading their experience over time*, fed from **persisted data**. Two
north-star lenses: **(A) token & cost savings** and **(B) retrieval quality**.
(Latency and usage-volume are captured in the data but their dedicated dashboard
panels are explicitly out of scope for v1.1 — backlog.)

---

## 0. What changes

1. Every `search` (and `index`, and the new `feedback`) appends one JSON line to
   a persisted **metrics event log**.
2. A pure **aggregator** turns that log into KPIs + daily time series + windowed
   "improving/degrading" deltas.
3. A `cce dashboard` command serves a **self-contained local web page** that
   visualizes the aggregate, plus a JSON API.
4. A `cce feedback` command lets the user mark a past search result helpful / not.

This is runtime, time-based data: **unlike the core engine, the metrics
subsystem uses real wall-clock timestamps and unique event IDs.** To keep tests
deterministic, inject the clock and the ID source (a `now`/`clock` and an
`id_source`) so tests pass fixed values.

---

## 1. Constants (normative)

| Name | Value | Meaning |
|---|---|---|
| `METRICS_SCHEMA` | `"cce.metrics/v1"` | schema tag on every event and on the API |
| `METRICS_FILE` | `metrics.jsonl` | default log filename, in the store dir |
| `LOW_CONFIDENCE_THRESHOLD` | `0.30` | a non-empty search whose top score < this is "low confidence" |
| `TREND_WINDOW_DAYS` | `7` | current vs prior comparison window length |
| `DEFAULT_DASHBOARD_PORT` | `8787` | loopback port for `cce dashboard` |
| `DEFAULT_INPUT_PRICE_PER_MILLION` | `3.00` | USD per 1M input tokens, for the $ estimate (configurable) |
| `RECENT_SEARCHES_LIMIT` | `20` | how many recent searches the API returns |
| `DIRECTION_EPSILON` | `1e-9` | delta magnitude below this is "flat" |

Config keys (optional, defaulting to the above):
`dashboard.port`, `dashboard.input_price_per_million`, `metrics.enabled` (default
`true`).

---

## 2. Persisted metrics event log

Location: `<store-dir>/metrics.jsonl` (the store dir is where the index lives,
default `<indexed-dir>/.cce/`). Append-only; **one JSON object per line**; UTF-8.
Writing is **best-effort and must never break the command** — on any I/O or
serialization error, log a warning and continue (fail-open, like the base
engine's non-fatal paths).

Every event has: `schema` (=`METRICS_SCHEMA`), `event`, `ts` (ISO-8601 UTC,
second precision, e.g. `2026-07-05T13:04:11Z`), `id` (12 lowercase-hex chars,
unique per event).

### 2.1 `search` event — appended by `cce search`
```json
{
  "schema":"cce.metrics/v1","event":"search","ts":"...","id":"...",
  "query":"...", "top_k":5, "graph_enabled":true, "embedder":"hash",
  "result_count":3,
  "baseline_tokens":40000,   // sum of file_token_count over the DISTINCT files of the returned results
  "served_tokens":8000,      // sum of token_count(content) of the returned chunks
  "tokens_saved":32000,      // max(0, baseline_tokens - served_tokens)
  "savings_ratio":0.8,       // tokens_saved / baseline_tokens  (0.0 if baseline_tokens == 0)
  "top_score":0.9,           // final score of rank-1 result (0.0 if empty)
  "mean_score":0.7,          // mean final score of returned results (0.0 if empty)
  "empty":false,             // result_count == 0
  "low_confidence":false,    // result_count > 0 AND top_score < LOW_CONFIDENCE_THRESHOLD
  "latency_ms":5.0           // captured; no dedicated panel in v1.1
}
```
`token_count` uses the base spec's `token_count` (floor(bytes/4), min 1).
`baseline_tokens` requires knowing each file's *whole-file* token count — see §3.
`savings_ratio` stored as a plain JSON number (full precision); rounding happens
only in aggregation output (§4).

### 2.2 `index` event — appended by `cce index`
```json
{"schema":"cce.metrics/v1","event":"index","ts":"...","id":"...",
 "files_indexed":231,"chunks":1728,"index_bytes":123456,"duration_ms":740.0,
 "embedder":"hash","full":true}
```

### 2.3 `feedback` event — appended by `cce feedback`
```json
{"schema":"cce.metrics/v1","event":"feedback","ts":"...","id":"...",
 "target_id":"<a search event id>","helpful":true,"note":""}
```

### 2.4 Robustness
The reader must skip malformed/blank lines (count them as `skipped` but never
crash), and tolerate unknown future fields. An absent log = an empty dataset (the
dashboard renders a friendly "no data yet" state, not an error).

---

## 3. Index change: persist whole-file token counts

To compute `baseline_tokens` accurately (the counterfactual "read the whole
file"), `index` must persist, per indexed file, its **whole-file token count** =
`token_count(entire file content)`. Store a `file_path → file_token_count` map in
the store (a table/row/section — your choice). At search time,
`baseline_tokens = sum over the DISTINCT file_paths appearing in the returned
results of file_token_count[file_path]` (missing entry → contributes 0). This
mirrors the benchmark savings definition in base SPEC §10.3.

---

## 4. Aggregator (pure function — exact, testable, cross-language-identical)

Signature (conceptually): `aggregate(events, now, price) -> Aggregate`, where
`events` is the parsed log, `now` is a timestamp (injected; defaults to wall
clock), `price` = input price per 1M tokens. **No wall-clock or randomness inside
the aggregator** — it is a pure function of its inputs, so it is fully testable
and both languages must produce identical numbers.

Definitions:
- A **search** = event with `event=="search"`. Likewise index/feedback.
- **Current window** = events with `now - 7 days <= ts < now`.
  **Prior window** = `now - 14 days <= ts < now - 7 days`.
- Rounding for OUTPUT: ratios/scores/rates → **6 decimals**; cost → **2
  decimals**; both **round-half-away-from-zero**. Counts/token sums are integers.
- `direction(delta)` = `"up"` if `delta > DIRECTION_EPSILON`, `"down"` if
  `delta < -DIRECTION_EPSILON`, else `"flat"`. (For savings and quality metrics
  where higher is better, "up" = improving.)
- `helpful_rate` over a set of feedback = `helpful / (helpful + not_helpful)`, or
  **null** when there is no feedback.
- `mean_top_score` over a window = mean of `top_score` across **non-empty**
  searches (result_count>0) in the window; `0.0` if none.
- `empty_rate` over a window = `empty_searches / total_searches`; `0.0` if no
  searches. `low_conf_rate` = `low_confidence_searches / total_searches`; `0.0`
  if none.
- `mean_savings_ratio` over a set of searches = mean of `savings_ratio`
  (empties contribute 0.0); `0.0` if no searches.

**Output shape** (the `/api/metrics` body; the aggregator returns this minus
`generated_ts`):
```json
{
  "schema":"cce.metrics/v1",
  "generated_ts":"...",              // wall clock; NOT part of conformance
  "totals":{
    "searches":int,"indexes":int,"feedback":int,
    "tokens_saved":int,"cost_saved_usd":number,     // 2dp
    "mean_savings_ratio":number,                     // 6dp
    "helpful":int,"not_helpful":int,"helpful_rate":number|null   // 6dp
  },
  "north_star":{
    "savings":{
      "current":{"searches":int,"tokens_saved":int,"mean_savings_ratio":number},
      "prior":{"searches":int,"tokens_saved":int,"mean_savings_ratio":number},
      "delta_ratio":number,"direction":"up|down|flat"     // delta of mean_savings_ratio
    },
    "quality":{
      "current":{"mean_top_score":number,"empty_rate":number,"low_conf_rate":number,"helpful_rate":number|null},
      "prior":{"mean_top_score":number,"empty_rate":number,"low_conf_rate":number,"helpful_rate":number|null},
      "delta_top_score":number,"direction":"up|down|flat"  // delta of mean_top_score
    }
  },
  "series":{ "daily":[
     {"date":"YYYY-MM-DD","searches":int,"tokens_saved":int,
      "mean_savings_ratio":number,"mean_top_score":number,
      "empty_rate":number,"low_conf_rate":number,
      "helpful":int,"not_helpful":int}
     // one entry per UTC calendar date that has ANY event (search or feedback), sorted ascending
  ]},
  "recent_searches":[  // up to RECENT_SEARCHES_LIMIT most recent search events, newest first
     {"ts":"...","id":"...","query":"...","result_count":int,"tokens_saved":int,
      "savings_ratio":number,"top_score":number,"empty":bool,
      "feedback":"helpful|not_helpful|none"}   // resolved by matching feedback.target_id == this id (latest wins)
  ]
}
```
Per-day `mean_top_score` uses that day's non-empty searches (0.0 if none that
day). Days with feedback but no searches still appear (search-derived numbers 0).

### 4.1 Conformance anchor (MUST reproduce exactly)

Ship `test/fixture/metrics_sample.jsonl` with EXACTLY these 7 lines, and a test
that runs the aggregator with `now = 2026-07-05T00:00:00Z` and
`price = 3.00`, asserting the values below.

```
{"schema":"cce.metrics/v1","event":"index","ts":"2026-07-01T09:00:00Z","id":"000000000001","files_indexed":10,"chunks":100,"index_bytes":5000,"duration_ms":200.0,"embedder":"hash","full":true}
{"schema":"cce.metrics/v1","event":"search","ts":"2026-07-01T10:00:00Z","id":"aaaaaaaaaaaa","query":"login","top_k":5,"graph_enabled":true,"embedder":"hash","result_count":3,"baseline_tokens":40000,"served_tokens":8000,"tokens_saved":32000,"savings_ratio":0.8,"top_score":0.9,"mean_score":0.7,"empty":false,"low_confidence":false,"latency_ms":5.0}
{"schema":"cce.metrics/v1","event":"search","ts":"2026-07-02T10:00:00Z","id":"bbbbbbbbbbbb","query":"payment","top_k":5,"graph_enabled":true,"embedder":"hash","result_count":2,"baseline_tokens":20000,"served_tokens":4000,"tokens_saved":16000,"savings_ratio":0.8,"top_score":0.6,"mean_score":0.5,"empty":false,"low_confidence":false,"latency_ms":4.0}
{"schema":"cce.metrics/v1","event":"search","ts":"2026-07-02T11:00:00Z","id":"cccccccccccc","query":"zzz nonexistent","top_k":5,"graph_enabled":true,"embedder":"hash","result_count":0,"baseline_tokens":0,"served_tokens":0,"tokens_saved":0,"savings_ratio":0.0,"top_score":0.0,"mean_score":0.0,"empty":true,"low_confidence":false,"latency_ms":3.0}
{"schema":"cce.metrics/v1","event":"feedback","ts":"2026-07-02T12:00:00Z","id":"000000000002","target_id":"aaaaaaaaaaaa","helpful":true,"note":""}
{"schema":"cce.metrics/v1","event":"feedback","ts":"2026-07-03T09:00:00Z","id":"000000000003","target_id":"bbbbbbbbbbbb","helpful":false,"note":""}
{"schema":"cce.metrics/v1","event":"search","ts":"2026-06-25T10:00:00Z","id":"dddddddddddd","query":"legacy","top_k":5,"graph_enabled":true,"embedder":"hash","result_count":1,"baseline_tokens":10000,"served_tokens":5000,"tokens_saved":5000,"savings_ratio":0.5,"top_score":0.4,"mean_score":0.3,"empty":false,"low_confidence":false,"latency_ms":6.0}
```

Expected (assert exactly):
- `totals`: searches **4**, indexes **1**, feedback **2**, tokens_saved **53000**,
  cost_saved_usd **0.16**, mean_savings_ratio **0.525000**, helpful **1**,
  not_helpful **1**, helpful_rate **0.500000**.
- `north_star.savings.current`: searches **3**, tokens_saved **48000**,
  mean_savings_ratio **0.533333**. `.prior`: searches **1**, tokens_saved
  **5000**, mean_savings_ratio **0.500000**. `delta_ratio` **0.033333**,
  `direction` **"up"**.
- `north_star.quality.current`: mean_top_score **0.750000**, empty_rate
  **0.333333**, low_conf_rate **0.000000**, helpful_rate **0.500000**. `.prior`:
  mean_top_score **0.400000**, empty_rate **0.000000**, low_conf_rate
  **0.000000**, helpful_rate **null**. `delta_top_score` **0.350000**,
  `direction` **"up"**.
- `series.daily` has entries for dates **2026-06-25, 2026-07-01, 2026-07-02,
  2026-07-03** (2026-07-02 has searches **2**, empty_rate **0.500000**, helpful
  **1**; 2026-07-03 has searches **0**, not_helpful **1**).

Both implementations, given this fixture, MUST reproduce these numbers. That is
the cross-language equivalence gate for the dashboard.

---

## 5. CLI additions

- `cce search ...` (extended): after printing results, append a search event
  (unless `--no-metrics` or `metrics.enabled=false`). In human output, print a
  final line: `query-id: <id>  ·  rate with: cce feedback <id> --helpful|--not-helpful`.
  In `--json` output, add a top-level `"query_id":"<id>"` field. Metrics failure
  must not affect the search result or exit code.
- `cce feedback <query-id> --helpful | --not-helpful [--note "..."] [--store PATH|--dir DIR]`
  Appends a feedback event with `target_id = <query-id>`. If no search event with
  that id exists in the log, print a warning but still record it (or exit non-zero
  — your choice; document it). Exactly one of `--helpful`/`--not-helpful` required.
- `cce index ...` (extended): append an index event; persist whole-file token
  counts (§3).
- `cce dashboard [--dir DIR | --store PATH] [--port N] [--metrics PATH] [--no-open]`
  Starts the web server (§6). Print the URL. `--no-open` suppresses any
  browser-open behavior (browser-opening is optional; default may just print the
  URL).

---

## 6. Web dashboard (local, self-contained)

`cce dashboard` starts an HTTP server **bound to 127.0.0.1** on `--port`
(default 8787). It is **read-only** (no endpoint mutates anything) and **fully
self-contained**: the served HTML inlines all CSS and JS and draws its own charts
(inline SVG or canvas). **No external network, no CDN, no remote fonts/scripts** —
consistent with CCE's offline/local security posture. If you ever allow binding a
non-loopback address, require a token (mirror the base SECURITY model); default
loopback needs none.

Endpoints:
- `GET /` → the dashboard HTML page (200; text/html).
- `GET /api/metrics` → the aggregate JSON of §4 (200; application/json), computed
  fresh from the current log on each request (so it reflects new events live on
  refresh).
- `GET /api/health` → `{"status":"ok","events":<int>,"skipped":<int>}`.
- Any other path → 404 with a small JSON/text body.

Page content (minimum):
1. **Header KPIs (cards):** total tokens saved, estimated $ saved, total searches,
   helpful-rate.
2. **North-star A — Savings:** a big current-vs-prior delta with an up/down
   indicator ("↑ improving" / "↓ degrading" / "→ flat") on `mean_savings_ratio`,
   plus a daily line/bar chart of `tokens_saved` (and/or `mean_savings_ratio`).
3. **North-star B — Retrieval quality:** current-vs-prior delta with up/down on
   `mean_top_score`; plus daily charts for `mean_top_score`, `empty_rate`, and the
   `helpful`/`not_helpful` split.
4. **Recent searches table:** from `recent_searches` (query, results, saved,
   top score, feedback state).
5. A clear **empty state** when there is no data yet.

Keep it clean and legible; light/dark friendliness is nice-to-have, not required.
The visuals need not be identical between the two implementations, but both must
consume the **same `/api/metrics` shape** and show the same numbers.

---

## 7. Dependencies

- **Ruby:** you may add a minimal HTTP server (e.g. the `webrick` gem, or a
  `rack`+`puma` pair, or a hand-rolled `TCPServer`). Prefer the smallest thing
  that works; add it to the Gemfile (dependabot already covers `bundler`).
- **Rust:** you may add a minimal HTTP server crate (e.g. `tiny_http`) or use
  `std::net::TcpListener` directly. Charts hand-rolled — no charting crate needed.
  Pin the version in `Cargo.toml` (dependabot covers `cargo`).

No new heavy frameworks. The dashboard is a local dev tool.

---

## 8. TDD, tests, gates

Build test-first. Cover at least:
- Event append: correct schema/fields, with an **injected clock and id source**
  (deterministic). `--no-metrics` / `metrics.enabled=false` suppresses writes.
- Best-effort robustness: a corrupt/blank line is skipped; a read-only/missing
  path doesn't crash search.
- Whole-file token-count persistence (§3) and the `baseline_tokens` sum over
  distinct result files.
- The **aggregator anchor** (§4.1) — exact expected values.
- Empty-log → valid "no data" aggregate.
- `cce feedback` writes a correct event and resolves into `recent_searches`.
- HTTP: `GET /api/health` and `GET /api/metrics` return the expected JSON;
  `GET /` returns HTML 200; unknown path → 404. (Bind to an ephemeral port in
  tests.)
- All existing gates stay green: Ruby `bundle exec rake test`; Rust `cargo test`
  + `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt
  --check`. **The base engine conformance must be unchanged** — re-run
  `cce conformance test/fixture` and confirm `conformance.json` is byte-identical
  to the committed one.

---

## 9. Docs & release (v1.1.0)

- Bump version to **1.1.0** everywhere it is stated: `CHANGELOG.md` (new
  `1.1.0 - <release date>` section under `Unreleased`, Keep a Changelog format),
  `CITATION.cff` (`version: 1.1.0`), and (Rust) `Cargo.toml` `version = "1.1.0"`.
- `README.md`: add a **"Dashboard & observability"** section — what it tracks
  (savings + retrieval quality, trended, improving/degrading), and the commands
  (`cce dashboard`, `cce feedback`), with a short worked example.
- `docs/how-to.md`: recipes for viewing the dashboard and giving feedback.
- `docs/architecture.md` (or a new `docs/dashboard.md`): the metrics pipeline
  (log → aggregator → API → page), the event schema, and the aggregation
  formulas; include a short "where this would strain" note (e.g. JSONL grows
  unbounded; no rotation in v1.1).
- `SECURITY.md`: add the dashboard server to the threat model (loopback-only,
  read-only, self-contained, no external network; token required if ever
  non-loopback).
- `llms.txt` and `AGENTS.md`: mention the new metrics/dashboard modules and the
  fact that the metrics subsystem is the one place wall-clock time is allowed.
- Keep every self-reference pointing at the correct repo URL.

---

## 10. Delivery

Do all work on a branch named **`feat/dashboard`** in your existing repo. Commit
there with clear messages. **Do NOT push and do NOT open a PR** — the orchestrator
will verify, push, and open the PR. Do not read the sibling-language repository;
implement solely from your own repo + `SPEC.md` + this document, so the two
implementations remain independent (preserving the cross-language equivalence
check on the aggregator anchor).

When done, report: what you built, new test count + coverage, confirmation the
aggregator anchor passes and base `conformance.json` is unchanged, all gates
green, and the `feat/dashboard` branch commit hash.
```
