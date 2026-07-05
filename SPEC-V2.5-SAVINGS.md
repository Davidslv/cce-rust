# CCE v2.5.0 — The Savings Layers (chunk/output/grammar compression · memory · progressive disclosure · the 7-bucket ledger)

**Status:** normative build specification for a new track. **Rust-first:** ships on
**cce-rust** as v2.5.0 now; cce-ruby stays at 2.4.1 and catches up to the same pinned
format in a later reconciliation track. Additive — single-repo `conformance.json`, the
CCE Sync artifact, and the existing v2.4 MCP tool contract (the three tools) stay unchanged.

## 0. Why this track exists (read first)

Our v2.4 clean-room engine implemented **retrieval only** — `context_search` returns
the relevant chunks instead of whole files. But end-to-end benchmarking shows retrieval
alone does **not** reliably beat a modern agent's own grep/partial-read baseline. The
headline savings of code-context tools come from **seven** layers, not one:

- the classic "94% saved" figure is measured **vs reading whole files** — a baseline modern
  agents don't use; against an agent that already greps, real savings are far lower (~20–50%);
- returning **full chunk bodies** can cost more than a targeted grep+read; the win needs
  **compressed signatures + docstrings** (~89% smaller per chunk);
- the biggest savings live in the output/grammar/memory/summarization layers a
  retrieval-only engine doesn't have.

This track builds layers 2–7 and the measurement ledger, so CCE actually reduces cost
on the workloads it's meant for, and so we can report savings **honestly** (both the
internal "vs full-file" number and the real-world end-to-end number).

## 1. Invariants (non-negotiable, inherited)

1. **Deterministic + format-pinned (Rust-first).** Every transform is a pure function of
   `(chunk/AST, language pack, level)`, serialized to a **byte-pinned** format. **cce-rust
   is the reference implementation for this track**; cross-language byte-identity with
   cce-ruby is **deferred, not abandoned** — because every format is pinned and deterministic,
   cce-ruby can later reconcile to cce-rust's exact bytes. Golden fixtures are authored from
   cce-rust and become the target for Ruby's future catch-up.
2. **Additive & backward-compatible.** Old indexes, old Sync artifacts, old metrics logs
   still load. Every new field is optional and degrades gracefully.
3. **Offline-first.** No layer requires the network. Pricing tables for `cce savings` are
   embedded (updatable), never fetched at runtime by default.
4. **Secret-safe.** Anything persisted (memory, summaries) runs through the v2.1 redactor
   before it is written.
5. **Honest measurement.** The ledger records savings **vs the full-file baseline** AND
   labels it as such; docs state plainly it is not the real-world Claude-Code delta, and
   ship a harness to measure the real end-to-end number.
6. **Precision over volume** (guiding thesis):
   *provide all the necessary context, and absolutely no unnecessary context.* Every layer
   is judged on **what it stops passing**, not what it passes. Default to less; expand on
   demand (Layer 7). A layer that adds tokens without removing more is a regression.

## 2. The seven layers

### Layer 1 — Retrieval (already shipped; formalize accounting)
`context_search` returns ranked chunks. **Add** per-result accounting: `baseline_tokens`
= token count of the **full files** the returned chunks belong to (deduped by file);
`served_tokens` = tokens actually returned; `saved = baseline − served`. Attribute to the
`retrieval` bucket. (Token counter: a deterministic, cross-language whitespace/BPE-approx
counter defined in §4 — same counter both engines.)

### Layer 2 — Chunk compression  *(highest leverage)*
A deterministic, AST-driven reduction of a chunk to its **signature form**. `context_search`
gains `detail: "signature" | "compact" | "full"` (config default `compact`).
- **signature**: the declaration line(s) only — e.g. Ruby `def charge(amount)`, class/module
  headers, constant names; TS/JS `export function render(): string`; C prototypes.
- **compact** (default): signature **+** the leading doc-comment/docstring **+** the first
  non-trivial body line, then an elision marker `# …(N lines)` (exact marker byte-pinned).
- **full**: today's behaviour (whole chunk body).
Each language **pack** declares its signature/doc extraction rules against exact tree-sitter
node types (as packs already do for chunk kinds). The transform is deterministic and must
round-trip through `expand_chunk` (Layer 7) to recover `full`. Report `chunk_compression`
savings = `full_tokens − served_tokens`.

### Layer 3 — Grammar compression
A **compact, stable serialization** of tool output: one canonical, minimal-token result
format (no prose scaffolding, sorted deterministic fields, elision markers), plus an
optional response-format hint the tool advertises. Lowest impact; spec it as "the tool
never emits filler; the result grammar is fixed and byte-pinned." Report `grammar` savings
= tokens saved vs the verbose v2.4 result format (measured against a pinned sample).

### Layer 4 — Output compression  *(cheap, high impact)*
`cce init` injects a **marker-bounded block** into `CLAUDE.md`/`AGENTS.md` with a configurable
**level**: `off | lite | standard | max`.
- **off**: nothing. **lite**: "be concise, drop filler." **standard** (default): "answer in
  the fewest words that are correct; show only changed lines in code edits, never rewrite
  whole files; no preamble/postamble." **max**: telegraphic; code shown as minimal diffs.
The block text for each level is **static and byte-pinned** (same on both engines). Add an
MCP tool `set_output_compression(level)` to switch at runtime (writes the session preference,
does not rewrite CLAUDE.md). Savings here are **output-side** — accounted by comparing
output tokens against an `off` control in the benchmark harness (§7), not self-estimated.

### Layer 5 — Memory recall
Cross-session memory so the agent doesn't re-derive. Two MCP tools + a store:
- `record_decision(text, tags?, area?)` → append to `.cce/memory.jsonl` (redacted first),
  content-addressed id = first 16 hex of SHA-256(normalized text).
- `session_recall(query, top_k?)` → hybrid search over stored decisions/areas (reuse the
  retrieval engine), returns compact entries + ids.
Deterministic storage & retrieval; workspace-aware (per-member + workspace-level memory).
Never pushed by Sync unless a future opt-in; local-only by default (it's conversational, not
reproducible). Report `memory` savings = baseline (re-deriving) is **not** auto-estimated;
counted in the harness.
**Anti-pollution rule:** naive "save every answer and replay it"
POLLUTES context — a bad answer re-injected makes things worse, not cheaper. Therefore:
`record_decision` is for **validated** decisions only (explicit call, never auto-capture of
raw model output); `session_recall` is **precision-filtered** (confidence-thresholded, small
top_k) and returns entries the agent must choose to use, not an auto-injected blob. Memory
that lowers answer quality is a bug even if it lowers tokens.

### Layer 6 — Turn summarization
For long MCP sessions, keep the running context small. Tool `summarize_context(scope?)`
returns a deterministic, compact summary of what has been retrieved/decided so far in the
session (server keeps a per-session ledger of tool calls + returned chunk ids; the summary
is a bounded, structured digest, byte-deterministic given the same call sequence). Optional
auto-trigger when the session's returned-token budget exceeds a config threshold. Report
`turn_summarization` savings vs re-sending the raw history.

### Layer 7 — Progressive disclosure
Pairs with Layer 2. `context_search` returns **compact** chunks + ids; the agent pulls detail
only when needed:
- `expand_chunk(chunk_id, scope?: "body"|"file"|"neighbors")` → the full body / enclosing
  file slice / graph-neighbors of a previously-returned chunk.
- `related_context(chunk_id, top_k?)` → import-graph neighbors on demand (reuse v2.0 graph).
Deterministic. This is what makes Layer 2 safe: compact-by-default, expand-on-demand.

## 3. The savings ledger (measurement — cross-cutting)

Extend the metrics event schema (additive) with a `savings` object bucketed by the seven
layers: `{retrieval, chunk_compression, grammar, output, memory, turn_summarization,
progressive_disclosure}`, each `{saved_tokens, baseline_tokens}`. Add:
- **`cce savings`** — prints the per-bucket ledger, totals, and **dollar estimate** from an
  **embedded, offline** pricing table (model → $/Mtok in/out; updatable via a checked-in
  file; never fetched at runtime).
- **Dashboard panel** `savings_by_layer` in `/api/metrics` (cross-language identical shape),
  loopback-only, purely log-derived (no network — consistent with v2.4.1 D42).
- **Honesty:** every surface that shows the ledger MUST label it *"vs full-file baseline —
  not your real end-to-end agent cost"* and link the real-world harness (§7).

## 4. Deterministic token counter

Define ONE cross-language token counter used everywhere savings are computed: a pinned,
dependency-free approximation (e.g. bytes/4 with a fixed rule for whitespace runs and CJK) —
specified byte-exactly so Ruby and Rust agree on every count. It is an **estimator**, labeled
as such; it is NOT a model tokenizer. Golden test: a fixture corpus → identical counts both
engines.

## 5. Config (`~/.cce/config.yml` and/or `.cce/config`)

```
retrieval:
  detail: compact            # signature | compact | full   (Layer 2 default)
  top_k: 8
  confidence_threshold: 0.0  # drop results below (Layer 2/1)
  max_tokens: null           # optional hard cap on returned context
output:
  level: standard            # off | lite | standard | max   (Layer 4)
memory:
  enabled: true              # Layer 5
summarization:
  auto_tokens: null          # Layer 6 auto-trigger threshold (null = manual only)
savings:
  pricing: builtin           # embedded table id; offline
```
All optional; absent ⇒ the documented defaults. Defaults chosen to save by default
(`detail: compact`, `output.level: standard`).

## 6. MCP tool contract additions (identical both engines)

Existing (unchanged): `context_search`, `index_status`, `record_feedback`.
`context_search` **gains** optional inputs `detail`, `max_tokens`, `confidence_threshold`
(all backward-compatible; absent ⇒ config defaults), and its output includes `chunk_id`s
suitable for `expand_chunk`.
New tools (exact schemas byte-identical across engines): `expand_chunk`, `related_context`,
`record_decision`, `session_recall`, `set_output_compression`, `summarize_context`. Each has
a pinned JSON input schema + a byte-pinned output grammar. `tools/list` ordering is fixed.

## 7. Testing (hermetic, deterministic, cross-language)

- **Determinism goldens:** for a shared fixture, each layer's transform (compact chunk,
  each output-level block, a memory entry, a session summary given a fixed call sequence,
  an `expand_chunk` round-trip) is byte-identical Ruby↔Rust (checked-in golden bytes +
  checksums, same reconciliation discipline as Sync).
- **Round-trip:** `expand_chunk` recovers the exact `full` chunk that `detail:full` returns.
- **Ledger:** per-layer `saved/baseline` computed deterministically on a fixture.
- **Real-world harness (shipped in-repo), run as an eval suite:** the A/B benchmark method
  we validated — run the same question through the agent with cce `off` vs `on`, headless,
  **correctness-gated** (cheap non-answers don't count), **cost-primary** (cost includes
  sub-agents; raw token counts undercount them). A fixed question set with pinned
  ground-truth, run per release to catch savings/correctness regressions — context tested
  like code. It measures the *real* end-to-end delta, the honest counterpart to the internal
  `cce savings` ledger.
- Gates unchanged: Ruby `rake test` ≥93%, Rust `test`+`clippy`+`fmt` ≥92%; single-repo
  `conformance.json` byte-identical; offline-first intact.

## 8. Documentation (first-class, VERIFIED — cold-start)

- README "Token savings — honestly": explain the 7 layers, the full-file-vs-real-world
  distinction, how to read `cce savings` vs the real-world harness, and which workloads
  benefit (large codebase, many cross-file queries, long sessions) vs which don't
  (single-file locate). No 94% headline without the asterisk.
- `docs/savings.md`: each layer, its config, its determinism guarantee, its accounting.
- Cold-start VERIFIED run recorded in `docs/VERIFIED.md` (online + offline).

## 9. Phasing (build order by leverage)

- **Phase A (do first):** Layer 2 (chunk compression) + Layer 7 (expand/related) +
  Layer 4 (output rules) + the ledger + `cce savings` + the real-world harness. These flip
  the benchmark and are cheap/deterministic.
- **Phase B:** Layer 5 (memory), Layer 6 (turn summarization), Layer 3 (grammar), auto
  thresholds. Compounding value over long sessions.

## 10. Version & sequencing

Ships as **2.5.0 on cce-rust first**, after v2.4.1. A single fresh agent builds it in
cce-rust, test-first — **Phase A then Phase B** — PR + merge on green. **cce-ruby stays at
2.4.1**; the spec is filed there as backlog so a later track reconciles it to cce-rust's
pinned v2.5 format. Re-run the real-world A/B harness (§7) with Phase A on to quantify the
delta before team review.
