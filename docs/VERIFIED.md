# CCE — cold-start verification transcripts

This file records the **mandatory cold-start passes**: the documented install +
walkthroughs followed from scratch, confirming **every documented command runs
verbatim** and its output matches the docs. A doc example that does not run is a bug.

Four passes are recorded, all real captured runs:

- **Offline cold start (THE guarantee)** — with **no network and no sync remote
  configured**, `index` · `search` · `stats` · `dashboard` · `workspace` · `cce mcp`
  all work exactly as documented (Part 1).
- **Online cold start** — the parts that *do* touch the network: `cce sync
  init/push/pull/verify` against a git cache, and the `cce init --remote`
  plug-and-play flow (Part 2).
- **v2.5 Savings Layers cold start** — a fresh `cce index` → a nine-tool `cce mcp`
  session (compact `context_search` → `expand_chunk` → `record_decision` /
  `session_recall` → `summarize_context`) → `cce savings`, all offline (Part 3).
- **Knowledge-corpus sync cold start (M5)** — feed → `cce knowledge index` →
  `cce knowledge push` to a local bare cache → a bare-directory consumer:
  `sync list` (knowledge section), `pull --all` (corpus installed), `verify
  --checksum-only` (knowledge row), MCP `index_status` + `context_search
  source: both` (Part 4).

- **Engine:** `cce 2.5.5` (release build). Parts 1–2 are re-confirmed at 2.5.5: v2.5
  is **additive**, so the offline core and Sync behave exactly as before (only the
  agent-facing `cce mcp` `context_search` now serves **compact** chunks with a
  `chunk_id`, shown updated in Part 1 §6 and exercised in Part 3).
- **Environment:** macOS (Darwin 25.3.0), `git version 2.50.1`.
- **git-LFS:** *not installed on this machine* — so the Sync walkthrough uses
  `--no-lfs` (a plain-git cache), and the LFS smoke test
  (`tests/sync.rs::lfs_round_trip_smoke_or_skip`) **SKIPS** gracefully, exactly as
  SPEC-SYNC §11 requires.
- **Isolation:** `CCE_HOME` was pointed at a temp dir so the working clone never
  touched `~/.cce`. Absolute paths appear as `$WORK`; the concrete commit shas differ
  per environment, but the **checksums and chunk counts are the real, stable values**
  a Ruby or CI build of the same `repo@sha` must reproduce.
- **Sync format:** the reconciled canonical artifact — `cce_version = "2.3"`, the
  **artifact format version, decoupled from the app version** (`SYNC_FORMAT_VERSION`).
  The v2.4.1 consolidation **and the v2.5 Savings Layers** are additive and do **not**
  change the artifact format, so the format version stays `2.3` and the content address
  stays `hash/2.3/…` — these releases do not invalidate existing caches or diverge from
  Ruby. The shared golden
  checksum on `test/fixture/samples` (`repo_id=cce/demo`, `sha=0…0`, 21 chunks,
  `edges:[]`) is
  `581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` — **equal to
  Ruby's**.

---

# Part 1 — Offline cold start (no network, no remote)

Everything in this part runs with **no network access and no sync remote
configured**. These commands make zero network calls by construction — the only
things that ever touch the network are the optional Ollama embedder, `cce sync
push/pull`, and installing the binary (see [Offline-first](../README.md#offline-first-verified)).

## 0. Versions

```console
$ git --version
git version 2.50.1 (Apple Git-155)
$ cce --version
cce 2.5.5
```

## 1. A tiny project

```console
$ cd "$WORK/myproject"
$ git init -q -b main
$ printf 'def hash_password(pw):\n    return pw + "salt"\n' > auth.py
$ printf 'import auth\n\ndef process_payment(amount):\n    return auth.hash_password(str(amount))\n' > payments.py
$ git add -A && git commit -q -m "initial project"
$ git rev-parse --short HEAD
4d8f068
```

## 2. `cce index` — build the local index (offline)

```console
$ cce index .
Indexed .
  files indexed     : 2
  files skipped     : 0
  sensitive skipped : 0
  total chunks      : 2
  embedder          : hash
  store             : ./.cce/index.json
  elapsed           : 0.002s
```

## 3. `cce search` — hybrid retrieval (offline)

```console
$ cce search "where is the password hashed" --top-k 3
 1. [0.825000] auth.py:1-2 (function/function_definition)
    def hash_password(pw):
 2. [0.816803] payments.py:3-4 (function/function_definition)
    def process_payment(amount):
query-id: 58bf435c5c8b  ·  rate with: cce feedback 58bf435c5c8b --helpful|--not-helpful
```

## 4. `cce stats` (offline)

```console
$ cce stats
Store: ./.cce/index.json
  chunks         : 2
  files          : 2
  avg token/chunk: 14.0
  store size     : 2904 bytes
  by language:
    python      : 2
  by kind:
    function_definition : 2
```

## 5. Secret-safety — a sensitive file is skipped by default

Add a secret file and re-index; the secure-by-default walk skips it, and the count
feeds the dashboard's secret-safety panel.

```console
$ printf 'AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY\n' > .env
$ cce index .
Indexed .
  files indexed     : 2
  files skipped     : 0
  sensitive skipped : 1
  total chunks      : 2
  embedder          : hash
  store             : ./.cce/index.json
  elapsed           : 0.001s
```

## 6. `cce mcp` — serve the local index to an agent (offline)

The editor drives `cce mcp` over stdio. Here the exact JSON-RPC it sends is piped in.
An agent `context_search` runs the same §6 retrieval as the CLI and tags its metrics
event `source: "mcp"`. Since v2.5 it serves **compact** chunks (each with a
`#chunk_id` to `expand_chunk`) — see the compact hint line below. The full nine-tool
session is exercised in [Part 3](#part-3--v25-savings-layers-cold-start-offline).

```console
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"process a payment","top_k":2}}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"index_status"}}' \
  | cce mcp --dir .
```

- **initialize** → `serverInfo {"name":"cce","version":"2.5.5"}`
- **context_search** (id 2) — compact chunks, each carrying a `#chunk_id` →

  ```
   1. [0.825000] auth.py:1-2 (function/function_definition) #188febb74a94633e
  def hash_password(pw):
  return pw + "salt"

   2. [0.816803] payments.py:3-4 (function/function_definition) #81ef5c5a698d2118
  def process_payment(amount):
  return auth.hash_password(str(amount))

  Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; related_context(chunk_id) for import-graph neighbours.
  query_id: ba44d660f3ad
  Rate this with record_feedback (query_id="ba44d660f3ad", helpful=true|false).
  ```

- **index_status** (id 3) →

  ```
  Index status
    store   : ./.cce/index.json
    indexed : yes
    chunks  : 2
    files   : 2
    embedder: hash
    by language:
      python      : 2
    by kind:
      function_definition   : 2
    source  : local (built by cce index)
    remote  : (no sync remote configured — pure local)
  ```

## 7. `cce dashboard` — the refreshed panels (offline, loopback-only)

```console
$ cce search "hash the password" --top-k 2   # a CLI search, tagged source: "cli"
$ cce dashboard --no-open
cce dashboard: serving http://127.0.0.1:8787/  (loopback only, read-only)
metrics log : ./.cce/metrics.jsonl
press Ctrl-C to stop.
```

`GET /api/metrics` returns the v2.4.1 panels — **agent-vs-human** (`by_source`),
**secret-safety**, and **index-freshness** — all computed from the log with **no
network call** (`index_freshness` is purely log-derived — no `remote_latest`/
`behind_remote`; that live comparison lives in `cce sync status`):

```console
$ curl -s http://127.0.0.1:8787/api/metrics | jq '{by_source, secret_safety, index_freshness, mean_top_score: .totals.mean_top_score}'
{
  "by_source": {
    "cli": { "searches": 2, "tokens_saved": 8, "mean_savings_ratio": 0.125, "mean_top_score": 0.825 },
    "mcp": { "searches": 1, "tokens_saved": 4, "mean_savings_ratio": 0.125, "mean_top_score": 0.825 }
  },
  "secret_safety": { "sensitive_skipped": 1, "index_runs": 2 },
  "index_freshness": {
    "indexes": 2,
    "source": "local",
    "sha": "4d8f068ab19ec441a5a80230d81f3be20c702b28",
    "indexed_ts": "2026-07-05T14:44:38Z"
  },
  "mean_top_score": 0.825
}
$ curl -s http://127.0.0.1:8787/api/health
{"status":"ok","events":5,"skipped":0}
```

The agent's `context_search` (`mcp`) sits beside the human's `cce search` (`cli`) —
the agent-vs-human split is proven offline. `index_freshness` carries only what the
log knows (`source: "local"`, the indexed `sha`, `indexed_ts`); the dashboard makes
**zero network calls**, so it works with the network fully down.

## 8. `cce workspace` — federated ecosystem (offline)

```console
$ cd "$WORK/shop"                       # web/ (package.json) + api/, committed
$ cce workspace init .
Wrote ./.cce/workspace.yml
workspace: shop
members (1):
  web              javascript   web · package web
$ cce index --workspace .
Indexing workspace: shop
  web              files    2 · chunks    2 · ./web/.cce/index.json
workspace totals: files 2 · chunks 2
cross-member edges (0) → ./.cce/workspace-graph.json
$ cce search "shopping cart" --workspace . --top-k 3
 1. [0.869194] web · src/index.ts:1-1 (function/function_declaration)
 2. [0.490902] web · package.json:1-2 (module/module)
$ cce stats --workspace .
workspace: shop
  web (package web)
    files : 2
    chunks: 2
      function_declaration: 1
      module            : 1
totals: files 2 · chunks 2
edges (0):
```

The federated dashboard's **per-package** panel breaks savings/searches/quality down
by member (populated here by a member-scoped `cce search --dir web`):

```console
$ cce dashboard --workspace . --no-open &   # loopback only
$ curl -s http://127.0.0.1:8787/api/metrics | jq '.by_package'
[
  {
    "package": "web",
    "searches": 1,
    "tokens_saved": 2,
    "mean_savings_ratio": 0.166667,
    "mean_top_score": 0.869194
  }
]
```

**Result: OFFLINE cold-start PASSED.** `index` · `search` · `stats` · `dashboard`
(all four refreshed panels) · `workspace` · `cce mcp` all ran verbatim with no network
and no remote.

---

# Part 2 — Online cold start (the network-touching parts)

The only workflows that need the network are `cce sync push/pull` (a git cache) and
installing the binary. This part runs them against a local **bare git remote**
(`file://`, fully hermetic — no internet), which exercises the exact same code path a
real SSH/HTTPS remote would.

## 1. Create the cache remote and a project

```console
$ git init --bare -q -b main "$WORK/cache.git"
$ cd "$WORK/billing"                     # src/auth.py + src/pay.py + .gitignore (.cce/)
$ git add -A && git commit -q -m "initial billing service"
$ git rev-parse --short HEAD
71400cd
$ cce index .                            # a hash index is what gets shared
```

## 2. `cce sync init` + `cce sync push`

```console
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
Configured sync remote: file://$WORK/cache.git
  git-LFS       : disabled
  repo_id       : github.com__acme__billing
  working clone : $CCE_HOME/sync/e1b946a294f94ae3
  config        : ./.cce/config
$ cce sync push
Pushed github.com__acme__billing@71400cd6d8c1211475e034aedf6d79f18a54e977
  key      : hash/2.3/github.com__acme__billing/71400cd6d8c1211475e034aedf6d79f18a54e977.cce
  checksum : 7deb21139c1fac4a74db5ab9dc936b4dd5859e26790a61ea478efba10f062337
```

## 3. A teammate clones, pulls, and verifies — bit-for-bit

```console
$ git clone -q "file://$WORK/billing" "$WORK/billing-teammate" && cd "$WORK/billing-teammate"
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
$ cce sync pull
Pulled github.com__acme__billing@71400cd6d8c1211475e034aedf6d79f18a54e977
  chunks   : 3
  checksum : 7deb21139c1fac4a74db5ab9dc936b4dd5859e26790a61ea478efba10f062337
  store    : ./.cce/index.json
  tree     : matches — pulled index used as-is
$ cce sync verify
verify OK: github.com__acme__billing@71400cd6d8c1211475e034aedf6d79f18a54e977
  checksum : 7deb21139c1fac4a74db5ab9dc936b4dd5859e26790a61ea478efba10f062337
```

The pull checksum `7deb2113…` is **identical** to the push checksum — content
addressability proven end-to-end. Search then runs fully offline over the pulled index:

```console
$ cce search "authenticate user password" --no-metrics --top-k 2
 1. [0.908333] src/auth.py:1-2 (function/function_definition)
    def login(user, password):
 2. [0.856835] src/pay.py:3-4 (function/function_definition)
    def charge(user, amount):
```

## 4. Sync freshness — where each fact lives

A `cce sync pull` records a `sync-pull` **index event** in the log, so the dashboard's
**index-freshness** panel shows the pulled provenance **purely from the log — no
network call** (`index_freshness` is exactly `{indexes, source, sha, indexed_ts}`):

```console
$ curl -s http://127.0.0.1:8787/api/metrics | jq '.index_freshness'
{
  "indexes": 1,
  "source": "sync-pull",
  "sha": "71400cd6d8c1211475e034aedf6d79f18a54e977",
  "indexed_ts": "2026-07-05T14:46:22Z"
}
```

The **live behind-remote comparison** belongs in `cce sync status` and MCP
`index_status` (which *do* consult the remote), never on the dashboard:

```console
$ cce sync status
remote        : file://$WORK/cache.git
git-LFS       : off
repo_id       : github.com__acme__billing
local cache   : 71400cd6d8c1211475e034aedf6d79f18a54e977 (7deb21139c1f)
remote latest : 71400cd6d8c1211475e034aedf6d79f18a54e977 (ref main)
working tree  : 71400cd6d8c1211475e034aedf6d79f18a54e977
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"index_status"}}' | cce mcp --dir .
Index status
  ...
  source  : pulled via cce sync (sha 71400cd6d8c1)
  remote latest: 71400cd6d8c1
  behind remote: no
```

(The `cce init --remote <url>` plug-and-play flow wraps the same `cce sync pull
--latest` and then wires `.mcp.json` + `CLAUDE.md` — see [`docs/mcp.md`](mcp.md).)

## 5. Offline-first & error paths (SPEC-SYNC §9)

**A. No remote configured — local commands are unaffected:**

```console
$ cce sync status
remote        : (none — pure local CCE)
```

**B. `push` refuses a dirty working tree:**

```console
$ cce sync push
error: refusing to push: the working tree is dirty. Commit your changes and push a clean sha (a cache is content-addressed by commit).
```

**C. `pull` reports a cache miss clearly:**

```console
$ cce sync pull --commit deadbeef…deadbeef --force
error: cache miss: hash/2.3/github.com__acme__billing/deadbeef…deadbeef.cce not found on the remote
```

**D. MCP is offline-safe under auto-pull when the remote is absent** — the server
still starts, warms silently (no crash/hang), and answers `index_status` (verified by
`tests/mcp.rs::mcp_is_offline_safe_when_the_configured_remote_is_absent`).

---

# Part 3 — v2.5 Savings Layers cold start (offline)

A fresh cold start of the [Savings Layers](savings.md) surface: `cce index` → a
**nine-tool** `cce mcp` session that exercises compact `context_search`,
`expand_chunk`, the memory pair (`record_decision` / `session_recall`), and
`summarize_context`, then `cce savings`. All offline, no remote — captured verbatim
on `cce 2.5.5`.

## 1. A tiny project + index

```console
$ cd "$WORK/myproject"
$ cat auth.py
"""Authentication helpers."""

import hashlib


def hash_password(password: str, salt: str) -> str:
    """Hash a password with a salt using SHA-256.

    This is the single place passwords are hashed; callers never
    hash inline. Returns the hex digest.
    """
    digest = hashlib.sha256((salt + password).encode()).hexdigest()
    return digest


def verify_password(password: str, salt: str, expected: str) -> bool:
    """Return True when the password hashes to the expected digest."""
    return hash_password(password, salt) == expected
$ cce index .
Indexed .
  files indexed     : 2
  files skipped     : 0
  sensitive skipped : 0
  total chunks      : 3
  embedder          : hash
  store             : ./.cce/index.json
  elapsed           : 0.003s
```

## 2. `tools/list` — the nine tools, fixed order

```console
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | cce mcp --dir .
# → tools (in order):
#   context_search · index_status · record_feedback · expand_chunk · related_context
#   · set_output_compression · record_decision · session_recall · summarize_context
```

## 3. A session — find → expand → remember → recall → summarize

The calls run in one `cce mcp` process so the per-session ledger (L6) accumulates.

```console
$ printf '%s\n' "$INIT" "$ACK" \
    '{...,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"where is the password hashed","top_k":2}}}' \
    '{...,"name":"expand_chunk","arguments":{"chunk_id":"04578fe98cec59bd","scope":"body"}}' \
    '{...,"name":"record_decision","arguments":{"text":"Passwords are hashed only in auth.hash_password (SHA-256 + salt); never hash inline.","tags":["security","auth"],"area":"auth"}}' \
    '{...,"name":"session_recall","arguments":{"query":"how are passwords hashed"}}' \
    '{...,"name":"summarize_context","arguments":{}}' \
  | cce mcp --dir .
```

**context_search** — COMPACT chunks (signature + doc + first line + elision), each
with a `#chunk_id`; the top-K includes an import-graph neighbour (`payments.py`):

```
 1. [0.864806] auth.py:6-13 (function/function_definition) #04578fe98cec59bd
def hash_password(password: str, salt: str) -> str:
"""Hash a password with a salt using SHA-256.
digest = hashlib.sha256((salt + password).encode()).hexdigest()
… (+5 lines)

 2. [0.863693] auth.py:16-18 (function/function_definition) #ae61011b82d7a777
def verify_password(password: str, salt: str, expected: str) -> bool:
"""Return True when the password hashes to the expected digest."""
return hash_password(password, salt) == expected

 3. [0.000000] payments.py:6-9 (function/function_definition) #873decd4dc46ba36
def process_payment(user: str, amount: int, salt: str) -> str:
"""Charge a user and return a receipt token."""
receipt = hash_password(f"{user}:{amount}", salt)
… (+1 lines)

Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; related_context(chunk_id) for import-graph neighbours.
query_id: 48613f2358a4
Rate this with record_feedback (query_id="48613f2358a4", helpful=true|false).
```

**expand_chunk(scope=body)** — recovers the EXACT full body (round-trips `detail:full`):

```
def hash_password(password: str, salt: str) -> str:
    """Hash a password with a salt using SHA-256.

    This is the single place passwords are hashed; callers never
    hash inline. Returns the hex digest.
    """
    digest = hashlib.sha256((salt + password).encode()).hexdigest()
    return digest
```

**record_decision** → **session_recall** — a validated decision is stored
(secret-scrubbed, content-addressed) and recalled with precision:

```
Recorded decision #46f3ebd005279048. Retrieve it later with session_recall.

Recalled 1 of 1 remembered decision(s):

 1. [0.851650] #46f3ebd005279048 area=auth tags=security,auth
Passwords are hashed only in auth.hash_password (SHA-256 + salt); never hash inline.

These are validated decisions you MAY reuse — apply only what fits; they are not auto-injected.
```

**summarize_context** — the deterministic, structured session digest (NOT an LLM
summary): the files/chunks touched, the query, and the decision recorded this session:

```
CCE session digest
files (2):
- auth.py
- payments.py
chunks (3):
- 04578fe98cec59bd
- 873decd4dc46ba36
- ae61011b82d7a777
queries (1):
- where is the password hashed
decisions (1):
- #46f3ebd005279048 Passwords are hashed only in auth.hash_password (SHA-256 + s…
```

## 4. `cce savings` — the seven-bucket ledger (offline)

The searches above logged their per-layer token deltas; `cce savings` sums them and
prints the offline `$` estimate. The honesty note is printed on the first and last
lines — these are the internal "vs full-file" figures, not the real agent cost.

```console
$ cce savings --dir .
CCE savings ledger  (vs full-file baseline — not your real end-to-end agent cost)
  source : ./.cce/metrics.jsonl
  pricing: cce.pricing/builtin-v1  (offline, embedded; edit src/pricing.json to change)

  layer                       saved_tokens   baseline_tokens
  retrieval                             28               202
  chunk_compression                     41               174
  grammar                               75               133
  output                                 0                 0
  memory                                 0                 0
  turn_summarization                     0                 0
  progressive_disclosure                 0                 0
  --------------------------------------------------------
  total                                144               509

  estimated $ saved: $0.00  (default-model input rate)

  This is the internal "vs full-file" figure, NOT your real agent cost.
  For the real end-to-end delta, run the A/B eval harness: see eval/README.md.
```

## 5. `cce eval` — the real-world A/B harness (canned demo)

The honest counterpart to the ledger — correctness-gated, cost-primary, paired. Pure
aggregation (no model call), run here on the bundled example:

```console
$ cce eval eval/runs.example.jsonl --questions eval/questions.jsonl
CCE eval — real end-to-end A/B (cost-primary, correctness-gated, paired)
  questions: 6   skipped runs: 0
  off : correct 5/6 runs · punts 1 · incorrect 0 · correct_cost $2.45 · mean $0.49
  on  : correct 6/6 runs · punts 0 · incorrect 0 · correct_cost $0.83 · mean $0.14
  paired-correct (both arms): 5
  paired cost: off $2.45 · on $0.67 · saved $1.78  (72.7%)
```

---

# Part 4 — Knowledge-corpus sync cold start (M5)

The SPEC-SYNC-KNOWLEDGE §10.5-style verification gate for M5.4: the documented
knowledge walkthrough ([knowledge.md](knowledge.md) "Syncing a corpus",
[sync.md](sync.md) consumer mode) followed from scratch against a **local bare
git remote** (`file://`), no network, LFS off. Engine: `cce 2.6.9` (dev build at
the M5.3+M5.4 change). Absolute paths appear as `$WORK`; the snapshot ids and
checksums are the real, deterministic values for this fixture feed.

## 1. Producer — index (redacts) and push the corpus

A producer root with `sync.remote` + `knowledge.sync.*` configured, and a
two-record `cce.knowledge/v1` feed (`corpus.jsonl`). One code repo
(`github.com__acme__billing`) was pushed to the same cache first, the
documented Part-2 way.

```console
$ cce knowledge index producer/corpus.jsonl --dir producer
Indexed knowledge from producer/corpus.jsonl
  schema    : cce.knowledge/v1
  records   : 2
  chunks    : 2
  snapshot  : cd0ebbdf8dcc0972
  store     : producer/.cce/knowledge/cd0ebbdf8dcc0972.json

$ cce knowledge push --dir producer
Pushed corpus internal-tickets@cd0ebbdf8dcc0972
  key        : knowledge/v1/internal-tickets/cd0ebbdf8dcc0972.cck
  checksum   : c6bda840f22cc142388e04fae361cf90c5a53b1c554b00dbb5969ca7f3cb8457
  records    : 2 · chunks : 2
  data as-of : 2026-07-01T09:00:00Z
  pushed at  : 2026-07-08T16:27:59Z
```

`data as-of` (deterministic, inside the artifact) and `pushed at` (outside, in
the published `corpus.json`) are the two §4.4 freshness signals.

## 2. Discovery — `cce sync list` grows the knowledge section

```console
$ cce sync list --remote file://$WORK/cache.git
remote        : file://$WORK/cache.git

repo_id                    latest                                    artifacts  bytes
github.com__acme__billing  416acb0718d9f68aa3c3dd39fc07d3e94496913b          1   3349

total         : 1 repo, 1 artifact, 3349 bytes

knowledge:
corpus_id         current           snapshots  bytes  data as-of
internal-tickets  cd0ebbdf8dcc0972          1   6723  2026-07-01T09:00:00Z
```

`--json` stays `cce.synclist/v1` and gains the optional `knowledge` array
(emitted only because a corpus exists — a knowledge-free cache's listing is
byte-identical to Part 2's):

```json
  "knowledge": [
    {
      "bytes": 6723,
      "corpus_id": "internal-tickets",
      "current": "cd0ebbdf8dcc0972",
      "data_as_of": "2026-07-01T09:00:00Z",
      "pushed_at": "2026-07-08T16:27:59Z",
      "snapshots": 1
    }
  ],
```

## 3. Consumer — a bare directory gets code AND knowledge in one command

```console
$ cce sync pull --all --into ctx --remote file://$WORK/cache.git
remote        : file://$WORK/cache.git

  billing          pulled      github.com__acme__billing@416acb0718d9f68aa3c3dd39fc07d3e94496913b  chunks 1  (c4b5ad7ad0db)
  knowledge        pulled      internal-tickets@cd0ebbdf8dcc0972  → .cce/knowledge/

workspace     : ctx/.cce/workspace.yml (1 member)
summary       : 1 pulled · 0 up-to-date · 0 skipped

$ cce sync verify --checksum-only --dir ctx
verify OK (checksum-only): 1 member
  billing          github.com__acme__billing@416acb0718d9f68aa3c3dd39fc07d3e94496913b  (d02501b4a5f6)
  knowledge        internal-tickets@cd0ebbdf8dcc0972  (027ad6e20f95)
```

The installed `ctx/.cce/knowledge/cd0ebbdf8dcc0972.json` is **byte-identical**
to the producer's (the §7 bar — asserted continuously by
`tests/knowledge_sync.rs`). A second run is idempotent — nothing re-fetched:

```console
$ cce sync pull --all --into ctx --remote file://$WORK/cache.git
  billing          up-to-date  github.com__acme__billing@416acb0718d9f68aa3c3dd39fc07d3e94496913b
  knowledge        up-to-date  internal-tickets@cd0ebbdf8dcc0972
  …
summary       : 0 pulled · 1 up-to-date · 0 skipped
```

## 4. MCP over the consumer — freshness + the blended search

`cce mcp --workspace --dir ctx`, driven over stdio:

```console
# index_status → the §4.4 knowledge block
Workspace status: ctx
  billing (package billing) — files 1, chunks 1
  totals  : files 1, chunks 1
  edges (0):
  source  : local (built by cce index)
  remote  : (no sync remote configured — pure local)
  knowledge :
    corpus         : internal-tickets
    snapshot       : cd0ebbdf8dcc0972
    records/chunks : 2 / 2
    data as-of     : 2026-07-01T09:00:00Z
    remote current : cd0ebbdf8dcc0972
    behind remote  : no

# context_search {"query": "password hashing policy", "source": "both"}
 1. [0.988523] [knowledge] Password hashing policy — closed · 2026-07-01T09:00:00Z · https://tickets.example/41 #268f67628c20b2b9
# Password hashing policy

## Rule

Store each password only as a salted slow hash; never keep the plaintext password.

 2. [0.876031] billing · billing.py:1-2 (function/function_definition) #33459f1fa15c580c
def hash_password(password):
return slow_salted_hash(password)
```

The *why* (the policy record, with full provenance) ranks beside the *what*
(the pulled code chunk) — on a machine that never ran an adapter and never had
a source checkout. **Result: knowledge cold-start PASSED.**

## Automated gates (re-run to reproduce)

```console
$ cargo test                                                   # 416 tests, all green (+1 #[ignore] Ollama)
$ cargo clippy --all-targets --all-features -- -D warnings     # clean
$ cargo fmt --check                                            # clean
$ cargo llvm-cov --summary-only                                # total 93.9% line (≥ 92%)
$ cce conformance test/fixture/samples                         # conformance.json byte-identical
```

The hermetic MCP suite (`tests/mcp.rs`) drives the real binary over stdio through
initialize → tools/list (the **nine** tools, fixed order) → context_search (compact,
logging a metrics event with the savings buckets) → index_status → record_feedback →
expand_chunk / related_context → set_output_compression → record_decision /
session_recall → summarize_context → the missing-index and stale-chunk paths → `cce
init` idempotency → the sync auto-pull soft dependency behind a `file://` bare remote
(offline-safe when absent). The sync
suite (`tests/sync.rs`) covers init → push → pull → search → verify plus the refusals
and the offline guarantee. The dashboard suites (`src/dashboard.rs`, `tests/dashboard.rs`)
assert the refreshed `/api/metrics` panels over a real loopback socket.

**Result: cold-start PASSED (offline + online + v2.5 Savings Layers + M5 knowledge sync).** Every
documented command ran verbatim and its output matched the docs.
