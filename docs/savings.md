# Token savings — the seven layers, honestly

This document describes the **Savings Layers** (v2.5): the seven ways CCE reduces
the tokens an agent spends on your codebase, the deterministic **ledger** that
measures them, the `cce savings` command that reports them, and — most
importantly — the **honest framing** you must keep in mind when reading any of
those numbers.

Built test-first from [`SPEC-V2.5-SAVINGS.md`](../SPEC-V2.5-SAVINGS.md). Additive:
the base engine, `conformance.json`, and the CCE Sync artifact are unchanged.

## Read this first — the honest disclaimer

Every savings figure CCE prints is measured **vs a full-file baseline**: the tokens
you would spend if a tool read the *whole file(s)* the returned chunks came from.
That is a real, reproducible number, but it is **not** your real end-to-end agent
cost, because a modern coding agent does not read whole files — it greps and reads
slices. So:

> **The ledger is labelled everywhere: "vs full-file baseline — not your real
> end-to-end agent cost."** There is no "94%" headline without that asterisk.

The real-world value is **workload-dependent**. CCE helps most on **large
codebases** with **many cross-file questions** and **long sessions**; it wins least
on a single-file locate where you already know the path (just open the file). To
measure the number that actually matters — the real end-to-end delta of running
your agent with CCE off vs on — use the [`cce eval`](#the-real-world-eval-harness)
A/B harness, the honest counterpart to the internal ledger.

The guiding thesis is **precision over volume**: *provide all the necessary
context, and absolutely no unnecessary context.* Every layer is judged on what it
**stops** passing, not what it passes — default to less, expand on demand. A layer
that adds tokens without removing more is a regression.

## The seven layers

Each layer is a deterministic transform and reports into its own bucket of the
ledger. The bucket names are fixed and identical across the Ruby and Rust engines:
`retrieval`, `chunk_compression`, `grammar`, `output`, `memory`,
`turn_summarization`, `progressive_disclosure`.

### L1 — Retrieval (the baseline win)

`context_search` / `cce search` returns ranked chunks instead of whole files.
Per result: `baseline_tokens` = the token count of the **distinct full files** the
returned chunks belong to; `served_tokens` = the tokens actually returned; the
difference is attributed to the **`retrieval`** bucket.

### L2 — Chunk compression *(highest leverage)*

A deterministic, **AST-driven** reduction of each chunk to its signature form,
declared per language pack. `context_search` gains a `detail` level:

| `detail` | What is served |
|---|---|
| `signature` | the declaration line(s) only — e.g. `def charge(amount)`, a class/module header, a C prototype |
| `compact` **(default)** | signature **+** the leading doc-comment/docstring **+** the first non-trivial body line, then an elision marker `… (+N lines)` |
| `full` | the whole chunk body (the pre-v2.5 behaviour) |

Compact is a **retrieval-time serialization only** — the store always keeps the
full body, so [`expand_chunk`](#l7--progressive-disclosure) recovers it exactly.
Container chunks (a class/module) render their header + doc + **member
signatures**; the Ruby pack includes the DSL declarations `has_many`,
`belongs_to`, and `validates`. Reported into **`chunk_compression`** as
`full_tokens − served_tokens`.

### L3 — Grammar compression

The MCP read-tool result **grammars** are byte-pinned to a compact, filler-free
format: one canonical result line per hit (`#. [score] file:start-end
(chunk_type/kind) #chunk_id`), sorted deterministic fields, no prose scaffolding.
Lowest impact, but self-measured: the **`grammar`** bucket is the tokens saved by
the compact grammar vs a pinned verbose baseline, counted with `cce.tokens/v1`.

### L4 — Output compression *(cheap, high impact)*

`cce init` writes a leveled `Output compression` block into `CLAUDE.md` steering
the agent's **own** replies, at `output.level` = `off | lite | standard | max`
(default **standard**): "answer in the fewest words that are correct; show only
changed lines in code edits, never reprint whole files; no preamble/postamble."
The block text per level is static and byte-pinned. The
[`set_output_compression`](mcp.md#6-set_output_compression--l4-dial-answer-terseness)
MCP tool switches the level for the running session (in memory; it does not rewrite
`CLAUDE.md`). Output savings are output-side, so they are measured against an `off`
control in the [eval harness](#the-real-world-eval-harness) — the ledger's
**`output`** bucket is not self-estimated.

### L5 — Memory

Cross-session memory so the agent does not re-derive settled decisions. Two MCP
tools over a local `.cce/memory.jsonl`:
[`record_decision`](mcp.md#7-record_decision--l5-remember-a-validated-decision) and
[`session_recall`](mcp.md#8-session_recall--l5-recall-remembered-decisions). The
store is **local-only** (never pushed by Sync — it is conversational, not
reproducible), **secret-scrubbed** before write (v2.1 redactor), and
content-addressed so recording the same decision twice is a no-op.

**Anti-pollution is the design centre:** a naive "save every answer and replay it"
scheme *pollutes* context — a bad answer re-injected makes things worse, not
cheaper. So `record_decision` is for **validated** decisions only (an explicit
call, never auto-capture of raw model output), and `session_recall` is
**precision-filtered** (score ≥ 0.30 and a shared query token, small `top_k`) and
returns entries the agent *chooses* to use, never an auto-injected blob. Memory
that lowers answer quality is a bug even if it lowers tokens. Reported into
**`memory`** (measured, not auto-estimated).

### L6 — Turn summarization

`summarize_context(scope)` returns a compact digest of what the session has done so
far — files and chunks touched, queries run, decisions recorded — so the agent can
compress a long session instead of re-sending the raw transcript. It is a
**deterministic, structured** digest of the server's per-session ledger, **NOT an
LLM summary**: the same sequence of tool calls always yields the same bytes, and it
needs no model and no network. Reported into **`turn_summarization`**.

### L7 — Progressive disclosure

The safety net that makes compact-by-default safe: compact chunks come with a
`chunk_id`, and the agent pulls detail only when it needs it.

- [`expand_chunk(chunk_id, scope)`](mcp.md#4-expand_chunk--l7-read-the-full-detail)
  — `scope=body` recovers the exact full body (round-trips `detail:full`);
  `scope=file` returns every chunk in the file; `scope=neighbors` returns
  import-graph-related chunks.
- [`related_context(chunk_id, top_k)`](mcp.md#5-related_context--l7-widen-via-the-import-graph)
  — import-graph neighbours (both imports **and** consumers) on demand.

Reported into **`progressive_disclosure`**.

## The deterministic token estimator (`cce.tokens/v1`)

All savings are counted with **one** cross-language estimator so the Ruby and Rust
engines agree on every number:

```
estimate_tokens(text) = max(1, floor(byte_length(text) / 4))
```

It is a pinned, dependency-free **estimator** (roughly 4 bytes per token), labelled
as such — it is **NOT** a model tokenizer, and it will not match a specific model's
exact token count. Its job is a stable, reproducible measuring stick, identical on
both engines and across runs.

## `cce savings` — reading the ledger

`cce savings` aggregates the `savings` object on every recorded `search` event into
the seven buckets, totals them, and prints an **offline** dollar estimate from an
embedded pricing table. It is purely log-derived and makes **zero network calls**.

```console
$ cce savings --dir ./src
CCE savings ledger  (vs full-file baseline — not your real end-to-end agent cost)
  source : ./src/.cce/metrics.jsonl
  pricing: cce.pricing/builtin-v1  (offline, embedded; edit src/pricing.json to change)

  layer                       saved_tokens   baseline_tokens
  retrieval                             56               404
  chunk_compression                     82               348
  grammar                              150               266
  output                                 0                 0
  memory                                 0                 0
  turn_summarization                     0                 0
  progressive_disclosure                 0                 0
  --------------------------------------------------------
  total                                288              1018

  estimated $ saved: $0.00  (default-model input rate)

  This is the internal "vs full-file" figure, NOT your real agent cost.
  For the real end-to-end delta, run the A/B eval harness: see eval/README.md.
```

- **Pricing is offline and embedded** (`cce.pricing/builtin-v1`, in
  `src/pricing.json`). Nothing is fetched at runtime; edit the file to change the
  rate. Small corpora round to `$0.00` — the point is the token deltas.
- Flags: `--dir DIR` / `--store PATH` / `--metrics PATH` to locate the log,
  `--json` to emit the machine-readable shape.
- `--json` emits `savings_by_layer` — the **same object** the dashboard exposes on
  `/api/metrics` (see [`dashboard.md`](dashboard.md)), each bucket
  `{saved_tokens, baseline_tokens}`, plus a `note` carrying the disclaimer:

```console
$ cce savings --dir ./src --json
{
  "estimated_dollars_saved": "0.00",
  "pricing_id": "cce.pricing/builtin-v1",
  "savings_by_layer": {
    "chunk_compression": { "baseline_tokens": 348, "saved_tokens": 82 },
    "grammar":           { "baseline_tokens": 266, "saved_tokens": 150 },
    "memory":            { "baseline_tokens": 0,   "saved_tokens": 0 },
    "note": "vs full-file baseline — not your real end-to-end agent cost",
    "output":            { "baseline_tokens": 0,   "saved_tokens": 0 },
    "progressive_disclosure": { "baseline_tokens": 0, "saved_tokens": 0 },
    "retrieval":         { "baseline_tokens": 404, "saved_tokens": 56 },
    "total":             { "baseline_tokens": 1018, "saved_tokens": 288 },
    "turn_summarization":{ "baseline_tokens": 0,   "saved_tokens": 0 }
  },
  "source": "./src/.cce/metrics.jsonl"
}
```

## The real-world eval harness

`cce savings` is the *internal* number. To measure the number that actually matters
— the real end-to-end delta — the repo ships an A/B harness under
[`eval/`](../eval/README.md). Its method is deliberate:

- **Headless & reproducible** — a pinned question set with ground truth
  (`eval/questions.jsonl`); an answer is correct iff it contains every
  `must_include` substring and is not a punt.
- **Correctness-gated** — a cheap non-answer ("I couldn't find it") is a *punt* and
  never counts as a saving; the headline is computed only over questions answered
  correctly in **both** arms (paired).
- **Cost-primary** — cost is the metric, and it **includes sub-agents** (raw token
  totals undercount sub-agent work, so real cost is recorded).

`cce eval` does **not** call a model — it aggregates run outputs recorded by
`eval/run.sh`, so the aggregation itself is pure and deterministic. Try it on the
bundled canned example:

```console
$ cce eval eval/runs.example.jsonl --questions eval/questions.jsonl
CCE eval — real end-to-end A/B (cost-primary, correctness-gated, paired)
  questions: 6   skipped runs: 0
  off : correct 5/6 runs · punts 1 · incorrect 0 · correct_cost $2.45 · mean $0.49
  on  : correct 6/6 runs · punts 0 · incorrect 0 · correct_cost $0.83 · mean $0.14
  paired-correct (both arms): 5
  paired cost: off $2.45 · on $0.67 · saved $1.78  (72.7%)
```

Run it per release to catch savings/correctness regressions — context tested like
code. See [`eval/README.md`](../eval/README.md) for the formats and how to drive a
live agent.

## Configuration

All keys are optional; absent ⇒ the documented default. In `~/.cce/config.yml`
and/or the project `.cce/config`:

```
retrieval:
  detail: compact            # signature | compact | full   (L2 default)
  top_k: 8
  confidence_threshold: 0.0  # drop results below this score
  max_tokens: null           # optional hard cap on returned context
output:
  level: standard            # off | lite | standard | max   (L4)
memory:
  enabled: true              # L5 (set false to make the memory tools a no-op)
summarization:
  auto_tokens: null          # L6 auto-trigger threshold (null = manual only)
savings:
  pricing: builtin           # embedded, offline pricing table id
```

The defaults are chosen to **save by default**: `detail: compact`,
`output.level: standard`.

## Determinism & offline-first

Every layer is a pure function of `(chunk/AST, language pack, level)` serialized to
a byte-pinned format; cce-rust is the reference implementation and the golden bytes
are the target for cce-ruby's later catch-up. No layer requires the network: the
pricing table is embedded, memory and summaries are local, and everything persisted
(memory, summaries) passes through the v2.1 redactor before it is written.

## See also

- [`docs/mcp.md`](mcp.md) — the nine MCP tools and their schemas.
- [`docs/dashboard.md`](dashboard.md) — the `savings_by_layer` panel on
  `/api/metrics`.
- [`eval/README.md`](../eval/README.md) — the real-world A/B harness.
- [`SPEC-V2.5-SAVINGS.md`](../SPEC-V2.5-SAVINGS.md) — the normative spec.
- [`docs/VERIFIED.md`](VERIFIED.md) — the cold-start transcript.
</content>
</invoke>
