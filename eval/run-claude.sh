#!/usr/bin/env bash
# eval/run-claude.sh — a CONCRETE live-agent driver for the A/B eval harness.
#
# WHY: `eval/run.sh` is the generic template; its `agent_answer` errors on
# purpose so nobody mistakes the recipe for a live runner. This script is the
# recipe *instantiated* with a headless Claude Code agent (`claude -p`), used to
# produce the first measured baseline (Epic #8 · U3.1 / gap G5). It is the
# reproducible command behind `eval/runs.jsonl` + `eval/BASELINE.md`.
#
# METHOD (SPEC-V2.5 §7): each pinned question is answered twice by the SAME
# model against the SAME repo — once with cce OFF (a plain agent: Bash/Grep/
# Glob/Read) and once with cce ON (that same agent PLUS the cce MCP context
# tools it may choose to use). The marginal value of cce is the OFF→ON delta.
# Cost is REAL: `claude --output-format json` reports `total_cost_usd` per run.
# Sub-agents are disabled (`--disallowedTools Task`), so `cost_usd` is the whole
# run cost and `subagent_cost_usd` is honestly 0.
#
# The agent is never told the expected answer — the harness's `must_include`
# substring gate judges each answer independently (a punt never counts).
#
# NOT run in CI (needs a live agent + API keys). Deterministic aggregation of
# whatever this emits lives in `cce eval` (unit-tested; no model call in CI).
#
# Usage:
#   CCE_BIN=./target/release/cce CCE_EVAL_REPO="$PWD" \
#     ./eval/run-claude.sh eval/questions.jsonl > eval/runs.jsonl
#   ./target/release/cce eval eval/runs.jsonl --questions eval/questions.jsonl
set -euo pipefail

QUESTIONS="${1:-eval/questions.jsonl}"
REPO="${CCE_EVAL_REPO:-$PWD}"
CCE_BIN="${CCE_BIN:-./target/release/cce}"
MODEL="${AGENT_MODEL:-sonnet}"

MCP_CONFIG="$(mktemp)"
trap 'rm -f "$MCP_CONFIG"' EXIT
cat > "$MCP_CONFIG" <<EOF
{"mcpServers":{"cce":{"command":"$CCE_BIN","args":["mcp","--dir","$REPO"]}}}
EOF

BASE_TOOLS="Bash,Grep,Glob,Read"
ON_TOOLS="$BASE_TOOLS,mcp__cce__context_search,mcp__cce__expand_chunk,mcp__cce__related_context,mcp__cce__index_status"

SYS='Answer the question about THIS repository concisely, in 1-3 sentences. Name the specific file and the specific function/identifier/value. Do not speculate: if you genuinely cannot locate it, reply that you could not find it.'
ON_HINT=' You additionally have the cce context tools (mcp__cce__context_search, mcp__cce__expand_chunk, mcp__cce__related_context): a fast semantic + BM25 index of this repo. Prefer them to locate the relevant code, then read the cited file to confirm.'

run_agent() {
  local cond="$1" question="$2"
  if [ "$cond" = "on" ]; then
    claude -p "${SYS}${ON_HINT}"$'\n\nQuestion: '"$question" \
      --model "$MODEL" --strict-mcp-config --mcp-config "$MCP_CONFIG" \
      --allowedTools "$ON_TOOLS" --disallowedTools "Task" --output-format json
  else
    claude -p "${SYS}"$'\n\nQuestion: '"$question" \
      --model "$MODEL" --strict-mcp-config \
      --allowedTools "$BASE_TOOLS" --disallowedTools "Task" --output-format json
  fi
}

while IFS= read -r line; do
  [ -z "$line" ] && continue
  qid=$(printf '%s' "$line" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  question=$(printf '%s' "$line" | python3 -c 'import sys,json;print(json.load(sys.stdin)["question"])')
  for cond in off on; do
    out=$(run_agent "$cond" "$question")
    printf '%s' "$out" | QID="$qid" COND="$cond" python3 -c '
import sys, json, os
d = json.load(sys.stdin)
ans = d.get("result") or ""
cost = float(d.get("total_cost_usd") or 0.0)
rec = {"question_id": os.environ["QID"], "condition": os.environ["COND"],
       "answer": ans, "cost_usd": round(cost, 6), "subagent_cost_usd": 0.0}
print(json.dumps(rec))
'
  done
done < "$QUESTIONS"
