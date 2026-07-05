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

use crate::config::METRICS_FILE;
use crate::mcp::protocol::{self, Request};
use crate::mcp::tools;
use crate::mcp::{MCP_PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};
use crate::store::{default_metrics_path, default_store_path};
use crate::sync::commands::{cmd_pull, PullTarget};
use crate::sync::config::SyncConfig;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// The resolved context for a CCE MCP session (SPEC-MCP §"The server").
pub struct McpServer {
    /// `--dir`: the project directory (also the sync/metrics root when set).
    dir: Option<PathBuf>,
    /// `--store`: an explicit index path (overrides `--dir`/cwd resolution).
    store: Option<PathBuf>,
    /// `--workspace`: federate over the workspace members (SPEC-V2.2).
    workspace: bool,
}

impl McpServer {
    /// Construct from the resolved CLI options.
    pub fn new(dir: Option<PathBuf>, store: Option<PathBuf>, workspace: bool) -> Self {
        McpServer { dir, store, workspace }
    }

    /// Whether this session federates over a workspace.
    pub fn is_workspace(&self) -> bool {
        self.workspace
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
            let _ = cmd_pull(&root, PullTarget::Latest, false, self.workspace);
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
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // EOF: the editor closed the pipe.
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(trimmed) {
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
            Err(_) => {
                return Some(protocol::error(Value::Null, protocol::PARSE_ERROR, "parse error"))
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
            other => {
                return Ok(unknown_tool_result(other));
            }
        };
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
        let (idx, _) = Index::build_from_dir(&fixture, &HashEmbedder);
        idx.save(&default_store_path(dir)).unwrap();
    }

    fn server_for(dir: &Path) -> McpServer {
        McpServer::new(Some(dir.to_path_buf()), None, false)
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
    fn tools_list_returns_the_three_tools() {
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
        assert_eq!(names, vec!["context_search", "index_status", "record_feedback"]);
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
