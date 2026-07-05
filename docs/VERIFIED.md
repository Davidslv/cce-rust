# CCE Sync — cold-start verification transcript

This file records the **mandatory cold-start pass** (SPEC-SYNC §10.5): the documented
install + walkthrough followed from scratch against a **local bare git remote**
(`file://`, fully hermetic — no network), confirming every documented command runs
and its output matches the docs.

- **Engine:** `cce 2.3.0` (release build).
- **Environment:** macOS (Darwin 25.3.0), `git version 2.50.1`.
- **git-LFS:** *not installed on this machine* — so the walkthrough uses
  `--no-lfs` (a plain-git cache), and the LFS smoke test
  (`tests/sync.rs::lfs_round_trip_smoke_or_skip`) **SKIPS** gracefully, exactly as
  SPEC-SYNC §11 requires. The macOS/Ubuntu LFS install steps are documented in the
  README and `docs/sync.md` but are not exercised here for lack of the binary.
- **Isolation:** `CCE_HOME` was pointed at a temp dir so the working clone never
  touched `~/.cce`. Absolute paths and the commit `<sha>` below are
  environment-specific; the **checksums and chunk counts are the real, stable
  values** a Ruby or CI build of the same `repo@sha` must reproduce.
- **Format:** the artifact is the reconciled v2.3 canonical format
  (`SPEC-SYNC-RECONCILE.md`) — no provenance (`built_at`/`built_by` removed),
  `file_tokens` in the manifest, graph line `{"edges":[…],"nodes":[…]}`,
  `pack_set_id = c,javascript,python,ruby,rust,typescript`, checksum over the whole
  stream with `checksum:""`. The shared golden checksum on `test/fixture/samples`
  (`repo_id=cce/demo`, `sha=0…0`, 21 chunks) is
  `028fa30ba1424e4fa119a5ab00bebc98f057088720bb3da2cdfc06c391733ca3`, and the raw
  bytes are emitted to `/tmp/cce_artifact_rust.cce` for a byte-for-byte diff against
  Ruby.

The commands are copy-pasteable verbatim. Only the absolute scratch path (shown here
as `$WORK`) and the concrete commit sha differ per environment.

---

## 0. Versions

```console
$ git --version
git version 2.50.1 (Apple Git-155)
$ cce --version
cce 2.3.0
```

## 1. Create the cache remote (a bare git repo)

```console
$ git init --bare -q -b main "$WORK/cache.git"
```

## 2. A project to index, committed

```console
$ cd "$WORK/billing" && git init -q -b main && git add -A && git commit -q -m "initial billing service"
$ git rev-parse --short HEAD
7b9dec7
```

## 3. `cce sync init`

```console
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
Configured sync remote: file://$WORK/cache.git
  git-LFS       : disabled
  repo_id       : github.com__acme__billing
  working clone : $CCE_HOME/sync/507cf2021d44e3f3
  config        : ./.cce/config
```

## 4. `cce sync push`

```console
$ cce sync push
Pushed github.com__acme__billing@7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f
  key      : hash/2.3/github.com__acme__billing/7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f.cce
  checksum : 18ca676d989dee00af072ad269c60e28af64441483613954132c530c4fb4ff05
```

## 5. `cce sync status`

```console
$ cce sync status
remote        : file://$WORK/cache.git
git-LFS       : off
repo_id       : github.com__acme__billing
local cache   : (none pulled yet)
remote latest : 7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f (ref main)
working tree  : 7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f
```

## 6–7. A teammate clones and configures

```console
$ git clone -q "file://$WORK/billing" "$WORK/billing-teammate" && cd "$WORK/billing-teammate"
$ cce sync init --remote "file://$WORK/cache.git" --no-lfs --repo-id github.com__acme__billing
Configured sync remote: file://$WORK/cache.git
  ...
```

## 8. `cce sync pull` — the checksum matches the pusher's, bit-for-bit

```console
$ cce sync pull
Pulled github.com__acme__billing@7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f
  chunks   : 2
  checksum : 18ca676d989dee00af072ad269c60e28af64441483613954132c530c4fb4ff05
  store    : ./.cce/index.json
  tree     : matches — pulled index used as-is
```

The pull checksum `18ca676d…` is **identical** to the push checksum in step 4 —
content-addressability proven end-to-end.

## 9. `cce sync verify` and `cce search` over the pulled index

```console
$ cce sync verify
verify OK: github.com__acme__billing@7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f
  checksum : 18ca676d989dee00af072ad269c60e28af64441483613954132c530c4fb4ff05

$ cce search "authenticate user password" --no-metrics
 1. [0.920470] src/auth.py:1-3 (function/function_definition)
    def login(user, password):
 2. [0.875340] src/pay.py:3-6 (function/function_definition)
    def charge(user, amount):
```

---

## Offline-first & error paths (SPEC-SYNC §9)

**A. No remote configured — local commands are unaffected:**

```console
$ cce index .        # in a project with no sync config
indexed OK (no remote)
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

---

## Automated gates (re-run to reproduce)

```console
$ cargo test                                                   # 200+ tests, all green
$ cargo clippy --all-targets --all-features -- -D warnings     # clean
$ cargo fmt --check                                            # clean
$ cargo llvm-cov --summary-only                                # sync module ≥ 92%
```

The hermetic sync integration suite (`tests/sync.rs`) drives the real binary through
exactly this flow against a `file://` bare remote — init → push → pull → search →
verify — plus `--latest`, the dirty-tree/cache-miss refusals, the offline guarantee,
and the SKIP-if-unavailable LFS smoke test.

**Result: cold-start PASSED.** Every documented command ran verbatim and its output
matched the docs.
