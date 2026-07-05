# CCE MCP — use CCE as a native agent tool (v2.4)

CCE MCP turns the index into something an **agent** invokes directly. `cce mcp` is a
[Model Context Protocol](https://modelcontextprotocol.io) server over stdio; `cce
init` wires an editor (Claude Code) up so it is plug-and-play. This closes the last
gap between the clean-room CCE and the original Python implementation: the agent
integration.

It answers two questions directly:

1. **"How do I ensure my agent uses CCE?"** → a real MCP tool (`context_search`)
   plus a `CLAUDE.md` block that steers the model to prefer it over Read/Grep.
2. **"How do I know it used it?"** → every search is a visible tool call **and** is
   logged to `.cce/metrics.jsonl`, so `cce dashboard` shows the agent's queries and
   token savings.

CCE MCP is **additive**: the CLI and the single-repo `conformance.json` are
untouched, and it is **read-only** and **offline** (no network unless the index was
built with the optional Ollama embedder, exactly as the CLI).

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
marker-bounded block into `CLAUDE.md`:

```markdown
<!-- BEGIN CCE MCP -->
## Code Context Engine (CCE)

This project is indexed by CCE, exposed as MCP tools. Prefer them over reading or grepping files.

- **PREFER `context_search`** to locate code, understand behaviour, or answer "where is X / how does Y work". …
- Reserve file reads for opening a specific path `context_search` points you to.
- Use `index_status` to check how fresh the index is, and `record_feedback` to rate a result.
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

## The three tools

The tool **names, input schemas, and output structure are identical** in the Ruby
and Rust engines — that cross-language parity is the contract, so an agent gets the
same tools whichever engine serves.

### 1. `context_search` — the headline

> PREFERRED tool for any question about THIS project's code. Use INSTEAD OF reading
> or grepping files to locate functions, understand behaviour, or answer 'where is X
> / how does Y work'. Returns the most relevant code chunks (file:line + kind) from a
> hybrid vector + BM25 index, so you don't pay tokens for whole files. Reserve file
> reads for opening a specific path this tool points you to.

**Input schema:**

```json
{
  "type": "object",
  "properties": {
    "query":      { "type": "string" },
    "top_k":      { "type": "integer", "default": 8 },
    "package":    { "type": "string", "description": "scope to one workspace member (optional)" },
    "no_graph":   { "type": "boolean", "default": false },
    "max_tokens": { "type": "integer", "description": "cap the returned context (optional)" }
  },
  "required": ["query"]
}
```

**Output** is a text block — one header line per result followed by the chunk body:

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

In a workspace the header carries the member package: `1. [score] billing ·
lib/billing.rb:2-4 (method/method)`. `max_tokens` trims the returned bodies to a
budget. Each call **records a `search` event** to `.cce/metrics.jsonl` (identical to
the CLI path), and the printed `query_id` is what `record_feedback` targets.

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
- The tool set is deliberately small and focused. Graph `related` / `expand_chunk`
  tools are a possible fast-follow.
- The design spec is [`SPEC-MCP.md`](../SPEC-MCP.md); the cold-start verification
  transcript is in [`docs/VERIFIED.md`](VERIFIED.md).
