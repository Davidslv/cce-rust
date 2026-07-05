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

  query_id: cb30eaa953a0
  Rate this with record_feedback (query_id="cb30eaa953a0", helpful=true|false).
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
