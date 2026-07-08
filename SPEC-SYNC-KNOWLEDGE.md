# CCE Knowledge Sync — knowledge corpora through the content-addressed cache (M5)

**Status:** Build specification for **M5: knowledge-corpus sync** (issue #56), the
sibling of SPEC-SYNC.md. SPEC-SYNC moved *code* indexes through a git+LFS
content-addressed cache; this document reapplies that pattern — the same remote,
the same offline-first posture, the same additive-key discipline — to the v2.6
**knowledge system** (`cce.knowledge/v1`, docs/knowledge.md), so issues/docs/tickets
travel to consumers who cannot run every adapter themselves. It is **spec-first**:
this document merges before any implementation PR, and the implementation is held
to it.

**One-line model:** *the sync pattern, reapplied.* A knowledge snapshot is already
a deterministic, location-independent hash of its input feed (M3); content-
addressing therefore applies unchanged. A corpus is pushed as a canonical `.cck`
artifact under its own key space in the **same** cache remote, and a pulled corpus
is **byte-identical** to a locally-ingested one, so retrieval (`source:
code|knowledge|both`) needs zero changes.

**Locked decisions (this track):** same git+LFS remote as code artifacts · a new,
additive `knowledge/…` key space (`SYNC_FORMAT_VERSION` and every code-artifact
byte untouched) · **trust-the-pusher** (no rebuild-verify analogue exists — §4.2
states this honestly) · redaction at index time, before anything reaches a remote.

---

## 0. Why

Code context already travels: a consumer with only git access to the sync remote
gets full federated search over every cached repo (`cce sync list`, `pull --all`,
`verify --checksum-only`; SPEC-SYNC §5). **Knowledge does not travel at all.** The
`cce.knowledge/v1` NDJSON contract feeds a local snapshot-keyed store
(`.cce/knowledge/`), and corpus sync was explicitly deferred at M5. So the *why*
behind the code — epics, issues, policy docs — stays stranded on the machine that
ingested it, which is precisely the machine of the one person who could run the
adapter. This spec un-defers M5: the knowledge corpus becomes one more thing the
dumb cache carries, pushed by a builder job and pulled by anyone the git ACL
admits.

---

## 1. Invariants (normative)

1. **Deterministic / byte-pinned.** The `.cck` artifact is a pure function of
   `(feed bytes, corpus_id)` — no timestamps, no host names, no builder identity
   inside the artifact (the same provenance-free discipline as the reconciled
   `.cce` format). Exporting the same store twice yields identical bytes;
   installing a pulled corpus yields a native store **byte-identical** to a local
   `cce knowledge index` of the same feed.
2. **Additive.** `SYNC_FORMAT_VERSION`, the `.cce` artifact bytes, the
   `hash/<ver>/…` key space, ref-pointer semantics, `conformance.json`, and every
   existing golden are untouched. The `knowledge/…` prefix is a new key family
   under the SPEC-SYNC §3 additive-keys rule: clients MUST ignore keys they do not
   understand, and old clients never see it (`sync list` walks `hash/<ver>` for
   repos; the knowledge walk is new code). The knowledge artifact versions on its
   **own** contract id (`cce.knowledge/v1`), never on `SYNC_FORMAT_VERSION`.
3. **Offline-first.** The same git-only network posture as SPEC-SYNC §9: no remote
   configured ⇒ every knowledge command behaves exactly as today; an unreachable
   remote fails the sync command cleanly and nothing else; the local
   `.cce/knowledge/` store is always authoritative for local operations; a failed
   push/pull never breaks local ingest or search.
4. **Secret-safe.** The v2.1 redactor runs at **index time**, before chunking, so
   the store — and therefore the artifact — never contains an unredacted secret.
   The raw NDJSON feed MUST NOT be pushed to any remote (§4.6).
5. **Zero retrieval changes.** A pulled corpus is indistinguishable from a
   locally-ingested one: same on-disk bytes, same `current` pointer mechanics, so
   `context_search source: knowledge|both` works on a repo-less consumer with no
   change to the M4 retrieval blend, staleness rules, or provenance grammar.

---

## 2. The corpus artifact (`.cck` — canonical, byte-exact)

**What is uploaded: the built store, never the feed.** Two candidates existed —
the raw NDJSON feed, or the built knowledge store. The feed is disqualified on
secret-safety alone: redaction happens at index time, so pushing the feed would put
pre-redaction bytes on the remote, violating the v2.1 posture. The built store is
also the cheaper pull (no chunking, no embedding on the consumer) and the stronger
determinism (the consumer installs exactly what retrieval will serve). The
**snapshot id stays the content address**: the first 16 lowercase-hex chars of
SHA-256 over the raw feed bytes (M3, `knowledge::store::snapshot_id`) — pushing
does not re-key anything.

Like the `.cce` artifact, the `.cck` container is a **canonical, engine-neutral
serialization**, not either engine's native store dump. It is a UTF-8 stream with
an LF after **every** line (including the last):

- **Line 1 — the manifest**, one JSON object with **sorted keys and compact
  separators**:

  ```
  {"checksum":"…","chunk_count":N,"contract":"cce.knowledge/v1",
   "corpus_id":"…","data_as_of":"…"|null,"records":N,"snapshot":"…"}
  ```

  - `contract` — the pinned schema id of the feed the store was built from.
  - `corpus_id` — the §4.1 identity the artifact is keyed under.
  - `snapshot` — the M3 snapshot id (hash of the feed bytes).
  - `records` / `chunk_count` — as in the store.
  - `data_as_of` — the **lexicographic maximum** `updated_at` across all chunks
    (ISO-8601 strings compare correctly lexicographically), or `null` when no
    record carries one. Deterministic — a content property, not a push property
    (§4.4).
  - `checksum` — lowercase-hex SHA-256 over the ENTIRE stream built with the
    manifest's `checksum` value set to `""` (the exact `.cce` rule).
  - **No provenance fields** (`built_at`, `built_by`, host, user): the artifact
    is reproducible or it is nothing.

- **Lines 2..N+1 — one JSON object per chunk**, in **store order** (feed record
  order, then section order — already deterministic per M1/M3; chunks are NOT
  re-sorted, because document order is meaningful and the store's order is the
  canonical one). Sorted keys, compact separators. Every `KnowledgeChunk` field is
  always present (absent optionals serialize as JSON `null`, empty lists as `[]`)
  so the line shape is fixed:

  ```
  {"chunk_id":"…","content":"…","embedding":"<base64>","end_line":N,
   "group":…,"kind":"…","labels":[…],"links":[…],"name":"…","record_id":"…",
   "source":"…","start_line":N,"state":…,"state_reason":…,"title":"…",
   "token_count":N,"updated_at":…,"url":…}
  ```

  `embedding` is **standard base64 (with padding) of the little-endian IEEE-754
  `f64` bytes** of the persisted embedding — the exact `.cce` codec — never
  decimal floats.

- **No graph line.** Knowledge has no import graph; the container ends after the
  last chunk line.

**Round-trip (normative).** `import(export(store)) == store`, and installing an
imported store writes native-store bytes identical to what a local
`cce knowledge index` of the same feed writes (`<root>/.cce/knowledge/
<snapshot>.json`, pretty JSON, declaration-order fields, trailing newline, plus
the one-line `current` pointer). This byte-identity is the acceptance bar for §7.

**Export precondition.** `knowledge push` MUST refuse a store whose chunks lack
persisted embeddings (a pre-v2.6.1 Phase-A snapshot, where `embedding` defaults to
`[]`) with a clear "re-ingest with this version" message — an artifact whose
consumer would have to recompute embeddings is not the byte-pinned store contract.

**Cross-language.** Both engines MUST produce identical `.cck` bytes for the same
`(feed, corpus_id)` and import each other's artifacts losslessly, proven the
SPEC-SYNC §10 way: a committed golden checksum for a shared fixture feed.

---

## 3. Content address (key space)

A corpus lives under its own top-level prefix in the **same remote**:

```
knowledge/<contract_version>/<corpus_id>/<snapshot>.cck    # the artifact
knowledge/<contract_version>/<corpus_id>/current           # pointer: "<snapshot>\n"
knowledge/<contract_version>/<corpus_id>/corpus.json       # published metadata (§4.4)
   e.g.  knowledge/v1/internal-tickets/9f1c2a3b4c5d6e7f.cck
```

- `contract_version` = the version segment of the pinned contract id (`v1` from
  `cce.knowledge/v1`). A contract bump is a new directory — old clients keep
  reading `v1`, exactly the `cce_version` cache-window behaviour of SPEC-SYNC §3.
- `corpus_id` — §4.1.
- `<snapshot>` — the M3 snapshot id. Distinct snapshots are distinct files, so
  concurrent pushes never conflict in content (only ref advancement races —
  handled by the existing fetch-rebase-retry).
- `current` — a one-line pointer naming the corpus's active snapshot, the exact
  analogue of the code cache's `refs/<ref>` pointer files (and of the local
  store's `.cce/knowledge/current`). `knowledge pull` resolves it by default.
- `corpus.json` — a tiny, non-LFS published-metadata blob (the #55 well-known-key
  pattern), specified in §4.4.

**Additivity (normative, mirroring the #55 note in SPEC-SYNC §3).** The
`knowledge/` prefix is disjoint from every `<embedder_id>/…` prefix. Introducing
it is **not** a format change: it is neither a `.cce` artifact nor a `refs/<ref>`
pointer, so existing artifact keys, pointer semantics, `SYNC_FORMAT_VERSION`, and
old-client pulls of code artifacts are all unaffected — clients MUST ignore keys
they do not understand. Only a change to the **`.cck` bytes' shape** moves the
knowledge contract version, and it moves `cce.knowledge/vN` — never
`SYNC_FORMAT_VERSION`.

**git-LFS:** `*.cck` joins `*.cce` in the cache's `.gitattributes` (corpora carry
embeddings; they are large). `current` and `corpus.json` are plain text blobs —
readable without smudge, which is what keeps `sync list` read-cheap (§6).

---

## 4. The six decisions (normative)

### 4.1 Corpus identity — `corpus_id` is an adapter-chosen stable slug

`corpus_id` is chosen by the adapter/operator and validated **like `repo_id`**:
non-empty, charset `[A-Za-z0-9._-]`, and it MUST be **sanitize-stable** (push
refuses an id that `sanitize_id` would alter, rather than silently rewriting it).
It is resolved explicitly — `--corpus <id>` or the `knowledge.sync.corpus_id`
config key — **never derived**: knowledge has no git origin to normalize, and a
guessed identity would silently fork a corpus. Renaming a corpus is a new key
prefix; the old one ages out under retention (§4.5).

*Rationale:* the same property that makes `repo_id` work — a stable, path-safe,
human-legible name that the pusher owns — with the derivation dropped because no
equivalent of "the git origin" exists for a ticket feed.

### 4.2 Trust — trust-the-pusher; stated honestly, never implied away

Code artifacts have `artifact == build(sha)`: any source-holder can rebuild and
compare. **No such analogue exists for knowledge** — the puller lacks the source
feed, so a knowledge corpus is *not rebuild-verifiable by consumers*, and this
spec MUST never imply verify-parity with code artifacts (docs and command output
included).

The posture, explicitly:

- **Trust the pusher.** The canonical pusher is a CI adapter job (§9), the same
  CI-as-canonical-pusher stance as SPEC-SYNC §7.
- **The git host's ACL is the gate** (SPEC-SYNC §6): whoever can push to the
  cache repo can publish a corpus; whoever can read it can pull one. No custom
  RBAC.
- **Content-address integrity:** import recomputes and verifies the manifest
  `checksum` on every pull — a corrupted or tampered-in-transit artifact fails
  loudly. And `verify --checksum-only` covers pulled knowledge stores (§7), so
  post-install corruption is detectable offline.
- **Pusher-side determinism enables audit, not verification:** anyone holding the
  same feed bytes can re-export and compare checksums. That is an *audit path for
  feed-holders*, not a consumer verification.
- **Detached signatures are deferred** (§13) — the container's fixed byte shape
  makes a future signature a sibling key (`<snapshot>.cck.sig`), additive again.

The honest comparison, side by side:

| property                              | code artifact (`.cce`)                  | knowledge corpus (`.cck`)                       |
|---------------------------------------|------------------------------------------|--------------------------------------------------|
| rebuild-verify (`artifact == build`)  | **yes** — `cce sync verify` (needs source) | **no** — the puller lacks the source feed        |
| checksum verified on pull             | yes                                       | yes                                              |
| offline corruption check after install| yes (`verify --checksum-only`)            | yes (`verify --checksum-only`, §7)               |
| determinism                           | pure function of `repo@sha`               | pure function of `(feed, corpus_id)` — auditable by feed-holders only |
| canonical pusher                      | CI on merge                               | CI adapter cron (§9)                             |
| access control                        | git ACL (SPEC-SYNC §6)                    | git ACL — same repo or per-corpus remote (§4.3)  |
| secrets                               | redacted at index; push always rebuilds protected | redacted at index, unconditionally (§4.6) |
| signatures                            | deferred                                  | deferred                                         |

### 4.3 Access boundary — same-remote default, per-corpus `remote:` override

Default: a corpus is pushed to and pulled from the project's `sync.remote` — one
cache, one ACL, the SPEC-SYNC §6 guidance unchanged ("the Sync repo's read access
MUST equal the intended audience of everything cached in it" — now including
corpora).

A corpus whose audience differs from the code it annotates (e.g. internal tickets
beside shareable code) sets `knowledge.sync.remote` (§8), which overrides
`sync.remote` for knowledge commands only. Since the builder job for a corpus is a
dedicated project root (§9), one root ⇒ one corpus ⇒ one remote is the v1 shape; a
richer per-corpus map under a single root is flagged maintainer-overridable (§13).

*Rationale:* compartmentalization stays git's job (one cache repo per access
boundary), and the override is the minimal knob that lets a different-audience
corpus obey it.

### 4.4 Freshness — a deterministic data age plus a push age, surfaced everywhere

Two distinct staleness questions get two signals:

- **How old is the data?** `data_as_of` (§2) — the max `updated_at` in the corpus.
  Deterministic, inside the artifact, computable locally from any installed store.
- **How recently was the corpus published?** `pushed_at` — deliberately **outside**
  the artifact (it would break reproducibility), carried in the published
  `corpus.json` metadata blob, rewritten on every push:

  ```json
  {
    "schema": "cce.knowledgemeta/v1",
    "corpus_id": "internal-tickets",
    "current": "9f1c2a3b4c5d6e7f",
    "records": 412,
    "chunk_count": 1873,
    "data_as_of": "2026-07-01T09:00:00Z",
    "pushed_at": "2026-07-08T03:00:00Z"
  }
  ```

  (sorted-keys, pretty-printed, trailing newline — the house `--json` grammar.)
  `current` (the pointer file) stays the single source of truth for *pulls*;
  `corpus.json` is best-effort display metadata and its absence degrades to `null`
  fields, never an error.

**Surfacing (exact fields):**

- `cce sync list` — §6.
- MCP `index_status` — when a knowledge store exists at the served root, the
  report gains a knowledge block (mirroring the existing sync-freshness lines):

  ```
    knowledge :
      corpus         : <corpus_id, or "(local ingest)" when no sync marker exists>
      snapshot       : <snapshot id>
      records/chunks : <records> / <chunks>
      data as-of     : <data_as_of, or "-">
      remote current : <the remote pointer's snapshot, or "-">
      behind remote  : yes — run `cce knowledge pull` | no
  ```

  `remote current`/`behind remote` are best-effort and offline-safe exactly like
  the code freshness lines (any error ⇒ `-` / `no`); `behind remote` is `yes`
  only when both snapshots are known and differ.

### 4.5 Retention — a per-corpus `KeepLast` analogue

`knowledge.sync.retention: all | keep-last-<n>` (default `all`), the exact
`sync.retention` grammar. At push time, after the new snapshot and pointer land,
the pusher prunes the oldest `<snapshot>.cck` keys beyond `n` — **oldest by the
cache repo's commit history for the key** (corpora have no sha ordering; git
history is the only order the cache itself carries). The snapshot named by
`current` is never pruned regardless of `n`. Pruning is push-side and best-effort:
a prune failure warns and never fails the push.

*Rationale:* corpora re-snapshot on every adapter run (daily crons ⇒ hundreds of
LFS blobs a year); `KeepLast` is the same answer sync already gives, per corpus.

### 4.6 Redaction — index-time, unconditional; the feed never travels

- The v2.1 redactor runs at `cce knowledge index` time, **before chunking**, so
  chunk ids and token counts derive from redacted text and the store — hence the
  artifact — never holds an unredacted secret. This is unconditional: **`cce
  knowledge index` has no `--allow-secrets` analogue**, by design.
- The adapter is expected to **pre-scrub org-specific PII** before emitting the
  contract (the regulated-deployment posture restated from docs/knowledge.md:
  curation is the adapter's job; the redactor catches secret-shaped material, not
  org-specific semantics).
- **The raw NDJSON feed MUST NOT be pushed to any remote** — only the built,
  redacted store travels (§2). A feed on the builder is ephemeral input.
- Mirroring the code path's `--allow-secrets` posture: code `sync push` never
  reuses store bytes — it always rebuilds with protection on, so an
  `--allow-secrets` index can never be laundered into the cache. Knowledge push
  cannot rebuild (it has no feed at push time in general), so the guard is
  structural instead: the only store `knowledge index` can produce is a redacted
  one. **Normative future-proofing:** if a redaction-bypass flag is ever added to
  `knowledge index`, the store MUST record the bypass and `knowledge push` MUST
  refuse such a store — the exact analogue of the non-hash-embedder refusal.

---

## 5. CLI

```
cce knowledge push [--corpus <id>] [--dir <root>]
    # export the CURRENT local knowledge store as a .cck, put it at its
    # content-addressed key, advance the corpus `current` pointer, publish
    # corpus.json — one commit/push (put_many). Applies retention (§4.5).

cce knowledge pull [--corpus <id>] [--latest | --snapshot <id>] [--force] [--dir <root>]
    # fetch the corpus's current snapshot (--latest is the explicit spelling of
    # the default; --snapshot pins one), verify the checksum, install it into
    # <root>/.cce/knowledge/ exactly as a local ingest would, record the marker.

cce sync list [--remote <url>] [--json]     # grows a knowledge section (§6)
cce sync pull --all --into <dir> [--corpus <id>]   # also installs knowledge (§7)
cce sync verify --checksum-only             # covers pulled knowledge stores (§7)
```

Rules (normative):

- **push** refuses: no local knowledge store (`current` missing); an unresolved
  `corpus_id` (§4.1); an invalid `corpus_id`; a store without persisted
  embeddings (§2); a (future) redaction-bypassed store (§4.6). It is best-effort
  and never blocks local work. The remote resolves per §4.3.
- **pull** verifies the manifest checksum before installing; a mismatch is a hard
  failure naming the key. Install = write `<root>/.cce/knowledge/<snapshot>.json`
  (native-store bytes, §2 byte-identity) + point `current` at it + write the
  **knowledge sync marker** `<root>/.cce/knowledge/synced.json`:

  ```json
  {"corpus_id":"…","snapshot":"…","checksum":"…","installed_sha256":"…"}
  ```

  `installed_sha256` is hashed from the exact snapshot file just written to disk
  (read back), the #55 mechanism verbatim — version-independent by construction.
- **Overwrite guard (§9.4 analogue):** pulling a **different corpus** than the
  marker records refuses without `--force`. Pulling a **newer snapshot of the
  same corpus** supersedes silently — that is precisely local re-ingest
  semantics, and what makes refresh idempotent.
- A local `cce knowledge index` after a pull simply supersedes the pulled
  snapshot (the marker becomes stale for the new snapshot; `index_status` then
  reports `(local ingest)`). Nothing forbids it — local is authoritative.
- Offline / no remote / auth failure ⇒ a clear message; local ingest and search
  continue to work (§1.3).
- There is **no `cce knowledge init`**: the config keys (§8) are written by hand
  or by the adapter job; `cce sync init` continues to own the remote clone setup.

---

## 6. Discovery — `cce sync list` grows a knowledge section

**Human output:** after the repos table (and only when the cache carries at least
one corpus), a knowledge section:

```
knowledge:
corpus_id         current           snapshots  bytes      data as-of
internal-tickets  9f1c2a3b4c5d6e7f          7  48211324   2026-07-01T09:00:00Z
runbooks          0a1b2c3d4e5f6a7b          2   1048576   -
```

Columns: `corpus_id` · `current` (the pointer's snapshot, `-` when absent) ·
`snapshots` (distinct `.cck` keys) · `bytes` (LFS-aware, the existing
pointer-size rule) · `data as-of` (from `corpus.json`, `-` when absent). Rows
sort by `corpus_id`. A knowledge-free cache prints **no** knowledge section — the
existing output is byte-identical.

**JSON — the schema decision:** the listing **stays `cce.synclist/v1`** and gains
an OPTIONAL top-level `knowledge` array, emitted **only when the cache carries at
least one corpus**:

```json
{
  "schema": "cce.synclist/v1",
  "remote": "…",
  "repos": [ … ],
  "knowledge": [
    { "corpus_id": "internal-tickets", "current": "9f1c2a3b4c5d6e7f",
      "snapshots": 7, "bytes": 48211324,
      "data_as_of": "2026-07-01T09:00:00Z", "pushed_at": "2026-07-08T03:00:00Z" }
  ]
}
```

(`current`, `data_as_of`, `pushed_at` are `null`able — a field never disappears
within a row.) **Why not `/v2`:** no existing field changes shape or meaning; a
knowledge-free cache renders byte-identically, so every existing golden holds;
and tolerant-reader additivity is already this project's normative rule twice
over (SPEC-SYNC §3 additive keys; the additive `installed_sha256` in
`.cce/synced.json`). A consumer that keys on `schema` keeps working; a consumer
that cannot tolerate an unknown key was already broken by that rule. This choice
is flagged maintainer-overridable (§13) for anyone preferring strict
schema-versioning semantics.

Reading stays cheap and read-only: the knowledge walk needs only `current` and
`corpus.json` blobs (plain text, no LFS smudge) plus key/size enumeration — the
same no-mutation posture as the existing `cmd_list`.

---

## 7. Consumer integration — indistinguishable from a local ingest

**Where a pulled corpus lands.** The MCP server loads knowledge from the served
**root** (`KnowledgeStore::load_current(server.root())`), for a workspace the
workspace root. Therefore `pull --all --into <dir>` installs knowledge at
**`<dir>/.cce/knowledge/`** — the workspace root, not a member — and a repo-less
consumer's `cce mcp --workspace --dir <dir>` serves `source: knowledge|both`
immediately, with zero retrieval changes.

**Single-active-corpus invariant (v1, normative).** The local knowledge store has
one `current` pointer per root; that is the local semantics today (a newer ingest
supersedes), and pulled corpora inherit it: one active corpus per consumer root.
Blending multiple corpora in one search is a retrieval change and is explicitly
deferred (§13).

**`pull --all` corpus selection:** `--corpus <id>` wins; else, a cache carrying
**exactly one** corpus installs it; else (multiple corpora, no flag) the run
**warns and skips knowledge** — listing the corpus ids so the user can choose —
and never fails the member pulls. Refresh is idempotent via the marker: an
unmoved remote `current` is reported `up-to-date` and not re-fetched (the exact
member-pull rule).

**`verify --checksum-only` covers knowledge.** When the verified root carries a
knowledge sync marker, the report gains a knowledge row: re-hash
`.cce/knowledge/<snapshot>.json` against the marker's `installed_sha256` — same
mechanism, same exit-code rules, same "corruption, not a malicious build" caveat
(sharpened for knowledge: there is no full-`verify` escalation path at all,
§4.2). A marker without `installed_sha256` is the same explicit notice + exit 0.
A root with only a locally-ingested store (no marker) verifies exactly as today —
no knowledge row, no error.

**Acceptance test shape (hermetic, end-to-end — the M5 exit bar):**

1. Producer root: write a fixture `cce.knowledge/v1` NDJSON → `cce knowledge
   index` → `cce knowledge push --corpus fixture` to a `file://` bare remote
   (beside pushed code artifacts for at least one repo).
2. Fresh consumer dir (no source, no prior state): `cce sync pull --all --into
   consumer/` (or `cce knowledge pull --corpus fixture --dir consumer/`).
3. Assert `consumer/.cce/knowledge/<snapshot>.json` is **byte-identical** to the
   producer's, and `current` names the same snapshot.
4. MCP `context_search source: both` over the consumer returns the knowledge hit
   with the byte-identical provenance line, content, and blended ranking as the
   producer-side search.
5. `cce sync list --json` shows the corpus; `cce sync verify --checksum-only`
   passes; re-running `pull --all` reports `up-to-date` with no fetch.

---

## 8. Config

Extending `.cce/config` (all keys optional; absent ⇒ knowledge sync off, pure
local knowledge exactly as today):

```yaml
knowledge:
  sync:
    corpus_id: internal-tickets   # required to push (or pass --corpus)
    remote: null                  # per-corpus override; default = sync.remote (§4.3)
    retention: keep-last-10       # all | keep-last-<n>; default all (§4.5)
```

The existing `knowledge.enabled`, `knowledge.min_score`,
`knowledge.default_source`, and `markdown.max_section_tokens` keys are untouched.

---

## 9. Ingestion reference — a builder job, never a serving process

The production axis is a scheduled adapter run: CI cron fetches from the source
tool, emits the contract, indexes, pushes. **It is a builder pushing to the dumb
cache — not a service.** Nothing serves knowledge at runtime; consumers pull from
git like every other artifact.

Reference workflow (ships verbatim as `docs/ci/cce-knowledge-sync.yml` in M5.4,
the sibling of `docs/ci/cce-sync.yml`):

```yaml
# Scheduled knowledge-corpus builder (SPEC-SYNC-KNOWLEDGE §9).
# Fetch → emit cce.knowledge/v1 NDJSON → index (redacts) → push the corpus.
name: cce-knowledge-sync

on:
  schedule:
    - cron: "0 3 * * *"          # nightly; the corpus re-snapshots per run
  workflow_dispatch: {}

concurrency:
  group: cce-knowledge-sync
  cancel-in-progress: false

jobs:
  build-and-push-corpus:
    runs-on: ubuntu-latest
    steps:
      - name: Install git-LFS + the cce binary
        run: |
          sudo apt-get update && sudo apt-get install -y git-lfs
          git lfs install
          cargo install --git https://github.com/davidslv/cce-rust --tag vX.Y.Z

      - name: Configure git identity for the cache working clone
        run: |
          git config --global user.name  "cce-ci"
          git config --global user.email "cce-ci@users.noreply.github.com"

      - name: Fetch and emit the contract (the adapter — YOUR code)
        env:
          SOURCE_TOKEN: ${{ secrets.KNOWLEDGE_SOURCE_TOKEN }}
        # Any program that emits cce.knowledge/v1 NDJSON. The adapter owns
        # curation: drop wontfix/low-signal records, pre-scrub org PII (§4.6).
        run: ./adapter/fetch-and-emit > corpus.jsonl

      - name: Index (redacts at index time, before anything leaves this job)
        run: |
          mkdir -p corpus-root
          cce knowledge index corpus.jsonl --dir corpus-root

      - name: Push the corpus
        env:
          # WRITE access to the CACHE repo only — never the source tool.
          CCE_SYNC_TOKEN: ${{ secrets.CCE_SYNC_TOKEN }}
        run: |
          cd corpus-root
          cce sync init \
            --remote "https://x-access-token:${CCE_SYNC_TOKEN}@github.com/acme/cce-cache.git"
          cce knowledge push --corpus internal-tickets

# The feed (corpus.jsonl) is ephemeral builder input: it is never committed,
# uploaded, or cached (§4.6) — only the redacted store travels.
```

Credential guidance is SPEC-SYNC §6/§7 unchanged, plus one addition: the
source-tool token (`KNOWLEDGE_SOURCE_TOKEN`) and the cache token
(`CCE_SYNC_TOKEN`) are distinct secrets with disjoint scopes.

---

## 10. Offline-first guarantees (normative)

1. No remote configured ⇒ `cce knowledge index` and all retrieval behave exactly
   as today; `knowledge push/pull` fail with the clear "no sync remote" message.
2. Remote configured but unreachable ⇒ knowledge sync commands fail gracefully;
   ingest, search, and MCP serving are unaffected.
3. The local `.cce/knowledge/` store is always authoritative for local
   operations; `index_status` freshness lookups are best-effort and never block.
4. `knowledge pull` never silently replaces a different corpus without `--force`
   (§5); superseding a snapshot of the same corpus is the documented local
   semantics.

---

## 11. Testing (hermetic — no network)

A local bare git repo as the remote throughout (the existing sync test harness).
Required coverage, house style:

- `.cck` codec: export/import round-trip; byte-identity of `export(ingest(feed))`
  across runs; the committed **golden checksum** for the shared fixture feed;
  checksum verification failure on a flipped byte; refusal of an
  embedding-less store.
- Push: key + pointer + `corpus.json` land in one commit; corpus_id validation
  refusals; retention pruning (current never pruned); offline failure is clean.
- Pull: install byte-identity vs a local ingest; marker written with
  `installed_sha256`; different-corpus refusal without `--force`; same-corpus
  supersede; `--snapshot` pin.
- `sync list`: knowledge section human + JSON; **golden: a knowledge-free cache's
  listing is byte-identical to today's**; nullable fields when `corpus.json` is
  absent.
- `pull --all`: single-corpus auto-install; multi-corpus warn-and-skip naming the
  ids; idempotent refresh (`up-to-date`, no fetch); knowledge lands at the
  workspace root.
- `verify --checksum-only`: knowledge row pass/fail/no-record; local-ingest root
  untouched.
- MCP: `index_status` knowledge block fields incl. behind-remote; `context_search
  source: both` on a pulled consumer — the §7 acceptance test.
- Conformance: `conformance.json`, code-artifact goldens, and
  `SYNC_FORMAT_VERSION` are untouched (asserted, not assumed).

---

## 12. Milestones

Implementation is dispatched in phases, each with its own acceptance bar:

- **M5.1 — the `.cck` container.** `knowledge::artifact` (or sibling): canonical
  export/import per §2, checksum, embedding codec reuse, embedding-less refusal.
  *Bar:* round-trip + byte-identity tests green; golden fixture checksum
  committed; zero changes outside new code.
- **M5.2 — `cce knowledge push` / `pull`.** Key space (§3), pointer +
  `corpus.json` publishing, corpus_id validation (§4.1), overwrite guard, sync
  marker with `installed_sha256`, retention (§4.5), config keys (§8). *Bar:*
  hermetic push→pull round trip installs a byte-identical store; every §5 refusal
  has a test; offline messages clean.
- **M5.3 — the consumer surface.** `sync list` knowledge section (human +
  `cce.synclist/v1` extension, §6), `pull --all` knowledge install (§7), `verify
  --checksum-only` knowledge row, MCP `index_status` knowledge block (§4.4).
  *Bar:* knowledge-free-cache listing golden byte-identical; idempotent-refresh
  and multi-corpus-skip tests green; the §7 end-to-end acceptance test green.
- **M5.4 — docs + ingestion reference.** `docs/ci/cce-knowledge-sync.yml`
  verbatim from §9; docs/knowledge.md M5 section un-deferred and rewritten;
  docs/sync.md cross-reference; README. *Bar:* the SPEC-SYNC §10.5 verification
  gate — a cold-start pass of the documented walkthrough against a local remote,
  transcript recorded in docs/VERIFIED.md.

---

## 13. Deferred / maintainer-overridable

**Deferred (documented next steps, out of scope for M5):**

- **Detached signatures** for `.cck` artifacts (`<snapshot>.cck.sig` as an
  additive sibling key) — the upgrade path for the §4.2 trust posture.
- **Multi-corpus blend:** more than one active corpus per consumer root requires
  retrieval changes (per-corpus stores + a merged ranking) and is a contract for
  a future milestone, not this one.
- **Ruby-engine parity** for knowledge sync follows the same spec; the golden
  fixture checksum (§2) is the conformance hook.

**Flagged maintainer-overridable (normative as written, but a defensible
alternative exists):**

- §6 — keeping the schema id `cce.synclist/v1` with an optional `knowledge` key
  vs bumping to `/v2`.
- §4.3 — the single `knowledge.sync.remote` override vs a per-corpus map under
  one root.
- §7 — the multi-corpus `pull --all` behaviour (warn-and-skip) vs installing the
  lexicographically-first corpus.
