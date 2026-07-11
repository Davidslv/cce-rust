//! # mcp::tools — the nine CCE MCP tools (SPEC-MCP §"Tools" + SPEC-V2.5 §6/§7)
//!
//! **Why this file exists:** The tool contract — names, input schemas, and output
//! shape — is the cross-language interface an agent binds to. It MUST be byte-for-
//! byte the same intent in cce-rust and cce-ruby so an agent gets identical tools
//! whichever engine serves. This module owns that contract and each tool's
//! read-only execution over the local index.
//!
//! **What it is / does:** Declares the nine tools and their handlers. The three v2.4
//! tools — `context_search` (now serving L2-compressed bodies at a `detail` level +
//! each result's `chunk_id`), `index_status`, `record_feedback` — plus the two
//! Layer-7 progressive-disclosure tools: `expand_chunk` (body/file/neighbors; body
//! recovers the exact full bytes) and `related_context` (import-graph neighbours,
//! imports AND consumers) — plus the Layer-4 `set_output_compression`, which switches
//! the running session's output-compression preference in memory (NOT CLAUDE.md) —
//! plus the two Layer-5 memory tools: `record_decision` (redact → dedupe → append a
//! VALIDATED decision to the local `.cce/memory.jsonl`) and `session_recall`
//! (precision-filtered hybrid search over that memory). `context_search` logs an
//! identical `search` metrics event carrying the retrieval + `chunk_compression`
//! savings buckets (SPEC-V2.5 §2/§3) — plus the Layer-6 `summarize_context`, which
//! renders the server's per-session ledger into a deterministic, bounded, STRUCTURED
//! digest (files/chunks touched, queries issued, decisions recorded) so the agent can
//! compress a long session's history instead of re-sending the raw transcript. The
//! context-touching tools (`context_search`, `expand_chunk`, `related_context`,
//! `record_decision`) each append an order-preserving, wall-clock-free entry to that
//! ledger as they run.
//!
//! **Responsibilities:**
//! - Own the nine tool schemas, output formatting, L2 serving, and `max_tokens`.
//! - Reuse `retriever`/`federation`/`compress`/`metrics`/`memory`/`session`/`sync` —
//!   never reimplement.
//! - Feed the per-session ledger (`session`) as tools run; the digest itself is a pure
//!   function computed there.
//! - Handle a missing/empty index, a stale chunk_id, or disabled memory with a
//!   friendly message.

use crate::chunker::{token_count, Chunk};
use crate::compress::{compress, DetailLevel};
use crate::config::{
    FooterMode, KnowledgeConfig, MemoryConfig, OutputLevel, RetrievalConfig, CHARS_PER_TOKEN,
};
use crate::grammar::FooterFacts;
use crate::embedder::{format6, score_key, Embedder, HashEmbedder, OllamaEmbedder};
use crate::federation::{
    federated_bm25_only_search, federated_search_over, load_member_stores, parse_scope,
    workspace_stats, CachedWorkspace, MemberStore,
};
use crate::knowledge::{same_document_sections, KnowledgeHit};
use crate::mcp::server::McpServer;
use crate::mcp::MCP_DEFAULT_TOP_K;
use crate::memory::{self, RecallHit};
use crate::metrics::{HexIdSource, MetricsWriter, SystemClock};
use crate::packs::Registry;
use crate::retriever::{bm25_only_search, build_search_record, search, SearchResult};
use crate::session::{short_label, SummaryScope};
use crate::store::{default_store_path, Index};
use crate::sync::commands::{freshness, IndexSource};
use crate::sync::knowledge_commands::knowledge_freshness;
use crate::workspace::{manifest_path, Manifest, WorkspaceGraph};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

/// The `context_search` description (SPEC-V2.5 §6 + SPEC-V2.5-TUNING §B): core
/// purpose, explicit trigger conditions from measured behaviour, the tool
/// relationships, and the expand-first rule that stops the re-search reflex.
/// Byte-pinned; mirrored verbatim in the Ruby engine.
const CONTEXT_SEARCH_DESC: &str = "Search THIS project's code by meaning, across files. Use it \
FIRST for any cross-file question — \"where is X\", \"how does Y work\", \"what calls Z\" — or \
whenever you cannot already name the exact file to open. Returns the most relevant code chunks \
(file:line + kind) from a hybrid vector + BM25 index, so you don't pay tokens for whole files. \
Skip it only when you already know the single file you need — reading that path directly is fine; \
cce does not win there. Results are COMPACT and each carries a `chunk_id`; to read a full body \
call `expand_chunk(chunk_id)` — do NOT re-issue `context_search` for a target you already found. \
Widen to import-graph neighbours with `related_context(chunk_id)`.";

/// The result of running a tool: the text block shown to the agent plus the MCP
/// `isError` flag. A missing/empty index is *not* an error — it is a normal result
/// carrying guidance — so `is_error` is reserved for a malformed tool call.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl ToolOutput {
    fn ok(text: impl Into<String>) -> Self {
        ToolOutput { text: text.into(), is_error: false }
    }
    fn err(text: impl Into<String>) -> Self {
        ToolOutput { text: text.into(), is_error: true }
    }

    /// Render as the MCP `tools/call` result: a single text content block.
    pub fn to_content(&self) -> Value {
        json!({
            "content": [ { "type": "text", "text": self.text } ],
            "isError": self.is_error,
        })
    }
}

/// The `expand_chunk` description (SPEC-V2.5 §6/§7 + SPEC-V2.5-TUNING §B): the
/// full-body reader for a chunk `context_search` already returned, carrying the same
/// expand-first rule so the agent expands rather than re-searches. Byte-pinned.
const EXPAND_CHUNK_DESC: &str = "Read the FULL detail of a chunk `context_search` already \
returned, by its `chunk_id`. `context_search` serves COMPACT chunks (a header + members, or a \
signature + first line); when you need the real body, call this — do NOT re-run `context_search` \
for a chunk you already have. scope=body recovers the exact full body; scope=file returns every \
chunk in the same file; scope=neighbors returns chunks from import-graph-related files. A stale \
or unknown `chunk_id` returns a short, actionable error you can retry from, never a crash.";

/// The `related_context` description (SPEC-V2.5 §6/§7 + SPEC-V2.5-TUNING §B): the
/// on-demand import-graph widener, stating its purpose and its place in the
/// find → expand → widen chain. Byte-pinned.
const RELATED_CONTEXT_DESC: &str = "Given a `chunk_id` from `context_search`, return the chunks \
connected to it through the import graph — both what it imports AND its consumers (reverse edges) \
— as compact entries. Use it to widen context on demand — trace how a symbol is used or what it \
depends on across files — instead of pre-loading whole neighbourhoods; expand any result with \
`expand_chunk(chunk_id)`. Pairs with `context_search` (find first) and `expand_chunk` (read the \
full body).";

/// The `set_output_compression` description (SPEC-V2.5 §6 + §2 Layer 4): its purpose
/// (dial THIS session's answer terseness) and when to use it, plus the explicit note
/// that it is a session preference, not a CLAUDE.md rewrite. Byte-pinned; mirrored
/// verbatim in the Ruby engine.
const SET_OUTPUT_COMPRESSION_DESC: &str = "Set how terse THIS session's answers should be — the \
output-compression level the agent applies to its OWN replies. Levels: `off` (no rules), `lite` \
(concise; drop filler/preamble/postamble), `standard` (fewest correct words; code as minimal \
diffs, never whole files; no preamble or postamble), `max` (standard + telegraphic prose; code \
as minimal diffs only). Use it to dial verbosity down when you want terse diffs, or up (`off`) \
when you want full explanations — mid-session, without editing CLAUDE.md. It sets a session \
preference only; it does not rewrite CLAUDE.md and resets when the server restarts.";

/// The `record_decision` description (SPEC-V2.5 §2 Layer 5, §6): remember ONE
/// validated decision, with the explicit anti-pollution warning that this is for
/// confirmed decisions only, never raw model output. Byte-pinned; mirrored verbatim
/// in the Ruby engine.
const RECORD_DECISION_DESC: &str = "Remember a VALIDATED decision for future sessions — an \
explicit, deliberate note you or the user have confirmed is correct (an architecture choice, a \
convention, a resolved trade-off), so it need not be re-derived later. The text is secret-redacted \
before storage, content-addressed, and de-duplicated: recording the same decision twice is a \
no-op that returns the same id. Do NOT record raw model output, guesses, or unverified answers — \
memory that replays a bad answer POLLUTES future context. Optional `tags` and an `area` help \
recall. Returns the decision's id; retrieve later with `session_recall`.";

/// The `session_recall` description (SPEC-V2.5 §2 Layer 5, §6): search remembered
/// decisions, precision-filtered, agent-chosen (not auto-injected). Byte-pinned.
const SESSION_RECALL_DESC: &str = "Search THIS project's remembered decisions (recorded with \
`record_decision`) for ones relevant to `query`, so you don't re-derive what was already settled. \
Hybrid vector + BM25 search, PRECISION-FILTERED: it returns only high-confidence matches (a small \
top_k) as compact entries with ids, which you CHOOSE to use — it is never an auto-injected blob. \
Returning nothing when there is no confident match is normal and correct; proceed without it \
rather than forcing a weak memory into context.";

/// The `summarize_context` description (SPEC-V2.5 §2 Layer 6, §6): compress THIS
/// session's history into a small, deterministic digest instead of re-sending the raw
/// transcript. States plainly that it is a STRUCTURED digest of the server's per-session
/// ledger — not an LLM prose summary — so the agent knows the same call sequence yields
/// the same bytes. Byte-pinned; mirrored verbatim in the Ruby engine.
const SUMMARIZE_CONTEXT_DESC: &str = "Get a compact, deterministic digest of what THIS session \
has done so far — the files and chunks you have touched, the queries you have run, and the \
decisions you have recorded — so you can compress a long session's history into a few lines \
instead of re-sending the raw transcript. It is a STRUCTURED digest built from the server's \
per-session ledger, NOT an LLM-written summary: the same sequence of tool calls always yields the \
same bytes. Optional `scope` narrows it: \"all\" (default), \"files\" (the files AND chunks \
touched), \"queries\", or \"decisions\". Long lists are bounded with a `… (+N more)` marker. An \
unknown `scope` returns a short, actionable error, never a crash.";

/// The nine tool definitions returned by `tools/list`, with the EXACT byte-pinned
/// schemas of SPEC-MCP §"Tools" + SPEC-V2.5 §6. The order is stable and fixed:
/// the three v2.4 tools first (context_search the headline), then the two Layer-7
/// progressive-disclosure tools, then the Layer-4 `set_output_compression`, then the
/// two Layer-5 memory tools (`record_decision`, `session_recall`), then the Layer-6
/// `summarize_context` appended last. Mirrored verbatim for cce-ruby's catch-up.
pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "context_search",
            "description": CONTEXT_SEARCH_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string" },
                    "top_k":      { "type": "integer", "default": 8 },
                    "package":    { "type": "string", "description": "scope to one workspace member (optional)" },
                    "no_graph":   { "type": "boolean", "default": false },
                    "max_tokens": { "type": "integer", "description": "cap the returned context (optional)" },
                    "detail":     { "type": "string", "enum": ["signature", "compact", "full"], "description": "chunk compression level (optional; default from config, usually compact)" },
                    "source":     { "type": "string", "enum": ["code", "knowledge", "both"], "description": "which pools to search (optional; default `both` when a knowledge store exists, else `code`)" }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "index_status",
            "description": "Check whether this project is indexed and how fresh it is.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "record_feedback",
            "description": "Record whether a prior `context_search` result was helpful, to improve the quality signal on the dashboard.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query_id": { "type": "string" },
                    "helpful":  { "type": "boolean" },
                    "note":     { "type": "string" }
                },
                "required": ["query_id", "helpful"]
            }
        }),
        json!({
            "name": "expand_chunk",
            "description": EXPAND_CHUNK_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": { "type": "string" },
                    "scope":    { "type": "string", "enum": ["body", "file", "neighbors"], "default": "body" }
                },
                "required": ["chunk_id"]
            }
        }),
        json!({
            "name": "related_context",
            "description": RELATED_CONTEXT_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chunk_id": { "type": "string" },
                    "top_k":    { "type": "integer", "default": 8 }
                },
                "required": ["chunk_id"]
            }
        }),
        json!({
            "name": "set_output_compression",
            "description": SET_OUTPUT_COMPRESSION_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "level": { "type": "string", "enum": ["off", "lite", "standard", "max"], "description": "how terse THIS session's answers should be" }
                },
                "required": ["level"]
            }
        }),
        json!({
            "name": "record_decision",
            "description": RECORD_DECISION_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "the validated decision to remember" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "optional labels" },
                    "area": { "type": "string", "description": "optional area/component this decision is about" }
                },
                "required": ["text"]
            }
        }),
        json!({
            "name": "session_recall",
            "description": SESSION_RECALL_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":  { "type": "string" },
                    "top_k":  { "type": "integer", "default": 5 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "summarize_context",
            "description": SUMMARIZE_CONTEXT_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": { "type": "string", "enum": ["all", "files", "queries", "decisions"], "default": "all", "description": "which slice of the session digest to return" }
                }
            }
        }),
    ]
}

// --- context_search ---

/// Which pool(s) `context_search` searches (SPEC-V2.6 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceSel {
    Code,
    Knowledge,
    Both,
}

/// Resolve the effective `source` (SPEC-V2.6 §5). An explicit, valid `source` arg wins.
/// Absent (or an unknown value) ⇒ the default: when a `.cce/knowledge/current` store
/// exists AND knowledge is enabled, the config `knowledge.default_source` (itself
/// `both` by default); otherwise `code` — so today's code-only behaviour is preserved
/// exactly whenever there is no knowledge store.
fn resolve_source(server: &McpServer, args: &Value) -> SourceSel {
    if let Some(s) = args.get("source").and_then(Value::as_str) {
        match s.trim() {
            "code" => return SourceSel::Code,
            "knowledge" => return SourceSel::Knowledge,
            "both" => return SourceSel::Both,
            _ => {} // unknown ⇒ fall through to the default
        }
    }
    let root = server.root();
    let cfg = KnowledgeConfig::load(&root);
    if cfg.enabled && server.knowledge().is_some() {
        match cfg.default_source.as_str() {
            "code" => SourceSel::Code,
            "knowledge" => SourceSel::Knowledge,
            _ => SourceSel::Both,
        }
    } else {
        SourceSel::Code
    }
}

/// `context_search` (SPEC-MCP §1 + SPEC-V2.6 §5): ranked context for a query, over the
/// code index, the knowledge store, or both (blended by the one shared ranking).
pub fn context_search(server: &McpServer, args: &Value) -> ToolOutput {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if query.is_empty() {
        return ToolOutput::err("context_search requires a non-empty `query`.");
    }
    match resolve_source(server, args) {
        // Code-only is the UNCHANGED v2.5 path — byte-identical to today.
        SourceSel::Code => context_search_code(server, args, &query),
        SourceSel::Knowledge => context_search_knowledge(server, args, &query),
        SourceSel::Both => context_search_both(server, args, &query),
    }
}

/// The code-only path (SPEC-MCP §1): the exact v2.5 behaviour, preserved byte-for-byte.
fn context_search_code(server: &McpServer, args: &Value, query: &str) -> ToolOutput {
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(MCP_DEFAULT_TOP_K);
    let no_graph = args.get("no_graph").and_then(Value::as_bool).unwrap_or(false);
    let max_tokens = args.get("max_tokens").and_then(Value::as_u64).map(|n| n as usize);
    let package = args.get("package").and_then(Value::as_str).map(|s| s.to_string());
    // L2 detail (SPEC-V2.5 §2): the tool arg wins; absent ⇒ `.cce/config`
    // `retrieval.detail`; absent ⇒ the compiled default (compact).
    let detail = args
        .get("detail")
        .and_then(Value::as_str)
        .and_then(DetailLevel::parse)
        .unwrap_or_else(|| RetrievalConfig::load(&server.root()).detail);

    if server.is_workspace() {
        context_search_workspace(server, query, top_k, no_graph, max_tokens, package, detail)
    } else {
        context_search_single(server, query, top_k, no_graph, max_tokens, detail)
    }
}

/// Single-repo retrieval: the exact §6 pipeline + an identical `search` event.
fn context_search_single(
    server: &McpServer,
    query: &str,
    top_k: usize,
    no_graph: bool,
    max_tokens: Option<usize>,
    detail: DetailLevel,
) -> ToolOutput {
    // Cached across calls (issue #31): reused while the store file is unchanged.
    let index = match server.load_index() {
        Ok(i) => i,
        Err(_) => return ToolOutput::ok(missing_index_message(false)),
    };
    let start = Instant::now();
    // Ollama-built store with Ollama down (issue #30): BM25-only, with the
    // pinned notice — visible degradation, never a cross-space cosine.
    let (results, notice) = match pick_embedder(&index) {
        QueryEmbedder::Ready(emb) => (search(&index, emb.as_ref(), query, top_k, !no_graph), None),
        QueryEmbedder::Bm25Only => {
            (bm25_only_search(&index, query, top_k), Some(OLLAMA_DOWN_NOTICE))
        }
    };
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Identical to the CLI path: a `cce.metrics/v1` search event beside the store,
    // so `cce dashboard` shows the agent's query and token savings. `detail` drives
    // the L2 chunk_compression bucket (SPEC-V2.5 §2).
    let record =
        build_search_record(&index, &results, query, top_k, !no_graph, latency_ms, "mcp", detail);
    let query_id = write_search_event(&server.metrics_path(), &record);
    // v2.8: accrue the record's own tokens_saved into the session usage counters.
    server.note_search_usage(record.tokens_saved);

    // L6: record this search (query + the ids/paths it returned) in the session ledger.
    let chunk_ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
    let file_paths: Vec<String> = results.iter().map(|r| r.file_path.clone()).collect();
    server.record_search(query, &chunk_ids, &file_paths);

    let rows: Vec<Row> = results.iter().map(Row::from_single).collect();
    let text = format_rows(&rows, query_id.as_deref(), max_tokens, index.chunks.len(), detail);
    // Opt-in usage footer (v2.8): appended last; `off` leaves the bytes untouched.
    let text = append_usage_footer(server, &footer_facts(&record, index.chunks.len()), text);
    ToolOutput::ok(with_notice(notice, text))
}

/// Workspace retrieval: SPEC-V2.2 federation over the in-scope members.
#[allow(clippy::too_many_arguments)]
fn context_search_workspace(
    server: &McpServer,
    query: &str,
    top_k: usize,
    no_graph: bool,
    max_tokens: Option<usize>,
    package: Option<String>,
    detail: DetailLevel,
) -> ToolOutput {
    let root = server.root();
    let manifest = match Manifest::load(&root) {
        Ok(m) => m,
        Err(_) => return ToolOutput::ok(missing_index_message(true)),
    };
    // An empty-but-present `package` ("" / "," / whitespace) is a user mistake
    // (issue #45): surface the guidance, never a silent zero-member federation.
    let scope = match parse_scope(package) {
        Ok(s) => s,
        Err(e) => return ToolOutput::ok(e),
    };
    // Cached federated union (issue #26): built once per scope, reused across calls, and
    // shared as the metrics baseline below — so a warm workspace search matches the CLI.
    let bundle = match server.workspace_bundle(&manifest, scope.as_deref()) {
        Ok(b) => b,
        // Unknown package or an unindexed member: surface the guidance, not a crash.
        Err(e) => return ToolOutput::ok(e),
    };
    let members = &bundle.members;
    let combined = &bundle.combined;
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);

    let start = Instant::now();
    // Ollama-built members with Ollama down (issue #30): BM25-only over the
    // union, with the pinned notice — never a cross-space cosine.
    let (results, notice) = match pick_workspace_embedder(members) {
        QueryEmbedder::Ready(emb) => (
            federated_search_over(combined, members, &graph, emb.as_ref(), query, top_k, !no_graph),
            None,
        ),
        QueryEmbedder::Bm25Only => {
            (federated_bm25_only_search(combined, members, query, top_k), Some(OLLAMA_DOWN_NOTICE))
        }
    };
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Metrics: log a search event beside the workspace-root store so the root
    // `cce dashboard` sees agent usage. Baseline/served tokens come from the (cached)
    // union — no second assembly.
    let namespaced: Vec<SearchResult> = results
        .iter()
        .map(|r| SearchResult {
            rank: r.rank,
            chunk_id: r.chunk_id.clone(),
            file_path: format!("{}/{}", r.member, r.file_path),
            start_line: r.start_line,
            end_line: r.end_line,
            chunk_type: r.chunk_type.clone(),
            kind: r.kind.clone(),
            score: r.score,
            content: r.content.clone(),
        })
        .collect();
    let record = build_search_record(
        combined,
        &namespaced,
        query,
        top_k,
        !no_graph,
        latency_ms,
        "mcp",
        detail,
    );
    let query_id = write_search_event(&server.metrics_path(), &record);
    // v2.8: accrue the record's own tokens_saved into the session usage counters.
    server.note_search_usage(record.tokens_saved);

    // L6: record this search in the session ledger. File paths are member-prefixed
    // (`member/path`) so two members' same-named files stay distinct in the digest.
    let chunk_ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
    let file_paths: Vec<String> =
        results.iter().map(|r| format!("{}/{}", r.member, r.file_path)).collect();
    server.record_search(query, &chunk_ids, &file_paths);

    let total_chunks: usize = members.iter().map(|m| m.index.chunks.len()).sum();
    let rows: Vec<Row> = results.iter().map(Row::from_fed).collect();
    let text = format_rows(&rows, query_id.as_deref(), max_tokens, total_chunks, detail);
    // Opt-in usage footer (v2.8): appended last; `off` leaves the bytes untouched.
    let text = append_usage_footer(server, &footer_facts(&record, total_chunks), text);
    ToolOutput::ok(with_notice(notice, text))
}

// --- knowledge + blend (SPEC-V2.6 §5) ---

/// The shared query params for a `context_search` call (parsed once, used by the
/// knowledge and blend paths — the code path parses its own so it stays byte-identical).
struct SearchParams {
    top_k: usize,
    no_graph: bool,
    max_tokens: Option<usize>,
    package: Option<String>,
    detail: DetailLevel,
}

fn parse_params(server: &McpServer, args: &Value) -> SearchParams {
    SearchParams {
        top_k: args
            .get("top_k")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|n| *n > 0)
            .unwrap_or(MCP_DEFAULT_TOP_K),
        no_graph: args.get("no_graph").and_then(Value::as_bool).unwrap_or(false),
        max_tokens: args.get("max_tokens").and_then(Value::as_u64).map(|n| n as usize),
        package: args.get("package").and_then(Value::as_str).map(|s| s.to_string()),
        detail: args
            .get("detail")
            .and_then(Value::as_str)
            .and_then(DetailLevel::parse)
            .unwrap_or_else(|| RetrievalConfig::load(&server.root()).detail),
    }
}

/// Load the ranked knowledge hits for a query (SPEC-V2.6 §5), honouring
/// `knowledge.enabled` and `knowledge.min_score`. No store (or disabled) ⇒ empty.
/// The loaded+embedded store is cached across calls by the server (issue #31).
fn load_knowledge_hits(server: &McpServer, query: &str, top_k: usize) -> Vec<KnowledgeHit> {
    let cfg = KnowledgeConfig::load(&server.root());
    if !cfg.enabled {
        return Vec::new();
    }
    match server.knowledge() {
        Some(k) => k.search(query, top_k, cfg.min_score),
        None => Vec::new(),
    }
}

/// One code hit as an owned row (so it can be interleaved with knowledge without
/// borrowing the transient `SearchResult`/`FedResult`). Fields mirror what the pinned
/// result-header grammar (`crate::grammar::compact_line`) needs, so a code row renders
/// byte-identically whether it is served alone or blended.
struct CodeRow {
    score: f64,
    package: Option<String>,
    chunk_id: String,
    file_path: String,
    start: usize,
    end: usize,
    chunk_type: String,
    kind: String,
    content: String,
}

/// A blended candidate: a code row or a knowledge hit, ranked by one shared score.
enum BlendItem {
    Code(CodeRow),
    Knowledge(KnowledgeHit),
}

impl BlendItem {
    fn score(&self) -> f64 {
        match self {
            BlendItem::Code(c) => c.score,
            BlendItem::Knowledge(k) => k.score,
        }
    }
    fn chunk_id(&self) -> &str {
        match self {
            BlendItem::Code(c) => &c.chunk_id,
            BlendItem::Knowledge(k) => &k.chunk_id,
        }
    }
    /// Sort priority when scores tie: code before knowledge (a stable, pinned order).
    fn source_rank(&self) -> u8 {
        match self {
            BlendItem::Code(_) => 0,
            BlendItem::Knowledge(_) => 1,
        }
    }
}

/// What a code-side gather returns for blending: the rows, the logged
/// `query_id`, the code chunk count, when the store is ollama-built and
/// Ollama is down (issue #30) the pinned degradation notice to surface, and —
/// when a `search` record was built — the v2.8 footer facts read off it.
type CodeHits = (Vec<CodeRow>, Option<String>, usize, Option<&'static str>, Option<FooterFacts>);

/// Run code retrieval and return owned code rows plus the logged `query_id` and the
/// code chunk count — reusing the SAME §6 pipeline, metrics event, and session-ledger
/// recording as the code-only path (so the dashboard and digest are unchanged), but
/// deferring rendering so the rows can be blended with knowledge.
fn gather_code_hits(server: &McpServer, query: &str, p: &SearchParams) -> CodeHits {
    if server.is_workspace() {
        gather_code_hits_workspace(server, query, p)
    } else {
        gather_code_hits_single(server, query, p)
    }
}

fn gather_code_hits_single(server: &McpServer, query: &str, p: &SearchParams) -> CodeHits {
    // Cached across calls (issue #31): reused while the store file is unchanged.
    let index = match server.load_index() {
        Ok(i) => i,
        // A store that was never built (a knowledge-only project) is true absence —
        // knowledge-only is then the correct, complete answer, so stay silent. A
        // store that EXISTS but fails to load (corrupt/unreadable) must NOT be
        // swallowed into zero code rows (issue #132): surface the pinned notice
        // through the same channel as OLLAMA_DOWN_NOTICE (issue #30).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), None, 0, None, None)
        }
        Err(_) => return (Vec::new(), None, 0, Some(CODE_INDEX_LOAD_ERROR_NOTICE), None),
    };
    let start = Instant::now();
    let (results, notice) = match pick_embedder(&index) {
        QueryEmbedder::Ready(emb) => {
            (search(&index, emb.as_ref(), query, p.top_k, !p.no_graph), None)
        }
        QueryEmbedder::Bm25Only => {
            (bm25_only_search(&index, query, p.top_k), Some(OLLAMA_DOWN_NOTICE))
        }
    };
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let record = build_search_record(
        &index,
        &results,
        query,
        p.top_k,
        !p.no_graph,
        latency_ms,
        "mcp",
        p.detail,
    );
    let query_id = write_search_event(&server.metrics_path(), &record);
    // v2.8: accrue the record's own tokens_saved into the session usage counters.
    server.note_search_usage(record.tokens_saved);
    let facts = footer_facts(&record, index.chunks.len());
    let chunk_ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
    let file_paths: Vec<String> = results.iter().map(|r| r.file_path.clone()).collect();
    server.record_search(query, &chunk_ids, &file_paths);
    let rows = results
        .into_iter()
        .map(|r| CodeRow {
            score: r.score,
            package: None,
            chunk_id: r.chunk_id,
            file_path: r.file_path,
            start: r.start_line,
            end: r.end_line,
            chunk_type: r.chunk_type,
            kind: r.kind,
            content: r.content,
        })
        .collect();
    (rows, query_id, index.chunks.len(), notice, Some(facts))
}

fn gather_code_hits_workspace(server: &McpServer, query: &str, p: &SearchParams) -> CodeHits {
    let root = server.root();
    let manifest = match Manifest::load(&root) {
        Ok(m) => m,
        // Same absent-vs-failed split as the single-repo path (issue #132): a
        // manifest that is truly absent stays silent; one that EXISTS but fails to
        // parse means code retrieval is broken and must be visible.
        Err(_) => {
            let notice =
                manifest_path(&root).exists().then_some(WORKSPACE_CODE_INDEX_LOAD_ERROR_NOTICE);
            return (Vec::new(), None, 0, notice, None);
        }
    };
    // An empty-but-present `package` is invalid (issue #45); in this path a scope
    // error yields no code rows, exactly like an unknown package below.
    let scope = match parse_scope(p.package.clone()) {
        Ok(s) => s,
        Err(_) => return (Vec::new(), None, 0, None, None),
    };
    // Cached federated union (issue #26): same bundle the code-only path uses.
    let bundle = match server.workspace_bundle(&manifest, scope.as_deref()) {
        Ok(b) => b,
        // A workspace whose members were ALL never indexed has no member store on
        // disk — true absence, silent (the knowledge-only case). If any member store
        // EXISTS while the bundle still fails to load, code retrieval is incomplete —
        // whether from a corrupt/unreadable member OR a member not yet indexed (the
        // common partially-indexed case) — so surface the workspace notice, whose
        // wording covers all of them without a false corruption claim (issue #132).
        Err(_) => {
            let any_member_store_exists =
                manifest.members.iter().any(|m| default_store_path(&root.join(&m.path)).exists());
            let notice = any_member_store_exists.then_some(WORKSPACE_CODE_INDEX_LOAD_ERROR_NOTICE);
            return (Vec::new(), None, 0, notice, None);
        }
    };
    let members = &bundle.members;
    let combined = &bundle.combined;
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);
    let start = Instant::now();
    let (results, notice) = match pick_workspace_embedder(members) {
        QueryEmbedder::Ready(emb) => (
            federated_search_over(
                combined,
                members,
                &graph,
                emb.as_ref(),
                query,
                p.top_k,
                !p.no_graph,
            ),
            None,
        ),
        QueryEmbedder::Bm25Only => (
            federated_bm25_only_search(combined, members, query, p.top_k),
            Some(OLLAMA_DOWN_NOTICE),
        ),
    };
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;
    let namespaced: Vec<SearchResult> = results
        .iter()
        .map(|r| SearchResult {
            rank: r.rank,
            chunk_id: r.chunk_id.clone(),
            file_path: format!("{}/{}", r.member, r.file_path),
            start_line: r.start_line,
            end_line: r.end_line,
            chunk_type: r.chunk_type.clone(),
            kind: r.kind.clone(),
            score: r.score,
            content: r.content.clone(),
        })
        .collect();
    let record = build_search_record(
        combined,
        &namespaced,
        query,
        p.top_k,
        !p.no_graph,
        latency_ms,
        "mcp",
        p.detail,
    );
    let query_id = write_search_event(&server.metrics_path(), &record);
    // v2.8: accrue the record's own tokens_saved into the session usage counters.
    server.note_search_usage(record.tokens_saved);
    let chunk_ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
    let file_paths: Vec<String> =
        results.iter().map(|r| format!("{}/{}", r.member, r.file_path)).collect();
    server.record_search(query, &chunk_ids, &file_paths);
    let total_chunks: usize = members.iter().map(|m| m.index.chunks.len()).sum();
    let facts = footer_facts(&record, total_chunks);
    let rows = results
        .into_iter()
        .map(|r| CodeRow {
            score: r.score,
            package: Some(r.package),
            chunk_id: r.chunk_id,
            file_path: r.file_path,
            start: r.start_line,
            end: r.end_line,
            chunk_type: r.chunk_type,
            kind: r.kind,
            content: r.content,
        })
        .collect();
    (rows, query_id, total_chunks, notice, Some(facts))
}

/// The knowledge-only path (SPEC-V2.6 §5, `source:"knowledge"`).
fn context_search_knowledge(server: &McpServer, args: &Value, query: &str) -> ToolOutput {
    let p = parse_params(server, args);
    let hits = load_knowledge_hits(server, query, p.top_k);
    // Record the knowledge search in the session ledger (its query + returned ids/docs).
    let chunk_ids: Vec<String> = hits.iter().map(|h| h.chunk_id.clone()).collect();
    let record_ids: Vec<String> = hits.iter().map(|h| h.record_id.clone()).collect();
    server.record_search(query, &chunk_ids, &record_ids);
    let items: Vec<BlendItem> = hits.into_iter().map(BlendItem::Knowledge).collect();
    ToolOutput::ok(render_blend(items, None, &p, knowledge_total_chunks(server)))
}

/// The blended path (SPEC-V2.6 §5, `source:"both"`): code + knowledge candidates merged
/// through the one shared ranking into a single top-K.
fn context_search_both(server: &McpServer, args: &Value, query: &str) -> ToolOutput {
    let p = parse_params(server, args);
    let (code_rows, query_id, code_chunks, notice, facts) = gather_code_hits(server, query, &p);
    let khits = load_knowledge_hits(server, query, p.top_k);
    let mut items: Vec<BlendItem> = Vec::with_capacity(code_rows.len() + khits.len());
    items.extend(code_rows.into_iter().map(BlendItem::Code));
    items.extend(khits.into_iter().map(BlendItem::Knowledge));
    let total = code_chunks + knowledge_total_chunks(server);
    let mut text = render_blend(items, query_id.as_deref(), &p, total);
    // Opt-in usage footer (v2.8): the recorded (code) event's numbers, with the
    // blended chunk count the renderer shows. Only when a search was recorded.
    if let Some(mut f) = facts {
        f.total_chunks = total as u64;
        text = append_usage_footer(server, &f, text);
    }
    ToolOutput::ok(with_notice(notice, text))
}

/// The knowledge store's chunk count (for the "no matches" hint), 0 if none/disabled.
fn knowledge_total_chunks(server: &McpServer) -> usize {
    if !KnowledgeConfig::load(&server.root()).enabled {
        return 0;
    }
    server.knowledge().map(|k| k.store.chunks.len()).unwrap_or(0)
}

/// The byte-pinned compact header for ONE knowledge hit (SPEC-V2.6 §5), mirroring the
/// code grammar's shape but carrying the provenance in place of `file:line (type/kind)`:
/// `«rank». [«score»] [knowledge] «title» — «state» · «updated_at» · «url» #«chunk_id»`
/// (missing facets omitted cleanly). No trailing newline — the caller adds it.
fn knowledge_compact_line(rank: usize, score6: &str, hit: &KnowledgeHit) -> String {
    format!("{:>2}. [{}] {} #{}", rank, score6, hit.provenance(), hit.chunk_id)
}

/// Render a blended list (SPEC-V2.6 §5): sort code + knowledge candidates by the one
/// shared score (desc), tie-break code-before-knowledge then `chunk_id` asc, truncate to
/// `top_k`, and render each — code rows via the UNCHANGED pinned grammar, knowledge rows
/// with the provenance header — with the same body-serving, `max_tokens`, hint, and
/// `query_id` footer as the code path.
fn render_blend(
    mut items: Vec<BlendItem>,
    query_id: Option<&str>,
    p: &SearchParams,
    total_chunks: usize,
) -> String {
    items.sort_by(|a, b| {
        score_key(b.score())
            .cmp(&score_key(a.score()))
            .then_with(|| a.source_rank().cmp(&b.source_rank()))
            .then_with(|| a.chunk_id().cmp(b.chunk_id()))
    });
    items.truncate(p.top_k);

    if items.is_empty() {
        let mut s = format!(
            "No matching context found. The index has {total_chunks} chunk(s) — try broader or \
             different terms."
        );
        if let Some(id) = query_id {
            s.push_str(&format!("\n\nquery_id: {id}\n"));
        }
        return s;
    }

    let registry = Registry::default();
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, item) in items.iter().enumerate() {
        let rank = i + 1;
        let (header, served) = match item {
            BlendItem::Code(c) => {
                // The UNCHANGED pinned result grammar (Layer 3) — code rows stay
                // byte-for-byte identical to the code-only path (aside from their rank,
                // which reflects the blended position).
                let header = crate::grammar::compact_line(&crate::grammar::GrammarRow {
                    rank,
                    score6: format6(c.score),
                    package: c.package.clone(),
                    file_path: c.file_path.clone(),
                    start: c.start,
                    end: c.end,
                    chunk_type: c.chunk_type.clone(),
                    kind: c.kind.clone(),
                    chunk_id: c.chunk_id.clone(),
                });
                let served = compress(&registry, &c.file_path, &c.content, p.detail);
                (header, served)
            }
            BlendItem::Knowledge(k) => {
                // Knowledge bodies are already heading-sized markdown sections; serve
                // them whole (redacted at index time). Provenance rides the header.
                (knowledge_compact_line(rank, &format6(k.score), k), k.content.clone())
            }
        };
        out.push_str(&header);
        out.push('\n');
        let body = match p.max_tokens {
            Some(max) => {
                let (b, cut) = trim_to_tokens(&served, max.saturating_sub(used));
                truncated |= cut;
                b
            }
            None => served,
        };
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        used += token_count(&body);
        if let Some(max) = p.max_tokens {
            if used >= max && i + 1 < items.len() {
                truncated = true;
                break;
            }
        }
    }
    if truncated {
        out.push_str("… (context truncated to max_tokens)\n\n");
    }
    if p.detail != DetailLevel::Full {
        out.push_str(
            "Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; \
             related_context(chunk_id) for import-graph neighbours.\n",
        );
    }
    if let Some(id) = query_id {
        out.push_str(&format!("query_id: {id}\n"));
        out.push_str(&format!(
            "Rate this with record_feedback (query_id=\"{id}\", helpful=true|false).\n"
        ));
    }
    out
}

/// Append a `search` event and return its id (the query-id), fail-open.
fn write_search_event(
    metrics_path: &Path,
    record: &crate::metrics::SearchRecord,
) -> Option<String> {
    let clock = SystemClock;
    let ids = HexIdSource::default();
    let writer = MetricsWriter::new(metrics_path.to_path_buf(), &clock, &ids, true);
    writer.log_search(record)
}

/// The footer facts for a search: every value read straight off the ALREADY-BUILT
/// `search` record (plus the corpus chunk count the renderer shows). Pure
/// projection — nothing here computes a new metric (SPEC-USAGE-VISIBILITY §3.2).
fn footer_facts(record: &crate::metrics::SearchRecord, total_chunks: usize) -> FooterFacts {
    FooterFacts {
        result_count: record.result_count as u64,
        total_chunks: total_chunks as u64,
        served_tokens: record.served_tokens,
        baseline_tokens: record.baseline_tokens,
        tokens_saved: record.tokens_saved,
        savings_ratio: record.savings_ratio,
    }
}

/// Append the opt-in usage footer (SPEC-USAGE-VISIBILITY §3) to a rendered
/// `context_search` result. `off` (the default) returns `text` UNTOUCHED —
/// byte-identical to pre-v2.8 — which is what keeps the MCP golden and the
/// conformance surfaces pinned. `on` appends the one byte-pinned line; `session`
/// adds the running session clause. Rendered AFTER all savings measurement, from
/// values already on the recorded event: toggling it never changes a recorded
/// metric (Invariant 1).
fn append_usage_footer(server: &McpServer, facts: &FooterFacts, mut text: String) -> String {
    let session = match server.footer_mode() {
        FooterMode::Off => return text,
        FooterMode::On => None,
        FooterMode::Session => Some(server.session_usage()),
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&crate::grammar::usage_footer_line(facts, session));
    text.push('\n');
    text
}

/// The pinned notice line served when an ollama-built store is queried while
/// Ollama is down (issue #30): the session must not crash (the friendly-error
/// pattern), but the degradation must be VISIBLE — the results below it are
/// keyword (BM25) matches only, never a hash-vs-ollama cosine.
pub(crate) const OLLAMA_DOWN_NOTICE: &str = "NOTICE: this index was built with the ollama \
embedder but Ollama is unreachable — vector recall is disabled; results are keyword (BM25) \
matches only. Start Ollama, or re-index with the default hash embedder (`cce index <dir>`), to \
restore semantic search.";

/// The pinned notice line served when the blended path's code store EXISTS on disk
/// but fails to load (issue #132): a corrupt/unreadable index must be VISIBLE — the
/// results below it are knowledge-only, missing all code — while a store that is
/// legitimately absent (a knowledge-only project) stays silent, because
/// knowledge-only is then the correct, complete answer.
pub(crate) const CODE_INDEX_LOAD_ERROR_NOTICE: &str = "NOTICE: the code index exists but could \
not be loaded (corrupt or unreadable store) — code results are MISSING from this answer; \
anything shown below is knowledge-only. Re-run `cce index` (or `cce index --workspace`) to \
rebuild it.";

/// The workspace counterpart of [`CODE_INDEX_LOAD_ERROR_NOTICE`] (issue #132). The
/// federated path only knows that at least one member store is on disk while the
/// bundle failed — a proxy that lumps together a corrupt/unreadable member AND a
/// member that was simply never indexed (the common partially-indexed steady state).
/// The wording therefore covers all three and claims only that code results are
/// INCOMPLETE, never that a specific store is corrupt.
pub(crate) const WORKSPACE_CODE_INDEX_LOAD_ERROR_NOTICE: &str = "NOTICE: one or more workspace \
member code indexes are missing or could not be loaded (corrupt, unreadable, or not yet \
indexed) — code results are INCOMPLETE for this answer; anything shown below may be missing code \
rows. Re-run `cce index --workspace` to (re)build them.";

/// The query embedder decision for a loaded store (issue #30).
enum QueryEmbedder {
    /// The store's own backend is usable — run the full §6 pipeline.
    Ready(Box<dyn Embedder>),
    /// The store was built with ollama and Ollama is down: degrade EXPLICITLY
    /// to BM25-only (with [`OLLAMA_DOWN_NOTICE`]) — never cosine across two
    /// different embedding spaces.
    Bm25Only,
}

/// Pick the query embedder: the deterministic hash backend for hash stores, a
/// health-checked Ollama for ollama stores, and the explicit BM25-only
/// degradation when an ollama store's server is unreachable.
fn pick_embedder(index: &Index) -> QueryEmbedder {
    if index.embedder_name == "ollama" {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            return QueryEmbedder::Ready(Box::new(oll));
        }
        return QueryEmbedder::Bm25Only;
    }
    QueryEmbedder::Ready(Box::new(HashEmbedder))
}

/// The workspace twin of [`pick_embedder`]: ollama applies if ANY member's
/// store was built with it (mirrors the CLI federation path).
fn pick_workspace_embedder(members: &[MemberStore]) -> QueryEmbedder {
    let uses_ollama = members.iter().any(|m| m.index.embedder_name == "ollama");
    if uses_ollama {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            return QueryEmbedder::Ready(Box::new(oll));
        }
        return QueryEmbedder::Bm25Only;
    }
    QueryEmbedder::Ready(Box::new(HashEmbedder))
}

/// Prepend a pinned degradation notice ([`OLLAMA_DOWN_NOTICE`],
/// [`CODE_INDEX_LOAD_ERROR_NOTICE`]) to a rendered result body.
fn with_notice(notice: Option<&'static str>, body: String) -> String {
    match notice {
        Some(n) => format!("{n}\n\n{body}"),
        None => body,
    }
}

// --- index_status ---

/// `index_status` (SPEC-MCP §2): counts + sync freshness. Workspace auto-detected.
pub fn index_status(server: &McpServer) -> ToolOutput {
    if server.is_workspace() {
        index_status_workspace(server)
    } else {
        index_status_single(server)
    }
}

fn index_status_single(server: &McpServer) -> ToolOutput {
    let store = server.store_path();
    // Cached across calls (issue #31): reused while the store file is unchanged.
    let index = match server.load_index() {
        Ok(i) => i,
        Err(_) => {
            // A knowledge-only consumer root (`cce knowledge pull` with no code
            // index) still reports its knowledge freshness — additive: with no
            // knowledge store the message is byte-identical to before.
            let mut msg = format!(
                "not indexed — no store at {}. Run `cce index` (or `cce init`).",
                store.display()
            );
            append_knowledge_freshness(&mut msg, &server.root());
            return ToolOutput::ok(msg);
        }
    };
    let mut per_lang: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_kind: BTreeMap<String, usize> = BTreeMap::new();
    for c in &index.chunks {
        *per_lang.entry(c.language.clone()).or_insert(0) += 1;
        *per_kind.entry(c.kind.clone()).or_insert(0) += 1;
    }
    let mut out = String::new();
    out.push_str("Index status\n");
    out.push_str(&format!("  store   : {}\n", store.display()));
    out.push_str("  indexed : yes\n");
    out.push_str(&format!("  chunks  : {}\n", index.chunks.len()));
    out.push_str(&format!("  files   : {}\n", index.files().len()));
    out.push_str(&format!("  embedder: {}\n", index.embedder_name));
    out.push_str("  by language:\n");
    for (l, n) in &per_lang {
        out.push_str(&format!("    {l:<12}: {n}\n"));
    }
    out.push_str("  by kind:\n");
    for (k, n) in &per_kind {
        out.push_str(&format!("    {k:<22}: {n}\n"));
    }
    append_freshness(&mut out, &server.root());
    append_knowledge_freshness(&mut out, &server.root());
    ToolOutput::ok(out)
}

fn index_status_workspace(server: &McpServer) -> ToolOutput {
    let root = server.root();
    let manifest = match Manifest::load(&root) {
        Ok(m) => m,
        Err(_) => {
            return ToolOutput::ok(
                "not a workspace — no `.cce/workspace.yml`. Run `cce workspace init` then \
                 `cce index --workspace` (or `cce init`)."
                    .to_string(),
            )
        }
    };
    let members = match load_member_stores(&root, &manifest, None) {
        Ok(m) => m,
        Err(e) => return ToolOutput::ok(e),
    };
    let stats = workspace_stats(&members);
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);

    let mut out = String::new();
    out.push_str(&format!("Workspace status: {}\n", manifest.name));
    let mut total_files = 0usize;
    let mut total_chunks = 0usize;
    for s in &stats {
        total_files += s.files;
        total_chunks += s.chunks;
        out.push_str(&format!(
            "  {} (package {}) — files {}, chunks {}\n",
            s.name, s.package, s.files, s.chunks
        ));
    }
    out.push_str(&format!("  totals  : files {total_files}, chunks {total_chunks}\n"));
    out.push_str(&format!("  edges ({}):\n", graph.edges.len()));
    for e in &graph.edges {
        out.push_str(&format!("    {} -> {} (via {})\n", e.from, e.to, e.via));
    }
    append_freshness(&mut out, &root);
    append_knowledge_freshness(&mut out, &root);
    ToolOutput::ok(out)
}

/// Append the sync freshness lines (source / remote-latest / behind-remote).
fn append_freshness(out: &mut String, root: &Path) {
    let f = freshness(root);
    let source = match f.source {
        IndexSource::Local => "local (built by cce index)".to_string(),
        IndexSource::Pulled => match &f.sha {
            Some(sha) => format!("pulled via cce sync (sha {})", short_sha(sha)),
            None => "pulled via cce sync".to_string(),
        },
    };
    out.push_str(&format!("  source  : {source}\n"));
    match &f.remote_latest {
        Some(latest) => {
            out.push_str(&format!("  remote latest: {}\n", short_sha(latest)));
            out.push_str(&format!(
                "  behind remote: {}\n",
                if f.behind_remote {
                    "yes — run `cce sync pull --latest`"
                } else {
                    "no"
                }
            ));
        }
        None => out.push_str("  remote  : (no sync remote configured — pure local)\n"),
    }
}

/// First 12 chars of a sha (or the whole string if shorter).
fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// Append the knowledge freshness block (SPEC-SYNC-KNOWLEDGE §4.4) when a
/// knowledge store exists at the served root; with no store, nothing is
/// appended and the report stays byte-identical to pre-M5. `remote current` /
/// `behind remote` follow the code freshness rules exactly: best-effort,
/// offline-safe (any error ⇒ `-` / `no`); `behind remote` is `yes` only when
/// both snapshots are known and differ.
fn append_knowledge_freshness(out: &mut String, root: &Path) {
    let Some(f) = knowledge_freshness(root) else { return };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("  knowledge :\n");
    out.push_str(&format!(
        "    corpus         : {}\n",
        f.corpus.as_deref().unwrap_or("(local ingest)")
    ));
    out.push_str(&format!("    snapshot       : {}\n", f.snapshot));
    out.push_str(&format!("    records/chunks : {} / {}\n", f.records, f.chunks));
    out.push_str(&format!("    data as-of     : {}\n", f.data_as_of.as_deref().unwrap_or("-")));
    out.push_str(&format!("    remote current : {}\n", f.remote_current.as_deref().unwrap_or("-")));
    out.push_str(&format!(
        "    behind remote  : {}\n",
        if f.behind_remote {
            "yes — run `cce knowledge pull`"
        } else {
            "no"
        }
    ));
}

// --- record_feedback ---

/// `record_feedback` (SPEC-MCP §3): append a `feedback` event to `metrics.jsonl`.
pub fn record_feedback(server: &McpServer, args: &Value) -> ToolOutput {
    let query_id = args.get("query_id").and_then(Value::as_str).unwrap_or("").trim();
    if query_id.is_empty() {
        return ToolOutput::err(
            "record_feedback requires a `query_id` (the id from a prior context_search).",
        );
    }
    let helpful = match args.get("helpful").and_then(Value::as_bool) {
        Some(h) => h,
        None => {
            return ToolOutput::err("record_feedback requires a boolean `helpful` (true or false).")
        }
    };
    let note = args.get("note").and_then(Value::as_str).unwrap_or("");

    let clock = SystemClock;
    let ids = HexIdSource::default();
    let writer = MetricsWriter::new(server.metrics_path(), &clock, &ids, true);
    match writer.log_feedback(query_id, helpful, note) {
        Some(_) => {
            let verdict = if helpful { "helpful" } else { "not helpful" };
            ToolOutput::ok(format!(
                "Recorded feedback ({verdict}) for {query_id}. This feeds the dashboard's \
                 retrieval-quality signal (`cce dashboard`)."
            ))
        }
        None => ToolOutput::err("could not record feedback (the metrics log is not writable)."),
    }
}

// --- Layer 4: output compression (set_output_compression) ---

/// `set_output_compression` (SPEC-V2.5 §2 Layer 4, §6): set THIS session's
/// output-compression level. Updates the running server's in-memory session
/// preference (NOT CLAUDE.md) and returns the active level + a one-line
/// confirmation. A missing or unrecognised `level` is an actionable tool-level
/// error, never a crash, and leaves the session level unchanged.
pub fn set_output_compression(server: &McpServer, args: &Value) -> ToolOutput {
    let raw = args.get("level").and_then(Value::as_str).unwrap_or("").trim();
    if raw.is_empty() {
        return ToolOutput::err(
            "set_output_compression requires a `level` (one of: off, lite, standard, max).",
        );
    }
    match OutputLevel::parse(raw) {
        Some(level) => {
            server.set_output_level(level);
            ToolOutput::ok(format!(
                "Output compression is now `{}` for this session (in-memory; CLAUDE.md unchanged).",
                level.as_str()
            ))
        }
        None => ToolOutput::err(format!(
            "set_output_compression: unknown level {raw:?}; use \"off\", \"lite\", \"standard\", \
             or \"max\"."
        )),
    }
}

// --- Layer 5: memory recall (record_decision / session_recall) ---

/// The memory store `record_decision` WRITES to: the workspace-level (root) store in
/// workspace mode, else the single-repo store — resolved like the other `.cce/`
/// stores. A cross-cutting decision lands at the root; per-member memory arises
/// naturally when `cce mcp` is rooted inside a member directory.
fn memory_write_path(server: &McpServer) -> PathBuf {
    memory::memory_path(&server.root())
}

/// The memory stores `session_recall` READS (as a union): always the workspace-level
/// (root) store, plus — in workspace mode — every member's `.cce/memory.jsonl`. So a
/// decision recorded at either scope is recalled at the workspace level. De-dup by id
/// is handled by `memory::load_entries`.
fn memory_read_paths(server: &McpServer) -> Vec<PathBuf> {
    let root = server.root();
    let mut paths = vec![memory::memory_path(&root)];
    if server.is_workspace() {
        if let Ok(manifest) = Manifest::load(&root) {
            for m in &manifest.members {
                paths.push(memory::memory_path(&root.join(&m.path)));
            }
        }
    }
    paths
}

/// The message returned when `memory.enabled` is false (SPEC-V2.5 §5): a normal
/// result, not an error — the tool is simply a no-op in this project.
fn memory_disabled_message() -> String {
    "Memory is disabled for this project (memory.enabled=false in .cce/config). \
     Set it to true to record and recall decisions."
        .to_string()
}

/// Collect a JSON string array into `Vec<String>` (trimmed, empties dropped).
fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `record_decision` (SPEC-V2.5 §2 Layer 5, §6): remember a VALIDATED decision.
/// Redacts secrets, dedupes by content-addressed id, appends, and returns the id.
/// This is an EXPLICIT call — it never auto-captures raw model output (anti-pollution).
pub fn record_decision(server: &McpServer, args: &Value) -> ToolOutput {
    if !MemoryConfig::load(&server.root()).enabled {
        return ToolOutput::ok(memory_disabled_message());
    }
    let text = args.get("text").and_then(Value::as_str).unwrap_or("").trim();
    if text.is_empty() {
        return ToolOutput::err(
            "record_decision requires a non-empty `text` (a validated decision to remember).",
        );
    }
    let tags = string_array(args.get("tags"));
    let area = args.get("area").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());

    let path = memory_write_path(server);
    let clock = SystemClock;
    match memory::record(&path, text, &tags, area, &clock) {
        Ok(outcome) => {
            // L6: record the decision (id + a short, ALREADY-REDACTED label) in the
            // session ledger. `outcome.entry.text` is the redacted store text, so the
            // label is secret-safe. Recorded on both new and dedupe outcomes — the
            // decision is part of THIS session's history either way.
            server.record_decision_event(&outcome.entry.id, &short_label(&outcome.entry.text));
            let verb = if outcome.is_new { "Recorded" } else { "Already recorded" };
            ToolOutput::ok(format!(
                "{verb} decision #{}. Retrieve it later with session_recall.",
                outcome.entry.id
            ))
        }
        Err(_) => {
            ToolOutput::err("could not record the decision (the memory store is not writable).")
        }
    }
}

/// `session_recall` (SPEC-V2.5 §2 Layer 5, §6): precision-filtered hybrid search over
/// remembered decisions. Returns compact entries + ids the agent CHOOSES to use —
/// never an auto-injected blob; returns nothing when there is no confident match.
pub fn session_recall(server: &McpServer, args: &Value) -> ToolOutput {
    if !MemoryConfig::load(&server.root()).enabled {
        return ToolOutput::ok(memory_disabled_message());
    }
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim();
    if query.is_empty() {
        return ToolOutput::err("session_recall requires a non-empty `query`.");
    }
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(memory::MEMORY_DEFAULT_TOP_K);

    let entries = memory::load_entries(&memory_read_paths(server));
    let hits = memory::recall(&entries, query, top_k, memory::MEMORY_RECALL_MIN_SCORE);
    ToolOutput::ok(format_recall(&hits, entries.len()))
}

/// Render the recall hits as a compact, byte-deterministic block. Empty ⇒ an
/// explicit "nothing recalled" so the agent proceeds without forcing weak memory.
fn format_recall(hits: &[RecallHit], total: usize) -> String {
    if hits.is_empty() {
        return format!(
            "No confident memory match ({total} remembered decision(s) searched). \
             Nothing recalled — proceed without memory."
        );
    }
    let mut out = format!("Recalled {} of {} remembered decision(s):\n", hits.len(), total);
    for h in hits {
        out.push('\n');
        let area = h.area.as_deref().map(|a| format!(" area={a}")).unwrap_or_default();
        let tags = if h.tags.is_empty() {
            String::new()
        } else {
            format!(" tags={}", h.tags.join(","))
        };
        out.push_str(&format!("{:>2}. [{}] #{}{}{}\n", h.rank, format6(h.score), h.id, area, tags));
        out.push_str(&h.text);
        if !h.text.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(
        "\nThese are validated decisions you MAY reuse — apply only what fits; \
         they are not auto-injected.\n",
    );
    out
}

// --- Layer 6: turn summarization (summarize_context) ---

/// `summarize_context` (SPEC-V2.5 §2 Layer 6, §6): render the server's per-session
/// ledger into a deterministic, bounded, STRUCTURED digest so the agent can compress a
/// long session's history instead of re-sending the raw transcript. NOT an LLM summary
/// — a pure function of the recorded call sequence (`session::SessionLedger::digest`).
/// `scope` (default `all`) narrows to a slice; an unknown scope is an actionable
/// tool-level error, never a crash. An absent/blank `scope` means `all`.
pub fn summarize_context(server: &McpServer, args: &Value) -> ToolOutput {
    let raw = args.get("scope").and_then(Value::as_str).unwrap_or("all").trim();
    let raw = if raw.is_empty() { "all" } else { raw };
    match SummaryScope::parse(raw) {
        Some(scope) => ToolOutput::ok(server.session_digest(scope)),
        None => ToolOutput::err(format!(
            "summarize_context: unknown scope {raw:?}; use \"all\", \"files\", \"queries\", or \
             \"decisions\"."
        )),
    }
}

// --- Layer 7: progressive disclosure (expand_chunk / related_context) ---

/// The read-only index expand/related work over: the server-cached single-repo store,
/// or the server-cached federated union (shared, so a chunk_id from a workspace
/// `context_search` resolves without re-federating). Both expose `&Index` via
/// [`WorkingIndex::index`].
enum WorkingIndex {
    Single(Rc<Index>),
    Workspace(Rc<CachedWorkspace>),
}

impl WorkingIndex {
    fn index(&self) -> &Index {
        match self {
            WorkingIndex::Single(i) => i.as_ref(),
            WorkingIndex::Workspace(w) => &w.combined,
        }
    }
}

/// Resolve the read-only index that expand/related work over: the single-repo store,
/// or the member-namespaced union in workspace mode (so a chunk_id from a workspace
/// `context_search` resolves). Both reuse the server's cache (issues #26/#31) instead
/// of re-loading and re-assembling every call. A missing index yields the friendly
/// guidance string.
fn working_index(server: &McpServer) -> Result<WorkingIndex, String> {
    if server.is_workspace() {
        let root = server.root();
        let manifest = Manifest::load(&root).map_err(|_| missing_index_message(true))?;
        let bundle = server.workspace_bundle(&manifest, None)?;
        Ok(WorkingIndex::Workspace(bundle))
    } else {
        server.load_index().map(WorkingIndex::Single).map_err(|_| missing_index_message(false))
    }
}

/// A one-line chunk header for expand/related output:
/// `file:start-end (chunk_type/kind) #chunk_id`.
fn chunk_header(c: &Chunk) -> String {
    format!(
        "{}:{}-{} ({}/{}) #{}",
        c.file_path, c.start_line, c.end_line, c.chunk_type, c.kind, c.chunk_id
    )
}

/// Render a titled group of `(chunk, body)` blocks (header + body each). Byte-pinned
/// and deterministic — callers pass an already-sorted slice.
fn render_group(title: &str, blocks: &[(&Chunk, String)]) -> String {
    let mut out = title.to_string();
    out.push('\n');
    for (c, body) in blocks {
        out.push('\n');
        out.push_str(&chunk_header(c));
        out.push('\n');
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The chunks of `file_path`, sorted deterministically by `(start_line, chunk_id)`.
fn chunks_in_file<'a>(index: &'a Index, file_path: &str) -> Vec<&'a Chunk> {
    let mut v: Vec<&Chunk> = index.chunks.iter().filter(|c| c.file_path == file_path).collect();
    v.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.chunk_id.cmp(&b.chunk_id)));
    v
}

/// The chunks of every import-graph neighbour of `file_path`, sorted by
/// `(file_path, start_line, chunk_id)`. `neighbors` unions successors (imports) AND
/// predecessors (consumers / reverse edges), so callers see who depends on it too.
fn neighbor_chunks<'a>(index: &'a Index, file_path: &str) -> Vec<&'a Chunk> {
    let neighbors = index.graph.neighbors(file_path);
    let mut v: Vec<&Chunk> =
        index.chunks.iter().filter(|c| neighbors.iter().any(|n| n == &c.file_path)).collect();
    v.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
    v
}

/// `expand_chunk` (SPEC-V2.5 §7): pull more of a chunk `context_search` returned.
/// `scope=body` recovers the EXACT full bytes (round-trips `detail:full`);
/// `scope=file` returns every chunk in the file; `scope=neighbors` returns chunks
/// from import-graph-related files. All output is store-derived (already redacted).
pub fn expand_chunk(server: &McpServer, args: &Value) -> ToolOutput {
    let chunk_id = args.get("chunk_id").and_then(Value::as_str).unwrap_or("").trim();
    if chunk_id.is_empty() {
        return ToolOutput::err(
            "expand_chunk requires a `chunk_id` (from a prior context_search result).",
        );
    }
    let scope = args.get("scope").and_then(Value::as_str).unwrap_or("body").trim();
    if !matches!(scope, "body" | "file" | "neighbors") {
        return ToolOutput::err(format!(
            "expand_chunk: unknown scope {scope:?}; use \"body\", \"file\", or \"neighbors\"."
        ));
    }
    // L6: record the well-formed expand call (even if the id later proves stale — the
    // agent still made it) in the session ledger.
    server.record_expand(chunk_id, scope);
    let code_index = working_index(server);
    // Code chunk first — unchanged behaviour for a code chunk_id.
    if let Ok(wi) = &code_index {
        let index = wi.index();
        if let Some(target) = index.chunks.iter().find(|c| c.chunk_id == chunk_id) {
            return match scope {
                // scope=body recovers the EXACT full body (round-trips `detail:full`).
                "body" => ToolOutput::ok(target.content.clone()),
                "file" => {
                    let file_chunks = chunks_in_file(index, &target.file_path);
                    let blocks: Vec<(&Chunk, String)> =
                        file_chunks.iter().map(|c| (*c, c.content.clone())).collect();
                    let title = format!("file {} — {} chunk(s):", target.file_path, blocks.len());
                    ToolOutput::ok(render_group(&title, &blocks))
                }
                _ => {
                    let ns = neighbor_chunks(index, &target.file_path);
                    if ns.is_empty() {
                        return ToolOutput::ok(format!(
                            "no import-graph neighbours for {} in the current index.",
                            target.file_path
                        ));
                    }
                    let blocks: Vec<(&Chunk, String)> =
                        ns.iter().map(|c| (*c, c.content.clone())).collect();
                    let title =
                        format!("neighbours of {} — {} chunk(s):", target.file_path, blocks.len());
                    ToolOutput::ok(render_group(&title, &blocks))
                }
            };
        }
    }
    // Knowledge chunk fallback (SPEC-V2.6 §5): body / same-document sections.
    if let Some(out) = expand_knowledge_chunk(server, chunk_id, scope) {
        return out;
    }
    // Found in neither pool: a missing code index shows the friendly guidance; an
    // unknown id with an index present is stale (unchanged for code-only setups).
    match code_index {
        Ok(_) => ToolOutput::ok(stale_chunk_message(chunk_id)),
        Err(msg) => ToolOutput::ok(msg),
    }
}

/// A one-line header for a knowledge section in expand/related output:
/// `«record_id»:«start»-«end» («kind») #«chunk_id»`.
fn knowledge_chunk_header(kc: &crate::knowledge::KnowledgeChunk) -> String {
    format!("{}:{}-{} ({}) #{}", kc.record_id, kc.start_line, kc.end_line, kc.kind, kc.chunk_id)
}

/// Render a titled group of knowledge sections (header + body each). Deterministic.
fn render_knowledge_group(
    title: &str,
    blocks: &[(&crate::knowledge::KnowledgeChunk, String)],
) -> String {
    let mut out = title.to_string();
    out.push('\n');
    for (kc, body) in blocks {
        out.push('\n');
        out.push_str(&knowledge_chunk_header(kc));
        out.push('\n');
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// `expand_chunk` over a knowledge chunk_id (SPEC-V2.6 §5). `body` returns the section;
/// `file`/`neighbors` return the document's sections (neighbours exclude the target).
/// Returns `None` when the id is not a knowledge chunk, so the caller can fall through.
fn expand_knowledge_chunk(server: &McpServer, chunk_id: &str, scope: &str) -> Option<ToolOutput> {
    if !KnowledgeConfig::load(&server.root()).enabled {
        return None;
    }
    let loaded = server.knowledge()?;
    let store = &loaded.store;
    let target = store.chunks.iter().find(|c| c.chunk_id == chunk_id)?;
    let out = match scope {
        "body" => target.content.clone(),
        "file" => {
            let sections = same_document_sections(store, &target.record_id, None);
            let blocks: Vec<(&crate::knowledge::KnowledgeChunk, String)> =
                sections.iter().map(|c| (*c, c.content.clone())).collect();
            let title = format!("document {} — {} section(s):", target.record_id, blocks.len());
            render_knowledge_group(&title, &blocks)
        }
        _ => {
            let sections = same_document_sections(store, &target.record_id, Some(chunk_id));
            if sections.is_empty() {
                return Some(ToolOutput::ok(format!(
                    "no same-document neighbours for {} in the knowledge store.",
                    target.record_id
                )));
            }
            let blocks: Vec<(&crate::knowledge::KnowledgeChunk, String)> =
                sections.iter().map(|c| (*c, c.content.clone())).collect();
            let title =
                format!("neighbours of {} — {} section(s):", target.record_id, blocks.len());
            render_knowledge_group(&title, &blocks)
        }
    };
    Some(ToolOutput::ok(out))
}

/// `related_context` (SPEC-V2.5 §7): import-graph neighbours (imports AND consumers)
/// of a chunk, as COMPACT entries with chunk_ids to expand. Deterministic ordering.
pub fn related_context(server: &McpServer, args: &Value) -> ToolOutput {
    let chunk_id = args.get("chunk_id").and_then(Value::as_str).unwrap_or("").trim();
    if chunk_id.is_empty() {
        return ToolOutput::err(
            "related_context requires a `chunk_id` (from a prior context_search result).",
        );
    }
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(MCP_DEFAULT_TOP_K);
    // L6: record the well-formed related_context call in the session ledger.
    server.record_related(chunk_id);
    let code_index = working_index(server);
    if let Ok(wi) = &code_index {
        let index = wi.index();
        if let Some(target) = index.chunks.iter().find(|c| c.chunk_id == chunk_id) {
            let mut ns = neighbor_chunks(index, &target.file_path);
            ns.truncate(top_k);
            if ns.is_empty() {
                return ToolOutput::ok(format!(
                    "no import-graph neighbours for {}.",
                    target.file_path
                ));
            }
            let registry = Registry::default();
            let blocks: Vec<(&Chunk, String)> = ns
                .iter()
                .map(|c| (*c, compress(&registry, &c.file_path, &c.content, DetailLevel::Compact)))
                .collect();
            let title = format!(
                "related to {} via import graph — {} chunk(s):",
                target.file_path,
                blocks.len()
            );
            return ToolOutput::ok(render_group(&title, &blocks));
        }
    }
    // Knowledge chunk fallback (SPEC-V2.6 §5): related = same-document neighbours.
    if let Some(out) = related_knowledge_chunk(server, chunk_id, top_k) {
        return out;
    }
    match code_index {
        Ok(_) => ToolOutput::ok(stale_chunk_message(chunk_id)),
        Err(msg) => ToolOutput::ok(msg),
    }
}

/// `related_context` over a knowledge chunk_id (SPEC-V2.6 §5): the other sections of the
/// same document, COMPACT, up to `top_k`. `None` when the id is not a knowledge chunk.
fn related_knowledge_chunk(server: &McpServer, chunk_id: &str, top_k: usize) -> Option<ToolOutput> {
    if !KnowledgeConfig::load(&server.root()).enabled {
        return None;
    }
    let loaded = server.knowledge()?;
    let store = &loaded.store;
    let target = store.chunks.iter().find(|c| c.chunk_id == chunk_id)?;
    let mut sections = same_document_sections(store, &target.record_id, Some(chunk_id));
    sections.truncate(top_k);
    if sections.is_empty() {
        return Some(ToolOutput::ok(format!(
            "no same-document neighbours for {} in the knowledge store.",
            target.record_id
        )));
    }
    let blocks: Vec<(&crate::knowledge::KnowledgeChunk, String)> = sections
        .iter()
        .map(|c| {
            (*c, compress(&Registry::default(), &c.record_id, &c.content, DetailLevel::Compact))
        })
        .collect();
    let title = format!(
        "related to {} in the same document — {} section(s):",
        target.record_id,
        blocks.len()
    );
    Some(ToolOutput::ok(render_knowledge_group(&title, &blocks)))
}

/// The message for a chunk_id absent from the current index (stale after re-index).
fn stale_chunk_message(chunk_id: &str) -> String {
    format!(
        "no chunk with id {chunk_id} in the current index — it may be stale; re-run context_search."
    )
}

// --- output formatting ---

/// One row of the rendered result list (single-repo or federated).
struct Row<'a> {
    rank: usize,
    score: f64,
    package: Option<&'a str>,
    chunk_id: &'a str,
    file_path: &'a str,
    start: usize,
    end: usize,
    chunk_type: &'a str,
    kind: &'a str,
    content: &'a str,
}

impl<'a> Row<'a> {
    fn from_single(r: &'a SearchResult) -> Row<'a> {
        Row {
            rank: r.rank,
            score: r.score,
            package: None,
            chunk_id: &r.chunk_id,
            file_path: &r.file_path,
            start: r.start_line,
            end: r.end_line,
            chunk_type: &r.chunk_type,
            kind: &r.kind,
            content: &r.content,
        }
    }
    fn from_fed(r: &'a crate::federation::FedResult) -> Row<'a> {
        Row {
            rank: r.rank,
            score: r.score,
            package: Some(&r.package),
            chunk_id: &r.chunk_id,
            file_path: &r.file_path,
            start: r.start_line,
            end: r.end_line,
            chunk_type: &r.chunk_type,
            kind: &r.kind,
            content: &r.content,
        }
    }
}

/// Render the results as the SPEC-MCP §1 text block: one header line per result —
/// `#. [score] <package · >file:start-end (chunk_type/kind) #chunk_id` — followed by
/// the chunk body **served at `detail`** (L2 chunk compression, SPEC-V2.5 §2),
/// trimmed to `max_tokens` if given, then the `query_id` + progressive-disclosure
/// hint. The `#chunk_id` is what the agent passes to `expand_chunk`/`related_context`.
fn format_rows(
    rows: &[Row],
    query_id: Option<&str>,
    max_tokens: Option<usize>,
    total_chunks: usize,
    detail: DetailLevel,
) -> String {
    if rows.is_empty() {
        let mut s = format!(
            "No matching code found. The index has {total_chunks} chunk(s) — try broader or \
             different terms."
        );
        if let Some(id) = query_id {
            s.push_str(&format!("\n\nquery_id: {id}\n"));
        }
        return s;
    }

    let registry = Registry::default();
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, row) in rows.iter().enumerate() {
        // The result-header grammar is owned by `crate::grammar` (Layer 3): one
        // byte-pinned compact line per result. Rendering from it here guarantees the
        // bytes served match the bytes the `grammar` savings bucket is measured on.
        out.push_str(&crate::grammar::compact_line(&crate::grammar::GrammarRow {
            rank: row.rank,
            score6: format6(row.score),
            package: row.package.map(str::to_string),
            file_path: row.file_path.to_string(),
            start: row.start,
            end: row.end,
            chunk_type: row.chunk_type.to_string(),
            kind: row.kind.to_string(),
            chunk_id: row.chunk_id.to_string(),
        }));
        out.push('\n');
        // Serve the body at the requested detail (L2). The store keeps the full body;
        // this is a serialization-time transform only. `expand_chunk` recovers it.
        let served = compress(&registry, row.file_path, row.content, detail);
        let body = match max_tokens {
            Some(max) => {
                let (b, cut) = trim_to_tokens(&served, max.saturating_sub(used));
                truncated |= cut;
                b
            }
            None => served,
        };
        out.push_str(&body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        used += token_count(&body);
        if let Some(max) = max_tokens {
            if used >= max && i + 1 < rows.len() {
                truncated = true;
                break;
            }
        }
    }
    if truncated {
        out.push_str("… (context truncated to max_tokens)\n\n");
    }
    if detail != DetailLevel::Full {
        out.push_str(
            "Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; \
             related_context(chunk_id) for import-graph neighbours.\n",
        );
    }
    if let Some(id) = query_id {
        out.push_str(&format!("query_id: {id}\n"));
        out.push_str(&format!(
            "Rate this with record_feedback (query_id=\"{id}\", helpful=true|false).\n"
        ));
    }
    out
}

/// Trim `content` to about `max_tokens` tokens, cutting on a char boundary.
/// Returns `(text, was_truncated)`. A zero budget yields an empty string.
fn trim_to_tokens(content: &str, max_tokens: usize) -> (String, bool) {
    if max_tokens == 0 {
        return (String::new(), true);
    }
    if token_count(content) <= max_tokens {
        return (content.to_string(), false);
    }
    // token_count = floor(bytes / CHARS_PER_TOKEN); budget the byte count to match.
    let byte_budget = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    let mut end = byte_budget.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (content[..end].to_string(), true)
}

/// The friendly "index not built" message (SPEC-MCP §"Missing/empty index").
fn missing_index_message(workspace: bool) -> String {
    if workspace {
        "This workspace is not indexed yet — run `cce index --workspace` (or `cce init`). \
         No results are available until then."
            .to_string()
    } else {
        "This project is not indexed yet — run `cce index` (or `cce init`). \
         No results are available until then."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_match_the_spec_contract() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 9);
        assert_eq!(defs[0]["name"], "context_search");
        assert_eq!(defs[1]["name"], "index_status");
        assert_eq!(defs[2]["name"], "record_feedback");
        assert_eq!(defs[3]["name"], "expand_chunk");
        assert_eq!(defs[4]["name"], "related_context");
        assert_eq!(defs[5]["name"], "set_output_compression");
        assert_eq!(defs[6]["name"], "record_decision");
        assert_eq!(defs[7]["name"], "session_recall");
        assert_eq!(defs[8]["name"], "summarize_context");

        // context_search schema: required query, top_k default 8, no_graph default
        // false, and the new L2 `detail` enum (SPEC-V2.5 §6).
        let cs = &defs[0]["inputSchema"];
        assert_eq!(cs["required"], json!(["query"]));
        assert_eq!(cs["properties"]["top_k"]["default"], 8);
        assert_eq!(cs["properties"]["no_graph"]["default"], false);
        assert_eq!(cs["properties"]["detail"]["enum"], json!(["signature", "compact", "full"]));
        // Descriptions are byte-pinned (SPEC-V2.5 §6 + SPEC-V2.5-TUNING §B): the
        // exact expand-first strings, and the do-NOT-re-search rule on the two tools
        // that carry a chunk_id, so the agent expands rather than re-issuing a search.
        assert_eq!(defs[0]["description"], json!(CONTEXT_SEARCH_DESC));
        assert_eq!(defs[3]["description"], json!(EXPAND_CHUNK_DESC));
        assert_eq!(defs[4]["description"], json!(RELATED_CONTEXT_DESC));
        assert!(CONTEXT_SEARCH_DESC
            .contains("do NOT re-issue `context_search` for a target you already found"));
        assert!(EXPAND_CHUNK_DESC.contains("do NOT re-run `context_search`"));

        // record_feedback requires query_id + helpful.
        assert_eq!(defs[2]["inputSchema"]["required"], json!(["query_id", "helpful"]));

        // expand_chunk: required chunk_id, scope enum with default "body".
        let ec = &defs[3]["inputSchema"];
        assert_eq!(ec["required"], json!(["chunk_id"]));
        assert_eq!(ec["properties"]["scope"]["enum"], json!(["body", "file", "neighbors"]));
        assert_eq!(ec["properties"]["scope"]["default"], "body");

        // related_context: required chunk_id, top_k default 8.
        let rc = &defs[4]["inputSchema"];
        assert_eq!(rc["required"], json!(["chunk_id"]));
        assert_eq!(rc["properties"]["top_k"]["default"], 8);

        // set_output_compression: byte-pinned description + a required `level` enum
        // of exactly the four L4 levels (SPEC-V2.5 §2 Layer 4, §6).
        assert_eq!(defs[5]["description"], json!(SET_OUTPUT_COMPRESSION_DESC));
        let so = &defs[5]["inputSchema"];
        assert_eq!(so["required"], json!(["level"]));
        assert_eq!(so["properties"]["level"]["enum"], json!(["off", "lite", "standard", "max"]));

        // record_decision (L5): byte-pinned description + a required `text`, optional
        // `tags` (string array) and `area` (SPEC-V2.5 §2 Layer 5, §6).
        assert_eq!(defs[6]["description"], json!(RECORD_DECISION_DESC));
        let rd = &defs[6]["inputSchema"];
        assert_eq!(rd["required"], json!(["text"]));
        assert_eq!(rd["properties"]["tags"]["type"], "array");
        assert_eq!(rd["properties"]["tags"]["items"]["type"], "string");
        assert_eq!(rd["properties"]["area"]["type"], "string");
        // The anti-pollution warning is part of the pinned contract.
        assert!(RECORD_DECISION_DESC.contains("Do NOT record raw model output"));

        // session_recall (L5): byte-pinned description + required `query`, top_k
        // default 5 (a small, precision-first default).
        assert_eq!(defs[7]["description"], json!(SESSION_RECALL_DESC));
        let sr = &defs[7]["inputSchema"];
        assert_eq!(sr["required"], json!(["query"]));
        assert_eq!(sr["properties"]["top_k"]["default"], 5);
        assert!(SESSION_RECALL_DESC.contains("PRECISION-FILTERED"));

        // summarize_context (L6): byte-pinned description + an optional `scope` enum of
        // exactly the four slices, default "all", and NO required inputs (SPEC-V2.5 §2
        // Layer 6, §6).
        assert_eq!(defs[8]["description"], json!(SUMMARIZE_CONTEXT_DESC));
        let sc = &defs[8]["inputSchema"];
        assert!(sc.get("required").is_none(), "summarize_context has no required inputs");
        assert_eq!(
            sc["properties"]["scope"]["enum"],
            json!(["all", "files", "queries", "decisions"])
        );
        assert_eq!(sc["properties"]["scope"]["default"], "all");
        assert!(SUMMARIZE_CONTEXT_DESC.contains("NOT an LLM-written summary"));
    }

    #[test]
    fn summarize_context_digests_the_session_and_rejects_bad_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        // A fresh session digests to the pinned empty body.
        let empty = summarize_context(&server, &json!({}));
        assert!(!empty.is_error);
        assert_eq!(empty.text, "CCE session digest\n(nothing recorded this session yet)");

        // Record a decision, then summarize just that slice.
        record_decision(&server, &json!({ "text": "use RRF to fuse ranks" }));
        let dec = summarize_context(&server, &json!({ "scope": "decisions" }));
        assert!(!dec.is_error);
        assert!(dec.text.starts_with("CCE session digest\ndecisions (1):\n- #"));
        assert!(dec.text.contains("use RRF to fuse ranks"));

        // A blank scope means `all`; an unknown scope is an actionable error.
        let all = summarize_context(&server, &json!({ "scope": "  " }));
        assert!(!all.is_error);
        assert!(all.text.contains("decisions (1):"));
        let bad = summarize_context(&server, &json!({ "scope": "everything" }));
        assert!(bad.is_error);
        assert!(bad.text.contains("unknown scope"), "got: {}", bad.text);
    }

    #[test]
    fn record_decision_then_session_recall_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);

        let rec = record_decision(
            &server,
            &json!({ "text": "use RRF to fuse BM25 and vector ranks", "tags": ["ranking"], "area": "retriever" }),
        );
        assert!(!rec.is_error);
        assert!(rec.text.contains("Recorded decision #"), "got: {}", rec.text);

        // Recall finds it (shared tokens) and returns a compact, id-addressed entry.
        let out = session_recall(&server, &json!({ "query": "fuse BM25 vector ranks" }));
        assert!(!out.is_error);
        assert!(out.text.contains("use RRF to fuse"), "got: {}", out.text);
        assert!(out.text.contains("Recalled 1 of 1"), "got: {}", out.text);

        // A query with no lexical overlap recalls nothing (anti-pollution).
        let none = session_recall(&server, &json!({ "query": "unrelated kubernetes helm" }));
        assert!(none.text.contains("Nothing recalled"), "got: {}", none.text);
    }

    #[test]
    fn record_decision_dedupes_same_text() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        let a = record_decision(&server, &json!({ "text": "prefer small top_k for memory" }));
        let b = record_decision(&server, &json!({ "text": "prefer   small top_k for memory  " }));
        assert!(a.text.contains("Recorded decision #"));
        assert!(b.text.contains("Already recorded decision #"), "got: {}", b.text);
    }

    #[test]
    fn memory_tools_validate_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        assert!(record_decision(&server, &json!({})).is_error);
        assert!(session_recall(&server, &json!({})).is_error);
    }

    #[test]
    fn memory_tools_are_noops_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
        std::fs::write(tmp.path().join(".cce").join("config"), "memory:\n  enabled: false\n")
            .unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        let rec = record_decision(&server, &json!({ "text": "should not persist" }));
        assert!(!rec.is_error);
        assert!(rec.text.contains("Memory is disabled"), "got: {}", rec.text);
        // Nothing was written.
        assert!(!crate::memory::memory_path(tmp.path()).exists());
    }

    #[test]
    fn set_output_compression_sets_the_session_level_and_confirms() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        assert_eq!(server.output_level(), OutputLevel::Standard); // default

        let out = set_output_compression(&server, &json!({ "level": "lite" }));
        assert!(!out.is_error);
        assert!(out.text.contains("lite"), "confirmation missing level: {}", out.text);
        assert_eq!(server.output_level(), OutputLevel::Lite);

        // Every level round-trips through the tool.
        for (arg, lvl) in [
            ("off", OutputLevel::Off),
            ("max", OutputLevel::Max),
            ("standard", OutputLevel::Standard),
        ] {
            let out = set_output_compression(&server, &json!({ "level": arg }));
            assert!(!out.is_error);
            assert_eq!(server.output_level(), lvl);
        }
    }

    #[test]
    fn set_output_compression_rejects_bad_or_missing_level() {
        let tmp = tempfile::tempdir().unwrap();
        let server = McpServer::new(Some(tmp.path().to_path_buf()), None, false);
        // Unknown level ⇒ actionable error, session unchanged.
        let out = set_output_compression(&server, &json!({ "level": "turbo" }));
        assert!(out.is_error);
        assert!(out.text.contains("unknown level"), "got: {}", out.text);
        assert_eq!(server.output_level(), OutputLevel::Standard);
        // Missing level ⇒ actionable error.
        let out = set_output_compression(&server, &json!({}));
        assert!(out.is_error);
        assert!(out.text.contains("requires a `level`"), "got: {}", out.text);
        assert_eq!(server.output_level(), OutputLevel::Standard);
    }

    #[test]
    fn trim_to_tokens_caps_and_flags() {
        let long = "abcdefgh".repeat(10); // 80 bytes ~ 20 tokens
        let (short, cut) = trim_to_tokens(&long, 4);
        assert!(cut);
        assert!(short.len() <= 4 * CHARS_PER_TOKEN);
        let (whole, cut2) = trim_to_tokens("hi", 100);
        assert!(!cut2);
        assert_eq!(whole, "hi");
        let (empty, cut3) = trim_to_tokens("anything", 0);
        assert!(cut3);
        assert!(empty.is_empty());
    }

    #[test]
    fn format_rows_empty_reports_chunk_count_and_query_id() {
        let s = format_rows(&[], Some("abc123def456"), None, 7, DetailLevel::Compact);
        assert!(s.contains("The index has 7 chunk(s)"));
        assert!(s.contains("query_id: abc123def456"));
    }

    #[test]
    fn format_rows_full_renders_header_with_chunk_id_body_and_feedback_hint() {
        let rows = vec![Row {
            rank: 1,
            score: 0.5,
            package: None,
            chunk_id: "cafef00dcafef00d",
            file_path: "auth.py",
            start: 1,
            end: 3,
            chunk_type: "function",
            kind: "function_definition",
            content: "def hash_password(pw):\n    return pw\n",
        }];
        // detail:full serves the whole body and shows the chunk_id for expansion.
        let s = format_rows(&rows, Some("id0000000000"), None, 5, DetailLevel::Full);
        assert!(s.contains(
            " 1. [0.500000] auth.py:1-3 (function/function_definition) #cafef00dcafef00d"
        ));
        assert!(s.contains("def hash_password"));
        assert!(s.contains("return pw"));
        assert!(s.contains("query_id: id0000000000"));
        assert!(s.contains("record_feedback"));
    }

    #[test]
    fn format_rows_compact_reduces_body_and_hints_expansion() {
        let rows = vec![Row {
            rank: 1,
            score: 0.5,
            package: None,
            chunk_id: "cafef00dcafef00d",
            file_path: "auth.py",
            start: 1,
            end: 4,
            chunk_type: "function",
            kind: "function_definition",
            content:
                "def hash_password(pw):\n    salt = gen()\n    digest = h(pw)\n    return digest",
        }];
        let s = format_rows(&rows, Some("id0000000000"), None, 5, DetailLevel::Compact);
        // Signature + first body line + elision; the deeper lines are elided.
        assert!(s.contains("def hash_password(pw):"), "got: {s}");
        assert!(s.contains("salt = gen()"), "got: {s}");
        assert!(s.contains("… (+2 lines)"), "got: {s}");
        assert!(!s.contains("return digest"), "compact leaked the elided body: {s}");
        assert!(s.contains("expand_chunk(chunk_id"), "got: {s}");
    }

    #[test]
    fn format_rows_full_result_grammar_is_byte_pinned() {
        // The whole context_search result grammar (Layer 3): dense header line, the
        // body (served verbatim at Full), one blank-line separator, then the query_id +
        // feedback lines. No prose scaffolding, fixed order. Byte-pinned; cce-ruby
        // reconciles to exactly these bytes.
        let rows = vec![Row {
            rank: 1,
            score: 0.5,
            package: None,
            chunk_id: "cafef00dcafef00d",
            file_path: "auth.py",
            start: 1,
            end: 3,
            chunk_type: "function",
            kind: "function_definition",
            content: "def hash_password(pw):\n    return pw\n",
        }];
        let s = format_rows(&rows, Some("id0000000000"), None, 5, DetailLevel::Full);
        let golden =
            " 1. [0.500000] auth.py:1-3 (function/function_definition) #cafef00dcafef00d\n\
             def hash_password(pw):\n    return pw\n\n\
             query_id: id0000000000\n\
             Rate this with record_feedback (query_id=\"id0000000000\", helpful=true|false).\n";
        assert_eq!(s, golden);
    }

    #[test]
    fn format_rows_compact_footer_grammar_is_byte_pinned() {
        // At compact detail the grammar adds exactly one pinned expansion-hint line
        // between the results and the query_id block; the body itself is L2-compressed
        // (golden'd separately), so it is sourced from `compress` here, not hard-coded.
        let content =
            "def hash_password(pw):\n    salt = gen()\n    digest = h(pw)\n    return digest";
        let rows = vec![Row {
            rank: 1,
            score: 0.5,
            package: None,
            chunk_id: "cafef00dcafef00d",
            file_path: "auth.py",
            start: 1,
            end: 4,
            chunk_type: "function",
            kind: "function_definition",
            content,
        }];
        let s = format_rows(&rows, Some("id0000000000"), None, 5, DetailLevel::Compact);
        let body = compress(&Registry::default(), "auth.py", content, DetailLevel::Compact);
        let golden = format!(
            " 1. [0.500000] auth.py:1-4 (function/function_definition) #cafef00dcafef00d\n\
             {body}\n\n\
             Bodies shown compact. expand_chunk(chunk_id, scope=body|file|neighbors) for more; \
             related_context(chunk_id) for import-graph neighbours.\n\
             query_id: id0000000000\n\
             Rate this with record_feedback (query_id=\"id0000000000\", helpful=true|false).\n"
        );
        assert_eq!(s, golden);
    }

    #[test]
    fn render_group_result_grammar_is_byte_pinned() {
        // The expand_chunk(file|neighbors) / related_context group grammar: a title,
        // then per chunk a `file:start-end (type/kind) #id` header + body. Byte-pinned.
        let c = Chunk {
            chunk_id: "aaaa000000000000".to_string(),
            file_path: "auth.py".to_string(),
            start_line: 1,
            end_line: 2,
            chunk_type: "function".to_string(),
            kind: "function_definition".to_string(),
            language: "python".to_string(),
            content: "def a():\n    return 1".to_string(),
            token_count: 5,
            embedding: vec![],
        };
        let blocks = vec![(&c, "def a():\n    return 1".to_string())];
        let out = render_group("neighbours of auth.py — 1 chunk(s):", &blocks);
        let golden = "neighbours of auth.py — 1 chunk(s):\n\n\
             auth.py:1-2 (function/function_definition) #aaaa000000000000\n\
             def a():\n    return 1\n";
        assert_eq!(out, golden);
        // chunk_header alone is the pinned one-line header.
        assert_eq!(
            chunk_header(&c),
            "auth.py:1-2 (function/function_definition) #aaaa000000000000"
        );
    }

    #[test]
    fn format_recall_result_grammar_is_byte_pinned() {
        // The session_recall grammar: a count line, then per hit a
        // `rank. [score] #id area=… tags=…` header + the (redacted) text, then a single
        // pinned reuse note. Byte-pinned.
        let hits = vec![RecallHit {
            rank: 1,
            id: "deadbeefdeadbeef".to_string(),
            text: "use bcrypt for hashing".to_string(),
            tags: vec!["security".to_string()],
            area: Some("auth".to_string()),
            score: 0.5,
        }];
        let out = format_recall(&hits, 3);
        let golden = "Recalled 1 of 3 remembered decision(s):\n\n\
             \x201. [0.500000] #deadbeefdeadbeef area=auth tags=security\n\
             use bcrypt for hashing\n\n\
             These are validated decisions you MAY reuse — apply only what fits; \
             they are not auto-injected.\n";
        assert_eq!(out, golden);
        // The empty-recall grammar is a single pinned line, no scaffolding.
        assert_eq!(
            format_recall(&[], 3),
            "No confident memory match (3 remembered decision(s) searched). \
             Nothing recalled — proceed without memory."
        );
    }

    #[test]
    fn format_rows_workspace_prefixes_package() {
        let rows = vec![Row {
            rank: 1,
            score: 0.25,
            package: Some("billing"),
            chunk_id: "0011223344556677",
            file_path: "lib/billing.rb",
            start: 2,
            end: 4,
            chunk_type: "method",
            kind: "method",
            content: "def charge; end\n",
        }];
        let s = format_rows(&rows, None, None, 3, DetailLevel::Full);
        assert!(s.contains("billing · lib/billing.rb:2-4"), "got: {s}");
    }

    #[test]
    fn short_sha_truncates() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456789ab");
        assert_eq!(short_sha("short"), "short");
    }

    #[test]
    fn missing_messages_differ_by_mode() {
        assert!(missing_index_message(false).contains("cce index"));
        assert!(missing_index_message(true).contains("--workspace"));
    }
}
