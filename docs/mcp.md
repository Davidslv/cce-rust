# CCE MCP — use CCE as a native agent tool (v2.4 · nine tools since v2.5)

CCE MCP turns the index into something an **agent** invokes directly. `cce mcp` is a
[Model Context Protocol](https://modelcontextprotocol.io) server over stdio; `cce
init` wires an editor (Claude Code) up so it is plug-and-play. This closes the last
gap between the clean-room CCE and the original Python implementation: the agent
integration.

It answers two questions directly:

1. **"How do I ensure my agent uses CCE?"** → real MCP tools (headlined by
   `context_search`) plus a `CLAUDE.md` block that steers the model to prefer them
   over Read/Grep.
2. **"How do I know it used it?"** → every search is a visible tool call **and** is
   logged to `.cce/metrics.jsonl`, so `cce dashboard` shows the agent's queries and
   token savings.

Since **v2.5** the server exposes **nine tools** (in a fixed order): the three v2.4
tools plus the [Savings Layers](savings.md) tools — progressive disclosure
(`expand_chunk`, `related_context`), output compression (`set_output_compression`),
memory (`record_decision`, `session_recall`), and turn summarization
(`summarize_context`). `context_search` now serves **compact** chunks by default and
each result carries a `chunk_id` you expand on demand.

CCE MCP is **additive**: the CLI and the single-repo `conformance.json` are
untouched, and it is **read-only** and **offline** (no network unless the index was
built with the optional Ollama embedder, exactly as the CLI). Everything persisted
(memory) passes through the v2.1 redactor first, so it stays secret-safe.

---

## Quick start

```bash
cce init .          # ensure an index, write .mcp.json + a CLAUDE.md block
# restart Claude Code so it loads .mcp.json
# ask: "where is the password hashed?"  → the agent calls context_search
cce dashboard       # confirm the agent used it (the search is on the dashboard)
```

`cce init` writes a minimal, idempotent `.mcp.json`:

```json
{ "mcpServers": { "cce": { "command": "cce", "args": ["mcp", "--dir", "."] } } }
```

(a workspace gets `"args": ["mcp", "--workspace"]` instead) and merges a
marker-bounded block into `CLAUDE.md`. Since v2.5 the block also carries the
leveled **Output compression** rules (Savings Layer 4, default `standard`):

```markdown
<!-- BEGIN CCE MCP -->
## Code Context Engine (CCE)

This project is indexed by CCE, exposed as MCP tools. Prefer them over reading or grepping files.

- **PREFER `context_search`** to locate code, understand behaviour, or answer "where is X / how does Y work". …
- Reserve file reads for opening a specific path `context_search` points you to.
- Use `index_status` to check how fresh the index is, and `record_feedback` to rate a result.

### Output compression

Answer in the fewest words that are correct; when editing code show ONLY the changed lines (a minimal diff), never reprint whole files; no preamble or postamble.
<!-- END CCE MCP -->
```

Re-running `cce init` is safe: the `cce` server entry and the block are **merged**,
never duplicated. Other MCP servers already in `.mcp.json` and other content in
`CLAUDE.md` are preserved.

`cce init` flags:

| Flag | Meaning |
|---|---|
| `<dir>` | Project directory to initialise (default: current directory). |
| `--agent claude` | Target agent. v1 targets Claude Code; Cursor / VS Code / Codex are a documented fast-follow. |
| `--remote <sync-url>` | Pull the CI-built index from a CCE Sync remote instead of indexing locally (see [Freshness](#freshness-via-cce-sync)). |
| `--force` | Force the index refresh (a `--force` sync pull past a sha mismatch). |

---

## The server: `cce mcp`

`cce mcp` speaks **MCP over stdio, JSON-RPC 2.0** on stdin/stdout (newline-delimited
messages). It pins protocol version **`2025-06-18`**.

- **Handshake:** `initialize` → `{ protocolVersion, capabilities: { tools: {} },
  serverInfo: { name: "cce", version } }`; accepts `notifications/initialized`.
- **Methods:** `tools/list`, `tools/call`, `ping`.
- **Store resolution** is exactly the CLI's: `--dir` / `--store` / cwd, and
  `--workspace` for ecosystems (SPEC-V2.2).
- **Missing/empty index:** tools still respond — `context_search` returns a clear
  *"not indexed — run `cce index`"* message rather than erroring; `index_status`
  reports "not indexed."
- **Secret-safe by construction:** it only returns what is already in the store,
  which was redacted at index time (v2.1). Nothing new to scrub.

You rarely run `cce mcp` by hand — the editor spawns it from `.mcp.json`. To drive
it manually (the shape the tests use):

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"where is the password hashed","top_k":3}}}' \
  | cce mcp --dir .
```

---

## The nine tools

The tool **names, input schemas, and output structure are identical** in the Ruby
and Rust engines — that cross-language parity is the contract, so an agent gets the
same tools whichever engine serves. `tools/list` returns them in this **fixed
order**:

| # | Tool | Layer | What it does |
|---|---|---|---|
| 1 | [`context_search`](#1-context_search--the-headline) | L1/L2 | Ranked, **compact** code chunks for a query; each carries a `chunk_id`. |
| 2 | [`index_status`](#2-index_status) | — | Is the project indexed, how fresh, and the sync source/sha. |
| 3 | [`record_feedback`](#3-record_feedback) | — | Rate a prior result to feed the dashboard's quality signal. |
| 4 | [`expand_chunk`](#4-expand_chunk--l7-read-the-full-detail) | L7 | Read the **full** body / file / neighbours of a returned chunk. |
| 5 | [`related_context`](#5-related_context--l7-widen-via-the-import-graph) | L7 | Import-graph neighbours (imports **and** consumers) of a chunk. |
| 6 | [`set_output_compression`](#6-set_output_compression--l4-dial-answer-terseness) | L4 | Dial THIS session's own answer terseness. |
| 7 | [`record_decision`](#7-record_decision--l5-remember-a-validated-decision) | L5 | Remember a **validated** decision (secret-scrubbed, local). |
| 8 | [`session_recall`](#8-session_recall--l5-recall-remembered-decisions) | L5 | Precision-filtered search over remembered decisions. |
| 9 | [`summarize_context`](#9-summarize_context--l6-compress-the-session) | L6 | Deterministic structured digest of the session so far. |

**The core workflow is find → expand → widen:** `context_search` finds compact
chunks; `expand_chunk` reads a full body when you actually need it;
`related_context` widens across the import graph. The tool descriptions carry an
**expand-first rule** — once you have a `chunk_id`, expand it; do **not** re-issue
`context_search` for a target you already found.

### 1. `context_search` — the headline

> Search THIS project's code by meaning, across files. Use it FIRST for any
> cross-file question — "where is X", "how does Y work", "what calls Z" … Results
> are COMPACT and each carries a `chunk_id`; to read a full body call
> `expand_chunk(chunk_id)` — do NOT re-issue `context_search` for a target you
> already found. Widen to import-graph neighbours with `related_context(chunk_id)`.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query":      { "type": "string" },
    "top_k":      { "type": "integer", "default": 8 },
    "package":    { "type": "string", "description": "scope to one workspace member (optional)" },
    "no_graph":   { "type": "boolean", "default": false },
    "max_tokens": { "type": "integer", "description": "cap the returned context (optional)" },
    "detail":     { "type": "string", "enum": ["signature", "compact", "full"], "description": "chunk compression level (optional; default from config, usually compact)" }
  },
  "required": ["query"]
}
```

`detail` (Savings Layer 2) picks the compression level — `signature`, `compact`
(default), or `full`; absent ⇒ the project's `retrieval.detail` config, which
defaults to `compact` (see [`savings.md`](savings.md)). **Output** is a text block:
one header line per result — `#. [score] file:start-end (chunk_type/kind)
#chunk_id` — followed by the chunk body served at that detail, then the
expand-on-demand hint and the `query_id`:

```
 1. [0.864806] auth.py:6-13 (function/function_definition) #04578fe98cec59bd
def hash_password(password: str, salt: str) -> str:
"""Hash a password with a salt using SHA-256.
digest = hashlib.sha256((salt + password).encode()).hexdigest()
… (+5 lines)

 2. [0.863693] auth.py:16-18 (function/function_definition) #ae61011b82d7a777
def verify_password(password: str, salt: str, expected: str) -> bool:
"""Return True when the password hashes to the expected digest."""
return hash_password(password, salt) == expected

Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; related_context(chunk_id) for import-graph neighbours.
query_id: 8c74824599a7
Rate this with record_feedback (query_id="8c74824599a7", helpful=true|false).
```

In a workspace the header carries the member package: `1. [score] billing ·
lib/billing.rb:2-4 (method/method) #…`. `max_tokens` trims the returned bodies to a
budget. Each call **records a `search` event** to `.cce/metrics.jsonl` (identical to
the CLI path, carrying the `retrieval` + `chunk_compression` savings buckets), and
the printed `query_id` is what `record_feedback` targets.

### 2. `index_status`

> Check whether this project is indexed and how fresh it is.

Input `{}`. Reports chunk/file counts, per-language and per-kind breakdowns, the
store path, and the sync **freshness**: the index source (local vs pulled), its sha,
and whether it is behind the remote. In a workspace it reports per-member counts and
the cross-member dependency edges.

### 3. `record_feedback`

> Record whether a prior `context_search` result was helpful, to improve the quality
> signal on the dashboard.

**Input:** `{ "query_id": string (required), "helpful": boolean (required), "note":
string (optional) }`. Appends a `feedback` event to `.cce/metrics.jsonl`, closing the
quality loop into the dashboard's retrieval-quality north-star.

### 4. `expand_chunk` — L7, read the full detail

> Read the FULL detail of a chunk `context_search` already returned, by its
> `chunk_id`. … do NOT re-run `context_search` for a chunk you already have.

**Input:** `{ "chunk_id": string (required), "scope": "body" | "file" | "neighbors"
(default "body") }`.

- `scope=body` recovers the **exact full body** — it round-trips `detail:full`, so
  compact-by-default never loses information.
- `scope=file` returns every chunk in the same file.
- `scope=neighbors` returns chunks from import-graph-related files.

A stale or unknown `chunk_id` (e.g. after a re-index) returns a short, actionable
message telling you to re-run `context_search` — never a crash.

```
$ expand_chunk("04578fe98cec59bd", scope=body)
def hash_password(password: str, salt: str) -> str:
    """Hash a password with a salt using SHA-256.

    This is the single place passwords are hashed; callers never
    hash inline. Returns the hex digest.
    """
    digest = hashlib.sha256((salt + password).encode()).hexdigest()
    return digest
```

### 5. `related_context` — L7, widen via the import graph

> Given a `chunk_id` from `context_search`, return the chunks connected to it
> through the import graph — both what it imports AND its consumers (reverse edges) —
> as compact entries.

**Input:** `{ "chunk_id": string (required), "top_k": integer (default 8) }`. Use it
to trace how a symbol is used or what it depends on across files, instead of
pre-loading whole neighbourhoods; expand any result with `expand_chunk`. Compact
entries carry `chunk_id`s of their own.

### 6. `set_output_compression` — L4, dial answer terseness

> Set how terse THIS session's answers should be — the output-compression level the
> agent applies to its OWN replies.

**Input:** `{ "level": "off" | "lite" | "standard" | "max" (required) }`. It sets an
in-memory **session preference** only — it does **not** rewrite `CLAUDE.md` and
resets when the server restarts. Dial down (`max`) for terse diffs, or up (`off`)
for full explanations, mid-session.

```
$ set_output_compression("max")
Output compression is now `max` for this session (in-memory; CLAUDE.md unchanged).
```

### 7. `record_decision` — L5, remember a validated decision

> Remember a VALIDATED decision for future sessions … Do NOT record raw model
> output, guesses, or unverified answers — memory that replays a bad answer POLLUTES
> future context.

**Input:** `{ "text": string (required), "tags": string[] (optional), "area": string
(optional) }`. The text is **secret-redacted before storage**, content-addressed,
and de-duplicated (recording the same decision twice is a no-op returning the same
id). The store is the local `.cce/memory.jsonl` — **never pushed by Sync**. Set
`memory.enabled=false` in `.cce/config` to make the memory tools a no-op.

```
$ record_decision("Passwords are hashed only in auth.hash_password …", tags=["security","auth"], area="auth")
Recorded decision #46f3ebd005279048. Retrieve it later with session_recall.
```

### 8. `session_recall` — L5, recall remembered decisions

> Search THIS project's remembered decisions … Hybrid vector + BM25 search,
> PRECISION-FILTERED: it returns only high-confidence matches (a small top_k) …
> which you CHOOSE to use — it is never an auto-injected blob.

**Input:** `{ "query": string (required), "top_k": integer (default 5) }`. Returns
nothing when there is no confident match (score ≥ 0.30 and a shared query token) —
that is normal and correct; proceed without it rather than forcing a weak memory
into context.

```
$ session_recall("how are passwords hashed")
Recalled 1 of 1 remembered decision(s):

 1. [0.851650] #46f3ebd005279048 area=auth tags=security,auth
Passwords are hashed only in auth.hash_password (SHA-256 + salt); never hash inline.

These are validated decisions you MAY reuse — apply only what fits; they are not auto-injected.
```

### 9. `summarize_context` — L6, compress the session

> Get a compact, deterministic digest of what THIS session has done so far … It is a
> STRUCTURED digest built from the server's per-session ledger, NOT an LLM-written
> summary: the same sequence of tool calls always yields the same bytes.

**Input:** `{ "scope": "all" | "files" | "queries" | "decisions" (default "all") }`.
The server keeps a wall-clock-free, order-preserving ledger of the session's tool
calls; the digest is a pure function of it — files and chunks touched, queries run,
and decisions recorded, deduped, sorted, and bounded with a `… (+N more)` marker.

```
$ summarize_context()
CCE session digest
files (2):
- auth.py
- payments.py
chunks (3):
- 04578fe98cec59bd
- 873decd4dc46ba36
- ae61011b82d7a777
queries (1):
- where is the password hashed
decisions (1):
- #46f3ebd005279048 Passwords are hashed only in auth.hash_password (SHA-256 + s…
```

---

## Workspaces

Run `cce mcp --workspace --dir <root>` (this is what `cce init` writes when a
`.cce/workspace.yml` is present). `context_search` then federates over the workspace
members exactly as `cce search --workspace` does: results are tagged by package, the
`package` argument scopes to one member, and cross-member dependency edges expand the
search. `index_status` reports per-member counts and the dependency graph.

Metrics for a workspace session land in the workspace-root `.cce/metrics.jsonl`, so
`cce dashboard --workspace` sees agent usage across the ecosystem.

---

## Freshness via CCE Sync

CCE MCP is the biggest beneficiary of [CCE Sync](sync.md): Sync keeps the agent's
context fresh **without the agent — or the developer — paying local indexing cost**.
The two compose, as a **soft dependency**:

- **Plug-and-play team context:** `cce init --remote <cache-repo>` **pulls** the
  CI-built index (seconds, not a full re-index), writes `.mcp.json` + `CLAUDE.md`,
  and the agent immediately searches fresh, team-shared context.
- **Warm on startup:** if a sync remote is configured **and** `sync.auto_pull` is on,
  `cce mcp` does a best-effort `sync pull --latest` before serving, bringing the
  local index up to the canonical `main@sha`. This never blocks or errors — offline
  or no-remote just serves the local index.
- **Observable:** `index_status` reports the source (local vs pulled), the sha, and
  whether the local index is behind the remote's latest.

**MCP does not hard-require Sync.** With no remote configured, every tool works fully
on the local index, offline. A failed or absent Sync never degrades MCP below "use
the local index."

Config lives in `.cce/config` (see [`docs/sync.md`](sync.md)); the relevant key is
`sync.auto_pull` (bool, default off).

---

## How to confirm the agent used it

Two independent signals:

1. **Tool-call log** — Claude Code shows each `context_search` call in its tool-call
   log, with the arguments and the returned chunks.
2. **Dashboard** — every `context_search` is a `search` event on `cce dashboard`
   (queries, counts, tokens saved, latency). This is proof of *use* and of *value*;
   `record_feedback` adds the quality signal.

```bash
cce dashboard            # open the loopback, read-only dashboard
# or, non-interactively:
cat .cce/metrics.jsonl   # one JSON line per search / feedback event
```

---

## Notes

- Read-only and offline-first: `cce mcp` never mutates source or the store, and makes
  no network calls unless the index used the optional Ollama embedder (localhost).
  The one thing it writes is memory (`record_decision`) — local-only,
  secret-scrubbed, and never pushed by Sync.
- The nine tools are the full [Savings Layers](savings.md) surface; the token deltas
  they produce roll up into the seven-bucket ledger you read with `cce savings`.
- The design specs are [`SPEC-MCP.md`](../SPEC-MCP.md) (the server + first three
  tools) and [`SPEC-V2.5-SAVINGS.md`](../SPEC-V2.5-SAVINGS.md) (the six v2.5 tools);
  the cold-start verification transcript is in [`docs/VERIFIED.md`](VERIFIED.md).
