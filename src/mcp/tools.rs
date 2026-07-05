//! # mcp::tools — the three CCE MCP tools (SPEC-MCP §"Tools")
//!
//! **Why this file exists:** The tool contract — names, input schemas, and output
//! shape — is the cross-language interface an agent binds to. It MUST be byte-for-
//! byte the same intent in cce-rust and cce-ruby so an agent gets identical tools
//! whichever engine serves. This module owns that contract and each tool's
//! read-only execution over the local index.
//!
//! **What it is / does:** Declares `context_search`, `index_status`, and
//! `record_feedback`: their JSON schemas (`tool_definitions`) and their handlers.
//! `context_search` runs the exact §6 retrieval (single-repo) or SPEC-V2.2
//! federation (workspace), logs an identical `search` metrics event to
//! `.cce/metrics.jsonl`, and renders ranked chunks + a `query_id`. `index_status`
//! reports counts + sync freshness; `record_feedback` appends a `feedback` event.
//!
//! **Responsibilities:**
//! - Own the tool schemas, output formatting, and `max_tokens` trimming.
//! - Reuse `retriever`/`federation`/`metrics`/`sync` — never reimplement them.
//! - Handle a missing/empty index with a friendly message, never a crash.

use crate::chunker::token_count;
use crate::config::CHARS_PER_TOKEN;
use crate::embedder::{format6, Embedder, HashEmbedder, OllamaEmbedder};
use crate::federation::{combined_index, federated_search, load_member_stores, workspace_stats};
use crate::mcp::server::McpServer;
use crate::mcp::MCP_DEFAULT_TOP_K;
use crate::metrics::{HexIdSource, MetricsWriter, SystemClock};
use crate::retriever::{build_search_record, search, SearchResult};
use crate::store::Index;
use crate::sync::commands::{freshness, IndexSource};
use crate::workspace::{Manifest, WorkspaceGraph};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

/// The `context_search` description, written to steer the agent to prefer it over
/// Read/Grep (SPEC-MCP §1). Mirrored verbatim in the Ruby engine.
const CONTEXT_SEARCH_DESC: &str = "PREFERRED tool for any question about THIS project's code. \
Use INSTEAD OF reading or grepping files to locate functions, understand behaviour, or answer \
'where is X / how does Y work'. Returns the most relevant code chunks (file:line + kind) from a \
hybrid vector + BM25 index, so you don't pay tokens for whole files. Reserve file reads for \
opening a specific path this tool points you to.";

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

/// The three tool definitions returned by `tools/list`, with the EXACT schemas of
/// SPEC-MCP §"Tools". The order is stable (context_search first — the headline).
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
                    "max_tokens": { "type": "integer", "description": "cap the returned context (optional)" }
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
    ]
}

// --- context_search ---

/// `context_search` (SPEC-MCP §1): ranked chunks for a natural-language query.
pub fn context_search(server: &McpServer, args: &Value) -> ToolOutput {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if query.is_empty() {
        return ToolOutput::err("context_search requires a non-empty `query`.");
    }
    let top_k = args
        .get("top_k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(MCP_DEFAULT_TOP_K);
    let no_graph = args.get("no_graph").and_then(Value::as_bool).unwrap_or(false);
    let max_tokens = args.get("max_tokens").and_then(Value::as_u64).map(|n| n as usize);
    let package = args.get("package").and_then(Value::as_str).map(|s| s.to_string());

    if server.is_workspace() {
        context_search_workspace(server, &query, top_k, no_graph, max_tokens, package)
    } else {
        context_search_single(server, &query, top_k, no_graph, max_tokens)
    }
}

/// Single-repo retrieval: the exact §6 pipeline + an identical `search` event.
fn context_search_single(
    server: &McpServer,
    query: &str,
    top_k: usize,
    no_graph: bool,
    max_tokens: Option<usize>,
) -> ToolOutput {
    let store = server.store_path();
    let index = match Index::load(&store) {
        Ok(i) => i,
        Err(_) => return ToolOutput::ok(missing_index_message(false)),
    };
    let emb = pick_embedder(&index);

    let start = Instant::now();
    let results = search(&index, emb.as_ref(), query, top_k, !no_graph);
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Identical to the CLI path: a `cce.metrics/v1` search event beside the store,
    // so `cce dashboard` shows the agent's query and token savings.
    let record = build_search_record(&index, &results, query, top_k, !no_graph, latency_ms);
    let query_id = write_search_event(&server.metrics_path(), &record);

    let rows: Vec<Row> = results.iter().map(Row::from_single).collect();
    ToolOutput::ok(format_rows(&rows, query_id.as_deref(), max_tokens, index.chunks.len()))
}

/// Workspace retrieval: SPEC-V2.2 federation over the in-scope members.
fn context_search_workspace(
    server: &McpServer,
    query: &str,
    top_k: usize,
    no_graph: bool,
    max_tokens: Option<usize>,
    package: Option<String>,
) -> ToolOutput {
    let root = server.root();
    let manifest = match Manifest::load(&root) {
        Ok(m) => m,
        Err(_) => return ToolOutput::ok(missing_index_message(true)),
    };
    let scope = package.map(|p| {
        p.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
    });
    let members = match load_member_stores(&root, &manifest, scope.as_deref()) {
        Ok(m) => m,
        // Unknown package or an unindexed member: surface the guidance, not a crash.
        Err(e) => return ToolOutput::ok(e),
    };
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);

    let uses_ollama = members.iter().any(|m| m.index.embedder_name == "ollama");
    let emb: Box<dyn Embedder> = if uses_ollama {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            Box::new(oll)
        } else {
            Box::new(HashEmbedder)
        }
    } else {
        Box::new(HashEmbedder)
    };

    let start = Instant::now();
    let results = federated_search(&members, &graph, emb.as_ref(), query, top_k, !no_graph);
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Metrics: log a search event beside the workspace-root store so the root
    // `cce dashboard` sees agent usage. Baseline/served tokens come from the union.
    let combined = combined_index(&members);
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
    let record = build_search_record(&combined, &namespaced, query, top_k, !no_graph, latency_ms);
    let query_id = write_search_event(&server.metrics_path(), &record);

    let total_chunks: usize = members.iter().map(|m| m.index.chunks.len()).sum();
    let rows: Vec<Row> = results.iter().map(Row::from_fed).collect();
    ToolOutput::ok(format_rows(&rows, query_id.as_deref(), max_tokens, total_chunks))
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

/// Pick the query embedder: the index's own backend if it is a healthy Ollama,
/// else the deterministic hash embedder (mirrors the CLI `search` path).
fn pick_embedder(index: &Index) -> Box<dyn Embedder> {
    if index.embedder_name == "ollama" {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            return Box::new(oll);
        }
    }
    Box::new(HashEmbedder)
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
    let index = match Index::load(&store) {
        Ok(i) => i,
        Err(_) => {
            return ToolOutput::ok(format!(
                "not indexed — no store at {}. Run `cce index` (or `cce init`).",
                store.display()
            ))
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

// --- output formatting ---

/// One row of the rendered result list (single-repo or federated).
struct Row<'a> {
    rank: usize,
    score: f64,
    package: Option<&'a str>,
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
/// `#. [score] <package · >file:start-end (chunk_type/kind)` — followed by the
/// chunk body, trimmed to `max_tokens` if given, then a `query_id` line.
fn format_rows(
    rows: &[Row],
    query_id: Option<&str>,
    max_tokens: Option<usize>,
    total_chunks: usize,
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

    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for (i, row) in rows.iter().enumerate() {
        let pkg = match row.package {
            Some(p) => format!("{p} · "),
            None => String::new(),
        };
        out.push_str(&format!(
            "{:>2}. [{}] {}{}:{}-{} ({}/{})\n",
            row.rank,
            format6(row.score),
            pkg,
            row.file_path,
            row.start,
            row.end,
            row.chunk_type,
            row.kind
        ));
        let body = match max_tokens {
            Some(max) => {
                let (b, cut) = trim_to_tokens(row.content, max.saturating_sub(used));
                truncated |= cut;
                b
            }
            None => row.content.to_string(),
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
        assert_eq!(defs.len(), 3);
        assert_eq!(defs[0]["name"], "context_search");
        assert_eq!(defs[1]["name"], "index_status");
        assert_eq!(defs[2]["name"], "record_feedback");

        // context_search schema: required query, top_k default 8, no_graph default false.
        let cs = &defs[0]["inputSchema"];
        assert_eq!(cs["required"], json!(["query"]));
        assert_eq!(cs["properties"]["top_k"]["default"], 8);
        assert_eq!(cs["properties"]["no_graph"]["default"], false);
        assert!(defs[0]["description"].as_str().unwrap().contains("PREFERRED"));

        // record_feedback requires query_id + helpful.
        assert_eq!(defs[2]["inputSchema"]["required"], json!(["query_id", "helpful"]));
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
        let s = format_rows(&[], Some("abc123def456"), None, 7);
        assert!(s.contains("The index has 7 chunk(s)"));
        assert!(s.contains("query_id: abc123def456"));
    }

    #[test]
    fn format_rows_renders_header_body_and_feedback_hint() {
        let rows = vec![Row {
            rank: 1,
            score: 0.5,
            package: None,
            file_path: "auth.py",
            start: 1,
            end: 3,
            chunk_type: "function",
            kind: "function_definition",
            content: "def hash_password(pw):\n    return pw\n",
        }];
        let s = format_rows(&rows, Some("id0000000000"), None, 5);
        assert!(s.contains(" 1. [0.500000] auth.py:1-3 (function/function_definition)"));
        assert!(s.contains("def hash_password"));
        assert!(s.contains("query_id: id0000000000"));
        assert!(s.contains("record_feedback"));
    }

    #[test]
    fn format_rows_workspace_prefixes_package() {
        let rows = vec![Row {
            rank: 1,
            score: 0.25,
            package: Some("billing"),
            file_path: "lib/billing.rb",
            start: 2,
            end: 4,
            chunk_type: "method",
            kind: "method",
            content: "def charge; end\n",
        }];
        let s = format_rows(&rows, None, None, 3);
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
