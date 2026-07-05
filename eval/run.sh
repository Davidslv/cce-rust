#!/usr/bin/env bash
# eval/run.sh — produce A/B run outputs for the CCE real-world eval harness.
#
# WHY: `cce savings` reports the internal "vs full-file" ledger, which is honest
# but is NOT the real end-to-end agent cost. This script drives the A/B method
# SPEC-V2.5 §7 formalises: run each pinned question through your agent twice —
# once with cce OFF (no MCP server / no context tool) and once with cce ON — and
# record one JSON line per run. The deterministic aggregation, correctness-gating,
# and punt-detection then live in `cce eval` (unit-tested; no model call in CI).
#
# WHAT it emits: newline-delimited JSON, one object per (question, condition):
#   {"question_id": "...", "condition": "off"|"on", "answer": "...",
#    "cost_usd": <float>, "subagent_cost_usd": <float>, "punted": <bool?>}
# cost_usd is the primary answer cost; subagent_cost_usd is the cost of any
# sub-agents the run spawned (raw token totals undercount sub-agents, so we record
# real cost). Mark an explicit non-answer with "punted": true.
#
# This script is intentionally NOT run in CI (it needs a live agent + API keys).
# It is the documented, reproducible recipe; the canned `runs.example.jsonl` shows
# the exact output shape and is what the aggregation is demonstrated on:
#
#   cargo run -q -- eval eval/runs.example.jsonl --questions eval/questions.jsonl
#
# Usage (pseudocode — wire in your own headless agent invocation):
#   ./eval/run.sh > runs.jsonl
#   cce eval runs.jsonl --questions eval/questions.jsonl
#
set -euo pipefail

QUESTIONS="${1:-eval/questions.jsonl}"

# Replace `agent_answer` with a headless call to YOUR agent. It must print the
# answer text on stdout and the run's USD cost on fd 3 (answer cost) and fd 4
# (sub-agent cost). This stub errors on purpose so nobody mistakes it for a live
# runner.
agent_answer() {
  echo "eval/run.sh is a template: wire in your headless agent (see the header)." >&2
  exit 2
}

while IFS= read -r line; do
  [ -z "$line" ] && continue
  qid=$(printf '%s' "$line" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  question=$(printf '%s' "$line" | python3 -c 'import sys,json;print(json.load(sys.stdin)["question"])')
  for cond in off on; do
    # Toggle cce for the two arms here (e.g. start/stop the MCP server, or pass a
    # flag your agent understands), then capture the answer + cost.
    answer=$(agent_answer "$cond" "$question")
    printf '{"question_id":%s,"condition":"%s","answer":%s,"cost_usd":0.0,"subagent_cost_usd":0.0}\n' \
      "$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$qid")" \
      "$cond" \
      "$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$answer")"
  done
done < "$QUESTIONS"
