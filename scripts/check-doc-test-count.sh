#!/usr/bin/env bash
#
# Doc test-count honesty gate (U9.2 — signal-engine#33).
#
# Why this exists: the docs advertise a suite size ("1002 passing tests",
# "cargo test  # 1002 tests") in several files. That number is a claim, and
# claims rot — the sweep that added this script found README.md alone citing two
# different stale figures (660 and 605) against a suite that had actually grown
# past 1000. This gate greps every doc that states a "<N> tests" figure and
# asserts it equals the real passing-test count, so the number can never
# silently drift from reality again.
#
# The count is derived from the compiled test harness's own `--list` output (no
# hardcoded number here, and no second full test run — CI has already built the
# tests by the time this runs).
set -euo pipefail
cd "$(dirname "$0")/.."

list_terse() {
  cargo test --all-targets --all-features -- --list --format terse 2>/dev/null
}

total=$(list_terse | grep -c ': test$' || true)
ignored=$(cargo test --all-targets --all-features -- --list --ignored --format terse 2>/dev/null | grep -c ': test$' || true)
passing=$(( total - ignored ))

if [ "$passing" -le 0 ]; then
  echo "check-doc-test-count: could not determine the passing-test count (got total=$total ignored=$ignored)" >&2
  exit 2
fi

echo "actual suite: ${passing} passing (+${ignored} #[ignore])"

# Every doc that advertises the suite size. Add new ones here.
docs=(README.md AGENTS.md CONTRIBUTING.md llms.txt docs/getting-started.md)

status=0
for f in "${docs[@]}"; do
  [ -f "$f" ] || { echo "check-doc-test-count: missing $f" >&2; status=1; continue; }
  # Each "<N> tests" / "<N> passing tests" claim (plural 'tests' only, so the
  # "+1 #[ignore] ... integration test" note is not matched) must equal $passing.
  while read -r claimed; do
    [ -z "$claimed" ] && continue
    if [ "$claimed" != "$passing" ]; then
      echo "DRIFT: $f claims '${claimed} tests' but the suite has ${passing} passing" >&2
      status=1
    fi
  done < <(grep -oE '[0-9]{2,5} (passing )?tests\b' "$f" | grep -oE '^[0-9]+')
done

if [ "$status" -eq 0 ]; then
  echo "check-doc-test-count: all docs agree with reality (${passing})"
fi
exit "$status"
