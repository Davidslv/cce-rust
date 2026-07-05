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

// Byte-pinned copies of the three expand-first tool descriptions (SPEC-V2.5 §6 +
// SPEC-V2.5-TUNING §B). These mirror the private consts in `src/mcp/tools.rs`
// verbatim; `tools/list` must serve exactly these bytes, and cce-ruby reconciles to
// them. A drift here means the schema changed and both engines must be re-synced.
const CONTEXT_SEARCH_DESC: &str = "Search THIS project's code by meaning, across files. Use it \
FIRST for any cross-file question — \"where is X\", \"how does Y work\", \"what calls Z\" — or \
whenever you cannot already name the exact file to open. Returns the most relevant code chunks \
(file:line + kind) from a hybrid vector + BM25 index, so you don't pay tokens for whole files. \
Skip it only when you already know the single file you need — reading that path directly is fine; \
cce does not win there. Results are COMPACT and each carries a `chunk_id`; to read a full body \
call `expand_chunk(chunk_id)` — do NOT re-issue `context_search` for a target you already found. \
Widen to import-graph neighbours with `related_context(chunk_id)`.";

const EXPAND_CHUNK_DESC: &str = "Read the FULL detail of a chunk `context_search` already \
returned, by its `chunk_id`. `context_search` serves COMPACT chunks (a header + members, or a \
signature + first line); when you need the real body, call this — do NOT re-run `context_search` \
for a chunk you already have. scope=body recovers the exact full body; scope=file returns every \
chunk in the same file; scope=neighbors returns chunks from import-graph-related files. A stale \
or unknown `chunk_id` returns a short, actionable error you can retry from, never a crash.";

const RELATED_CONTEXT_DESC: &str = "Given a `chunk_id` from `context_search`, return the chunks \
connected to it through the import graph — both what it imports AND its consumers (reverse edges) \
— as compact entries. Use it to widen context on demand — trace how a symbol is used or what it \
depends on across files — instead of pre-loading whole neighbourhoods; expand any result with \
`expand_chunk(chunk_id)`. Pairs with `context_search` (find first) and `expand_chunk` (read the \
full body).";

const SET_OUTPUT_COMPRESSION_DESC: &str = "Set how terse THIS session's answers should be — the \
output-compression level the agent applies to its OWN replies. Levels: `off` (no rules), `lite` \
(concise; drop filler/preamble/postamble), `standard` (fewest correct words; code as minimal \
diffs, never whole files; no preamble or postamble), `max` (standard + telegraphic prose; code \
as minimal diffs only). Use it to dial verbosity down when you want terse diffs, or up (`off`) \
when you want full explanations — mid-session, without editing CLAUDE.md. It sets a session \
preference only; it does not rewrite CLAUDE.md and resets when the server restarts.";

const RECORD_DECISION_DESC: &str = "Remember a VALIDATED decision for future sessions — an \
explicit, deliberate note you or the user have confirmed is correct (an architecture choice, a \
convention, a resolved trade-off), so it need not be re-derived later. The text is secret-redacted \
before storage, content-addressed, and de-duplicated: recording the same decision twice is a \
no-op that returns the same id. Do NOT record raw model output, guesses, or unverified answers — \
memory that replays a bad answer POLLUTES future context. Optional `tags` and an `area` help \
recall. Returns the decision's id; retrieve later with `session_recall`.";

const SESSION_RECALL_DESC: &str = "Search THIS project's remembered decisions (recorded with \
`record_decision`) for ones relevant to `query`, so you don't re-derive what was already settled. \
Hybrid vector + BM25 search, PRECISION-FILTERED: it returns only high-confidence matches (a small \
top_k) as compact entries with ids, which you CHOOSE to use — it is never an auto-injected blob. \
Returning nothing when there is no confident match is normal and correct; proceed without it \
rather than forcing a weak memory into context.";

const SUMMARIZE_CONTEXT_DESC: &str = "Get a compact, deterministic digest of what THIS session \
has done so far — the files and chunks you have touched, the queries you have run, and the \
decisions you have recorded — so you can compress a long session's history into a few lines \
instead of re-sending the raw transcript. It is a STRUCTURED digest built from the server's \
per-session ledger, NOT an LLM-written summary: the same sequence of tool calls always yields the \
same bytes. Optional `scope` narrows it: \"all\" (default), \"files\" (the files AND chunks \
touched), \"queries\", or \"decisions\". Long lists are bounded with a `… (+N more)` marker. An \
unknown `scope` returns a short, actionable error, never a crash.";

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

    // tools/list — exactly the nine tools, in fixed order, with the schemas.
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
            "related_context",
            "set_output_compression",
            "record_decision",
            "session_recall",
            "summarize_context"
        ]
    );
    assert_eq!(tools[0]["inputSchema"]["required"], serde_json::json!(["query"]));
    assert_eq!(tools[0]["inputSchema"]["properties"]["top_k"]["default"], 8);
    assert_eq!(
        tools[0]["inputSchema"]["properties"]["detail"]["enum"],
        serde_json::json!(["signature", "compact", "full"])
    );
    // Tool descriptions are byte-pinned (part of the schema, SPEC-V2.5 §6): the
    // expand-first rewrite (SPEC-V2.5-TUNING §B) must match to the byte, because the
    // Ruby engine reconciles to these exact strings.
    assert_eq!(tools[0]["description"].as_str().unwrap(), CONTEXT_SEARCH_DESC);
    assert_eq!(tools[3]["description"].as_str().unwrap(), EXPAND_CHUNK_DESC);
    assert_eq!(tools[4]["description"].as_str().unwrap(), RELATED_CONTEXT_DESC);
    assert_eq!(tools[5]["description"].as_str().unwrap(), SET_OUTPUT_COMPRESSION_DESC);
    // The expand-first rule is present and steers away from the re-search reflex.
    assert!(CONTEXT_SEARCH_DESC
        .contains("do NOT re-issue `context_search` for a target you already found"));
    assert!(EXPAND_CHUNK_DESC.contains("do NOT re-run `context_search`"));
    // The Layer-7 tools carry their pinned schemas.
    assert_eq!(tools[3]["inputSchema"]["properties"]["scope"]["default"], "body");
    assert_eq!(tools[4]["inputSchema"]["required"], serde_json::json!(["chunk_id"]));
    // The Layer-4 tool carries its pinned schema: a required `level` enum.
    assert_eq!(tools[5]["inputSchema"]["required"], serde_json::json!(["level"]));
    assert_eq!(
        tools[5]["inputSchema"]["properties"]["level"]["enum"],
        serde_json::json!(["off", "lite", "standard", "max"])
    );
    // The Layer-5 memory tools carry their byte-pinned descriptions + schemas.
    assert_eq!(tools[6]["name"].as_str().unwrap(), "record_decision");
    assert_eq!(tools[6]["description"].as_str().unwrap(), RECORD_DECISION_DESC);
    assert_eq!(tools[6]["inputSchema"]["required"], serde_json::json!(["text"]));
    assert_eq!(tools[6]["inputSchema"]["properties"]["tags"]["type"], "array");
    assert_eq!(tools[7]["name"].as_str().unwrap(), "session_recall");
    assert_eq!(tools[7]["description"].as_str().unwrap(), SESSION_RECALL_DESC);
    assert_eq!(tools[7]["inputSchema"]["required"], serde_json::json!(["query"]));
    assert_eq!(tools[7]["inputSchema"]["properties"]["top_k"]["default"], 5);
    // The Layer-6 turn-summarization tool carries its byte-pinned description + schema:
    // an optional `scope` enum defaulting to "all", and NO required inputs.
    assert_eq!(tools[8]["name"].as_str().unwrap(), "summarize_context");
    assert_eq!(tools[8]["description"].as_str().unwrap(), SUMMARIZE_CONTEXT_DESC);
    assert!(tools[8]["inputSchema"].get("required").is_none());
    assert_eq!(
        tools[8]["inputSchema"]["properties"]["scope"]["enum"],
        serde_json::json!(["all", "files", "queries", "decisions"])
    );
    assert_eq!(tools[8]["inputSchema"]["properties"]["scope"]["default"], "all");

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
fn set_output_compression_switches_the_session_level_over_the_binary() {
    // SPEC-V2.5 §2 Layer 4: the 6th tool sets the running session's output level.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    // A valid switch confirms the active level; a bad one is an actionable error.
    let input = concat!(
        "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"set_output_compression\",\"arguments\":{\"level\":\"max\"}}}\n",
        "{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"set_output_compression\",\"arguments\":{\"level\":\"turbo\"}}}\n"
    );
    let out = drive(&["mcp", "--dir", &dir], input, &[]);
    let resps = responses(&out);

    let ok = by_id(&resps, 1);
    assert_eq!(ok["result"]["isError"], false);
    let text = ok["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("max"), "confirmation missing active level: {text}");
    assert!(text.contains("CLAUDE.md unchanged"), "should note it is session-only: {text}");

    let bad = by_id(&resps, 2);
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("unknown level"));
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

#[test]
fn summarize_context_digest_is_byte_pinned_for_a_fixed_session_and_scopes_slice() {
    // SPEC-V2.5 §2 Layer 6: over a REAL `cce mcp` process, a fixed sequence of tool
    // calls (two searches + an expand + a decision) produces a digest whose bytes are
    // EXACTLY the pinned golden — proving the per-session ledger accumulates in order
    // and the digest is deterministic given the call sequence. The whole session runs
    // in ONE process so the in-memory ledger persists across the calls.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    // Content-addressed chunk ids of the tiny repo (stable across runs).
    let auth_id = "039f8bd5b80a698e";
    let input = format!(
        concat!(
            "{{\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"context_search\",\"arguments\":{{\"query\":\"hash password\",\"no_graph\":true}}}}}}\n",
            "{{\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"context_search\",\"arguments\":{{\"query\":\"process payment amount\",\"no_graph\":true}}}}}}\n",
            "{{\"id\":3,\"method\":\"tools/call\",\"params\":{{\"name\":\"expand_chunk\",\"arguments\":{{\"chunk_id\":\"{}\",\"scope\":\"body\"}}}}}}\n",
            "{{\"id\":4,\"method\":\"tools/call\",\"params\":{{\"name\":\"record_decision\",\"arguments\":{{\"text\":\"store password hashes with bcrypt\",\"area\":\"auth\"}}}}}}\n",
            "{{\"id\":5,\"method\":\"tools/call\",\"params\":{{\"name\":\"summarize_context\",\"arguments\":{{}}}}}}\n",
            "{{\"id\":6,\"method\":\"tools/call\",\"params\":{{\"name\":\"summarize_context\",\"arguments\":{{\"scope\":\"files\"}}}}}}\n",
            "{{\"id\":7,\"method\":\"tools/call\",\"params\":{{\"name\":\"summarize_context\",\"arguments\":{{\"scope\":\"bogus\"}}}}}}\n"
        ),
        auth_id
    );
    let out = drive(&["mcp", "--dir", &dir], &input, &[]);
    let resps = responses(&out);

    // scope=all: the full digest, byte-for-byte.
    let digest = by_id(&resps, 5)["result"]["content"][0]["text"].as_str().unwrap();
    let golden = "CCE session digest\n\
         files (2):\n- auth.py\n- payments.py\n\
         chunks (2):\n- 039f8bd5b80a698e\n- 61707be0deb092a1\n\
         queries (2):\n- hash password\n- process payment amount\n\
         decisions (1):\n- #03774d8fa782583c store password hashes with bcrypt";
    assert_eq!(digest, golden);
    assert_eq!(by_id(&resps, 5)["result"]["isError"], false);

    // scope=files: only the files + chunks slice.
    let files = by_id(&resps, 6)["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        files,
        "CCE session digest\n\
         files (2):\n- auth.py\n- payments.py\n\
         chunks (2):\n- 039f8bd5b80a698e\n- 61707be0deb092a1"
    );

    // An unknown scope is an actionable tool-level error, not a crash.
    let bad = by_id(&resps, 7);
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"].as_str().unwrap().contains("unknown scope"));

    // Determinism: re-running the identical session yields identical digest bytes.
    let out2 = drive(&["mcp", "--dir", &dir], &input, &[]);
    let resps2 = responses(&out2);
    let digest2 = by_id(&resps2, 5)["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(digest2, golden);
}

#[test]
fn summarize_context_does_not_leak_across_a_fresh_server_process() {
    // The ledger is per-process: a second `cce mcp` invocation starts empty even after
    // a prior session recorded activity against the same project.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    // Session A: run a search, then close the process.
    let a = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"hash password\"}}}\n";
    drive(&["mcp", "--dir", &dir], a, &[]);

    // Session B: a brand-new process summarizes to the pinned EMPTY body.
    let b = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"summarize_context\"}}\n";
    let out = drive(&["mcp", "--dir", &dir], b, &[]);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(text, "CCE session digest\n(nothing recorded this session yet)");
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

#[test]
fn memory_record_and_recall_over_a_real_process_and_redacts_secrets() {
    // SPEC-V2.5 §2 Layer 5: record_decision → session_recall over a real `cce mcp`
    // process. A secret in the decision text is redacted before it reaches the store,
    // and recall returns the (redacted) decision by meaning. Memory is a SEPARATE,
    // local-only store (`.cce/memory.jsonl`) — never the code index, never Sync.
    let tmp = tempfile::tempdir().unwrap();
    // Assemble a secret literal at runtime so no contiguous secret is committed.
    let aws = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
    let input = format!(
        concat!(
            "{{\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"record_decision\",\"arguments\":{{\"text\":\"rotate the deploy key AWS = \\\"{}\\\" every quarter\",\"area\":\"secops\",\"tags\":[\"security\"]}}}}}}\n",
            "{{\"id\":2,\"method\":\"tools/call\",\"params\":{{\"name\":\"session_recall\",\"arguments\":{{\"query\":\"rotate deploy key quarter\"}}}}}}\n"
        ),
        aws
    );
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], &input, &[]);
    let resps = responses(&out);

    // record_decision returned an id and did not error.
    let rec = by_id(&resps, 1);
    assert_eq!(rec["result"]["isError"], false);
    let rec_text = rec["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rec_text.contains("Recorded decision #"), "got: {rec_text}");

    // session_recall found the decision by meaning, and shows the REDACTED marker.
    let rc = by_id(&resps, 2);
    let rc_text = rc["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rc_text.contains("Recalled 1 of 1"), "got: {rc_text}");
    assert!(rc_text.contains("[REDACTED:AWS_ACCESS_KEY]"), "got: {rc_text}");
    assert!(!rc_text.contains(&aws), "recall leaked the secret: {rc_text}");

    // The on-disk memory store carries no secret, and is NOT the code index.
    let mem = tmp.path().join(".cce").join("memory.jsonl");
    assert!(mem.exists(), "memory store not written");
    let raw = std::fs::read_to_string(&mem).unwrap();
    assert!(raw.contains("[REDACTED:AWS_ACCESS_KEY]"));
    assert!(!raw.contains(&aws), "memory store leaked the secret");
    // No search/index event — memory tools do not touch the metrics ledger.
    assert!(!tmp.path().join(".cce").join("index.json").exists());
}
