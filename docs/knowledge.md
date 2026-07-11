# Knowledge Sources (v2.6)

Code says *what/how*; epics, issues, and policy docs say **why** — the intent and
constraints the code implements. CCE today indexes any `.md` as **one whole-file
chunk**, which buries the high-value big documents. v2.6 fixes that with a
**markdown-heading chunker** and adds a generic way to feed non-code knowledge in —
without CCE learning a single ticket system's API.

The guiding principle: **CCE owns the *engine* — chunking, a neutral ingest
contract, retrieval — never the *integrations*.** Teams bring their own extractor.

> **Scope.** Phase A (v2.6.0) covers M1 (the markdown-heading chunker), M2 (the
> `cce.knowledge/v1` contract), and M3 (`cce knowledge index`). Phase B (v2.6.1)
> covers M4 — the retrieval blend and the `source:` search filter (see
> [Searching knowledge](#searching-knowledge-m4--the-source-blend) below).
> Phase C (M5) is corpus sync — `cce knowledge push`/`pull`, the consumer
> surface, and the reference adapter workflow (see
> [Syncing a corpus](#syncing-a-corpus-m5--cce-knowledge-push--pull) below);
> the normative spec is [SPEC-SYNC-KNOWLEDGE.md](../SPEC-SYNC-KNOWLEDGE.md)
> ([#56](https://github.com/Davidslv/cce-rust/issues/56)).

## The markdown-heading chunker (M1)

Markdown is split by **heading section** instead of whole-file, mirroring how the
AST chunker splits code by function/class. It is parsed with the **tree-sitter-markdown**
block grammar (robust to code fences and nesting; deterministic).

**Boundary rule.** A chunk is a heading and its content down to (not including) the
next heading of the **same-or-higher** level. A deeper heading (`###` under `##`)
**rolls into its parent** — *unless* the parent section's estimated token count
exceeds the byte-pinned `markdown.max_section_tokens` budget (default **400**), in
which case the section **splits at its deeper headings**. Content before the first
heading is its own **leading (preamble)** chunk. A `#` inside a fenced code block is
**not** a heading. Setext (`===` / `---`) headings are recognised alongside ATX.

**Chunk fields** (byte-pinned, deterministic):

| field | meaning |
|-------|---------|
| `kind` | the heading text (raw inline markdown, trimmed); `(preamble)` for the leading chunk |
| `name` | a breadcrumb, markers reconstructed from level, e.g. `# Title › ## Section` (segments joined by ` › `, U+203A) |
| `chunk_id` | the existing `SHA-256(path:start:end:prefix)` scheme, over the section's trimmed bytes |
| `start_line` / `end_line` | 1-based; `end_line` is derived from the trimmed content so trailing blank lines are never counted |
| `token_count` | the shared `cce.tokens/v1` estimator (`floor(bytes/4)`, min 1) |

`compact`/`expand` apply for free: a `.md` chunk has no registered language pack, so
the shared L2 compressor uses its language-neutral rule — `compact` = the heading
line + a `… (+N lines)` elision, `full` recovers the exact stored bytes, which is
what `expand_chunk` (L7) re-serves.

**It is used ONLY by the knowledge ingest (M3).** The code index's `.md` handling is
unchanged, so `conformance.json` and the Sync *code* artifact stay byte-identical
(the markdown chunker is deliberately **not** a registered `LanguagePack`).
Heading-chunking code-repo `.md` too is a deferred future opt-in.

## The `cce.knowledge/v1` contract (M2) — the adapter boundary

A neutral **NDJSON** file — one knowledge record per line — that any adapter emits.
The schema id `cce.knowledge/v1` is pinned; a bump is a compatibility event.

```json
{ "id": "gh:owner/repo#123",     // required — stable unique id
  "title": "…",                  // required — becomes the top `# <title>` heading
  "body": "…markdown…",          // required — heading-chunked by M1
  "source": "github-issues",     // required — adapter/source tag
  "url": "https://…",            // optional
  "state": "open",               // optional — e.g. open | closed
  "state_reason": "completed",   // optional — completed | not_planned | reopened | null
  "updated_at": "2026-02-01T…",  // optional — ISO-8601; drives recency (M4)
  "labels": ["policy", "auth"],  // optional — free-form tags
  "group": "Checkout",           // optional — workstream / board column
  "links": ["https://…/pr/40"],  // optional — related URLs / PRs
  "extra": { … } }               // optional — adapter passthrough, ignored by retrieval
```

Parsing is robust: **unknown fields are ignored**, **absent optionals degrade** to
`None`/empty, blank lines are skipped, and a malformed line (bad JSON or a missing
required field) fails loudly with its 1-based line number.

## `cce knowledge index <file.jsonl>` (M3)

```
cce knowledge index curated.jsonl [--dir <root>]
```

For each record it:

1. **Renders** a deterministic markdown document: `# <title>\n\n<body>`.
2. **Redacts** that document with the v2.1 redactor **before** chunking, so a secret
   never reaches the store and chunk ids derive from redacted text (mirroring the
   code index's Layer 2).
3. **Heading-chunks** it with M1 (honouring `markdown.max_section_tokens`).
4. **Attaches facets** to every chunk: `source`, `url`, `state`, `state_reason`,
   `updated_at`, `group`, `labels`, and the record `id`.

The result is written to a **separate, snapshot-keyed knowledge store** under
`<root>/.cce/knowledge/` — never the code cache:

- `.cce/knowledge/<snapshot>.json` — the store for one extraction snapshot. The
  **snapshot id** is a deterministic hash of the input feed's bytes, so the store is
  reproducible and **location-independent**.
- `.cce/knowledge/current` — a one-line pointer naming the active snapshot. A newer
  ingest **supersedes** the old one.

Knowledge is *mutable*, which is exactly why it is snapshot-keyed rather than
`repo@sha`-keyed and never enters the byte-identical code cache.

## Searching knowledge (M4) — the `source:` blend

Since v2.6.1, knowledge chunks are searchable through the **exact same hybrid
retrieval as code** — the deterministic hash embedder + BM25 + RRF of SPEC §6; there
is no bespoke knowledge scorer. The MCP `context_search` tool gains an optional
`source` argument (**still nine tools**; the CLI `cce search` is code-only):

- `source: "code"` — the unchanged code path (byte-identical to pre-v2.6).
- `source: "knowledge"` — search only the knowledge store.
- `source: "both"` — code + knowledge candidates merged through the one shared
  ranking into a single top-K (ties break code-before-knowledge, then `chunk_id`).

When the caller omits `source`, the config `knowledge.default_source` applies (default
`both`) — **only if a knowledge store exists**; with no store, retrieval always
resolves to `code`, preserving the previous behaviour exactly.

**Provenance.** Every knowledge hit renders a byte-pinned provenance header in place
of the code grammar's `file:line (type/kind)`:

```
 1. [0.842311] [knowledge] Checkout retries — closed · 2026-02-01T09:00:00Z · https://…/issues/123 #a1b2c3d4e5f60789
```

(`[knowledge] <title> — <state> · <updated_at> · <url>`, missing facets omitted
cleanly), so freshness and origin are always judgeable at a glance.
`expand_chunk`/`related_context` work on knowledge chunks too — neighbours are the
same document's other sections.

**Staleness weighting** (deterministic, byte-pinned):

- Records whose `state_reason` is `not_planned` (wontfix) are **dropped** — rejected
  intent must never surface as guidance.
- The L5 **precision floor** applies: a hit below `knowledge.min_score` (default
  **0.30**) or without a shared query token is dropped, so a loosely-related or
  stale record never surfaces.
- A record whose `links` include a **merged-PR** reference is intent AND
  implementation, so its score is scaled by a pinned **×1.10** boost.
- Final order: score desc, then **recency** (`updated_at` newest-first), then
  `chunk_id` — a missing `updated_at` sorts oldest.

## Configuration (`.cce/config`)

```yaml
markdown:
  max_section_tokens: 400   # byte-pinned split budget for oversized sections
knowledge:
  enabled: true             # ingest + retrieval are on by default
  min_score: 0.30           # the recall precision floor (shared with L5 memory)
  default_source: both      # code | knowledge | both — used when a search omits `source`
```

## Syncing a corpus (M5) — `cce knowledge push` / `pull`

A built corpus travels through the **same content-addressed cache** as code
indexes ([sync.md](sync.md)), as a canonical `.cck` artifact under its own
additive `knowledge/<contract_version>/<corpus_id>/` key space — the built,
redacted store, **never the raw feed** (redaction runs at index time, so
pushing the feed would put pre-redaction bytes on a remote). The normative
reference is [SPEC-SYNC-KNOWLEDGE.md](../SPEC-SYNC-KNOWLEDGE.md); this section
is the practical tour.

```sh
# Producer side (usually a CI adapter job — see the reference workflow below):
cce knowledge index corpus.jsonl                      # ingest (redacts) → .cce/knowledge/
cce knowledge push [--corpus <id>] [--remote <url>] [--dry-run] [--force]
                                                      # artifact + `current` pointer + corpus.json,
                                                      # one commit; then retention

# Consumer side:
cce knowledge pull [--corpus <id>] [--latest | --snapshot <id>] [--force] [--remote <url>]
cce sync list [--json]              # corpora appear in a knowledge section
cce sync pull --all --into <dir> [--corpus <id>]   # code members AND the corpus, one command
cce sync verify --checksum-only     # covers the pulled knowledge store too
```

- **Identity.** `corpus_id` is an adapter-chosen stable slug (validated like a
  `repo_id`), resolved from `--corpus` or `knowledge.sync.corpus_id` — never
  derived: knowledge has no git origin to normalize.
- **Push** refuses a missing store, an unresolved/invalid corpus_id, and an
  embedding-less (pre-v2.6.1) store; it never blocks local work. Retention
  (`knowledge.sync.retention: keep-last-<n>`) prunes the oldest snapshots after
  the push — the snapshot named by `current` is never pruned, and a prune
  failure only warns.
- **The shrink guard (#90).** Push replaces the corpus's current snapshot
  wholesale, so before publishing it diffs the outgoing record-id set against
  the remote's current snapshot (fetched and checksum-verified with the pull
  machinery). A push that would **drop** record ids live on the remote — e.g. a
  local store rebuilt from only one of a corpus's feed sources (feedA of
  feedA+feedB) — prints the diff (record counts plus sorted `added` / `removed`
  / `changed` id lists) and refuses without `--force`. `changed` reflects a
  record's **rendered content** (title + body, compared byte-for-byte);
  facet-only edits (state/labels/url/updated_at) do not render into chunk
  content and do not register. Adds-only, changed-only, and unchanged pushes
  proceed exactly as quietly as before; a first publish (no remote `current`
  pointer) has nothing to diff and proceeds silently. If the pointer exists but
  its snapshot cannot be fetched or verified, the push refuses rather than
  silently replacing what it cannot read — `--force` is the only bypass (it
  skips the diff entirely). The guard reads remote state before publishing, so
  two simultaneous builders can both pass it — it is a guard, not a
  transaction; a push that loses the ref race re-applies and republishes
  without re-running the guard, so a racing competitor's additions can be
  unpublished without warning. It is also client-side: older cce versions push
  without it.
- **`--dry-run`** computes and prints the same diff, then exits 0 **without
  pushing anything** (no artifact, no pointer move, no retention) — the
  blast-radius preview. Against a corpus with no remote pointer it reports that
  the push would be the first publish. A CI job that builds from a subset of a
  corpus's sources should pass `--dry-run` first or be the corpus's sole
  publisher (see [`docs/ci/cce-knowledge-sync.yml`](ci/cce-knowledge-sync.yml)).
- **Pull** verifies the artifact checksum (a mismatch fails loudly, naming the
  key), installs into `.cce/knowledge/` **byte-identical to a local ingest**
  (so retrieval needs zero changes), and records a sync marker with the
  installed bytes' SHA-256. Pulling a *different* corpus than the marker
  records refuses without `--force` — one active corpus per root; a newer
  snapshot of the same corpus supersedes silently, exactly like a local
  re-ingest.

### The consumer flow — a corpus with no adapter and no source

A consumer with only git read access to the cache gets the whole thing in one
command:

```sh
cce sync pull --all --into ctx/          # code members + the corpus
cce mcp --workspace --dir ctx/           # context_search source: knowledge|both just works
```

`pull --all` installs the corpus at the **workspace root** (`ctx/.cce/knowledge/`,
where the MCP server loads knowledge from). Selection: `--corpus <id>` wins; a
cache carrying exactly one corpus installs it; with several and no flag the run
warns and skips knowledge, naming the ids — it never fails the member pulls.
Refresh is idempotent: an unmoved remote `current` reports `up-to-date` and
fetches nothing; a moved one refreshes exactly the corpus.

### Freshness — two signals, surfaced everywhere

- **How old is the data?** `data_as_of` — the maximum `updated_at` across the
  corpus. Deterministic, inside the artifact, computable from any installed
  store.
- **How recently was it published?** `pushed_at` — deliberately *outside* the
  artifact (it would break reproducibility), carried in the published
  `corpus.json` and rewritten on every push.

Both show up in `cce sync list` (and its `--json` `knowledge` array), and MCP
`index_status` gains a knowledge block — corpus, snapshot, records/chunks,
`data as-of`, plus best-effort `remote current` / `behind remote` lines that
follow the same offline-safe rules as the code freshness lines (see
[mcp.md](mcp.md)).

### Trust — stated honestly

Code artifacts are rebuild-verifiable: anyone with the source can check
`artifact == build(sha)`. **No such analogue exists for knowledge** — the
puller lacks the source feed, so a knowledge corpus is *not
rebuild-verifiable by consumers*, and nothing in the tooling implies
verify-parity with code artifacts. The posture: **trust the pusher** (the
canonical pusher is a CI adapter job), the **git host's ACL is the gate**,
and **content-address integrity** is checked on every pull, with
`verify --checksum-only` detecting post-install corruption offline. Pusher-side
determinism is an *audit path for feed-holders* (re-export, compare checksums),
not a consumer verification. Detached signatures are a deferred, additive
upgrade.

### The ingestion reference — a builder job, never a serving process

The production shape is a scheduled adapter run: CI cron fetches from the
source tool, emits `cce.knowledge/v1` NDJSON, runs `cce knowledge index`
(which redacts), and runs `cce knowledge push`. Nothing serves knowledge at
runtime; consumers pull from git like every other artifact. A ready-to-copy
workflow ships as [`docs/ci/cce-knowledge-sync.yml`](ci/cce-knowledge-sync.yml)
— note its two secrets have disjoint scopes (source-tool READ vs cache WRITE),
and the raw feed is ephemeral builder input: never committed, uploaded, or
cached.

Configured via `.cce/config` (all keys optional; absent ⇒ knowledge sync off,
pure local knowledge exactly as before):

```yaml
knowledge:
  sync:
    corpus_id: internal-tickets   # required to push (or pass --corpus)
    remote: null                  # per-corpus override; default = sync.remote
    retention: keep-last-10       # all | keep-last-<n>; default all
```

The `remote:` override exists because a corpus (say, internal tickets) may
have a different audience than the code it annotates: compartmentalization
stays git's job — one cache repo per access boundary.

## The plugin / adapter strategy

CCE ships the chunker + the `cce.knowledge/v1` contract + the ingest. **Real
extractors (GitHub, Jira, Linear, Notion…) are external** — anything that emits the
contract. There is **no ticket-system code in the engine**. Curation (dropping
closed-`not_planned`/`wontfix` or low-signal bodies, scrubbing org-specific PII) is
the adapter's job; CCE indexes what it is given and applies retrieval-side guards in
Phase B. If a line names a specific org/project, it is private config, not the
generic adapter.

## Honest framing

Knowledge helps **correctness** on "why" questions — the intent and constraints
behind the code. It favours **precision over volume** (a loosely-related or stale
record should not surface), and it carries **snapshot + provenance** so freshness is
always judgeable. It is not a bigger haystack; it is the *why* the code cannot state.
