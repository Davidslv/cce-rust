# CCE Sync — a distributed, offline-first cache for code indexes

**Status:** shipped in v2.3.0 (SPEC-SYNC). CCE Sync is **purely additive**: every
existing command works with no remote and no network. A failed push or pull never
breaks local indexing or search.

> One-line model: **git remotes for the index.** Your local `.cce/` is
> authoritative; an optional git-backed remote is a *content-addressed cache* you
> can push to and pull from. Because the index is deterministic, a cache for
> `repo@sha` is byte-identical no matter who — or which language engine — built it.

---

## 1. When to use it

- A teammate or CI already indexed `main@<sha>`; you want that index **instantly**
  instead of re-indexing locally.
- CI indexes `main` on every merge and pushes the cache, so everyone pulls a fresh
  base with one command.
- You want supply-chain safety: `cce sync verify` re-indexes locally and confirms
  the pulled cache's checksum, so you never have to *trust* the pusher.
- You want searchable, agent-ready context for a whole ecosystem **without
  checking out any source** — a review machine, a docs box, an agent sandbox.
  That is **consumer mode** (§7): `cce sync list` to see what a cache holds,
  `cce sync pull --all` to turn it into a ready-to-search workspace, and
  `cce sync verify --checksum-only` to integrity-check it, all repo-less.

Only the deterministic **hash embedder** produces shareable caches. Ollama/semantic
indexes are non-reproducible and are **local-only** — `cce sync push` refuses them.

---

## 2. The model

The index is a pure function of `(repo content at commit, cce version, pack set,
embedder)`. With the hash embedder that function is reproducible and identical
across people and across the Ruby/Rust engines (already proven by conformance).
Therefore the cache is **content-addressable**: no "whose version wins," no merge,
no conflict. A teammate's push == CI's push == a fresh local build, bit-for-bit.

**Builder independence is why the walker honors only committed `.gitignore`**
(v2.6.3): the ignore rules that shape the file set are part of the tree at the sha,
so every builder skips the same files. Machine-local ignore sources —
`.git/info/exclude` and the global `core.excludesfile` — and `.gitignore` files
above the walk root are deliberately **not** honored: they vary by machine and would
break `artifact == build(sha)`. `.git/` and `.cce/` are always skipped.

```
  CI on merge to main:  cce index (hash embedder)  →  cce sync push
  developer:            cce sync pull --latest      →  main@sha index, instantly
```

If your working tree differs from the pulled sha, run a normal local `cce index`
for a WIP index. (The incremental branch-overlay — re-indexing only changed files
on top of the pulled base — is a documented fast-follow, out of scope for v1.)

---

## 3. The artifact (portable interchange format)

Ruby stores in SQLite, Rust in JSON, so the cache is **neither** native store — it
is a canonical, deterministic interchange artifact both engines export and import.
It is a UTF-8 stream with an **LF after every line, including the last**:

```
line 1        the manifest JSON (includes file_tokens)
lines 2..N+1  one JSON object per chunk, N = chunk_count,
              sorted by (file_path, start_line, id)
line N+2      the graph JSON: { "edges": [...], "nodes": [...] }
```

- **Sorted keys, compact separators.** Every object is serialized with keys in
  ascending order and no insignificant whitespace (`,` and `:` only).
- **Embeddings are not decimals.** A 256-d vector is encoded as **standard base64
  (with padding) of its 256 little-endian IEEE-754 `f64` bytes**, so the bytes are
  identical across languages regardless of float→string formatting.
- **No provenance.** There is **no** `built_at` and **no** `built_by` — provenance
  is what made the file non-reproducible, so it is removed. The whole artifact,
  not just the checksum, is byte-identical for a given `repo@sha`.
- **Checksum.** `checksum` = lowercase-hex SHA-256 over the **entire** canonical
  stream serialized with the manifest's `checksum` value set to the empty string
  `""`; the real hex is then written into the field. Verify = set `checksum` to
  `""`, re-hash, compare.

Manifest fields (sorted): `cce_version` (`"2.3"` — the **artifact format version**,
`SYNC_FORMAT_VERSION`, decoupled from the app version; the format-compatible window,
bumped only when the artifact bytes change shape), `checksum`, `chunk_count`,
`embedder` (`"hash"`), `file_tokens` (sorted-key `{path: int}`), `pack_set_id`,
`repo_id`, `sha`.

Chunk fields (sorted): `chunk_type, content, embedding, end_line, file_path, id,
kind, language, start_line, token_count`.

Graph line: `{"edges":[…],"nodes":[…]}` — `nodes` are every indexed file
(`{"id": path}`, sorted by `id`); `edges` are the **resolved** `file → file` import
edges (base SPEC §6.7): an edge `A → B` exists only when a module imported by `A`
resolves — by the same stem-matching the retriever's graph expansion uses — to a
corpus file `B`. **External / unresolved imports (e.g. `os`, `fs`, `std`) produce no
edge.** Each edge is `{"source", "target", "type":"import"}`, sorted by
`(source, target, type)`. On import the graph is rebuilt from these resolved edges,
giving identical search-expansion behaviour (external imports never produced a hop,
so dropping them changes nothing).

`pack_set_id` = the sorted, comma-joined lowercase pack names —
`c,javascript,python,ruby,rust,typescript`.

A committed **shared golden** anchors the format cross-language
(`src/sync/artifact.rs::shared_golden_checksum_for_samples`): the
`test/fixture/samples` corpus exported with `repo_id=cce/demo`, `sha=0000…0000`
(whose imports are all external, so `edges:[]`) yields

```
581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48   (21 chunks)
```

That test also writes the raw bytes to `/tmp/cce_artifact_rust.cce`. The Ruby
engine, exporting the same fixture with the same identity, must produce this exact
checksum and a byte-identical file. If this value ever changes, the wire format
changed — treat it as a breaking-format decision, not a test to "fix."

---

## 4. Content address (cache key)

A cache is addressed by a path in the git remote built from its identity:

```
<embedder>/<cce_version_major.minor>/<repo_id>/<sha>.cce
   e.g.  hash/2.3/github.com__acme__billing/9f1c2a….cce
```

- `embedder` = `hash` (only shareable embedder).
- `cce_version` at `major.minor` — a format-compatible window; a mismatch is a
  cache miss (rebuild).
- `repo_id` = normalized git origin (`host__org__repo`), or the `sync.repo_id`
  override.
- `sha` = the commit the index was built from.

Distinct shas are distinct files, so concurrent pushes for different shas never
conflict in content. A small pointer file `…/<repo_id>/refs/<branch>` records the
latest sha pushed for a branch; that is what `cce sync pull --latest` reads. The
pointer is a fixed path rewritten by every push, so two racing pushes genuinely
conflict there: every key is whole-file last-writer-wins, and a lost push race
is retried by re-applying the write on the freshly fetched remote state. A push
that cannot land fails loudly — it never reports success without publishing.

---

## 5. Remote backend (git, LFS)

The remote is a **git repository** (`sync.remote`). A local working clone lives
under `~/.cce/sync/<remote-id>/` (override the base with `CCE_HOME`). `put` writes
the artifact at its content path, commits, and `git push` (a lost ref race
fetches, re-applies the write on the new remote state, and retries). `get`
fetches and reads the file back. A working clone found with a rebase in progress
(a state older versions could leave behind after a lost race) is recovered
automatically the next time any sync operation opens it.

Artifacts are large, so `*.cce` blobs use **git-LFS** by default (`sync.lfs: true`).
`cce sync init --lfs` writes the `.gitattributes` and runs `git lfs install`.
Permissions, transport, and auth are entirely git's (SSH/HTTPS credentials) — CCE
adds no RBAC of its own.

Future backends (S3/GCS, a thin HTTP server, a read-only Sourcegraph adapter) are
possible behind the same `SyncRemote` trait without CLI changes; none is built in
v1.

---

## 6. CLI

```
cce sync init  --remote <git-url> [--lfs|--no-lfs] [--repo-id <id>] [--dir <path>]
cce sync push  [--commit <sha>] [--workspace] [--dir <path>]
cce sync pull  [--commit <sha> | --latest] [--force] [--workspace] [--dir <path>]
cce sync pull  --all --into <dir> [--remote <url>] [--corpus <id>]
cce sync list  [--remote <url>] [--json] [--dir <path>]
cce sync status [--dir <path>]
cce sync verify [--commit <sha> | --checksum-only] [--dir <path>]
```

Rules:
- `push` refuses a **dirty working tree** (a cache is content-addressed by commit)
  and a **non-hash index**. It is best-effort and never blocks other work.
- `push` **always rebuilds the index from the working tree** before exporting
  (v2.6.2) — it never republishes an existing `.cce/index.json`, so a just-pulled
  or otherwise stale/foreign index can never be re-uploaded under a sha it was not
  built from. The invariant is `artifact == build(sha)`.
- `push` exports the **code index only**; the mutable `.cce/knowledge/` store is
  snapshot-keyed and never enters the byte-identical code cache. Knowledge
  corpora travel through their own additive `knowledge/…` key space via
  `cce knowledge push`/`pull` (SPEC-SYNC-KNOWLEDGE; see
  [`knowledge.md`](knowledge.md)). `cce knowledge push` replaces the corpus's
  current snapshot wholesale, so it diffs record ids against the remote first
  and **refuses a push that would drop remote-live records** without `--force`;
  `cce knowledge push --dry-run` prints the full added/removed/changed diff and
  pushes nothing (#90 — details in [`knowledge.md`](knowledge.md)).
- `pull` installs the artifact into `.cce/`. If the local working tree matches
  `sha`, the pulled index is used as-is. It never silently overwrites a local cache
  for a **different** sha without `--force`.
- `list` enumerates what the cache holds: one row per `repo_id` with its **latest
  sha** (the `refs/<branch>` pointer `pull --latest` reads; `-` when a repo has no
  pointer yet), **artifact count**, and **total artifact bytes** (LFS-aware).
  When `refs/main` is absent and a repo carries **exactly one** other
  `refs/<name>` pointer, that ref resolves the latest sha (#72) and the row is
  annotated — `<sha> (master)` in the human table, an **optional `"ref"` field**
  in the JSON row (main-resolved rows are byte-identical to before). With
  several non-main refs nothing is silently picked: the row stays `-`/null.
  **Read-only** — it never writes to the cache or the local `.cce/` — and
  **repo-less**: a bare directory plus `--remote <url>` is enough (no local store,
  source checkout, or config). Rows are sorted by `repo_id`; an empty cache is a
  friendly message, not an error. `--json` emits the stable `cce.synclist/v1`
  shape for scripting:
  `{"remote": …, "repos": [{"artifacts": N, "bytes": N, "latest_sha": sha|null, "repo_id": …}, …], "schema": "cce.synclist/v1"}`.
  A cache carrying **knowledge corpora** additionally renders a `knowledge:`
  section (one row per corpus: current snapshot, snapshot count, bytes,
  `data as-of`) and the JSON gains an **optional `knowledge` array** — emitted
  only when a corpus exists, so knowledge-free listings are byte-identical to
  before (the schema stays `cce.synclist/v1`; see [`knowledge.md`](knowledge.md)).
- `pull --all --into <dir>` is **consumer mode** (§7): enumerate the cache, pull
  every repo_id's latest artifact, and synthesize a ready-to-search workspace —
  no source checkout anywhere. A cache carrying a knowledge corpus also
  installs it at the workspace root (`<dir>/.cce/knowledge/`); with several
  corpora, pass `--corpus <id>` (otherwise knowledge is warn-skipped, naming
  the ids — member pulls never fail because of it).
- `verify --checksum-only` re-hashes the **pulled** store against the SHA-256
  **recorded from the installed bytes at pull time** — no source checkout, no
  rebuild, no remote, and no version coupling (§7, the consumer integrity
  check). A pulled knowledge corpus gets the same treatment: a `knowledge` row
  passes/fails/notices exactly like a member row. Full `verify`
  (rebuild-and-compare) remains the source-holders' check — and note that
  knowledge corpora have **no full-verify analogue at all** (the puller lacks
  the source feed; see the trust section of [`knowledge.md`](knowledge.md)).
- Offline / no remote / auth failure → a clear message; local indexing and search
  continue to work.
- **Workspace-aware:** `--workspace` iterates the manifest's members, each keyed by
  its own `repo_id@sha` (a member is keyed `<repo_id>__<member-name>`).
  `push --workspace` additionally **publishes the workspace metadata** — the
  canonical `workspace.yml` and the derived `workspace-graph.json` — at
  well-known keys under the workspace's **base** repo_id
  (`hash/<ver>/<base>/workspace.yml`, `…/workspace-graph.json`), making the
  cache **self-describing** for repo-less consumers (§7). Publishing is purely
  additive: artifact keys, ref pointers, and old-client pulls are unchanged.

Config keys (`<root>/.cce/config`, or global `~/.cce/config.yml`):
`sync.remote`, `sync.lfs` (default `true`), `sync.repo_id`, `sync.ref` (the
`refs/<name>` pointer `pull --latest` resolves for this project — for repos
whose CI pushes from a non-`main` default branch; #72), `sync.auto_pull`,
`sync.retention` (`all` | `keep-last-<n>`). All optional; absent ⇒ pure local CCE.

---

## 7. Consumer mode — repo-less pull (no source checkout)

A machine with only the `cce` binary and git read access to the cache can get
full search + MCP over everything in it, **with zero source checkouts**. The
artifact is a complete, self-contained corpus: search, `expand_chunk` (whole-file
reconstruction), and the import graph all come from the pulled index alone.

**Single repo.** A `--latest`/`--commit` pull needs no git checkout — only a
config naming the remote and the repo_id:

```console
$ mkdir billing && cce sync init --remote <url> --no-lfs --repo-id github.com__acme__billing --dir billing
$ cce sync pull --latest --dir billing
Pulled github.com__acme__billing@<sha>
  …
  tree     : (no source checkout — consumer mode; the pulled index is the corpus)
$ cce search "charge invoice" --dir billing        # or: cce mcp --dir billing
```

To refresh after the pointer moves, pass `--force` (the different-sha guard is
protecting a *source* workflow; in consumer mode the pulled index is all there is).

**The whole cache, one command.** `cce sync pull --all --into <dir>` enumerates
the cache (the `cce sync list` machinery), pulls every repo_id's latest artifact,
and synthesizes the workspace metadata, leaving `<dir>` immediately usable:

```console
$ cce sync pull --all --into ctx --remote <url>
remote        : <url>

  billing          pulled      github.com__acme__billing@<sha>  chunks 812  (18ca676d989d)
  web              pulled      github.com__acme__web@<sha>  chunks 240  (f00dfeed0123)
  warning: skipped github.com__acme__legacy — no latest pointer on `main` (nothing pushed for the ref yet)

workspace     : ctx/.cce/workspace.yml (2 members)
summary       : 2 pulled · 0 up-to-date · 1 skipped

$ cce search "charge invoice" ctx --workspace      # member-tagged, federated
$ cce mcp --workspace --dir ctx                    # federated context_search
```

Behaviour:

- **Naming.** The member name *and* directory are the repo_id's last `__` segment
  (`github.com__acme__billing` → `ctx/billing/`); collisions get the workspace
  `-2`/`-3` suffix in repo_id order. The full repo_id lives in the member's
  `.cce/config` (`sync.repo_id`), so a per-member `cce sync pull --latest
  --dir ctx/billing` keeps working, and refresh runs never re-derive names.
  A member directory whose config went missing (e.g. its `.cce/` was deleted
  by hand) is **re-adopted by name** on the next run — noted in the report,
  its config rewritten — rather than duplicated with a `-2` suffix.
- **Synthesized manifest.** Members are written with `type: store-only` — the
  neutral `MemberType` for a member with no source to classify. Detection never
  emits it; hand-written manifests are untouched and stay byte-compatible.
- **Idempotent refresh.** Re-running the same command re-pulls **only** members
  whose latest pointer moved (the installed sha is the `.cce/synced.json` marker)
  and reports `up-to-date` for the rest; new repo_ids in the cache become new
  members; a member whose repo_id vanished from the cache is **warned about,
  never deleted**.
- **Skips are not failures.** A repo with no latest pointer (rendered `-` by
  `sync list`) cannot be pulled `--latest`: it is warned and skipped, the run
  continues, and the summary counts it. Exit code stays 0.
- **Non-`main` default branches resolve too (#72).** A repo whose CI pushes
  from another branch (e.g. `master`) writes `refs/master`, not `refs/main`.
  When `refs/main` is absent and **exactly one** other ref pointer exists,
  `--latest` (single pull and `pull --all` alike) resolves it and **notes the
  ref** in the report (`(ref master)`; `sync list` annotates the row the same
  way). With **several** non-main refs nothing is silently picked: the repo is
  skipped with the available refs named — resolve it explicitly with
  `cce sync pull --latest --ref <name> --dir ctx/<member>`, or set `sync.ref:
  <name>` in the member's `.cce/config` so every later `pull --all` refresh
  resolves that pointer (the refresh rewrite preserves the key). `--ref` is
  **rejected with `--all`** — the repos in a cache have different default
  branches, so a global ref would be wrong for most of them; `sync.ref` is the
  per-member tool. `refs/main`, when present, always wins — byte-identical to
  before.
- **Independent shas are the supported shape.** Each member federates at its own
  latest sha — there is no monorepo one-sha assumption on the pull side (that
  assumption exists only in `push --workspace`, which pushes a single checkout).
- **Read-only towards the cache.** Consumer configs are written with
  `lfs: false`, so no consumer ever commits `.gitattributes` into the cache
  repo. (Reading an LFS-enabled cache still works — the smudge comes from the
  cache's own committed attributes — but needs `git-lfs` installed.)
- **Knowledge comes along.** A cache carrying a knowledge corpus installs it at
  the workspace root (`ctx/.cce/knowledge/`, byte-identical to a direct
  `cce knowledge pull`), so `cce mcp --workspace --dir ctx` serves
  `context_search source: knowledge|both` immediately. One active corpus per
  root: `--corpus <id>` picks one when the cache carries several (otherwise
  knowledge is warn-skipped, naming the ids). Refresh follows the member rule —
  an unmoved corpus `current` is `up-to-date`, nothing re-fetched. See
  [`knowledge.md`](knowledge.md).

**The self-describing cache (published workspace metadata).** `cce sync push
--workspace` also publishes the workspace's `workspace.yml` and its
cross-member `workspace-graph.json` under the **base** repo_id (§4's additive
well-known keys). Consumers use them so a repo-less workspace loses nothing
versus a source-holding one — in particular the **cross-member graph
expansion** edges, which are otherwise derived from source manifests a
consumer does not have:

- **`pull --workspace`** installs the published graph into the root `.cce/`
  and merges the published member metadata (real `type:`/`package:`) into the
  local manifest — members are matched **by name**, and the local `path:` (the
  consumer's actual layout) always wins. A consumer with **no manifest at
  all** (a bare directory plus a config naming the remote and the base
  repo_id) bootstraps its whole layout from the published manifest: its member
  paths become the consumer directories.
- **`pull --all`** discovers every published manifest in the cache and applies
  each one to **its own members** (the repos keyed `<base>__<member-name>`),
  enriching the synthesized entries with the real type/package; members
  covered by no manifest keep the store-only synthesis. The consumer layout
  stays the short-name one above — it is the refresh-stable layout — so the
  published graphs are installed with their member references **rewritten to
  the consumer member names**. With several published workspaces in one
  cache, a member-NAME collision follows the same rule as the directory
  naming: **the first taker in repo_id order keeps the bare name**; a later
  workspace's same-named member stays at its `-2`/`-3` name, with a warning.
- Caches that never published metadata behave exactly as before — the
  metadata is additive, best-effort, and its absence is not an error.

**Consumer integrity: `cce sync verify --checksum-only`.** Full `cce sync
verify` rebuilds the index from the working tree, which inherently needs the
source. Repo-less consumers instead re-hash the **pulled** store's on-disk
bytes against the SHA-256 **recorded from the installed bytes at pull time**
(the `installed_sha256` field `pull` writes into `.cce/synced.json`, hashed
from the exact `index.json` file it just installed) — zero source checkout,
zero rebuild, zero network. Because the baseline is the installed file itself,
never a re-export through the current code, the check is
**version-independent**: artifacts pushed by any older cce verify exactly like
current ones ("has this file changed since pull"):

```console
$ cce sync verify --checksum-only --dir ctx
verify OK (checksum-only): 2 members
  billing          github.com__acme__billing@<sha>  (18ca676d989d)
  web              github.com__acme__web@<sha>  (f00dfeed0123)
```

A corrupted or truncated store fails loudly, naming the member; exit codes
mirror full `verify` (non-zero only on a real mismatch or an unreadable
store). A store whose marker was written by an **older cce** (no
`installed_sha256` recorded) is reported as an explicit notice with **exit
0** — it is not known-bad, it is unverifiable until a re-pull records the
hash:

```
  legacy           no install checksum recorded (pulled by an older cce) — re-pull with `cce sync pull --force` to enable checksum verification
```

**The honest caveat:** checksum-only detects *corruption, not a malicious
build*. True `artifact == build(sha)` verification requires the source and
stays where the source lives (CI / source-holders, via full `verify`). The
trust posture for repo-less consumers is **CI as the canonical pusher + the
git host's access control** (a signed-manifest scheme would be a future
spec).

---

## 8. Permissions — delegated to git

Access control is **whoever can pull the CCE Sync git repo**. Guidance:

- The Sync repo's read access MUST equal the intended audience of every repo cached
  in it — a Sync-repo member can pull any cache in it, regardless of source access.
- **Uniform-access org** → one Sync repo. **Compartmentalized repos** → one Sync
  repo per access boundary; different projects/workspaces point at different sync
  remotes via `sync.remote`.
- Redaction (v2.1) runs before any push, so no secrets enter the cache — but it is
  still proprietary code, so the git gate matters.

---

## 9. CI recipe (GitHub Actions)

Index `main` and push the cache on every merge. See
[`docs/ci/cce-sync.yml`](ci/cce-sync.yml) for a ready-to-copy workflow.

```yaml
name: cce-sync
on:
  push:
    branches: [main]
jobs:
  index-and-push:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with: { fetch-depth: 0 }          # full history so the sha's commit metadata resolves
      - name: Install git-LFS
        run: sudo apt-get update && sudo apt-get install -y git-lfs && git lfs install
      - name: Install cce                 # pin the latest release tag (see CHANGELOG.md)
        run: cargo install --git https://github.com/davidslv/cce-rust --tag vX.Y.Z
      - name: Configure git identity for the cache clone
        run: |
          git config --global user.name  "cce-ci"
          git config --global user.email "cce-ci@users.noreply.github.com"
      - name: Index main with the hash embedder
        run: cce index .
      - name: Push the cache
        env:
          # A deploy key or PAT with write access to the CACHE repo (not the source).
          CCE_SYNC_TOKEN: ${{ secrets.CCE_SYNC_TOKEN }}
        run: |
          cce sync init --remote "https://x-access-token:${CCE_SYNC_TOKEN}@github.com/acme/cce-cache.git"
          cce sync push
```

**Credential note.** The token/deploy key needs **write** access to the *cache*
repo, not the source repo. Scope it to that one repo. Developers only need **read**
access to the cache repo to `pull`; members with write access may also push
(content-addressing makes concurrent pushes safe).

---

## 10. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `no sync remote configured` | `sync.remote` unset | `cce sync init --remote <git-url>` |
| `refusing to push: the working tree is dirty` | uncommitted changes | commit first, or `cce sync push --commit <sha>` after checking out a clean sha |
| `refusing to push a non-hash index` | store built with `--embedder ollama` | `cce index <dir>` (hash is the default) before pushing |
| `cache miss: …<sha>.cce not found` | nobody pushed this sha (or `cce_version` window differs) | push it, `--latest`, or just `cce index` locally |
| `could not clone remote …` | wrong URL / no auth / offline | check the URL and your git credentials; local commands are unaffected |
| `verify FAILED` | working tree differs from the sha, or the cache is untrustworthy | check out the exact sha and re-run, or rebuild locally |
| `verify FAILED (checksum-only): <member>` | the pulled store's on-disk bytes changed since pull (corruption, truncation, manual edit) | `cce sync pull --force` (or `pull --all` for a consumer workspace) to reinstall it |
| `verify FAILED (checksum-only) for knowledge corpus …` | the pulled knowledge store's bytes changed since pull | `cce knowledge pull --corpus <id>` to reinstall it |
| `no install checksum recorded (pulled by an older cce)` | the `.cce/synced.json` marker predates `installed_sha256` | not a failure (exit 0) — re-pull with `cce sync pull --force` to record the hash |
| LFS: `git-lfs` filter errors on `get` | `.gitattributes` routes `*.cce` through LFS but `git-lfs` is not installed | `git lfs install`, or `cce sync init --no-lfs` for a plain-git cache |
| `local cache is at … but you are pulling …` | pulling a different sha over an existing cache | `cce sync pull --force` (only if you intend to overwrite) |

Offline is never fatal: with no remote, or an unreachable one, `cce index`,
`cce search`, and `cce sync status` all still work — `status` simply reports the
local-only state.

---

## 11. See also

- [`SPEC-SYNC.md`](../SPEC-SYNC.md) — the design specification.
- [`SPEC-SYNC-KNOWLEDGE.md`](../SPEC-SYNC-KNOWLEDGE.md) — knowledge corpora
  through the same cache (and [`knowledge.md`](knowledge.md) for the tour).
- [`docs/VERIFIED.md`](VERIFIED.md) — the cold-start verification transcript.
- [`docs/architecture.md`](architecture.md) — where sync sits in the module tree.
- [`docs/DECISIONS.md`](DECISIONS.md) — the format/checksum decisions and their why.
