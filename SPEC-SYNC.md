# CCE Sync — a distributed, offline-first cache for code-context indexes

**Status:** Design specification for a *separate track* (call it **CCE Sync**).
It layers **client–server capabilities on top of the existing CCE core** without
changing the core's local-first nature. This is a bigger, infrastructure-shaped
track than the CLI features — treat this as the design of record; a build spec
per implementation will refine the exact wire/format details.

**Non-negotiable principle:** CCE is **offline-first**. Every existing command
works with no network and no remote configured. **Sync is purely additive** — a
failed push/pull must never break local indexing or search.

**One-line model:** *git remotes for the index.* Local `.cce/` is authoritative;
an optional git-backed remote is a **content-addressed cache** you can push to
and pull from. Because the index is deterministic, a cache for `repo@sha` is
byte-identical no matter who — or which language implementation — built it.

**Locked decisions (this track):** git-LFS for the cache blobs · a **git
repository** as the remote · **CI as the canonical pusher** (members may also
push — content-addressing makes it safe) · **trust the checksum** on pull, with
`--verify` for a paranoid rebuild-and-compare.

---

## 1. Why this works: determinism → content-addressable cache

The index is a pure function of `(repo content at commit, cce version, pack set,
embedder)`. With the **deterministic hashing embedder**, the produced index is
reproducible and identical across people and across the Ruby/Rust engines
(already proven by conformance). Therefore:

- The cache for `repo@sha` is **content-addressable** — no "whose version wins,"
  no merge, no conflict.
- A teammate's push == CI's push == a fresh local build — **bit-for-bit**.
- A puller can **verify** a cache by re-indexing and comparing a checksum
  (supply-chain safety: you don't have to trust the pusher).

**Constraint:** only the **deterministic (hash) embedder** produces shareable
caches. Ollama/semantic-embedded indexes are non-reproducible, so they are
**local-only** and MUST NOT be pushed. `cce sync push` refuses unless the index
was built with the hash embedder.

---

## 2. The cache artifact (portable interchange format)

Because the engines store differently on disk (Ruby = SQLite, Rust = JSON), the
Sync artifact is **not** either native store — it is a **canonical, deterministic
interchange format** both engines export and import:

- Content: for a repo at a commit — every chunk `{id, file_path, start_line,
  end_line, chunk_type, kind, token_count, content, embedding[256]}`, the import
  graph, and (for a workspace member) nothing extra; plus a **manifest**
  `{repo_id, sha, cce_version (major.minor), embedder:"hash", pack_set_id,
  chunk_count, checksum, built_at, built_by}`.
- **Canonical serialization (byte-exact, cross-language).** The artifact is a
  UTF-8, LF-terminated, newline-delimited stream: line 1 = the manifest JSON,
  then one JSON object per chunk (chunks sorted by `(file_path, start_line,
  chunk_id)`), then the graph. Every JSON object uses **sorted keys and compact
  separators** (`,` `:`, no insignificant whitespace). **Embeddings are NOT
  serialized as decimals** — a 256-d vector is encoded as base64 of its 256
  little-endian IEEE-754 `f64` values, so the bytes are identical across Ruby and
  Rust regardless of float→string formatting (the vectors are already bit-equal —
  the hash embedder is deterministic). `checksum` = lowercase-hex SHA-256 over the
  canonical bytes with the manifest's `checksum` field omitted. Result: the
  artifact for `repo@sha` is **byte-identical across people and across both
  engines** — so one cache serves everyone and `--verify` works cross-language.
- Both engines MUST round-trip: export local store → artifact → import into a
  fresh local store, losslessly. A Ruby-built artifact must import into Rust and
  vice versa (cross-language portability is a first-class test).

---

## 3. Content address (cache key)

A cache is addressed by a path (in the git remote) built from its identity:

```
<embedder_id>/<cce_version_major.minor>/<repo_id>/<sha>.cce
   e.g.  hash/2.3/github.com__acme__billing/9f1c2a....cce
```

- `embedder_id` = `hash` (only shareable embedder).
- `cce_version` at major.minor — a format-compatible window; a mismatch is a
  cache miss (rebuild).
- `repo_id` = normalized git origin (host + org + repo), or a configured override.
- `sha` = the commit the index was built from.

Distinct shas are distinct files → concurrent pushes for different shas never
conflict in content. Fixed-path keys (the `refs/<ref>` pointers, the workspace
metadata) ARE rewritten by every push, so two racing pushes genuinely conflict
there; every key is whole-file last-writer-wins, and a lost push race is
handled by re-applying the write on the freshly fetched remote state and
retrying (bounded). A push that cannot land MUST fail loudly — never report
success without publishing.

**Adding new keys is additive (normative).** Beside the `<sha>.cce` artifact
keys and the `refs/<ref>` pointer files, a repo_id prefix MAY carry other
well-known keys. The first such keys are the **published workspace metadata**
(the self-describing cache): `cce sync push --workspace` also puts

```
<embedder_id>/<cce_version_major.minor>/<base_repo_id>/workspace.yml
<embedder_id>/<cce_version_major.minor>/<base_repo_id>/workspace-graph.json
```

under the workspace's **base** repo_id (the prefix its members'
`<base>__<member>` repo_ids derive from), so repo-less consumers can recover
the real member metadata and the cross-member dependency edges. Introducing a
key of this kind is **not** a format change: it is neither an artifact nor a
ref pointer, so existing artifact keys, pointer semantics,
`SYNC_FORMAT_VERSION`, and old-client pulls of code artifacts are all
unaffected — clients MUST ignore keys they do not understand. Only a change to
the **artifact bytes' shape** moves `SYNC_FORMAT_VERSION`.

---

## 4. Remote backend (pluggable; git first)

A `SyncRemote` interface: `has(key) -> bool`, `get(key) -> artifact`,
`put(key, artifact)`, `list(repo_id) -> [sha…]`, `latest(repo_id, ref="main")`.

**GitRemote (the default and recommended first backend):**
- The remote is a git repository (URL configured). A local working clone lives
  under `~/.cce/sync/<remote-id>/`.
- `put` = write the artifact at its content-addressed path, commit, `git push`
  (a lost ref race fetches, re-applies the write on the new remote state, and
  retries — pointer keys conflict for real, so a rebase would wedge). `get` =
  `git fetch` + read the file.
- **Large blobs:** artifacts are large; use **git-LFS** for `*.cce` by default
  (`cce sync init` writes the `.gitattributes`). Alternative: a retention policy
  (keep the last N shas per repo + tagged releases) — configurable.
- **Permissions / transport / auth are git's** (SSH/HTTPS credentials). CCE adds
  no RBAC of its own (see §6).

**Future backends (documented, not built):** S3/GCS, a thin HTTP server. The
interface keeps them possible without CLI changes.

---

## 5. CLI

```
cce sync init  --remote <git-url> [--lfs] [--repo-id <id>]   # configure + set up local clone
cce sync push  [--commit <sha>]        # ensure local hash-index for HEAD/sha, export artifact, put to remote
cce sync pull  [--commit <sha> | --latest]  # fetch cache for sha (default HEAD; else latest main) → install into .cce/
cce sync pull  --all --into <dir> [--remote <url>]  # consumer mode: pull every repo_id's latest, synthesize a workspace (no source)
cce sync list  [--remote <url>] [--json]    # enumerate the cache: one row per repo_id (latest sha, artifact count, bytes); read-only, repo-less
cce sync status                        # remote, local cache sha vs remote latest, working-tree match
cce sync verify [--commit <sha>]       # re-index locally and confirm the pulled artifact's checksum
cce sync verify --checksum-only        # consumer integrity: re-hash the pulled store against the recorded checksum (no source)
```

Rules:
- `push` refuses a **dirty working tree** (must push a committed `sha`) and a
  **non-hash embedder** (§1). It is best-effort and never blocks other work.
- `pull` installs the artifact into the local `.cce/` store (importing the
  interchange format into the engine's native store). If the local working tree
  matches `sha`, the pulled index is used as-is.
- Offline / no remote / auth failure → a clear message; local indexing & search
  continue to work.
- **Workspace-aware:** `cce sync push/pull --workspace` iterates members, each
  keyed by its own `repo_id@sha` (a member is just a repo). This composes with
  SPEC-V2.2. `push --workspace` additionally publishes the workspace metadata at
  the §3 well-known keys, making the cache self-describing for repo-less
  consumers.
- **Consumer mode is repo-less by construction.** `list` and `pull --all` need
  no local store, config, or source checkout — a bare directory plus
  `--remote <url>` suffices. `pull --all` synthesizes the workspace manifest
  (members typed `store-only`) and is an idempotent refresh; `verify
  --checksum-only` re-hashes the installed bytes against the checksum recorded
  at pull time (version-independent — corruption detection, not
  `artifact == build(sha)`, which stays with source-holders via full `verify`).

---

## 6. Permissions — delegated to git (no custom RBAC)

Access control is **whoever can pull the CCE Sync git repo**. CCE reinvents
nothing. Guidance:
- The Sync repo's read access MUST equal the intended audience of every repo
  cached in it (a Sync-repo member can pull any cache in it, regardless of source
  access).
- **Uniform-access org** → one Sync repo. **Compartmentalized repos** → one Sync
  repo per access boundary; CCE lets different projects/workspaces point at
  different sync remotes (`sync.remote` config).
- Redaction (v2.1) runs before any push, so no secrets enter the cache — but it
  is still proprietary code, so the git gate matters.

---

## 7. Freshness — CI as the canonical builder, local overlay for WIP

```
  CI on merge to main:  cce index (clean, hash embedder)  →  cce sync push
  developer:            cce sync pull --latest   →  main@sha index, instantly
                        work on a branch → CCE re-indexes ONLY the changed files
                        on top of the pulled base (bounded incremental pass),
                        so search reflects WIP without a full reindex
```

The overlay requires an incremental "reindex changed files against a base"
capability. v1 fallback: if the working tree differs from the pulled `sha`, do a
normal local `cce index` (full); the incremental overlay is a fast-follow. A
sample CI workflow (GitHub Actions) ships in docs.

---

## 8. Config

`~/.cce/config.yml` (global) and/or per-project `.cce/config`:
`sync.remote` (git URL), `sync.lfs` (bool, default true), `sync.repo_id`
(override), `sync.auto_pull` (bool — pull latest on `index`/`search` if online),
`sync.retention` (keep-last-N | all). All optional; absent ⇒ pure local CCE.

---

## 9. Offline-first guarantees (normative)

1. No remote configured ⇒ every command behaves exactly as today.
2. Remote configured but unreachable ⇒ `sync` commands fail gracefully; all
   non-sync commands are unaffected.
3. The local `.cce/` store is always authoritative for local operations.
4. `pull` never silently overwrites a newer local index for a different sha
   without `--force`.

---

## 10. Cross-language portability (first-class)

The interchange artifact format is specified byte-exactly so:
- A cache pushed by the **Ruby** engine imports into the **Rust** engine and
  vice versa.
- For the same `repo@sha` (hash embedder), the artifact built by Ruby, by Rust,
  by CI, or by any teammate is **identical** (same checksum).

Test: build `fixture@sha` in Ruby and in Rust independently → identical artifact
bytes → identical checksum → each imports into the other's engine losslessly.

---

## 10.5 Documentation — first-class and VERIFIED (plug-and-play)

Documentation is a **deliverable, not an afterthought**, and it is **not "done"
until a cold-start run of it succeeds with zero friction.** Required:

- **README "CCE Sync" section** — what it is, when to use it, and a
  copy-pasteable **end-to-end walkthrough**: install prereqs → `cce sync init` →
  push (or let CI) → `cce sync pull` → `cce search`, each with the **real
  expected output** (captured from an actual run, not invented).
- **Installation & environment setup** — exact prerequisites for **macOS and
  Ubuntu**: git, **git-LFS** (incl. `git lfs install`), the CCE binary. Every
  command verified to produce a working setup.
- **A ready-to-copy CI recipe** — a GitHub Actions workflow that indexes `main`
  and `cce sync push`es on merge, with a note on the credential/secret it needs
  and how to scope it.
- **`docs/sync.md`** — the model, the artifact format, the content-address
  scheme, the permissions guidance (one sync repo per access boundary), and a
  **troubleshooting** section (auth failure, LFS not installed, cache miss, dirty
  tree, non-hash embedder, checksum mismatch).
- **VERIFICATION GATE (mandatory):** before finishing, do a **cold-start pass** —
  follow the documented install + walkthrough from scratch against a local git
  remote (hermetic), and confirm every documented command runs and its output
  matches what the docs show. Record the cold-start transcript in
  `docs/DECISIONS.md` (or a `docs/VERIFIED.md`). A doc example that doesn't run
  verbatim is a bug.

## 11. Testing (hermetic — no network)

- Use a **local bare git repo** in a temp dir as the remote. Tests: `sync init`
  → `push` from clone A → `pull` into clone B → assert B's imported `.cce/` is
  functionally identical (same search results) and the artifact checksum
  matches.
- Content-address keying; push ref-race retry; `verify` (re-index == checksum);
  offline fallback; refusal on dirty tree / non-hash embedder; workspace
  push/pull over members; retention pruning.
- Determinism/interop: an artifact for `fixture@sha` is byte-identical whether
  built by this engine or supplied by the sibling language (checked against a
  committed golden checksum in the shared fixture).

---

## 12. Scope & sequencing

- **Ships after** workspace mode (v2.2), as **v2.3.0** (`cce sync …`), on both
  engines with an identical artifact format.
- **In scope v1:** git backend + LFS, push/pull/status/verify, content-address
  cache, portable artifact, deterministic verify, offline guarantees, workspace
  awareness, a sample CI workflow, docs.
- **Out of scope v1 (documented next steps):** the incremental branch-overlay
  (v1 falls back to full local index), non-git backends (S3/HTTP), an optional
  Sourcegraph *adapter* (a read-only remote backend for shops that already run
  Sourcegraph — kept a plugin, never core).

---

## 13. On Sourcegraph (design note)

CCE Sync is deliberately **not built on Sourcegraph**. Sourcegraph is the
reference for org-scale human code search/navigation; CCE's niche is
offline-first, token/agent-optimized, self-hostable-in-an-afternoon context.
Depending on Sourcegraph would betray the local-first, tiny, no-heavy-deps value.
The *only* sanctioned integration is an **optional, read-only `SyncRemote`
adapter** so teams that already run Sourcegraph can reuse its indexing — a
plugin, never a dependency of the core.
