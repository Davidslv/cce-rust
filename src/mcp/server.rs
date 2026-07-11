//! # mcp::server — the `cce mcp` stdio dispatch loop (SPEC-MCP §"The server")
//!
//! **Why this file exists:** `cce mcp` is a long-lived MCP server the editor spawns
//! and talks to over stdin/stdout. Something must own the read-a-line/dispatch/
//! write-a-line loop, resolve the store exactly like the CLI (`--dir`/`--store`/cwd,
//! `--workspace`), best-effort warm the index via CCE Sync on startup, and route the
//! MCP methods (`initialize`, `tools/list`, `tools/call`, `ping`, and the
//! `notifications/initialized` handshake) to the tools. That is this file.
//!
//! **What it is / does:** `McpServer` holds the resolved store context. `run` drives
//! any reader/writer (so tests pipe JSON-RPC strings without a real editor);
//! `serve` wires it to the process stdio after a best-effort `sync pull --latest`.
//! `handle_line` parses one message and returns the response line (or `None` for a
//! notification).
//!
//! **Responsibilities:**
//! - Own dispatch, store/metrics/root resolution, and the sync-warm soft dependency.
//! - It is READ-ONLY over the store and never blocks or errors on a missing index,
//!   an absent remote, or an offline network.

use crate::config::{
    FooterMode, McpConfig, OutputConfig, OutputLevel, SummarizationConfig, METRICS_FILE,
};
use crate::federation::{load_cached_workspace, member_store_paths, CachedWorkspace};
use crate::knowledge::{KnowledgeStore, LoadedKnowledge};
use crate::mcp::protocol::{self, Request};
use crate::mcp::tools;
use crate::mcp::{MCP_PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
use crate::session::{SessionLedger, SummaryScope};
use crate::store::{default_metrics_path, default_store_path, Index};
use crate::sync::commands::{cmd_pull, PullTarget};
use crate::sync::config::SyncConfig;
use crate::tokenizer::estimate_tokens;
use crate::workspace::Manifest;
use serde_json::{json, Value};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

/// A cheap freshness fingerprint of one store file: `(mtime, len)` from a single
/// `fs::metadata` call (issue #31). A re-index or a `cce sync pull` replaces the file,
/// changing at least one of the two, so a stale cache is dropped on the next call.
type Fingerprint = (SystemTime, u64);

/// The fingerprint of `path`, or `None` if the file is missing/unreadable — the
/// signal to drop any cache entry derived from it (never serve a deleted store).
fn file_fingerprint(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The combined fingerprint of the file(s) `KnowledgeStore::load_current` reads: the
/// `current` pointer and the snapshot artifact it names. A re-ingest rewrites both.
fn knowledge_fingerprint(root: &Path) -> Option<Vec<Fingerprint>> {
    let pointer = KnowledgeStore::current_pointer_path(root);
    let pointer_fp = file_fingerprint(&pointer)?;
    let snapshot = std::fs::read_to_string(&pointer).ok()?;
    let snapshot_fp = file_fingerprint(&KnowledgeStore::snapshot_path(root, snapshot.trim()))?;
    Some(vec![pointer_fp, snapshot_fp])
}

/// One cached workspace bundle plus the member-store fingerprints it was built from.
type FingerprintedWorkspace = (Vec<Fingerprint>, Rc<CachedWorkspace>);

/// The resolved context for a CCE MCP session (SPEC-MCP §"The server").
pub struct McpServer {
    /// `--dir`: the project directory (also the sync/metrics root when set).
    dir: Option<PathBuf>,
    /// `--store`: an explicit index path (overrides `--dir`/cwd resolution).
    store: Option<PathBuf>,
    /// `--workspace`: federate over the workspace members (SPEC-V2.2).
    workspace: bool,
    /// L4 session output-compression preference (SPEC-V2.5 §2 Layer 4). Seeded from
    /// the project's `output.level` config and switched at runtime by the
    /// `set_output_compression` tool. In-memory only — never rewrites CLAUDE.md, and
    /// resets when the server restarts. `Cell` because the stdio loop is
    /// single-threaded and every handler takes `&self`.
    output_level: Cell<OutputLevel>,
    /// L6 per-session ledger (SPEC-V2.5 §2 Layer 6): an order-preserving,
    /// wall-clock-free record of THIS session's context_search / expand_chunk /
    /// related_context / record_decision calls, which `summarize_context` renders into
    /// a deterministic digest. In-memory only — a fresh server starts empty, so it
    /// never leaks across sessions. `RefCell` (not `Cell`) because it grows in place.
    ledger: RefCell<SessionLedger>,
    /// L6 auto-trigger threshold (SPEC-V2.5 §5, `summarization.auto_tokens`). Seeded
    /// from the project's `.cce/config`; `None` ⇒ manual only (the default). Never
    /// drives a model call — only the deterministic, offline [`Self::auto_summarize_due`]
    /// signal derived from `served_tokens`.
    auto_tokens: Option<u64>,
    /// Running total of tokens THIS session has served to the agent, counted with the
    /// one `cce.tokens/v1` estimator over every tool's returned text (SPEC-V2.5 §4).
    /// Deterministic given the call sequence; backs the auto-summarize threshold.
    served_tokens: Cell<u64>,
    /// The MCP result-footer mode (SPEC-USAGE-VISIBILITY §3, v2.8). Seeded from the
    /// project's `.cce/config` `mcp.result_footer` at startup — config-only,
    /// deliberately: there is no runtime tool to flip it, so the agent cannot
    /// toggle its own observability mid-session. Default: off.
    result_footer: FooterMode,
    /// Session usage counters behind the footer's `session` clause (v2.8): how many
    /// searches THIS session recorded and their summed `tokens_saved` — values read
    /// straight off each already-built `search` record (pure projection, in-memory
    /// only, never persisted). Accrued regardless of the footer mode so `session`
    /// reads the same totals whenever it is enabled.
    session_searches: Cell<u64>,
    session_tokens_saved: Cell<u64>,
    /// Federated-workspace cache (issue #26), keyed by scope. Assembling the union of
    /// the members' stores (load + BM25-over-union) is the entire cost of a federated
    /// query, so the long-lived server builds it once per distinct `--package` scope and
    /// reuses it across `context_search` calls — a warm workspace call then matches the
    /// CLI instead of re-federating every time. Each bundle carries the fingerprints of
    /// its in-scope member store files (issue #31): a member re-index or a mid-session
    /// `cce sync pull` changes a fingerprint, so the next call rebuilds the union
    /// instead of serving a stale one. `RefCell` because the stdio loop is
    /// single-threaded and every handler takes `&self`.
    workspace_cache: RefCell<HashMap<String, FingerprintedWorkspace>>,
    /// Single-repo index cache (issue #31). `Index::load` reads + JSON-parses the whole
    /// store AND rebuilds BM25 + the import graph — O(corpus) work that the long-lived
    /// server previously repeated on EVERY tool call. Cached once and reused while the
    /// store file's fingerprint (mtime+len) is unchanged; a re-index or `cce sync pull`
    /// invalidates it on the next call, and a deleted store drops it (never stale).
    index_cache: RefCell<Option<(Fingerprint, Rc<Index>)>>,
    /// Knowledge-store cache (issue #31): the parsed store + its lazily built ranking
    /// index, reused across calls under the same fingerprint rule (over the `current`
    /// pointer and the snapshot artifact it names). A re-ingest supersedes on the next
    /// call; a deleted store behaves exactly like no knowledge store at all.
    knowledge_cache: RefCell<Option<(Vec<Fingerprint>, Rc<LoadedKnowledge>)>>,
}

impl McpServer {
    /// Construct from the resolved CLI options. The session output level is seeded
    /// from the project's `.cce/config` `output.level` (absent ⇒ the default).
    pub fn new(dir: Option<PathBuf>, store: Option<PathBuf>, workspace: bool) -> Self {
        let root = dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let output_level = Cell::new(OutputConfig::load(&root).level);
        let auto_tokens = SummarizationConfig::load(&root).auto_tokens;
        let result_footer = McpConfig::load(&root).result_footer;
        McpServer {
            dir,
            store,
            workspace,
            output_level,
            ledger: RefCell::new(SessionLedger::new()),
            auto_tokens,
            served_tokens: Cell::new(0),
            result_footer,
            session_searches: Cell::new(0),
            session_tokens_saved: Cell::new(0),
            workspace_cache: RefCell::new(HashMap::new()),
            index_cache: RefCell::new(None),
            knowledge_cache: RefCell::new(None),
        }
    }

    /// The loaded single-repo `Index`, cached across tool calls (issue #31). One
    /// `fs::metadata` per call decides freshness: an unchanged fingerprint reuses the
    /// cached index (skipping the store parse + BM25/graph rebuild); a changed one —
    /// re-index, `cce sync pull` (startup auto-pull or mid-session) — reloads; a
    /// missing store drops the cache and errors exactly like `Index::load` today, so
    /// a deleted store is never served stale.
    pub fn load_index(&self) -> std::io::Result<Rc<Index>> {
        let store = self.store_path();
        let fp = match file_fingerprint(&store) {
            Some(fp) => fp,
            None => {
                self.index_cache.borrow_mut().take();
                // Missing/unreadable store: surface today's load error unchanged.
                return Index::load(&store).map(Rc::new);
            }
        };
        if let Some((cached_fp, index)) = self.index_cache.borrow().as_ref() {
            if *cached_fp == fp {
                return Ok(Rc::clone(index));
            }
        }
        match Index::load(&store) {
            Ok(index) => {
                let index = Rc::new(index);
                *self.index_cache.borrow_mut() = Some((fp, Rc::clone(&index)));
                Ok(index)
            }
            Err(e) => {
                // Unparseable store: drop any cached copy rather than serve stale.
                self.index_cache.borrow_mut().take();
                Err(e)
            }
        }
    }

    /// The loaded knowledge store (+ lazily built ranking index), cached across tool
    /// calls (issue #31) under the fingerprint of the files `load_current` reads (the
    /// `current` pointer + the snapshot artifact). `None` behaves exactly like
    /// `KnowledgeStore::load_current` failing today: no store, or an unreadable one.
    /// A re-ingest changes both files, so the next call reloads; a deleted store
    /// drops the cache — never a stale answer.
    pub fn knowledge(&self) -> Option<Rc<LoadedKnowledge>> {
        let root = self.root();
        let fp = match knowledge_fingerprint(&root) {
            Some(fp) => fp,
            None => {
                self.knowledge_cache.borrow_mut().take();
                return None;
            }
        };
        if let Some((cached_fp, k)) = self.knowledge_cache.borrow().as_ref() {
            if *cached_fp == fp {
                return Some(Rc::clone(k));
            }
        }
        match KnowledgeStore::load_current(&root) {
            Ok(store) => {
                let k = Rc::new(LoadedKnowledge::new(store));
                *self.knowledge_cache.borrow_mut() = Some((fp, Rc::clone(&k)));
                Some(k)
            }
            Err(_) => {
                self.knowledge_cache.borrow_mut().take();
                None
            }
        }
    }

    /// The assembled federated workspace for `scope` (issue #26), built once per distinct
    /// scope and cached while its member stores are unchanged. The first `context_search`
    /// in a scope pays the load+union cost; later calls reuse the same `CachedWorkspace`
    /// (members + union index), so a warm federated query is as fast as a single-repo one.
    /// Freshness (issue #31) is the combined fingerprint (mtime+len) of the in-scope
    /// member store files: a member re-index or a mid-session `cce sync pull` changes it,
    /// so the next call rebuilds the union; a missing member store drops the entry and
    /// surfaces today's "not indexed" guidance — never a stale union. `scope` is `None`
    /// for the whole workspace or the `--package` member list otherwise; the cache key is
    /// order-sensitive so it maps 1:1 to what `load_member_stores` selects. Errors
    /// (missing member, unknown package) are NOT cached — they are cheap and may be
    /// user-fixable mid-session.
    pub fn workspace_bundle(
        &self,
        manifest: &Manifest,
        scope: Option<&[String]>,
    ) -> Result<Rc<CachedWorkspace>, String> {
        let key = match scope {
            None => "\u{0}all".to_string(),
            Some(names) => format!("scope\u{0}{}", names.join("\u{0}")),
        };
        // The freshness fingerprint of the in-scope member stores. An unknown
        // member/package errors here with the same message as the loader; a missing
        // store file yields `None` (fall through to the loader's guidance).
        let store_paths = member_store_paths(&self.root(), manifest, scope)?;
        let fp: Option<Vec<Fingerprint>> =
            store_paths.iter().map(|p| file_fingerprint(p)).collect();
        match &fp {
            Some(fp) => {
                if let Some((cached_fp, bundle)) = self.workspace_cache.borrow().get(&key) {
                    if cached_fp == fp {
                        return Ok(Rc::clone(bundle));
                    }
                }
            }
            // A member store vanished: drop the stale bundle before erroring below.
            None => {
                self.workspace_cache.borrow_mut().remove(&key);
            }
        }
        let bundle = Rc::new(load_cached_workspace(&self.root(), manifest, scope)?);
        if let Some(fp) = fp {
            self.workspace_cache.borrow_mut().insert(key, (fp, Rc::clone(&bundle)));
        }
        Ok(bundle)
    }

    /// Whether this session federates over a workspace.
    pub fn is_workspace(&self) -> bool {
        self.workspace
    }

    /// The current L4 session output-compression level (SPEC-V2.5 §2 Layer 4).
    pub fn output_level(&self) -> OutputLevel {
        self.output_level.get()
    }

    /// Set the L4 session output-compression level (in-memory session state, set by
    /// the `set_output_compression` tool). Does NOT rewrite CLAUDE.md.
    pub fn set_output_level(&self, level: OutputLevel) {
        self.output_level.set(level);
    }

    // --- L6 turn summarization (SPEC-V2.5 §2 Layer 6) ---

    /// Record a `context_search` in the session ledger (its query + the ids/paths it
    /// returned). Order-preserving; no wall-clock.
    pub fn record_search(&self, query: &str, chunk_ids: &[String], file_paths: &[String]) {
        self.ledger.borrow_mut().record_search(query, chunk_ids, file_paths);
    }

    /// Record an `expand_chunk` (chunk_id + scope) in the session ledger.
    pub fn record_expand(&self, chunk_id: &str, scope: &str) {
        self.ledger.borrow_mut().record_expand(chunk_id, scope);
    }

    /// Record a `related_context` (chunk_id) in the session ledger.
    pub fn record_related(&self, chunk_id: &str) {
        self.ledger.borrow_mut().record_related(chunk_id);
    }

    /// Record a `record_decision` (id + a short, already-redacted label) in the ledger.
    pub fn record_decision_event(&self, id: &str, label: &str) {
        self.ledger.borrow_mut().record_decision(id, label);
    }

    /// The byte-pinned session digest for `scope` (what `summarize_context` returns).
    pub fn session_digest(&self, scope: SummaryScope) -> String {
        self.ledger.borrow().digest(scope)
    }

    /// The number of tool calls recorded in the session ledger so far.
    pub fn ledger_len(&self) -> usize {
        self.ledger.borrow().len()
    }

    /// Add `text`'s `cce.tokens/v1` estimate to the running served-token total. Called
    /// for every tool's returned text so the auto-summarize threshold is exact.
    pub fn note_served_tokens(&self, text: &str) {
        self.served_tokens.set(self.served_tokens.get() + estimate_tokens(text));
    }

    /// The running total of tokens served to the agent this session (`cce.tokens/v1`).
    pub fn served_tokens(&self) -> u64 {
        self.served_tokens.get()
    }

    /// The configured L6 auto-trigger threshold (`summarization.auto_tokens`); `None`
    /// ⇒ manual only.
    pub fn auto_tokens(&self) -> Option<u64> {
        self.auto_tokens
    }

    /// The configured MCP result-footer mode (SPEC-USAGE-VISIBILITY §3): read from
    /// `.cce/config` at startup, immutable for the session (config-only, no tool).
    pub fn footer_mode(&self) -> FooterMode {
        self.result_footer
    }

    /// Accrue one recorded search's `tokens_saved` into the session usage counters
    /// (v2.8). Called once per `context_search` that built a `search` record; the
    /// values are the record's own — no new accounting, nothing persisted.
    pub fn note_search_usage(&self, tokens_saved: u64) {
        self.session_searches.set(self.session_searches.get() + 1);
        self.session_tokens_saved.set(self.session_tokens_saved.get() + tokens_saved);
    }

    /// THIS session's `(searches, tokens_saved)` running totals — what the footer's
    /// `session` clause prints (including the call that just accrued).
    pub fn session_usage(&self) -> (u64, u64) {
        (self.session_searches.get(), self.session_tokens_saved.get())
    }

    /// Whether the session's served-token total has crossed the configured
    /// `summarization.auto_tokens` threshold — a deterministic, OFFLINE signal (no
    /// model call, no wall-clock). `None` threshold ⇒ never due (manual only). v1 does
    /// not auto-invoke `summarize_context` or auto-inject a digest; this predicate is
    /// exposed for the harness and Ruby's later catch-up.
    pub fn auto_summarize_due(&self) -> bool {
        self.auto_tokens.is_some_and(|t| self.served_tokens() >= t)
    }

    /// The project root (for sync + workspace + default metrics): `--dir` or cwd.
    pub fn root(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| PathBuf::from("."))
    }

    /// The single-repo store path: `--store`, else `--dir/.cce/index.json`, else
    /// `./.cce/index.json` (identical to the CLI's read-store resolution).
    pub fn store_path(&self) -> PathBuf {
        if let Some(s) = &self.store {
            s.clone()
        } else if let Some(d) = &self.dir {
            default_store_path(d)
        } else {
            default_store_path(Path::new("."))
        }
    }

    /// The metrics-log path (the search/feedback events land here): beside an
    /// explicit `--store`, else `<root>/.cce/metrics.jsonl`. In workspace mode it
    /// is always the root log, so the root `cce dashboard` sees agent usage.
    pub fn metrics_path(&self) -> PathBuf {
        if self.workspace {
            return default_metrics_path(&self.root());
        }
        match &self.store {
            Some(s) => match s.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.join(METRICS_FILE),
                _ => PathBuf::from(METRICS_FILE),
            },
            None => default_metrics_path(&self.root()),
        }
    }

    /// Best-effort warm/refresh via CCE Sync (SPEC-MCP §"CCE MCP × CCE Sync"). If a
    /// sync remote is configured AND `sync.auto_pull` is on, pull the latest CI-built
    /// index before serving. Soft dependency: any error (no remote, offline, cache
    /// miss, sha mismatch) is swallowed — MCP always falls back to the local index.
    pub fn warm_via_sync(&self) {
        let root = self.root();
        let cfg = SyncConfig::load(&root);
        if cfg.remote.is_some() && cfg.auto_pull {
            // force = false: never clobber a WIP local cache; a mismatch just no-ops.
            let _ = cmd_pull(&root, PullTarget::Latest, false, self.workspace, None);
        }
    }

    /// Serve over the process stdio until EOF, warming the index first.
    pub fn serve(&self) -> std::io::Result<()> {
        self.warm_via_sync();
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.run(stdin.lock(), stdout.lock())
    }

    /// Drive the server over any reader/writer: read newline-delimited JSON-RPC,
    /// dispatch, and write each response line. Notifications produce no output.
    pub fn run<R: BufRead, W: Write>(&self, mut reader: R, mut writer: W) -> std::io::Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            // Read raw bytes, not a `String`: one stray non-UTF-8 byte on a line
            // must NOT propagate an `InvalidData` error out of the loop and tear down
            // the long-lived session (#124). Read to the newline, then validate.
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                break; // EOF: the editor closed the pipe.
            }
            let response = match std::str::from_utf8(&buf) {
                // Non-UTF-8 bytes are a JSON-RPC parse error, never a fatal stream
                // error: JSON mandates UTF-8, so answer -32700 and keep serving the
                // next request instead of exiting (#124).
                Err(_) => Some(protocol::error(Value::Null, protocol::PARSE_ERROR, "parse error")),
                Ok(line) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        self.handle_line(trimmed)
                    }
                }
            };
            if let Some(response) = response {
                writer.write_all(response.as_bytes())?;
                writer.write_all(b"\n")?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    /// Handle one message line. Returns the response JSON string, or `None` for a
    /// notification (which must not be answered) or an unparseable blank.
    pub fn handle_line(&self, line: &str) -> Option<String> {
        let req = match protocol::parse_request(line) {
            Ok(r) => r,
            // Non-JSON: id is undeterminable ⇒ -32700 with a null id.
            Err(protocol::ParseError::Parse) => {
                return Some(protocol::error(Value::Null, protocol::PARSE_ERROR, "parse error"))
            }
            // Valid JSON but not a well-formed request: echo the recoverable id and
            // return -32600 so the client can correlate the error with its pending
            // call instead of being orphaned on a null id (#125).
            Err(protocol::ParseError::InvalidRequest { id }) => {
                return Some(protocol::error(id, protocol::INVALID_REQUEST, "invalid request"))
            }
        };
        if req.is_notification() {
            // `notifications/initialized` (and any other notification) needs no reply.
            return None;
        }
        // Safe: a non-notification carries an id.
        let id = req.id.clone().unwrap_or(Value::Null);
        let result = self.dispatch(&req);
        Some(match result {
            Ok(value) => protocol::success(&id, value),
            Err((code, message)) => protocol::error(id, code, &message),
        })
    }

    /// Route a request method to its handler.
    fn dispatch(&self, req: &Request) -> Result<Value, (i64, String)> {
        match req.method.as_str() {
            "initialize" => Ok(self.initialize_result()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::tool_definitions() })),
            "tools/call" => self.tools_call(&req.params),
            other => Err((protocol::METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }

    /// The `initialize` result: protocol version, tool capability, and identity.
    fn initialize_result(&self) -> Value {
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        })
    }

    /// Execute a `tools/call`: look up the tool by name and return its content
    /// result. An unknown tool is a tool-level error (`isError`), not a protocol
    /// error, so the agent sees a clear message and keeps its session.
    fn tools_call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((protocol::INVALID_PARAMS, "tools/call requires a tool `name`".to_string()))?;
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        let output = match name {
            "context_search" => tools::context_search(self, &args),
            "index_status" => tools::index_status(self),
            "record_feedback" => tools::record_feedback(self, &args),
            "expand_chunk" => tools::expand_chunk(self, &args),
            "related_context" => tools::related_context(self, &args),
            "set_output_compression" => tools::set_output_compression(self, &args),
            "record_decision" => tools::record_decision(self, &args),
            "session_recall" => tools::session_recall(self, &args),
            "summarize_context" => tools::summarize_context(self, &args),
            other => {
                return Ok(unknown_tool_result(other));
            }
        };
        // L6: account every served tool result against the session's returned-token
        // total (SPEC-V2.5 §4/§2 Layer 6), the offline signal behind auto_summarize_due.
        self.note_served_tokens(&output.text);
        Ok(output.to_content())
    }
}

/// An `isError` tool result for an unrecognised tool name.
fn unknown_tool_result(name: &str) -> Value {
    json!({
        "content": [ { "type": "text", "text": format!("unknown tool: {name}") } ],
        "isError": true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use crate::store::Index;
    use std::io::Cursor;
    use std::path::PathBuf;

    /// Build a hash index of the base fixture into `dir/.cce/index.json`.
    fn index_fixture(dir: &Path) {
        let fixture = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"));
        let (idx, _) = Index::build_from_dir(&fixture, &HashEmbedder).unwrap();
        idx.save(&default_store_path(dir)).unwrap();
    }

    fn server_for(dir: &Path) -> McpServer {
        McpServer::new(Some(dir.to_path_buf()), None, false)
    }

    /// Push `path`'s mtime clearly forward, so a rewrite within the same clock tick
    /// still changes the freshness fingerprint on filesystems with coarse mtimes.
    fn bump_mtime(path: &Path) {
        let f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
    }

    /// Run a `context_search` for `query` (no_graph, so results are query-only) and
    /// return the served text block.
    fn search_text(s: &McpServer, query: &str) -> String {
        let resp = s
            .handle_line(&format!(
                r#"{{"id":1,"method":"tools/call","params":{{"name":"context_search","arguments":{{"query":"{query}","no_graph":true}}}}}}"#,
            ))
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        v["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// Drop the per-call metrics lines (`query_id` is a fresh id each call by design);
    /// the ranked result block itself must be byte-identical warm vs cold.
    fn strip_query_id(t: &str) -> String {
        t.lines()
            .filter(|l| !l.starts_with("query_id:") && !l.starts_with("Rate this"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn single_repo_context_search_is_cached_and_warm_call_is_identical() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());

        // Cold call loads + caches the index.
        let cold = search_text(&s, "hash password");
        assert!(cold.contains("auth.py"), "expected auth.py, got: {cold}");
        let cold_ptr = Rc::as_ptr(&s.index_cache.borrow().as_ref().unwrap().1);

        // Warm call reuses the SAME cached index (pointer-identical) and serves a
        // byte-identical ranked block.
        let warm = search_text(&s, "hash password");
        let warm_ptr = Rc::as_ptr(&s.index_cache.borrow().as_ref().unwrap().1);
        assert_eq!(cold_ptr, warm_ptr, "warm call must reuse the cached index");
        assert_eq!(strip_query_id(&cold), strip_query_id(&warm));
    }

    #[test]
    fn single_repo_reindex_is_picked_up_on_the_next_call() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        assert!(search_text(&s, "hash password").contains("auth.py"));
        let cold_ptr = Rc::as_ptr(&s.index_cache.borrow().as_ref().unwrap().1);

        // Re-index from a DIFFERENT corpus over the same store path (what a re-index
        // or a mid-session `cce sync pull` does), bumping mtime explicitly so the
        // fingerprint change never depends on filesystem clock granularity.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("widgets.py"), "def frobnicate_widget():\n    return 42\n")
            .unwrap();
        let (idx, _) = Index::build_from_dir(src.path(), &HashEmbedder).unwrap();
        let store = default_store_path(tmp.path());
        idx.save(&store).unwrap();
        bump_mtime(&store);

        // The next call reflects the NEW store, and the cache was swapped.
        let after = search_text(&s, "frobnicate widget");
        assert!(after.contains("widgets.py"), "re-index not picked up: {after}");
        assert!(!after.contains("auth.py"), "stale corpus served: {after}");
        let new_ptr = Rc::as_ptr(&s.index_cache.borrow().as_ref().unwrap().1);
        assert_ne!(cold_ptr, new_ptr, "a changed fingerprint must reload the index");
    }

    #[test]
    fn deleting_the_store_mid_session_is_friendly_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        assert!(search_text(&s, "hash password").contains("auth.py"));
        assert!(s.index_cache.borrow().is_some(), "cold call must populate the cache");

        std::fs::remove_file(default_store_path(tmp.path())).unwrap();

        // Exactly today's behaviour: the friendly missing-index message — never the
        // cached (now deleted) index — and the cache is dropped.
        let after = search_text(&s, "hash password");
        assert!(after.contains("not indexed"), "got: {after}");
        assert!(!after.contains("auth.py"), "stale cached index served: {after}");
        assert!(s.index_cache.borrow().is_none(), "a deleted store must drop the cache");

        let resp = s
            .handle_line(r#"{"id":2,"method":"tools/call","params":{"name":"index_status"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("not indexed"));
    }

    /// Write + ingest a one-record knowledge feed whose body carries `sentence`.
    fn ingest_knowledge(root: &Path, id: &str, title: &str, sentence: &str) {
        let feed = root.join("feed.jsonl");
        std::fs::write(
            &feed,
            format!(
                "{{\"id\":\"{id}\",\"title\":\"{title}\",\"body\":\"## Rule\\n\\n{sentence}\",\"source\":\"github-issues\"}}\n"
            ),
        )
        .unwrap();
        crate::knowledge::ingest_file(&feed, root, 400).unwrap();
    }

    /// A knowledge-only `context_search`, returning the served text block.
    fn knowledge_search_text(s: &McpServer, query: &str) -> String {
        let resp = s
            .handle_line(&format!(
                r#"{{"id":1,"method":"tools/call","params":{{"name":"context_search","arguments":{{"query":"{query}","source":"knowledge"}}}}}}"#,
            ))
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        v["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    #[test]
    fn knowledge_search_is_cached_warm_identical_and_reingest_invalidates() {
        let tmp = tempfile::tempdir().unwrap();
        ingest_knowledge(
            tmp.path(),
            "gh:1",
            "Login policy",
            "Lock the account after five failed login attempts.",
        );
        let s = server_for(tmp.path());

        // Cold call loads + caches the knowledge store; warm call reuses it
        // (pointer-identical) and serves byte-identical text (knowledge-only
        // searches log no query_id, so the whole block must match).
        let cold = knowledge_search_text(&s, "login attempts lock account");
        assert!(cold.contains("Login policy"), "got: {cold}");
        let cold_ptr = Rc::as_ptr(&s.knowledge_cache.borrow().as_ref().unwrap().1);
        let warm = knowledge_search_text(&s, "login attempts lock account");
        let warm_ptr = Rc::as_ptr(&s.knowledge_cache.borrow().as_ref().unwrap().1);
        assert_eq!(cold_ptr, warm_ptr, "warm call must reuse the cached knowledge store");
        assert_eq!(cold, warm, "warm knowledge search must equal the cold one");

        // Re-ingest a superseding snapshot (new artifact + rewritten pointer), with an
        // explicit mtime bump on the pointer so granularity can never mask the change.
        ingest_knowledge(
            tmp.path(),
            "gh:2",
            "Refund policy",
            "Refund a captured login charge within thirty days.",
        );
        bump_mtime(&KnowledgeStore::current_pointer_path(tmp.path()));
        let after = knowledge_search_text(&s, "refund captured charge days");
        assert!(after.contains("Refund policy"), "re-ingest not picked up: {after}");
        assert!(!after.contains("Login policy"), "stale knowledge served: {after}");

        // Deleting the knowledge store behaves exactly like no store at all.
        std::fs::remove_dir_all(KnowledgeStore::dir(tmp.path())).unwrap();
        let gone = knowledge_search_text(&s, "refund captured charge days");
        assert!(!gone.contains("Refund policy"), "stale knowledge after delete: {gone}");
        assert!(s.knowledge_cache.borrow().is_none(), "a deleted store must drop the cache");
    }

    #[test]
    fn initialize_advertises_protocol_and_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp =
            s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(v["result"]["serverInfo"]["name"], "cce");
        assert_eq!(v["result"]["serverInfo"]["version"], SERVER_VERSION);
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialized_notification_gets_no_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        assert!(s
            .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .is_none());
    }

    #[test]
    fn ping_returns_empty_result() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s.handle_line(r#"{"id":9,"method":"ping"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"], json!({}));
        assert_eq!(v["id"], 9);
    }

    #[test]
    fn tools_list_returns_the_nine_tools_in_fixed_order() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s.handle_line(r#"{"id":2,"method":"tools/list"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "context_search",
                "index_status",
                "record_feedback",
                "expand_chunk",
                "related_context",
                "set_output_compression",
                "record_decision",
                "session_recall",
                "summarize_context"
            ]
        );
    }

    #[test]
    fn record_decision_then_session_recall_over_json_rpc() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        // record_decision appends a memory entry.
        let rec = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"record_decision","arguments":{"text":"cache invalidation uses versioned keys","area":"cache"}}}"#,
            )
            .unwrap();
        let rv: Value = serde_json::from_str(&rec).unwrap();
        assert_eq!(rv["result"]["isError"], false);
        assert!(rv["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Recorded decision #"));

        // session_recall retrieves it.
        let rc = s
            .handle_line(
                r#"{"id":2,"method":"tools/call","params":{"name":"session_recall","arguments":{"query":"cache invalidation keys"}}}"#,
            )
            .unwrap();
        let cv: Value = serde_json::from_str(&rc).unwrap();
        assert_eq!(cv["result"]["isError"], false);
        assert!(cv["result"]["content"][0]["text"].as_str().unwrap().contains("versioned keys"));

        // The memory store exists and holds exactly one entry.
        let entries = crate::memory::load_entries(&[crate::memory::memory_path(tmp.path())]);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn set_output_compression_changes_the_session_level() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        // Default (no config) is standard.
        assert_eq!(s.output_level(), OutputLevel::Standard);
        let resp = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"set_output_compression","arguments":{"level":"max"}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("max"));
        assert_eq!(s.output_level(), OutputLevel::Max);
    }

    #[test]
    fn session_ledger_accumulates_in_order_and_summarize_context_digests_it() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        // Two searches, an expand, a decision — then summarize.
        s.handle_line(
            r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"hash password","no_graph":true}}}"#,
        )
        .unwrap();
        s.handle_line(
            r#"{"id":2,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"validate token","no_graph":true}}}"#,
        )
        .unwrap();
        s.handle_line(
            r#"{"id":3,"method":"tools/call","params":{"name":"record_decision","arguments":{"text":"store password hashes with bcrypt"}}}"#,
        )
        .unwrap();
        // The ledger accumulated the 3 context-touching calls in order (the decision
        // records one event; the two searches record one each).
        assert!(s.ledger_len() >= 3);

        let resp = s
            .handle_line(
                r#"{"id":4,"method":"tools/call","params":{"name":"summarize_context","arguments":{}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("CCE session digest"));
        assert!(text.contains("queries (2):"));
        assert!(text.contains("- hash password"));
        assert!(text.contains("- validate token"));
        assert!(text.contains("decisions (1):"));
        assert!(text.contains("store password hashes with bcrypt"));
    }

    #[test]
    fn a_fresh_server_has_an_empty_ledger_so_it_does_not_leak_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        // First "session": record activity.
        let s1 = server_for(tmp.path());
        s1.handle_line(
            r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"hash password"}}}"#,
        )
        .unwrap();
        assert!(s1.ledger_len() >= 1);

        // A brand-new server (a fresh session/process) starts empty and summarizes to
        // the pinned empty body — the previous session's ledger did not leak.
        let s2 = server_for(tmp.path());
        assert_eq!(s2.ledger_len(), 0);
        let resp = s2
            .handle_line(r#"{"id":1,"method":"tools/call","params":{"name":"summarize_context"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(
            v["result"]["content"][0]["text"].as_str().unwrap(),
            "CCE session digest\n(nothing recorded this session yet)"
        );
    }

    #[test]
    fn summarize_context_unknown_scope_is_an_actionable_error_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"summarize_context","arguments":{"scope":"everything"}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("unknown scope"));
    }

    #[test]
    fn served_tokens_and_auto_summarize_due_track_the_config_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        // Default config ⇒ manual only ⇒ never due, whatever is served.
        let s = server_for(tmp.path());
        assert_eq!(s.auto_tokens(), None);
        s.handle_line(
            r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"hash password"}}}"#,
        )
        .unwrap();
        assert!(s.served_tokens() > 0, "a served search must count tokens");
        assert!(!s.auto_summarize_due(), "manual-only config is never auto-due");

        // A tiny threshold ⇒ due after the first served result.
        std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
        std::fs::write(
            tmp.path().join(".cce").join("config"),
            "summarization:\n  auto_tokens: 1\n",
        )
        .unwrap();
        let s2 = server_for(tmp.path());
        assert_eq!(s2.auto_tokens(), Some(1));
        assert!(!s2.auto_summarize_due(), "nothing served yet");
        s2.handle_line(
            r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"hash password"}}}"#,
        )
        .unwrap();
        assert!(s2.auto_summarize_due(), "served-token total crossed the threshold");
    }

    #[test]
    fn set_output_compression_bad_level_is_an_actionable_error_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"set_output_compression","arguments":{"level":"turbo"}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("unknown level"));
        // The session level is unchanged by a bad call.
        assert_eq!(s.output_level(), OutputLevel::Standard);
    }

    #[test]
    fn unknown_method_is_a_protocol_error() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s.handle_line(r#"{"id":3,"method":"no/such"}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], protocol::METHOD_NOT_FOUND);
    }

    #[test]
    fn bad_json_is_a_parse_error_with_null_id() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s.handle_line("definitely not json").unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], protocol::PARSE_ERROR);
        assert!(v["id"].is_null());
    }

    #[test]
    fn methodless_request_echoes_its_id_as_invalid_request_not_a_null_id_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());

        // Valid JSON, has an id, but `method` is not a string: the id is recoverable,
        // so the client must get -32600 with id 7 echoed — not -32700 with id null,
        // which would orphan its pending request (#125).
        let resp = s.handle_line(r#"{"jsonrpc":"2.0","id":7,"method":5}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], protocol::INVALID_REQUEST);
        assert_eq!(v["id"], 7);

        // Method entirely absent but id present ⇒ same treatment, string id echoed.
        let resp2 = s.handle_line(r#"{"id":"abc"}"#).unwrap();
        let v2: Value = serde_json::from_str(&resp2).unwrap();
        assert_eq!(v2["error"]["code"], protocol::INVALID_REQUEST);
        assert_eq!(v2["id"], "abc");

        // Truly non-JSON remains a -32700 parse error with a null id (unchanged).
        let resp3 = s.handle_line("not json at all").unwrap();
        let v3: Value = serde_json::from_str(&resp3).unwrap();
        assert_eq!(v3["error"]["code"], protocol::PARSE_ERROR);
        assert!(v3["id"].is_null());
    }

    #[test]
    fn context_search_over_a_fixture_returns_results_and_logs_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":4,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"hash password","top_k":5}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("auth.py"), "expected auth.py, got: {text}");
        assert!(text.contains("query_id:"));
        assert_eq!(v["result"]["isError"], false);

        // A `search` event was appended to the metrics log.
        let log = crate::metrics::read_log(&s.metrics_path());
        assert!(log.events.iter().any(|e| matches!(e, crate::metrics::Event::Search(_))));
    }

    #[test]
    fn context_search_missing_index_is_friendly_not_a_crash() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":5,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"anything"}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not indexed"), "got: {text}");
        assert_eq!(v["result"]["isError"], false);
    }

    #[test]
    fn context_search_requires_a_query() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":6,"method":"tools/call","params":{"name":"context_search","arguments":{}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("query"));
    }

    #[test]
    fn index_status_reports_counts_and_local_source() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(r#"{"id":7,"method":"tools/call","params":{"name":"index_status"}}"#)
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("indexed : yes"));
        assert!(text.contains("chunks  :"));
        assert!(text.contains("source  : local"));
        assert!(text.contains("no sync remote configured"));
    }

    #[test]
    fn record_feedback_appends_an_event() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        let resp = s
            .handle_line(
                r#"{"id":8,"method":"tools/call","params":{"name":"record_feedback","arguments":{"query_id":"abc123def456","helpful":true,"note":"great"}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], false);
        let log = crate::metrics::read_log(&s.metrics_path());
        assert!(log.events.iter().any(|e| match e {
            crate::metrics::Event::Feedback(f) => f.target_id == "abc123def456" && f.helpful,
            _ => false,
        }));
    }

    #[test]
    fn record_feedback_validates_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        // missing query_id
        let r1 = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"record_feedback","arguments":{"helpful":true}}}"#,
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&r1).unwrap()["result"]["isError"], true);
        // missing helpful
        let r2 = s
            .handle_line(
                r#"{"id":2,"method":"tools/call","params":{"name":"record_feedback","arguments":{"query_id":"x"}}}"#,
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&r2).unwrap()["result"]["isError"], true);
    }

    #[test]
    fn unknown_tool_is_an_iserror_result() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp =
            s.handle_line(r#"{"id":1,"method":"tools/call","params":{"name":"nope"}}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"]["isError"], true);
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("unknown tool"));
    }

    #[test]
    fn tools_call_without_a_name_is_invalid_params() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        let resp = s.handle_line(r#"{"id":1,"method":"tools/call","params":{}}"#).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["error"]["code"], protocol::INVALID_PARAMS);
    }

    #[test]
    fn run_loop_processes_a_stream_and_skips_blank_lines() {
        let tmp = tempfile::tempdir().unwrap();
        index_fixture(tmp.path());
        let s = server_for(tmp.path());
        let input = concat!(
            "{\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
            "\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"id\":2,\"method\":\"tools/list\"}\n"
        );
        let mut out: Vec<u8> = Vec::new();
        s.run(Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        // Two responses (initialize, tools/list); the notification produced none.
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "got: {text}");
        assert!(lines[0].contains("protocolVersion"));
        assert!(lines[1].contains("context_search"));
    }

    #[test]
    fn invalid_utf8_line_is_a_parse_error_and_the_session_survives() {
        let tmp = tempfile::tempdir().unwrap();
        let s = server_for(tmp.path());
        // ping(id=1), then a line carrying stray non-UTF-8 bytes, then ping(id=2).
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"{\"id\":1,\"method\":\"ping\"}\n");
        input.extend_from_slice(&[0xff, 0xfe]);
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":2,\"method\":\"ping\"}\n");

        let mut out: Vec<u8> = Vec::new();
        // The bad byte must NOT tear down the loop: `run` returns Ok, not an error.
        s.run(Cursor::new(input), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "every request must be answered, got: {text}");

        // id=1 answered.
        let v1: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v1["id"], 1);
        assert_eq!(v1["result"], json!({}));
        // The bad-byte line yields a graceful parse error (null id), not a crash.
        let v2: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v2["error"]["code"], protocol::PARSE_ERROR);
        assert!(v2["id"].is_null());
        // id=2 is still answered — the session survived.
        let v3: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(v3["id"], 2);
        assert_eq!(v3["result"], json!({}));
    }

    /// Copy the workspace fixture into `root`, write its manifest + graph, and index
    /// every member — the on-disk shape a `cce mcp --workspace` session reads.
    fn index_workspace_fixture(root: &Path) {
        use crate::store::default_store_path;
        use crate::workspace::{build_graph, build_manifest};
        let fixture = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"));
        for entry in walkdir::WalkDir::new(&fixture).into_iter().flatten() {
            let rel = entry.path().strip_prefix(&fixture).unwrap();
            let target = root.join(rel);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
        let manifest = build_manifest(root);
        manifest.save(root).unwrap();
        build_graph(root, &manifest).save(root).unwrap();
        for m in &manifest.members {
            let dir = root.join(&m.path);
            let (idx, _) = Index::build_from_dir(&dir, &HashEmbedder).unwrap();
            idx.save(&default_store_path(&dir)).unwrap();
        }
    }

    #[test]
    fn workspace_context_search_is_cached_and_warm_call_is_identical() {
        let tmp = tempfile::tempdir().unwrap();
        index_workspace_fixture(tmp.path());
        let s = McpServer::new(Some(tmp.path().to_path_buf()), None, true);
        assert!(s.is_workspace());

        let call = || {
            let resp = s
                .handle_line(
                    r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"billing charge amount","no_graph":true}}}"#,
                )
                .unwrap();
            let v: Value = serde_json::from_str(&resp).unwrap();
            v["result"]["content"][0]["text"].as_str().unwrap().to_string()
        };

        // Cold call federates + caches the union.
        let cold = call();
        assert_eq!(s.workspace_cache.borrow().len(), 1, "cold call must cache the union");
        // Warm call reuses the cached union and returns byte-identical results (minus
        // the per-call query_id, which is a fresh metrics id — strip it for comparison).
        let warm = call();
        assert_eq!(s.workspace_cache.borrow().len(), 1, "warm call must not add a bundle");
        // Drop the two lines that carry the per-call metrics query_id (a fresh id each
        // call by design); the ranked result block itself must be byte-identical.
        let strip = |t: &str| -> String {
            t.lines()
                .filter(|l| !l.starts_with("query_id:") && !l.starts_with("Rate this"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&cold), strip(&warm), "warm workspace search must equal the cold one");
        assert!(cold.contains("billing"), "expected a billing result, got: {cold}");
    }

    #[test]
    fn scoped_workspace_search_caches_under_a_distinct_key() {
        let tmp = tempfile::tempdir().unwrap();
        index_workspace_fixture(tmp.path());
        let s = McpServer::new(Some(tmp.path().to_path_buf()), None, true);
        // A scoped call caches one bundle; the full call caches a second, distinct one.
        s.handle_line(
            r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"billing charge amount","package":"billing","no_graph":true}}}"#,
        )
        .unwrap();
        s.handle_line(
            r#"{"id":2,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"billing charge amount","no_graph":true}}}"#,
        )
        .unwrap();
        assert_eq!(
            s.workspace_cache.borrow().len(),
            2,
            "scope and full workspace cache separately"
        );
    }

    #[test]
    fn workspace_member_reindex_is_picked_up_mid_session() {
        let tmp = tempfile::tempdir().unwrap();
        index_workspace_fixture(tmp.path());
        let s = McpServer::new(Some(tmp.path().to_path_buf()), None, true);

        // Cold call assembles + caches the union.
        let resp = s
            .handle_line(
                r#"{"id":1,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"billing charge amount","no_graph":true}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("billing"));
        assert_eq!(s.workspace_cache.borrow().len(), 1);
        let cold_ptr = Rc::as_ptr(&s.workspace_cache.borrow().values().next().unwrap().1);

        // Re-index ONE member mid-session (new file, rebuilt store, explicit mtime
        // bump so granularity can never mask the change).
        let manifest = Manifest::load(tmp.path()).unwrap();
        let billing = manifest.members.iter().find(|m| m.name == "billing").unwrap();
        let member_dir = tmp.path().join(&billing.path);
        std::fs::write(member_dir.join("widgets.py"), "def frobnicate_widget():\n    return 42\n")
            .unwrap();
        let (idx, _) = Index::build_from_dir(&member_dir, &HashEmbedder).unwrap();
        let store = default_store_path(&member_dir);
        idx.save(&store).unwrap();
        bump_mtime(&store);

        // The next call rebuilds the union and serves the new member content.
        let resp = s
            .handle_line(
                r#"{"id":2,"method":"tools/call","params":{"name":"context_search","arguments":{"query":"frobnicate widget","no_graph":true}}}"#,
            )
            .unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap();
        let text = v["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("widgets.py"), "member re-index not picked up: {text}");
        assert_eq!(s.workspace_cache.borrow().len(), 1, "the entry is replaced, not added");
        let new_ptr = Rc::as_ptr(&s.workspace_cache.borrow().values().next().unwrap().1);
        assert_ne!(cold_ptr, new_ptr, "a changed member fingerprint must rebuild the union");
    }

    #[test]
    fn store_and_metrics_resolution_variants() {
        // explicit --store: metrics land beside it.
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("sub").join("index.json");
        let s = McpServer::new(None, Some(store.clone()), false);
        assert_eq!(s.store_path(), store);
        assert_eq!(s.metrics_path(), tmp.path().join("sub").join(METRICS_FILE));
        // workspace: metrics at the root.
        let ws = McpServer::new(Some(tmp.path().to_path_buf()), None, true);
        assert!(ws.is_workspace());
        assert_eq!(ws.metrics_path(), default_metrics_path(tmp.path()));
    }
}
