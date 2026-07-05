# CCE real-world eval harness (SPEC-V2.5 §7)

`cce savings` reports the **internal** savings ledger — tokens saved **vs reading
the whole file**. That number is honest but it is **not** your real end-to-end
agent cost, because a modern agent doesn't read whole files; it greps and reads
slices. This harness measures the number that actually matters: the **real
end-to-end A/B delta** of running your agent with CCE **off** vs **on**.

The method is deliberately:

- **Headless & reproducible** — a fixed question set with pinned ground truth.
- **Correctness-gated** — a cheap non-answer ("I couldn't find it") is a *punt*
  and never counts as a saving. The headline is computed only over questions
  answered **correctly in both arms** (paired).
- **Cost-primary** — cost is the primary metric, and it **includes sub-agents**
  (raw token totals undercount sub-agent work, so we record real cost).

## Files

| File | Purpose |
|------|---------|
| `questions.jsonl` | The pinned question set + ground truth (`must_include` substrings). |
| `run.sh` | Template that drives your headless agent twice per question (off/on) and emits run JSONL. **Not run in CI** — needs a live agent. |
| `runs.example.jsonl` | Canned run outputs showing the exact shape; used to demo the aggregation. |

## Formats

**Question** (`questions.jsonl`, one JSON object per line):

```json
{"id": "q1", "question": "…", "must_include": ["auth.py", "hash_password"]}
```

An answer is **correct** iff it contains every `must_include` substring
(case-insensitive) and is not a punt.

**Run output** (produced by `run.sh`, one JSON object per line):

```json
{"question_id": "q1", "condition": "off", "answer": "…",
 "cost_usd": 0.42, "subagent_cost_usd": 0.00, "punted": false}
```

`condition` is `off` or `on`. `total cost = cost_usd + subagent_cost_usd`.
`punted` is optional; punts are also auto-detected from the answer text.

## Running

Aggregate recorded runs (no model call — pure, deterministic):

```sh
cce eval runs.jsonl --questions eval/questions.jsonl
# or JSON:
cce eval runs.jsonl --questions eval/questions.jsonl --json
```

Try it now on the canned example:

```sh
cargo run -q -- eval eval/runs.example.jsonl --questions eval/questions.jsonl
```

To produce real runs, wire your headless agent into `run.sh` (see its header),
then:

```sh
./eval/run.sh > runs.jsonl
cce eval runs.jsonl --questions eval/questions.jsonl
```

## What CI covers

CI does **not** call a model. The parsing, correctness-gating, punt-detection, and
cost-primary paired aggregation are all pure functions in `src/eval.rs`, unit-
tested deterministically on canned run outputs (`cargo test eval::`), plus an
integration test that runs `cce eval` over `runs.example.jsonl`. Run this harness
per release to catch savings/correctness regressions — context tested like code.
