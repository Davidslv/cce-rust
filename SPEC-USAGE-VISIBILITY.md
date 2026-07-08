# CCE v2.8 — Usage visibility (`cce usage` + an opt-in MCP result footer)

**Status:** normative build specification, as implemented (#35). **Rust-first** (cce-ruby
reconciles to the byte-pinned formats later). **Additive** — with the footer *off* (the default)
every existing byte-pinned surface is preserved.

> Renumbered from the v2.7 draft (the v2.7.x line shipped `cce update`); functional content is
> the draft's, with the drift fixes noted in §9.

## 0. Why

When an agent has `cce mcp` connected, the user has **no terminal-native or in-conversation** way to
see how much the agent leaned on CCE. The data already exists — every `context_search` writes a
`cce.metrics/v1` **`search` event** with `source:"mcp"` — but today it surfaces **only** through the
browser dashboard (`cce dashboard [--workspace]`). Two gaps:

1. **No one-shot CLI answer.** You can't ask, from a terminal or CI, *"how many times did the agent
   call CCE, and how many tokens did that save, since this morning?"* `cce savings` prints the
   seven-bucket **savings ledger** but not the **who-used-it / how-many-calls / which-queries** view;
   `cce dashboard` needs a browser and a running server.
2. **Nothing in the conversation.** The MCP tool result ends with `query_id` + a `record_feedback`
   hint and deliberately says nothing about savings, so the numbers never reach the user inline.

This track closes both — **without** computing anything new. Both features are **projections of the
already-recorded metrics event**; neither changes a single recorded number.

## 1. Invariants

1. **Pure projection — zero new accounting.** Both surfaces render the *existing* `search` event /
   aggregate. Turning either on **must not change** any recorded metric (`served_tokens`,
   `baseline_tokens`, `tokens_saved`, `savings_ratio`, the savings-layer buckets, `by_source`, …).
   A test asserts: same query, footer off vs on ⇒ an **identical recorded `search` event** (the
   per-call `ts`/`id`/`latency_ms` vary run-to-run by construction and are set aside; every other
   byte must match).
2. **Additive + byte-pinned.** With `mcp.result_footer: off` (default), the MCP tool-result bytes,
   `conformance.json`, and the MCP goldens are **preserved byte-identical**. `cce usage`'s human
   render and its `cce.usage/v1` JSON are **new** byte-pinned goldens (they add surfaces; they move
   nothing). The two aggregate fields v2.8 adds (§2.5) are additive.
3. **Deterministic + cross-engine identical.** Both reuse the pure `aggregate(events, now, price)`
   function, so cce-rust and cce-ruby produce identical numbers from the same log. `now` is injected
   (wall clock only at the CLI edge; fixed in tests). Every new grammar line is a pure, byte-pinned
   function.
4. **Same logs the dashboard reads.** `cce usage --workspace` aggregates every member log **and the
   workspace-root `.cce/metrics.jsonl`** (the `cce mcp --workspace` agent-search log), folded in once
   and guarded against a member whose path is the root — the exact rule shipped in #28. `cce usage`
   and `cce dashboard --workspace` therefore report identical totals.
5. **Offline + read-only.** No network; nothing mutates state. Same posture as `cce dashboard` /
   `cce savings`.
6. **Off by default = context hygiene.** The in-conversation footer is opt-in precisely because
   printing savings into every tool result costs the agent's own context window. The default keeps
   the tool result lean; the aggregate surfaces (dashboard, `cce usage`) carry the numbers.

---

## 2. M1 — `cce usage` (terminal-native usage summary)

The CLI counterpart to the dashboard's **agent-vs-human** panel: a one-shot, greppable, CI-friendly
answer to *"how much was CCE used, by whom, and what did it save — over this window?"*

### 2.1 Command

```
cce usage [<dir>]
          [--workspace]                # federate: member logs + workspace-root log (#28 rule)
          [--dir <dir>]                # project/workspace root (default: cwd)
          [--store <path>]             # single-repo: metrics beside a specific store
          [--metrics <path>]           # single-repo: an explicit metrics.jsonl
          [--since <when>]             # window start; default: all time
          [--source mcp|cli|all]       # display filter; default: all
          [--json]                     # emit cce.usage/v1 instead of the human render
```

- **`--since <when>`** accepts either an **ISO UTC instant/date** (`2026-07-01`,
  `2026-07-01T09:00:00Z`; a bare date is midnight UTC) or a **relative duration** (`90m`, `24h`,
  `7d`, `4w`). Events with `ts < cutoff` are dropped **before** aggregation; `cutoff` is derived
  from the injected `now`. Malformed `--since` is a clear error listing the accepted forms (no
  silent all-time fallback), exit non-zero.
- **`--source`** filters the **display only**: it narrows the human split lines and the human
  recent list to one source (`cli` matches every non-`mcp` source, the aggregate's rule). The JSON
  always carries both splits and the full recent list, with the filter echoed as `source_filter`,
  so a pipeline never loses data. An unknown value is a clear error.
- **Log resolution** is identical to `cce dashboard`: single-repo reads `<root>/.cce/metrics.jsonl`
  (or `--store`/`--metrics`); `--workspace` reuses the federated aggregation
  (`federation::federated_metrics_json_since(members, Some(root_log), now, price, since)`).

### 2.2 Human render (byte-pinned)

A compact block — window header, the agent/human split, the by-package table (workspace only), and
the recent queries. Pinned example (the committed `test/fixture/usage/metrics_usage.jsonl`,
all-time):

```
CCE usage — all time
  agent (mcp) : 2 searches · saved ~16,500 tok (88%) · quality 0.79 · 58 ms avg
  human (cli) : 1 searches · saved ~ 2,100 tok (81%) · quality 0.74 · 12 ms avg
  recent (newest first)
    mcp  09:58  "how does the payment flow create a new case"  5 hits  ~8.6k saved
    mcp  09:52  "where is the retry idempotency boundary"      5 hits  ~7.9k saved
    cli  09:00  "rrf fusion constant"                          3 hits  ~2.1k saved
```

Pinned rules:

- **Header**: `CCE usage — all time` (no `--since`), `CCE usage — last <spec> (since <cutoff ISO>)`
  (relative, `<spec>` normalized lowercase), or `CCE usage — since <cutoff ISO>` (ISO form).
- **Split lines**: one per displayed source, label `agent (mcp)` / `human (cli)`. Search counts and
  thousands-separated token counts are right-aligned to the widest displayed line (hence
  `~ 2,100`). Percent = `round(mean_savings_ratio × 100)`; quality = `mean_top_score` to 2 dp;
  latency = `round(mean_latency_ms)` + ` ms avg`. A source with no searches in the window renders a
  zero line (never disappears) unless `--source` filtered it out.
- **`--workspace`** inserts a `  by package` mini-table (the aggregate's `by_package`) between the
  split and `recent`: `    <package> : N searches · saved ~X tok (P%) · quality Q` with the same
  alignment rules and no latency column. Federated agent searches (from the root log) count in the
  split/headline but **not** in `by_package` — same honesty rule as #28.
- **Recent**: newest first (the aggregate's order), at most 10 lines, then the pinned elision
  `    … (N more; --json for all)`. Each line: source padded to 3, `HH:MM` from the event `ts`, the
  quoted query (longer than 44 chars ⇒ cut on a char boundary + one `…`) padded so the hits column
  aligns, `N hits`, and `~<short> saved` where `<short>` is the pinned short token form (`< 1000`
  verbatim; `< 100k` one-decimal `k`; else integer-`k` by floor).
- Empty window ⇒ the header plus the pinned `  no searches in this window` line, exit 0.
- Every number is a pure function of the log ⇒ identical to the dashboard and to `cce savings`.

### 2.3 JSON (`cce.usage/v1`)

A stable, versioned **projection** of the aggregate — `totals`, `by_source`, and `by_package` are
lifted **verbatim** from the aggregate value, so where they overlap with `/api/metrics` the shapes
and numbers are byte-identical (a re-shape, never a re-computation):

```json
{
  "schema": "cce.usage/v1",
  "generated_ts": "2026-07-06T10:00:00Z",
  "window": { "since": "2026-06-29T10:00:00Z", "until": "2026-07-06T10:00:00Z" },
  "source_filter": "all",
  "totals": { "searches": 3, "tokens_saved": 18600,
              "mean_savings_ratio": 0.856667, "mean_top_score": 0.773333 },
  "by_source": {
    "cli": { "mean_latency_ms": 12.0, "mean_savings_ratio": 0.81, "mean_top_score": 0.74,
             "searches": 1, "tokens_saved": 2100 },
    "mcp": { "mean_latency_ms": 58.0, "mean_savings_ratio": 0.88, "mean_top_score": 0.79,
             "searches": 2, "tokens_saved": 16500 }
  },
  "by_package": [ /* present iff --workspace; the aggregate's array, verbatim */ ],
  "recent": [ { "ts": "2026-07-05T09:58:00Z", "source": "mcp",
                "query": "how does the payment flow create a new case",
                "result_count": 5, "tokens_saved": 8600 } ]
}
```

- `window.since` is `null` (all time) or the cutoff instant; `generated_ts` and `window.until` are
  the injected `now` (not conformance-anchored; excluded from the byte-pinned body the same way
  `/api/metrics`'s `generated_ts` is).
- `by_package` is present **iff** `--workspace`. `recent` is the aggregate's `recent_searches`
  (≤ 20, newest first) re-shaped to the five fields above — unfiltered by `--source`.

### 2.4 Relationship to the existing surfaces

| Surface | Question it answers | Form |
|---|---|---|
| `cce dashboard [--workspace]` | live, visual, all panels | browser + local server |
| **`cce usage` (new)** | **how much / by whom / which queries, over a window** | **one-shot terminal + JSON** |
| `cce savings [--json]` | the seven-bucket **savings ledger** + `$` | one-shot terminal + JSON |

`cce usage` is the terminal answer to the user's question; it does not replace either neighbour.
`$` cost stays in `cce savings` (deliberately: one money number).

### 2.5 Additive aggregate fields (feed both `cce usage` and the dashboard)

Two fields join the pure aggregate (and therefore `/api/metrics`), both additive and log-derived:

- `by_source.<cli|mcp>.mean_latency_ms` — mean of the events' recorded `latency_ms` (an absent
  field on a pre-v2.4 event reads as `0.0`), rounded like the other means.
- `recent_searches[].source` — the event's `source` tag, so the recent view can label each query
  agent-vs-human.

The read side gains `latency_ms` on the parsed search event (absent ⇒ `0.0`). No write-side change.

---

## 3. M2 — the opt-in MCP result footer

Make **this call's** usage visible **in the conversation**, opt-in, one byte-pinned line.

### 3.1 Toggle

Per-project `.cce/config` (tolerant YAML, loaded like every other config block; precedence
**default → `.cce/config`**, no per-call arg in v2.8 so the agent can't flip it mid-session):

```yaml
mcp:
  result_footer: "off"       # off (default) | on | session
```

- **`off`** (default) — no footer. The tool-result bytes are **byte-identical to v2.7** ⇒
  `conformance.json` and the MCP goldens do not move.
- **`on`** — append one line reporting **this call's** accounting.
- **`session`** — as `on`, plus a running **session** clause from the server's in-memory session
  usage counters.
- YAML 1.1 note: a bare `on`/`off` parses as a boolean; the loader accepts both the quoted strings
  and the boolean forms. Unknown values fall back to `off` (tolerant, like every config block).
  The mode is read once at `cce mcp` startup.

### 3.2 Placement + the critical invariant

The footer is appended **after** the existing `query_id` + `record_feedback` hint, as the LAST
line, and it is an **observability annotation, not retrieved context**:

- It is rendered **after** all savings measurement, from the values **already** on the recorded
  `search` event. It is **excluded** from `served_tokens` / `baseline_tokens` / the grammar bucket.
- ⇒ **Invariant 1 holds:** the recorded `search` event and every `by_source`/savings number are
  identical whether the footer is `off`, `on`, or `session`. The footer only *prints* what was
  already computed. (This is what keeps the dashboard and `cce usage` honest regardless of the
  toggle.)
- It applies wherever a `context_search` records a `search` event: the single-repo path, the
  workspace (federated) path, and the code side of a knowledge blend (`source:"both"`, where the
  chunk count shown is the blended total the renderer shows). A knowledge-only search records no
  `search` event and carries no footer.

### 3.3 Format (byte-pinned grammar)

A new `crate::grammar` line (`usage_footer_line`, a sibling of `compact_line`), so Ruby/Rust render
identical bytes and it is conformance-testable when enabled.

`on`:
```
cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880 baseline · saved ~8,676 (88%)
```

`session` adds a trailing clause:
```
cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880 baseline · saved ~8,676 (88%) · session: 42 searches, ~310k saved
```

- Numbers come straight off the event: `result_count`, the corpus chunk count already passed to
  the result renderer, `served_tokens`, `baseline_tokens`, `tokens_saved`, and
  `round(savings_ratio × 100)`. Thousands separators are byte-pinned; the session clause's token
  form is the §2.2 short form (`~310k` from 310,880).
- The **session clause** reads the server's in-memory session usage counters: the count of
  `search` events THIS session recorded and their summed `tokens_saved` (values read off each
  already-built record — no new accounting). The running total **includes the current call**, is
  accrued whatever the footer mode (so enabling `session` mid-project shows honest totals), and
  resets with the server process — it never leaks across sessions.
- One line, no blank line above (keeps the context cost to a single line — the whole point of it
  being opt-in).

---

## 4. Config, CLI, and docs surface

- **Config:** new `mcp:` block in `.cce/config` with `result_footer` (M2). New `McpConfig::load`
  mirroring `OutputConfig`/`RetrievalConfig` (tolerant parse, default `off`).
- **`cce init`:** the report's "confirm it was used" step now points at `cce usage` alongside
  `cce dashboard`. (`cce init` does not generate a `.cce/config`, so the footer key is documented
  in `docs/mcp.md` rather than written commented-out — see §9.)
- **CLI:** new `Command::Usage { … }` in `main.rs` → `cmd_usage`, reusing `aggregate` /
  `federated_metrics_json_since` + the small `--since` event pre-filter and the two byte-pinned
  renderers (`crate::usage`).
- **Docs:** `docs/mcp.md` (footer toggle + "off by default for context hygiene" + `cce usage` as a
  confirm-usage signal), `docs/dashboard.md` (cross-link `cce usage`; the §2.5 additive fields),
  `docs/how-to.md` (the "who used CCE" recipe), and the command table in `README.md`.
  `SPEC-MCP.md` gains the footer note (output formatting only; contract unchanged).

## 5. Conformance & tests

1. **Footer-off byte-identity:** the MCP goldens + `conformance.json` unchanged; an explicit test
   asserts the default and `off` serve no footer byte and equal each other.
2. **Toggle-invariance:** same query, `off` vs `on` vs `session` ⇒ identical recorded `search`
   event (modulo the per-call `ts`/`id`/`latency_ms`); only the returned text differs. (Guards
   Invariant 1.)
3. **Footer grammar goldens:** byte-pinned `on` and `session` footer lines (unit) + the live line's
   shape and determinism over the real binary.
4. **`cce usage` goldens:** byte-pinned human render + `cce.usage/v1` JSON with injected `now`
   (unit), plus process-level byte-pins for the wall-clock-free forms (all-time, ISO `--since`),
   single-repo and `--workspace` (the latter proving the root log is folded in — a `source:"mcp"`
   root event appears in `by_source.mcp` but not `by_package`).
5. **Dashboard parity:** tests run BOTH real paths over one fixture log — the dashboard's
   `/api/metrics` body vs `cce usage --json` — and assert `totals`/`by_source`/`by_package`/recent
   agree field-for-field (single-repo and workspace).
6. **`--since` filter:** events before the cutoff excluded; malformed `--since` errors listing the
   accepted forms.
7. **Cross-engine anchor (deferred to cce-ruby's catch-up):** the `cce.usage/v1` body from the
   shared fixture log matches between cce-rust and cce-ruby.

## 6. Versioning & rollout

- **v2.8.0**, additive. `SYNC_FORMAT_VERSION` is **untouched** (no cache/artifact change). MCP
  protocol version **untouched** (no new tool; the footer is output formatting behind a config
  flag).
- **Rust-first**; cce-ruby reconciles to the `cce.usage/v1` and footer goldens afterward.

## 7. Out of scope (deferred)

- **A runtime MCP tool to flip the footer** (à la `set_output_compression`). Config-only in v2.8
  keeps the agent from toggling its own observability; revisit if a real need appears.
- **A `usage` MCP tool / resource** exposing the aggregate to the agent. The question is a *user's*,
  answered by *user* surfaces (CLI/dashboard); feeding usage back to the agent is a separate idea.
- **Per-package attribution of federated agent searches** (issue #28's Option 2/hybrid). Unchanged
  here: federated searches stay out of `by_package` in both new surfaces.
- **Cost in `$`** on `cce usage` — `cce savings` already owns the `$` estimate; keep `usage` about
  counts + tokens to avoid two divergent money numbers.

## 8. Acceptance bar ("done =")

- `cce usage --since 24h` and `cce usage --workspace --since 24h` print the byte-pinned block;
  `--json` emits `cce.usage/v1`; numbers equal the dashboard's for the same window (test-proven,
  both paths over one fixture).
- With `mcp.result_footer: on`, a `context_search` result carries the one-line footer; the recorded
  `search` event is **identical** to the `off` run (per §5.2).
- Full suite green; `conformance.json` + the MCP goldens unchanged; clippy + fmt clean; docs
  updated.

## 9. Drift fixed while renumbering (v2.7 draft → v2.8 as-built)

1. **Latency needed a home.** The draft's human render showed a per-source latency mean, but the
   aggregate carried no latency at all. v2.8 adds `by_source.*.mean_latency_ms` (and the read-side
   `latency_ms` on the parsed search event) **additively** — §2.5 — so the render stays a pure
   projection of the one aggregate rather than a side computation.
2. **Recent needed `source`.** Same story: the recent view labels each query `mcp`/`cli`, so
   `recent_searches[].source` joins the aggregate additively instead of `cce usage` re-deriving a
   second recent list.
3. **The session clause reads the session usage counters, not the L6 ledger.** The draft said the
   clause "reads the L6 session ledger totals", but the L6 ledger (SPEC-V2.5 Layer 6) records
   queries/ids only — it has no token totals. As built, the server keeps two in-memory session
   counters (searches, summed `tokens_saved`), accrued from each already-built record; the "omit
   the clause when no session ledger is active" case is gone because a running server always has
   the counters (they start at zero).
4. **`cce init` writes no `.cce/config`.** The draft had init writing a commented
   `mcp.result_footer: off` into "the generated `.cce/config`", but `cce init` has never generated
   that file (it writes `.mcp.json`, `CLAUDE.md`, `.gitignore`). As built, init's report points at
   `cce usage` / `cce dashboard` as the prove-the-agent-used-CCE surfaces, and the footer key is
   documented in `docs/mcp.md`.
5. **Byte-identity of the recorded event is asserted modulo `ts`/`id`/`latency_ms`.** Those three
   are per-call values (wall clock + unique id) that differ between any two runs by construction;
   every other recorded byte is compared exactly (§1.1, §5.2).
6. **Exact pinned formats.** §2.2/§2.3 now show the as-built byte-pinned renders (alignment rules,
   the short token form, the JSON's alphabetical `by_source` key order from canonical
   serialization) instead of the draft's illustrative sketches.
