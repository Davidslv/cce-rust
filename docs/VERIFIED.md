# CCE — cold-start verification transcripts

This file records the **mandatory cold-start passes**: the documented install +
walkthroughs followed from scratch, confirming every documented command runs verbatim
and its output matches the docs. Two features are verified here:

- **CCE Sync (v2.3)** — against a local bare git remote (`file://`, fully hermetic,
  no network).
- **CCE MCP (v2.4)** — `cce init` → a real `cce mcp` stdio session → `cce dashboard`,
  plus the `cce init --remote` sync-pull plug-and-play flow.

- **Engine:** `cce 2.4.0` (release build).
- **Environment:** macOS (Darwin 25.3.0), `git version 2.50.1`.
- **git-LFS:** *not installed on this machine* — so the Sync walkthrough uses
  `--no-lfs` (a plain-git cache), and the LFS smoke test
  (`tests/sync.rs::lfs_round_trip_smoke_or_skip`) **SKIPS** gracefully, exactly as
  SPEC-SYNC §11 requires.
- **Isolation:** `CCE_HOME` was pointed at a temp dir so the working clone never
  touched `~/.cce`. Absolute paths and the commit `<sha>` below are
  environment-specific; the **checksums and chunk counts are the real, stable
  values** a Ruby or CI build of the same `repo@sha` must reproduce.
- **Sync format:** the reconciled canonical artifact — `cce_version = "2.3"`, the
  **artifact format version, decoupled from the app version** (`SYNC_FORMAT_VERSION`).
  CCE MCP (v2.4) is additive and does **not** change the artifact format, so the
  format version stays `2.3` and the content address stays `hash/2.3/…` — a v2.4
  release does not invalidate existing caches or diverge from Ruby. No provenance,
  `file_tokens` in the manifest,
  `pack_set_id = c,javascript,python,ruby,rust,typescript`. The shared golden checksum
  on `test/fixture/samples` (`repo_id=cce/demo`, `sha=0…0`, 21 chunks, `edges:[]`) is
  `581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48` — **equal to
  Ruby's** — and the raw bytes are emitted to `/tmp/cce_artifact_rust.cce` for a
  byte-for-byte diff against Ruby.

The commands are copy-pasteable verbatim. Only the absolute scratch path (shown here
as `$WORK`) and the concrete commit sha differ per environment.

---

# Part A — CCE MCP (v2.4)

## 0. Versions

```console
$ git --version
git version 2.50.1 (Apple Git-155)
$ cce --version
cce 2.4.0
```

## 1. A tiny project, then `cce init`

```console
$ cd "$WORK/myproject"          # contains auth.py + payments.py
$ cce init .
CCE is wired up for Claude Code.
  index     : built 2 chunk(s) from 2 file(s)
  .mcp.json : ./.mcp.json (server "cce")
  CLAUDE.md : ./CLAUDE.md (context_search guidance)

Next steps:
  1. Restart your editor (Claude Code) so it loads .mcp.json.
  2. Ask a question about this codebase — the agent calls context_search.
  3. Confirm it was used: cce dashboard
```

`.mcp.json` is valid and idempotent (a second `cce init` leaves it byte-identical):

```console
$ cat .mcp.json
{
  "mcpServers": {
    "cce": {
      "args": [
        "mcp",
        "--dir",
        "."
      ],
      "command": "cce"
    }
  }
}
```

`CLAUDE.md` carries the marker-bounded block steering the agent to prefer
`context_search`:

```console
$ sed -n '3,5p' CLAUDE.md
<!-- BEGIN CCE MCP -->
## Code Context Engine (CCE)
```

## 2. An MCP session — the shape the editor drives (piped JSON-RPC over stdio)

```console
$ printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
    '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"where is the password hashed","top_k":3}}}' \
    '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"index_status"}}' \
  | cce mcp --dir .
```

- **initialize** →
  `{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"cce","version":"2.4.0"}}`
- **tools/list** → `["context_search","index_status","record_feedback"]`
- **context_search** (id 3) →

  ```
   1. [0.825000] auth.py:1-2 (function/function_definition)
  def hash_password(pw):
      return pw + "salt"

   2. [0.816803] payments.py:3-4 (function/function_definition)
  def process_payment(amount):
      return auth.hash_password(str(amount))

  query_id: 8c017cf1214f
  Rate this with record_feedback (query_id="8c017cf1214f", helpful=true|false).
  ```

- **index_status** (id 4) →

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

## 3. The search is recorded for the dashboard

```console
$ cat .cce/metrics.jsonl
{"baseline_tokens":32,"embedder":"hash","empty":false,"event":"search","graph_enabled":true,"id":"8c017cf1214f","latency_ms":0.078,"low_confidence":false,"mean_score":0.820…,"query":"where is the password hashed","result_count":2,"savings_ratio":0.125,"schema":"cce.metrics/v1","served_tokens":28,"tokens_saved":4,"top_k":3,"top_score":0.825,"ts":"2026-07-05T13:33:57Z"}
$ cce dashboard --no-open
cce dashboard: serving http://127.0.0.1:8787/  (loopback only, read-only)
metrics log : ./.cce/metrics.jsonl
press Ctrl-C to stop.
```

The agent's `context_search` is a `search` event on the dashboard — proof of use and
of value (`tokens_saved`).

## 4. Plug-and-play team context: `cce init --remote` (pulls the CI-built index)

```console
$ cce init . --remote "file://$WORK/cache.git"
CCE is wired up for Claude Code.
  index     : pulled from sync remote (cce sync pull --latest)
  .mcp.json : ./.mcp.json (server "cce")
  CLAUDE.md : ./CLAUDE.md (context_search guidance)
  ...
$ printf '%s\n' '{"id":1,"method":"tools/call","params":{"name":"index_status"}}' | cce mcp --dir .
Index status
  ...
  source  : pulled via cce sync (sha b84bc45d7685)
  remote latest: b84bc45d7685
  behind remote: no
```

`index_status` reports the index **source (pulled), its sha, and behind-remote** — the
sync freshness is observable. With no remote configured the same server works fully on
the local index, offline (Part A step 2).

---

# Part B — CCE Sync (v2.3)

## 0. Versions

```console
$ cce --version
cce 2.4.0
```

## 1. Create the cache remote (a bare git repo)

```console
$ git init --bare -q -b main "$WORK/cache.git"
```

## 2. A project to index, committed

```console
$ cd "$WORK/billing" && git init -q -b main && git add -A && git commit -q -m "initial billing service"
$ git rev-parse --short HEAD
b84bc45
```

## 3. `cce sync init`

```console
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
Configured sync remote: file://$WORK/cache.git
  git-LFS       : disabled
  repo_id       : github.com__acme__billing
  working clone : $CCE_HOME/sync/24e7837b9bdb4382
  config        : ./.cce/config
```

## 4. `cce sync push`

```console
$ cce sync push
Pushed github.com__acme__billing@b84bc45d76855cdb8f3f8d7ce47868517838e519
  key      : hash/2.3/github.com__acme__billing/b84bc45d76855cdb8f3f8d7ce47868517838e519.cce
  checksum : eb40a7eab5aa6d2d12e8889912ed87ef35b6268d9a97d0f9b8ce5fb641611289
```

## 5. `cce sync status`

```console
$ cce sync status
remote        : file://$WORK/cache.git
git-LFS       : off
repo_id       : github.com__acme__billing
local cache   : (none pulled yet)
remote latest : b84bc45d76855cdb8f3f8d7ce47868517838e519 (ref main)
working tree  : b84bc45d76855cdb8f3f8d7ce47868517838e519
```

## 6–8. A teammate clones, configures, and pulls — the checksum matches, bit-for-bit

```console
$ git clone -q "file://$WORK/billing" "$WORK/billing-teammate" && cd "$WORK/billing-teammate"
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
$ cce sync pull
Pulled github.com__acme__billing@b84bc45d76855cdb8f3f8d7ce47868517838e519
  chunks   : 3
  checksum : eb40a7eab5aa6d2d12e8889912ed87ef35b6268d9a97d0f9b8ce5fb641611289
  store    : ./.cce/index.json
  tree     : matches — pulled index used as-is
```

The pull checksum `eb40a7ea…` is **identical** to the push checksum in step 4 —
content-addressability proven end-to-end.

## 9. `cce sync verify` and `cce search` over the pulled index

```console
$ cce sync verify
verify OK: github.com__acme__billing@b84bc45d76855cdb8f3f8d7ce47868517838e519
  checksum : eb40a7eab5aa6d2d12e8889912ed87ef35b6268d9a97d0f9b8ce5fb641611289

$ cce search "authenticate user password" --no-metrics
 1. [0.897169] src/auth.py:1-2 (function/function_definition)
    def login(user, password):
 2. [0.855379] src/pay.py:3-5 (function/function_definition)
    def charge(user, amount):
 3. [0.841146] src/auth.py:4-5 (function/function_definition)
    def logout(user):
```

---

## Offline-first & error paths (SPEC-SYNC §9)

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
$ cargo test                                                   # 298 tests, all green
$ cargo clippy --all-targets --all-features -- -D warnings     # clean
$ cargo fmt --check                                            # clean
$ cargo llvm-cov --summary-only                                # total ≥ 92% (93.8% line)
```

The hermetic MCP integration suite (`tests/mcp.rs`) drives the real binary over stdio
through exactly this flow — initialize → tools/list → context_search (with a metrics
event) → index_status → record_feedback → the missing-index path → `cce init`
idempotency → the sync auto-pull soft dependency behind a `file://` bare remote
(offline-safe when absent). The sync suite (`tests/sync.rs`) covers init → push → pull
→ search → verify plus the refusals and the offline guarantee.

**Result: cold-start PASSED (MCP + Sync).** Every documented command ran verbatim and
its output matched the docs.
