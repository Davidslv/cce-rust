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
> [Searching knowledge](#searching-knowledge-m4--the-source-blend) below). Sync of
> the knowledge corpus and the reference adapter (M5, Phase C) are still future work.

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
