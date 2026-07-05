# CCE MCP — Specification (v2.4.0)

An evolution track: an MCP server so agents (Claude Code) use CCE as a native tool, plus `cce init` for plug-and-play. Additive; single-repo behaviour and `conformance.json` untouched. Sync-aware (soft dependency). Built the same way as prior tracks: fresh agent per repo, test-first, identical cross-language tool contract.

## Summary

Add **CCE MCP** — a Model Context Protocol server so an agent (Claude Code, Cursor, …) calls CCE as a **first-class tool it auto-invokes**, instead of us hoping it shells out to `cce search`. `cce init` wires the editor up so it's plug-and-play. This closes the one gap between our clean-room CCE and the original Python implementation: the agent integration.

It directly answers two user questions:
1. **"How do I ensure my agent uses CCE?"** → a real MCP tool + a `CLAUDE.md` block that steers the model to prefer it over Read/Grep.
2. **"How do I know it used it?"** → every search is a visible tool call **and** is logged to `.cce/metrics.jsonl`, so `cce dashboard` shows the agent's queries and token savings.

This is an **evolution track (target v2.4.0)**, additive; the CLI and single-repo `conformance.json` are untouched. Built the same way as the other tracks (test-first, both engines implement an identical tool contract). The Ruby and Rust repos must ship the **same tool names, input schemas, and output structure** so an agent gets identical tools regardless of engine.

---

## The server: `cce mcp`

- Speaks **MCP over stdio, JSON-RPC 2.0** on stdin/stdout. Handshake: handle `initialize` → respond with `protocolVersion`, `capabilities: { tools: {} }`, `serverInfo: { name: "cce", version }`; accept `notifications/initialized`; implement `tools/list`, `tools/call`, `ping`. Pin the current MCP protocol version (check the MCP spec at build time).
- Resolves the store exactly like the CLI: `--dir` / `--store` / cwd; `--workspace` for ecosystems (SPEC-V2.2).
- **Read-only** — loads the index, never mutates source or the store.
- **Offline** — no network (unless the index was built with the optional Ollama embedder, in which case query embedding hits localhost Ollama, same as the CLI).
- **Missing/empty index:** tools still respond; `context_search` returns a clear *"index not built — run `cce index` (or `cce index --workspace`)"* message instead of erroring; `index_status` reports "not indexed."
- **Secret-safe by construction:** it only ever returns what's already in the store, which is redacted at index time (v2.1). Nothing new to scrub.

## CCE MCP × CCE Sync (freshness for the agent) — important

The MCP server is the biggest beneficiary of **CCE Sync** (v2.3): Sync is what keeps the agent's context **always fresh without the agent — or the developer — paying local indexing cost**. The two must compose:

1. **Warm/refresh on startup (auto-pull):** when `cce mcp` starts, if a sync remote is configured and `sync.auto_pull` is on, it does a **best-effort `sync pull --latest`** (offline-safe) to bring the local index up to the canonical CI-built `main@sha` before serving. Offline / no remote → serve whatever is local. This never blocks the server or errors — offline-first is preserved.
2. **`cce init --remote <sync-url>` is the plug-and-play flow:** `cce init --remote <sync-repo>` → **pulls** the CI-built index (seconds, not a full local re-index) → writes `.mcp.json` + `CLAUDE.md` → restart the editor → the agent immediately searches **fresh, team-shared context**. Without `--remote` it behaves as today (local index).
3. **Freshness is observable:** `index_status` reports the index's **source (local vs pulled), its `sha`, and whether it is behind the remote's latest** — so "how current is my context" is answerable, and a `sync pull` refreshes it.
4. **Branch overlay (when Sync's overlay lands):** searches reflect the pulled `main@sha` **plus** the developer's local working diff — the agent sees both team-canonical code and your WIP.
5. **Soft dependency — MCP must NOT hard-require Sync.** If the Sync capability isn't present/built, or no remote is configured, MCP uses the local index exactly as specified elsewhere. Gate all sync-pull behaviour behind capability + config detection, so MCP can ship even if Sync slips, and a failed/absent Sync never degrades MCP below "use the local index."

**Sequencing:** build MCP (v2.4) **after** CCE Sync (v2.3) so the auto-pull path is real; if built concurrently, ship MCP with the sync hooks behind the capability check above.

Config: `sync.auto_pull` (bool), reusing the Sync `sync.*` keys.

## Tools (exact contract — identical in both repos)

### 1. `context_search` — the headline
**Description** (write it to steer the agent; mirror this intent):
> "PREFERRED tool for any question about THIS project's code. Use INSTEAD OF reading or grepping files to locate functions, understand behaviour, or answer 'where is X / how does Y work'. Returns the most relevant code chunks (file:line + kind) from a hybrid vector + BM25 index, so you don't pay tokens for whole files. Reserve file reads for opening a specific path this tool points you to."

**Input schema:**
```json
{
  "type": "object",
  "properties": {
    "query":    { "type": "string" },
    "top_k":    { "type": "integer", "default": 8 },
    "package":  { "type": "string", "description": "scope to one workspace member (optional)" },
    "no_graph": { "type": "boolean", "default": false },
    "max_tokens": { "type": "integer", "description": "cap the returned context (optional)" }
  },
  "required": ["query"]
}
```
**Output:** MCP `content` = a text block, one line per result:
`#. [score] <package · >file_path:start-end (chunk_type/kind)` followed by the chunk body (trim/compress to `max_tokens` if given). Include a `query_id` line so the agent can call `record_feedback`. **Records a `search` event to `.cce/metrics.jsonl`** (identical to the CLI path) so the dashboard sees agent usage.

### 2. `index_status`
**Description:** "Check whether this project is indexed and how fresh it is."
**Input:** `{}` (workspace auto-detected). **Output:** chunk count, file count, per-language/per-kind, store path, last-indexed time; for a workspace, per-member counts + the dependency edges.

### 3. `record_feedback`
**Description:** "Record whether a prior `context_search` result was helpful, to improve the quality signal on the dashboard."
**Input:** `{ "query_id": string (required), "helpful": boolean (required), "note": string (optional) }`. Appends a `feedback` event to `.cce/metrics.jsonl`.

_(Keep the tool set small and focused. Graph `related` / `expand_chunk` can be a fast-follow.)_

## `cce init` — plug-and-play editor config

`cce init [<dir>] [--agent claude] [--remote <sync-url>] [--force]`:
- Ensures the project has an index: if `--remote` is given (or a sync remote is already configured), **`cce sync pull --latest`** to fetch the CI-built index; otherwise run `cce index` (or `cce index --workspace` if a `workspace.yml` exists / members are detected). See "CCE MCP × CCE Sync" above.
- Writes/merges `<dir>/.mcp.json` with a `cce` server entry, idempotent:
  ```json
  { "mcpServers": { "cce": { "command": "cce", "args": ["mcp", "--dir", "."] } } }
  ```
  (use `--workspace` instead of `--dir .` when the project is a workspace).
- Writes/merges a `CLAUDE.md` block (bounded by a stable marker comment so it can be updated/removed) instructing the agent to prefer `context_search` over Read/Grep.
- Prints next steps ("restart your editor").
- **v1 targets Claude Code.** Cursor (`.cursor/mcp.json`), VS Code (`.vscode/mcp.json`), and Codex (`~/.codex/config.toml`) are documented as a fast-follow behind `--agent`.

## Observability — "how do I know it used it"

- Every `context_search` logs a `search` event to `.cce/metrics.jsonl` (same as the CLI), so `cce dashboard` shows the agent's queries, counts, and token savings — proof of use *and* value.
- The MCP tool calls are also visible in the editor's tool-call log.
- `record_feedback` closes the quality loop into the dashboard's retrieval-quality north-star.

## Testing (hermetic — no editor, no network)

Drive the server by piping JSON-RPC messages to its stdin and asserting stdout:
- `initialize` → capabilities + `serverInfo`.
- `tools/list` → exactly `context_search`, `index_status`, `record_feedback` with the schemas above.
- `tools/call context_search` over a fixture index → expected ranked results **and** a `search` event appended to `metrics.jsonl`; `package` scoping works on a workspace fixture.
- `index_status`, `record_feedback` (event written), and the missing-index → friendly-message path.
- `cce init` writes a valid `.mcp.json` + CLAUDE.md block and is idempotent (re-run adds no duplicates).
- **Sync integration (behind capability/config):** with a local bare git sync remote and `sync.auto_pull` on, starting `cce mcp` warms the local index via pull; `index_status` reports the pulled `sha` and "behind remote" state; with no remote/offline, the server serves the local index and never errors.

## Documentation (first-class, VERIFIED — cold-start gate)

- README "Use it with Claude Code (MCP)" section: `cce init` → restart editor → ask a question → confirm via `cce dashboard`, with real captured output.
- Install/setup verified from a cold start (per the project's docs bar).
- An explicit "how to confirm the agent used it" note (dashboard + tool-call log).

## Acceptance criteria

- [ ] `cce mcp` speaks MCP over stdio; `tools/list` returns `context_search`, `index_status`, `record_feedback` with the exact schemas.
- [ ] `context_search` returns ranked chunks, logs a metrics event, supports `--workspace` `package` scoping, and handles a missing index gracefully.
- [ ] `cce init` writes a valid, idempotent `.mcp.json` + `CLAUDE.md` block for Claude Code.
- [ ] **Sync-aware:** `cce init --remote` pulls the CI-built index; `cce mcp` auto-pulls the latest on startup when configured (offline-safe, soft dependency); `index_status` reports source/`sha`/behind-remote. MCP still works fully with no Sync.
- [ ] Cross-language parity: identical tool names/schemas/output in cce-ruby and cce-rust.
- [ ] Hermetic tests + a verified cold-start docs pass; single-repo `conformance.json` unchanged; all gates green; version bumped to 2.4.0.

## Notes

- Sequencing: naturally follows **CCE Sync (v2.3, in progress)**; independent of it.
- Sibling implementation (same tool contract, built independently): davidslv/cce-ruby
