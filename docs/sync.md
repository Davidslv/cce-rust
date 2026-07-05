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

Only the deterministic **hash embedder** produces shareable caches. Ollama/semantic
indexes are non-reproducible and are **local-only** — `cce sync push` refuses them.

---

## 2. The model

The index is a pure function of `(repo content at commit, cce version, pack set,
embedder)`. With the hash embedder that function is reproducible and identical
across people and across the Ruby/Rust engines (already proven by conformance).
Therefore the cache is **content-addressable**: no "whose version wins," no merge,
no conflict. A teammate's push == CI's push == a fresh local build, bit-for-bit.

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
It is a UTF-8, LF-terminated, newline-delimited stream:

```
line 1        the manifest JSON
lines 2..N+1  one JSON object per chunk, N = chunk_count,
              sorted by (file_path, start_line, chunk_id)
line N+2      the graph JSON: { "file_imports": {...}, "file_tokens": {...} }
```

- **Sorted keys, compact separators.** Every object is serialized with keys in
  ascending order and no insignificant whitespace (`,` and `:` only).
- **Embeddings are not decimals.** A 256-d vector is encoded as **base64 of its 256
  little-endian IEEE-754 `f64` bytes**, so the bytes are identical across languages
  regardless of float→string formatting.
- **Checksum.** `checksum` = lowercase-hex SHA-256 over the canonical stream with
  the manifest's `checksum` field omitted.
- **Deterministic provenance.** `built_by` is the neutral constant `"cce"`;
  `built_at` is the **commit date of `sha`** (read from git, identical across
  engines). So the whole artifact — not just the checksum — is byte-identical for a
  given `repo@sha`.

Manifest fields (sorted): `built_at, built_by, cce_version, checksum, chunk_count,
embedder, pack_set_id, repo_id, sha`.

Chunk fields (sorted): `chunk_type, content, embedding, end_line, file_path, id,
kind, language, start_line, token_count`.

A committed **golden checksum** anchors the format cross-language
(`src/sync/artifact.rs::golden_checksum_for_base_fixture`): the `test/fixture/base`
corpus exported with `repo_id=example.com__acme__demo`,
`sha=0000…0000`, `built_at=2026-01-01T00:00:00+00:00` yields

```
48d8066cec52668fef75811bcd9cbd6c3e6ed5bcabe8bbbfef5f667463db61ee   (7 chunks)
```

The Ruby engine, exporting the same fixture with the same identity, must produce
this exact checksum. If this value ever changes, the wire format changed — treat it
as a breaking-format decision, not a test to "fix."

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
conflict in content — only git-ref advancement can race, handled with
fetch-rebase-retry. A small pointer file `…/<repo_id>/refs/<branch>` records the
latest sha pushed for a branch; that is what `cce sync pull --latest` reads.

---

## 5. Remote backend (git, LFS)

The remote is a **git repository** (`sync.remote`). A local working clone lives
under `~/.cce/sync/<remote-id>/` (override the base with `CCE_HOME`). `put` writes
the artifact at its content path, commits, and `git push` (fetch-rebase-retry on a
ref race). `get` fetches and reads the file back.

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
cce sync status [--dir <path>]
cce sync verify [--commit <sha>] [--dir <path>]
```

Rules:
- `push` refuses a **dirty working tree** (a cache is content-addressed by commit)
  and a **non-hash index**. It is best-effort and never blocks other work.
- `pull` installs the artifact into `.cce/`. If the local working tree matches
  `sha`, the pulled index is used as-is. It never silently overwrites a local cache
  for a **different** sha without `--force`.
- Offline / no remote / auth failure → a clear message; local indexing and search
  continue to work.
- **Workspace-aware:** `--workspace` iterates the manifest's members, each keyed by
  its own `repo_id@sha` (a member is keyed `<repo_id>__<member-name>`).

Config keys (`<root>/.cce/config`, or global `~/.cce/config.yml`):
`sync.remote`, `sync.lfs` (default `true`), `sync.repo_id`, `sync.auto_pull`,
`sync.retention` (`all` | `keep-last-<n>`). All optional; absent ⇒ pure local CCE.

---

## 7. Permissions — delegated to git

Access control is **whoever can pull the CCE Sync git repo**. Guidance:

- The Sync repo's read access MUST equal the intended audience of every repo cached
  in it — a Sync-repo member can pull any cache in it, regardless of source access.
- **Uniform-access org** → one Sync repo. **Compartmentalized repos** → one Sync
  repo per access boundary; different projects/workspaces point at different sync
  remotes via `sync.remote`.
- Redaction (v2.1) runs before any push, so no secrets enter the cache — but it is
  still proprietary code, so the git gate matters.

---

## 8. CI recipe (GitHub Actions)

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
        with: { fetch-depth: 0 }          # full history so built_at is available
      - name: Install git-LFS
        run: sudo apt-get update && sudo apt-get install -y git-lfs && git lfs install
      - name: Install cce
        run: cargo install --git https://github.com/davidslv/cce-rust --tag v2.3.0
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

## 9. Troubleshooting

| Symptom | Cause | Fix |
| --- | --- | --- |
| `no sync remote configured` | `sync.remote` unset | `cce sync init --remote <git-url>` |
| `refusing to push: the working tree is dirty` | uncommitted changes | commit first, or `cce sync push --commit <sha>` after checking out a clean sha |
| `refusing to push a non-hash index` | store built with `--embedder ollama` | `cce index <dir>` (hash is the default) before pushing |
| `cache miss: …<sha>.cce not found` | nobody pushed this sha (or `cce_version` window differs) | push it, `--latest`, or just `cce index` locally |
| `could not clone remote …` | wrong URL / no auth / offline | check the URL and your git credentials; local commands are unaffected |
| `verify FAILED` | working tree differs from the sha, or the cache is untrustworthy | check out the exact sha and re-run, or rebuild locally |
| LFS: `git-lfs` filter errors on `get` | `.gitattributes` routes `*.cce` through LFS but `git-lfs` is not installed | `git lfs install`, or `cce sync init --no-lfs` for a plain-git cache |
| `local cache is at … but you are pulling …` | pulling a different sha over an existing cache | `cce sync pull --force` (only if you intend to overwrite) |

Offline is never fatal: with no remote, or an unreachable one, `cce index`,
`cce search`, and `cce sync status` all still work — `status` simply reports the
local-only state.

---

## 10. See also

- [`SPEC-SYNC.md`](../SPEC-SYNC.md) — the design specification.
- [`docs/VERIFIED.md`](VERIFIED.md) — the cold-start verification transcript.
- [`docs/architecture.md`](architecture.md) — where sync sits in the module tree.
- [`docs/DECISIONS.md`](DECISIONS.md) — the format/checksum decisions and their why.
