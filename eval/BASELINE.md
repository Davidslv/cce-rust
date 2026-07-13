# CCE A/B eval — first measured baseline

> The end-to-end A/B eval "the project says actually matters" had **never been
> run** (gap **G5**; Epic Davidslv/signal-engine#8 · U3.1). This is that run:
> the first real cost-delta measurement, produced by a live agent — not the
> canned `runs.example.jsonl`. Read the number honestly. It is a **baseline**,
> not a marketing figure, and it is **not** the with/without-corpus moat number
> owed against labelled incidents (that is U3.2 / #18).

## Result

```
CCE eval — real end-to-end A/B (cost-primary, correctness-gated, paired)
  questions: 6   skipped runs: 0
  off : correct 6/6 runs · punts 0 · incorrect 0 · correct_cost $0.80 · mean $0.13
  on  : correct 6/6 runs · punts 0 · incorrect 0 · correct_cost $0.85 · mean $0.14
  paired-correct (both arms): 6
  paired cost: off $0.80 · on $0.85 · saved $-0.05  (-6.0%)
```

**Headline (be honest about it): on this repo, cce ON cost ~6% *more* than cce
OFF — it did not save money.** Both arms answered all six pinned questions
correctly, so the comparison is fully paired; the delta is real, small, and
negative. cce's own *retrieval* is not the cost driver — it returns in **~2.4 ms
median** (1.94–2.51 ms over 2 544 chunks). The cost lives in the agent's file
reads, and on a small, well-structured repo a plain `grep`/`read` agent already
finds the answer cheaply, so the extra MCP round-trips in the ON arm are pure
overhead here.

This is the expected shape of a **lower bound**, and it is consistent with the
recorded engine limits rather than a surprise:

- **Offline `hash` embedder (OD5 / cce#36 — the BM25+hash semantic ceiling).**
  With no Ollama, `cce search` rarely surfaces the *exact* answer chunk at the
  top of `k`, so the ON agent still opens files to confirm — it pays the MCP
  cost without skipping the reads.
- **Small repo (185 files).** Blind `grep` is fast and reliable here; retrieval
  has little exploration cost to remove. The delta is expected to move as the
  target grows, when a semantic embedder is enabled, and on the *knowledge*
  corpus where there is no source tree to `grep` at all.

Per the measurement-honesty discipline (R29): this baseline says only what it
measured — **six questions, one model, one repo, the offline embedder.** It does
not license a "cce saves N%" claim, in either direction, beyond this setup.

## Method (SPEC-V2.5 §7)

Each pinned question is answered twice by the **same** model against the **same**
repo: once with cce **off** (a plain agent — `Bash`/`Grep`/`Glob`/`Read`) and
once with cce **on** (that same agent *plus* the cce MCP context tools it may
choose to use). The marginal value of cce is the off→on delta. Correctness is
the harness's `must_include` substring gate (case-insensitive; a punt never
counts); the agent is never shown the expected answer. Cost is **real** —
`claude --output-format json` reports `total_cost_usd` per run; sub-agents are
disabled (`--disallowedTools Task`) so `cost_usd` is the whole run and
`subagent_cost_usd` is honestly `0`.

| Field | Value |
|-------|-------|
| Date | 2026-07-13 |
| Agent | Claude Code `claude -p`, CLI 2.1.207, model `sonnet` |
| Repo + corpus | cce-rust @ `e70c20f`, indexed by cce 2.9.0 (`hash` embedder, 185 files, 2 544 chunks) |
| Questions | `eval/questions.jsonl` (n = 6), spanning the demo fixtures (`test/fixture/base/`) and cce's own source/docs |
| Runs | `eval/runs.jsonl` (12 = 6 × {off, on}) |
| Driver | `eval/run-claude.sh` (the live-agent instantiation of `eval/run.sh`) |
| Retrieval latency | median 2.36 ms, range 1.94–2.51 ms (retrieval-only `latency_ms`, per performance.md's "fold latency into the G5 run") |

## Reproduce

```sh
cargo build --release
./target/release/cce index . --no-metrics
CCE_BIN=./target/release/cce CCE_EVAL_REPO="$PWD" AGENT_MODEL=sonnet \
  ./eval/run-claude.sh eval/questions.jsonl > eval/runs.jsonl
./target/release/cce eval eval/runs.jsonl --questions eval/questions.jsonl
```

The runner is non-deterministic (a live model), so re-running moves the cents;
the committed `eval/runs.jsonl` is *this* baseline's frozen record. Re-run per
the eval-as-standing-infrastructure rules (U3.3 / #19): any model swap or major
corpus change starts a fresh baseline with no carried-over confidence.
