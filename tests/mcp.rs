//! # tests/mcp — end-to-end CCE MCP acceptance tests (SPEC-MCP §"Testing")
//!
//! **Why this file exists:** SPEC-MCP requires the server be proven by piping
//! JSON-RPC to a *real* `cce mcp` process's stdin and asserting stdout — the
//! handshake, `tools/list`, each tool over a fixture index, the missing-index
//! path, `cce init`'s idempotent files, and the sync auto-pull soft dependency
//! behind a local bare git remote (offline-safe when absent). Unit tests cannot
//! prove the stdio-transport or the cross-process behaviour; only spawning the
//! binary can.
//!
//! **What it is / does:** Spawns `cce mcp`/`cce init`/`cce sync` as subprocesses
//! against temp fixtures, feeds newline-delimited JSON-RPC, and parses the
//! responses. No editor and no network (the sync remote is a local `file://` bare
//! repo).
//!
//! **Responsibilities:**
//! - Own the process-level MCP acceptance tests.
//! - It does NOT touch library internals.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// Write a tiny self-contained Python repo into `dir`.
fn write_tiny_repo(dir: &Path) {
    std::fs::write(dir.join("auth.py"), "def hash_password(pw):\n    return pw + 'salt'\n")
        .unwrap();
    std::fs::write(
        dir.join("payments.py"),
        "import auth\n\ndef process_payment(amount):\n    return amount\n",
    )
    .unwrap();
}

/// Run `cce index <dir>` so the default store `<dir>/.cce/index.json` exists.
fn index_dir(dir: &Path) {
    let out = Command::new(bin()).args(["index"]).arg(dir).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Drive `cce <args>` (an MCP session) with `input` on stdin and `envs` set,
/// returning stdout. Closing stdin yields EOF, so the server exits cleanly.
fn drive(args: &[&str], input: &str, envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Parse newline-delimited JSON responses.
fn responses(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Find the response with the given id.
fn by_id(resps: &[Value], id: i64) -> &Value {
    resps.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("no response with id {id}"))
}

#[test]
fn handshake_list_and_search_over_a_fixture_index_logs_metrics() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());

    let input = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"hash password\",\"top_k\":5}}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n"
    );
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);

    // initialize
    let init = by_id(&resps, 1);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "cce");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    // tools/list — exactly the five tools, in fixed order, with the schemas.
    let list = by_id(&resps, 2);
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "context_search",
            "index_status",
            "record_feedback",
            "expand_chunk",
            "related_context"
        ]
    );
    assert_eq!(tools[0]["inputSchema"]["required"], serde_json::json!(["query"]));
    assert_eq!(tools[0]["inputSchema"]["properties"]["top_k"]["default"], 8);
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["detail"]["enum"],
        serde_json::json!(["signature", "compact", "full"])
    );
    assert!(tools[0]["description"].as_str().unwrap().contains("PREFERRED"));
    // The Layer-7 tools carry their pinned schemas.
    assert_eq!(tools[3]["inputSchema"]["properties"]["scope"]["default"], "body");
    assert_eq!(tools[4]["inputSchema"]["required"], serde_json::json!(["chunk_id"]));

    // context_search
    let search = by_id(&resps, 3);
    let text = search["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("auth.py"), "expected auth.py, got: {text}");
    assert!(text.contains("query_id:"));
    assert_eq!(search["result"]["isError"], false);

    // ping
    assert_eq!(by_id(&resps, 4)["result"], serde_json::json!({}));

    // A `search` event was logged for the dashboard.
    let log = std::fs::read_to_string(tmp.path().join(".cce").join("metrics.jsonl")).unwrap();
    assert!(log.contains("\"event\":\"search\""), "no search event logged: {log}");
}

/// The 16-hex `#chunk_id` on the first result header line mentioning `file_hint`.
fn chunk_id_for(text: &str, file_hint: &str) -> String {
    for line in text.lines() {
        if line.contains(file_hint) {
            if let Some(pos) = line.rfind('#') {
                let id: String =
                    line[pos + 1..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                if id.len() == 16 {
                    return id;
                }
            }
        }
    }
    panic!("no chunk_id for {file_hint} in:\n{text}");
}

#[test]
fn context_search_compact_then_expand_chunk_round_trips_to_full() {
    // SPEC-V2.5 §2/§7: context_search serves compact + a chunk_id; expand_chunk
    // (scope=body) recovers the EXACT full body the store holds.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    // Compact search: the body is reduced but the chunk_id is present for expansion.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"process payment amount\",\"no_graph\":true,\"detail\":\"compact\"}}}\n";
    let out = drive(&["mcp", "--dir", &dir], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("payments.py"), "got: {text}");
    assert!(text.contains("def process_payment(amount):"), "compact signature missing: {text}");
    // The compact footer advertises expansion.
    assert!(text.contains("expand_chunk(chunk_id"), "no expansion hint: {text}");

    let id = chunk_id_for(text, "payments.py");

    // expand_chunk(body) returns the exact stored full body — byte-for-byte.
    let einput = format!(
        "{{\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"expand_chunk\",\"arguments\":{{\"chunk_id\":\"{id}\",\"scope\":\"body\"}}}}}}\n"
    );
    let eout = drive(&["mcp", "--dir", &dir], &einput, &[]);
    let eresps = responses(&eout);
    let body = by_id(&eresps, 2)["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(body, "def process_payment(amount):\n    return amount");
    assert_eq!(by_id(&eresps, 2)["result"]["isError"], false);
}

#[test]
fn related_context_returns_import_graph_neighbours() {
    // payments.py imports auth.py; related_context on a payments chunk surfaces the
    // auth.py neighbour (SPEC-V2.5 §7), deterministically.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"process payment amount\",\"no_graph\":true}}}\n";
    let out = drive(&["mcp", "--dir", &dir], input, &[]);
    let text =
        by_id(&responses(&out), 1)["result"]["content"][0]["text"].as_str().unwrap().to_string();
    let id = chunk_id_for(&text, "payments.py");

    let rinput = format!(
        "{{\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"related_context\",\"arguments\":{{\"chunk_id\":\"{id}\"}}}}}}\n"
    );
    let rout = drive(&["mcp", "--dir", &dir], &rinput, &[]);
    let rresps = responses(&rout);
    let rtext = by_id(&rresps, 2)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rtext.contains("related to payments.py via import graph"), "got: {rtext}");
    assert!(rtext.contains("auth.py"), "neighbour auth.py missing: {rtext}");
    assert!(rtext.contains("hash_password"), "neighbour body missing: {rtext}");

    // Deterministic across runs.
    let rout2 = drive(&["mcp", "--dir", &dir], &rinput, &[]);
    let rresps2 = responses(&rout2);
    let rtext2 = by_id(&rresps2, 2)["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(rtext, rtext2);
}

#[test]
fn expand_chunk_unknown_id_is_friendly_not_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"expand_chunk\",\"arguments\":{\"chunk_id\":\"deadbeefdeadbeef\"}}}\n";
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no chunk with id deadbeefdeadbeef"), "got: {text}");
    assert_eq!(by_id(&resps, 1)["result"]["isError"], false);
}

#[test]
fn index_status_and_record_feedback_over_the_binary() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());

    let input = concat!(
        "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n",
        "{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"record_feedback\",\"arguments\":{\"query_id\":\"abc123def456\",\"helpful\":true,\"note\":\"nice\"}}}\n"
    );
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);

    let status = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(status.contains("indexed : yes"));
    assert!(status.contains("source  : local"));

    let fb = by_id(&resps, 2);
    assert_eq!(fb["result"]["isError"], false);
    let log = std::fs::read_to_string(tmp.path().join(".cce").join("metrics.jsonl")).unwrap();
    assert!(log.contains("\"event\":\"feedback\""));
    assert!(log.contains("abc123def456"));
}

#[test]
fn missing_index_is_a_friendly_message_not_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    // No index built.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"anything\"}}}\n";
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not indexed"), "got: {text}");
    assert_eq!(by_id(&resps, 1)["result"]["isError"], false);
}

/// Copy the committed workspace fixture into a writable temp dir.
fn copy_workspace_fixture() -> tempfile::TempDir {
    let src = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"));
    let tmp = tempfile::tempdir().unwrap();
    for entry in walkdir::WalkDir::new(&src).into_iter().flatten() {
        let rel = entry.path().strip_prefix(&src).unwrap();
        let target = tmp.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
    tmp
}

#[test]
fn workspace_context_search_scopes_to_a_package() {
    let tmp = copy_workspace_fixture();
    let dir = tmp.path().to_string_lossy().to_string();

    // Detect members + index the whole workspace.
    let out = Command::new(bin()).args(["workspace", "init"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "workspace init: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(bin()).args(["index", "--workspace"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "index --workspace: {}", String::from_utf8_lossy(&out.stderr));

    // A search scoped to the `billing` package returns only billing-labelled rows.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"billing charge amount\",\"package\":\"billing\",\"no_graph\":true}}}\n";
    let out = drive(&["mcp", "--workspace", "--dir", &dir], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("billing · "), "expected billing-scoped rows, got: {text}");
    assert!(!text.contains("app · "), "scope leaked to app: {text}");

    // index_status --workspace reports per-member counts.
    let input = "{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n";
    let out = drive(&["mcp", "--workspace", "--dir", &dir], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 2)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Workspace status:"), "got: {text}");
    assert!(text.contains("package billing"), "got: {text}");
}

#[test]
fn cce_init_writes_valid_idempotent_mcp_json_and_claude_md() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());

    let run = || Command::new(bin()).args(["init"]).arg(tmp.path()).output().unwrap();
    let out = run();
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(report.contains("wired up for Claude Code"));
    assert!(report.contains("Restart your editor"));

    // .mcp.json is valid and points at `cce mcp --dir .`.
    let mcp: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
            .unwrap();
    assert_eq!(mcp["mcpServers"]["cce"]["command"], "cce");
    assert_eq!(mcp["mcpServers"]["cce"]["args"], serde_json::json!(["mcp", "--dir", "."]));

    // CLAUDE.md carries the bounded block.
    let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
    assert!(claude.contains("<!-- BEGIN CCE MCP -->"));
    assert!(claude.contains("PREFER `context_search`"));

    // Idempotent: a second run leaves both files byte-identical.
    let mcp1 = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
    let claude1 = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
    run();
    assert_eq!(mcp1, std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap());
    assert_eq!(claude1, std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap());

    // The wired-up server actually serves against the built index.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"hash password\"}}}\n";
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("auth.py"), "got: {text}");
}

// --- CCE Sync soft dependency (behind a local bare git remote) ---

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
}

/// A source git repo on `main` with committed content.
fn source_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    write_tiny_repo(d);
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// A bare git repo acting as the sync remote; returns its `file://` URL.
fn bare_remote() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "--bare", "-q", "-b", "main"]);
    let url = format!("file://{}", tmp.path().to_string_lossy());
    (tmp, url)
}

/// Write a `.cce/config` with a remote and `auto_pull` set as given.
fn write_sync_config(dir: &Path, url: &str, auto_pull: bool) {
    let cce = dir.join(".cce");
    std::fs::create_dir_all(&cce).unwrap();
    let yaml = format!(
        "sync:\n  remote: {url}\n  lfs: false\n  repo_id: example.com__acme__demo\n  auto_pull: {auto_pull}\n  retention: all\n"
    );
    std::fs::write(cce.join("config"), yaml).unwrap();
}

#[test]
fn mcp_auto_pulls_the_latest_index_on_startup_when_configured() {
    let home = tempfile::tempdir().unwrap();
    let home_str = home.path().to_string_lossy().to_string();
    let (_bare, url) = bare_remote();

    // Producer: configure sync + push the CI-built index for HEAD.
    let src = source_repo();
    write_sync_config(src.path(), &url, false);
    let push = Command::new(bin())
        .args(["sync", "push", "--dir"])
        .arg(src.path())
        .env("CCE_HOME", &home_str)
        .output()
        .unwrap();
    assert!(push.status.success(), "push: {}", String::from_utf8_lossy(&push.stderr));

    // Consumer: a fresh dir with auto_pull ON and NO local index yet.
    let consumer = tempfile::tempdir().unwrap();
    write_sync_config(consumer.path(), &url, true);
    assert!(!consumer.path().join(".cce/index.json").exists());

    // Starting `cce mcp` warms the index via `sync pull --latest`; index_status
    // then reports the pulled source + sha.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n";
    let out = drive(
        &["mcp", "--dir", &consumer.path().to_string_lossy()],
        input,
        &[("CCE_HOME", &home_str)],
    );
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("indexed : yes"), "auto-pull did not warm the index: {text}");
    assert!(text.contains("source  : pulled via cce sync"), "got: {text}");
    // The pulled store now exists locally.
    assert!(consumer.path().join(".cce/index.json").exists());
}

#[test]
fn mcp_is_offline_safe_when_the_configured_remote_is_absent() {
    let home = tempfile::tempdir().unwrap();
    let consumer = tempfile::tempdir().unwrap();
    // A configured but non-existent remote, auto_pull ON, and no local index.
    write_sync_config(consumer.path(), "file:///definitely/not/here.git", true);

    // The server must still start, warm (fail silently), and answer — no crash/hang.
    let input = concat!(
        "{\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        "{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n"
    );
    let out = drive(
        &["mcp", "--dir", &consumer.path().to_string_lossy()],
        input,
        &[("CCE_HOME", &home.path().to_string_lossy())],
    );
    let resps = responses(&out);
    assert_eq!(by_id(&resps, 1)["result"]["protocolVersion"], "2025-06-18");
    // No index and an unreachable remote → a clean "not indexed" answer.
    let text = by_id(&resps, 2)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("not indexed"), "got: {text}");
}

#[test]
fn mcp_works_with_no_sync_configured_at_all() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    // No .cce/config exists — pure local CCE. index_status still answers, reporting
    // no remote configured.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"index_status\"}}\n";
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], input, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no sync remote configured"), "got: {text}");
}
