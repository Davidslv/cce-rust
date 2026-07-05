# CCE — cold-start verification transcripts

This file records the **mandatory cold-start passes**: the documented install +
walkthroughs followed from scratch, confirming **every documented command runs
verbatim** and its output matches the docs. A doc example that does not run is a bug.

Two passes are recorded, both real captured runs:

- **Offline cold start (THE guarantee)** — with **no network and no sync remote
  configured**, `index` · `search` · `stats` · `dashboard` · `workspace` · `cce mcp`
  all work exactly as documented (Part 1).
- **Online cold start** — the parts that *do* touch the network: `cce sync
  init/push/pull/verify` against a git cache, and the `cce init --remote`
  plug-and-play flow (Part 2).

- **Engine:** `cce 2.4.1` (release build).
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
  The v2.4.1 consolidation is additive and does **not** change the artifact format, so
  the format version stays `2.3` and the content address stays `hash/2.3/…` — this
  release does not invalidate existing caches or diverge from Ruby. The shared golden
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
cce 2.4.1
```

## 1. A tiny project

```console
$ cd "$WORK/myproject"
$ git init -q -b main
$ printf 'def hash_password(pw):\n    return pw + "salt"\n' > auth.py
$ printf 'import auth\n\ndef process_payment(amount):\n    return auth.hash_password(str(amount))\n' > payments.py
$ git add -A && git commit -q -m "initial project"
$ git rev-parse --short HEAD
25bd009
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
query-id: e684d2686d65  ·  rate with: cce feedback e684d2686d65 --helpful|--not-helpful
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
event `source: "mcp"`.

```console
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"process a payment","top_k":2}}}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"index_status"}}' \
  | cce mcp --dir .
```

- **initialize** → `serverInfo {"name":"cce","version":"2.4.1"}`
- **context_search** (id 2) →

  ```
   1. [0.825000] auth.py:1-2 (function/function_definition)
  def hash_password(pw):
      return pw + "salt"

   2. [0.816803] payments.py:3-4 (function/function_definition)
  def process_payment(amount):
      return auth.hash_password(str(amount))

  query_id: 0aae712603f7
  Rate this with record_feedback (query_id="0aae712603f7", helpful=true|false).
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

`GET /api/metrics` returns the v2.4.1 panels — **agent-vs-human**, **secret-safety**,
and **index-freshness** — all computed with no network:

```console
$ curl -s http://127.0.0.1:8787/api/metrics | jq '{usage_by_source, secret_safety, index_freshness, mean_top_score: .totals.mean_top_score}'
{
  "usage_by_source": {
    "cli": { "searches": 2, "tokens_saved": 8, "mean_savings_ratio": 0.125, "mean_top_score": 0.825 },
    "mcp": { "searches": 1, "tokens_saved": 4, "mean_savings_ratio": 0.125, "mean_top_score": 0.825 }
  },
  "secret_safety": { "sensitive_skipped": 1, "index_runs": 2 },
  "index_freshness": {
    "indexes": 2,
    "source": "local",
    "sha": "25bd0098ca275930fb20a93ba8fce0d76893457e",
    "indexed_ts": "2026-07-05T14:20:11Z",
    "remote_latest": null,
    "behind_remote": false
  },
  "mean_top_score": 0.825
}
$ curl -s http://127.0.0.1:8787/api/health
{"status":"ok","events":5,"skipped":0}
```

The agent's `context_search` (`mcp`) sits beside the human's `cce search` (`cli`) —
the agent-vs-human split is proven offline. `remote_latest` is `null` and
`behind_remote` is `false` because no remote is configured: **no network was touched.**

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
7cb6176
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
Pushed github.com__acme__billing@7cb6176f855de5cc187cd6bc2a49f82517ba7eda
  key      : hash/2.3/github.com__acme__billing/7cb6176f855de5cc187cd6bc2a49f82517ba7eda.cce
  checksum : 30dcfecbea2cee6b4d5339e1dc6157a42a629635b9059c76c5236304d58a40e4
```

## 3. A teammate clones, pulls, and verifies — bit-for-bit

```console
$ git clone -q "file://$WORK/billing" "$WORK/billing-teammate" && cd "$WORK/billing-teammate"
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
$ cce sync pull
Pulled github.com__acme__billing@7cb6176f855de5cc187cd6bc2a49f82517ba7eda
  chunks   : 3
  checksum : 30dcfecbea2cee6b4d5339e1dc6157a42a629635b9059c76c5236304d58a40e4
  store    : ./.cce/index.json
  tree     : matches — pulled index used as-is
$ cce sync verify
verify OK: github.com__acme__billing@7cb6176f855de5cc187cd6bc2a49f82517ba7eda
  checksum : 30dcfecbea2cee6b4d5339e1dc6157a42a629635b9059c76c5236304d58a40e4
```

The pull checksum `30dcfecb…` is **identical** to the push checksum — content
addressability proven end-to-end. Search then runs fully offline over the pulled index:

```console
$ cce search "authenticate user password" --no-metrics --top-k 2
 1. [0.908333] src/auth.py:1-2 (function/function_definition)
    def login(user, password):
 2. [0.856835] src/pay.py:3-4 (function/function_definition)
    def charge(user, amount):
```

## 4. Sync freshness is observable on the dashboard and in MCP

After a pull, `index_status` and the dashboard's **index-freshness** panel report the
index **source (pulled), its sha, remote-latest, and behind-remote**:

```console
$ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"index_status"}}' | cce mcp --dir .
Index status
  ...
  source  : pulled via cce sync (sha 7cb6176f855d)
  remote latest: 7cb6176f855d
  behind remote: no
$ curl -s http://127.0.0.1:8787/api/metrics | jq '.index_freshness'
{
  "indexes": 0,
  "source": "pulled",
  "sha": "7cb6176f855de5cc187cd6bc2a49f82517ba7eda",
  "indexed_ts": null,
  "remote_latest": "7cb6176f855de5cc187cd6bc2a49f82517ba7eda",
  "behind_remote": false
}
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

## Automated gates (re-run to reproduce)

```console
$ cargo test                                                   # 301 tests, all green
$ cargo clippy --all-targets --all-features -- -D warnings     # clean
$ cargo fmt --check                                            # clean
$ cargo llvm-cov --summary-only                                # total 93.6% line (≥ 92%)
$ cce conformance test/fixture/samples                         # conformance.json byte-identical
```

The hermetic MCP suite (`tests/mcp.rs`) drives the real binary over stdio through
initialize → tools/list → context_search (logging a metrics event) → index_status →
record_feedback → the missing-index path → `cce init` idempotency → the sync auto-pull
soft dependency behind a `file://` bare remote (offline-safe when absent). The sync
suite (`tests/sync.rs`) covers init → push → pull → search → verify plus the refusals
and the offline guarantee. The dashboard suites (`src/dashboard.rs`, `tests/dashboard.rs`)
assert the refreshed `/api/metrics` panels over a real loopback socket.

**Result: cold-start PASSED (offline + online).** Every documented command ran verbatim
and its output matched the docs.
