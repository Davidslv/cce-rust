//! # tests/mcp_footer — the opt-in MCP result footer end-to-end (SPEC-USAGE-VISIBILITY §3)
//!
//! **Why this file exists:** v2.8's two load-bearing invariants must be proven
//! over the REAL `cce mcp` binary:
//!
//! 1. **Pure projection** — toggling `mcp.result_footer` changes ONLY the served
//!    text: the recorded `search` event is identical off vs on (modulo the
//!    per-call `ts`/`id`/`latency_ms`, which vary run to run by construction).
//! 2. **Additive** — with the footer `off` (the default, including when there is
//!    no `.cce/config` at all) the tool result carries no footer line, so every
//!    pre-v2.8 byte-pinned MCP surface (tests/mcp.rs) is untouched.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for the footer modes (off/on/session)
//!   and the off-vs-on event-purity invariant.

use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn write_tiny_repo(dir: &Path) {
    std::fs::write(dir.join("auth.py"), "def hash_password(pw):\n    return pw + 'salt'\n")
        .unwrap();
    std::fs::write(
        dir.join("payments.py"),
        "import auth\n\ndef process_payment(amount):\n    return amount\n",
    )
    .unwrap();
}

fn index_dir(dir: &Path) {
    let out = Command::new(bin()).args(["index"]).arg(dir).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Write a `.cce/config` selecting a footer mode (quoted, so YAML 1.1 cannot
/// coerce `on`/`off` to booleans — though the loader accepts both spellings).
fn write_footer_config(dir: &Path, mode: &str) {
    let cce = dir.join(".cce");
    std::fs::create_dir_all(&cce).unwrap();
    std::fs::write(cce.join("config"), format!("mcp:\n  result_footer: \"{mode}\"\n")).unwrap();
}

/// Drive one `cce mcp` session with `input` on stdin, returning stdout.
fn drive(args: &[&str], input: &str) -> String {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn responses(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn by_id(resps: &[Value], id: i64) -> &Value {
    resps.iter().find(|r| r["id"] == id).unwrap_or_else(|| panic!("no response with id {id}"))
}

const SEARCH_1: &str = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"hash password\",\"no_graph\":true}}}\n";
const SEARCH_2: &str = "{\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"hash password\",\"no_graph\":true}}}\n";

/// Run one search against `dir` and return the served text block.
fn search_text(dir: &Path) -> String {
    let out = drive(&["mcp", "--dir", &dir.to_string_lossy()], SEARCH_1);
    let resps = responses(&out);
    by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap().to_string()
}

/// The footer line of a served block, if any (the one line starting `cce: `).
fn footer_line(text: &str) -> Option<&str> {
    text.lines().find(|l| l.starts_with("cce: "))
}

/// The LAST recorded `search` event of `dir`'s metrics log, with the per-call
/// fields (`ts`, `id`, `latency_ms`) removed and the rest canonically
/// serialized — every remaining byte is a recorded metric.
fn canonical_search_event(dir: &Path) -> String {
    let log = std::fs::read_to_string(dir.join(".cce").join("metrics.jsonl")).unwrap();
    let line = log
        .lines()
        .rfind(|l| l.contains("\"event\":\"search\""))
        .expect("no search event recorded");
    let mut v: Value = serde_json::from_str(line).unwrap();
    let obj = v.as_object_mut().unwrap();
    for per_call in ["ts", "id", "latency_ms"] {
        obj.remove(per_call);
    }
    serde_json::to_string(&v).unwrap()
}

#[test]
fn footer_is_absent_by_default_and_when_config_says_off() {
    // No config at all (the common case): not a single footer byte.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    let text = search_text(tmp.path());
    assert!(footer_line(&text).is_none(), "default must serve no footer: {text}");

    // Explicit `off`: byte-identical served text to the no-config run (the
    // additive invariant — the pre-v2.8 result grammar is untouched), modulo
    // the per-call query_id lines.
    let tmp2 = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp2.path());
    index_dir(tmp2.path());
    write_footer_config(tmp2.path(), "off");
    let text2 = search_text(tmp2.path());
    assert!(footer_line(&text2).is_none(), "off must serve no footer: {text2}");
    let strip = |t: &str| -> String {
        t.lines()
            .filter(|l| !l.starts_with("query_id:") && !l.starts_with("Rate this"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&text), strip(&text2), "off must equal the no-config bytes");
}

#[test]
fn footer_on_appends_the_pinned_line_deterministically() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    write_footer_config(tmp.path(), "on");

    let text = search_text(tmp.path());
    let footer = footer_line(&text).unwrap_or_else(|| panic!("no footer in: {text}"));
    // The tiny repo has exactly 2 chunks; the byte-pinned grammar shape holds.
    assert!(footer.starts_with("cce: 2 results from 2 chunks · served ~"), "got: {footer}");
    assert!(footer.contains(" tok vs ~"), "got: {footer}");
    assert!(footer.contains(" baseline · saved ~"), "got: {footer}");
    assert!(footer.ends_with("%)"), "`on` must carry no session clause: {footer}");
    // The footer is the LAST line — appended after the record_feedback hint.
    assert_eq!(text.lines().last(), Some(footer), "footer must be the last line: {text}");

    // Deterministic: an identical fresh session serves an identical footer.
    let again = search_text(tmp.path());
    assert_eq!(footer_line(&again), Some(footer));
}

#[test]
fn footer_session_adds_the_running_clause_per_session() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    write_footer_config(tmp.path(), "session");

    // Two searches in ONE session: the clause counts both.
    let input = format!("{SEARCH_1}{SEARCH_2}");
    let out = drive(&["mcp", "--dir", &tmp.path().to_string_lossy()], &input);
    let resps = responses(&out);
    let t1 = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    let t2 = by_id(&resps, 2)["result"]["content"][0]["text"].as_str().unwrap();
    let f1 = footer_line(t1).unwrap();
    let f2 = footer_line(t2).unwrap();
    assert!(f1.contains(" · session: 1 searches, ~"), "got: {f1}");
    assert!(f2.contains(" · session: 2 searches, ~"), "got: {f2}");

    // A fresh session starts its running total over (in-memory, never leaked).
    let fresh = search_text(tmp.path());
    assert!(
        footer_line(&fresh).unwrap().contains(" · session: 1 searches, ~"),
        "session totals must not leak across processes: {fresh}"
    );
}

#[test]
fn recorded_search_event_is_identical_footer_off_vs_on() {
    // Invariant 1 (pure projection): the SAME query over the SAME corpus records
    // the SAME `search` event whether the footer is off or on — every metric
    // (tokens, ratios, the seven-bucket ledger, source) byte-identical once the
    // per-call ts/id/latency are set aside.
    let run = |mode: Option<&str>| -> (String, bool) {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        index_dir(tmp.path());
        if let Some(m) = mode {
            write_footer_config(tmp.path(), m);
        }
        let text = search_text(tmp.path());
        (canonical_search_event(tmp.path()), footer_line(&text).is_some())
    };

    let (off_event, off_footer) = run(None);
    let (on_event, on_footer) = run(Some("on"));
    let (session_event, session_footer) = run(Some("session"));
    assert!(!off_footer && on_footer && session_footer, "toggle must change the text only");
    assert_eq!(off_event, on_event, "footer `on` changed a recorded metric");
    assert_eq!(off_event, session_event, "footer `session` changed a recorded metric");
}

#[test]
fn workspace_search_carries_the_footer_when_enabled() {
    // The federated path appends the same pinned line, counting the union.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixture/workspace");
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
    let out = Command::new(bin()).args(["workspace", "init"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "workspace init: {}", String::from_utf8_lossy(&out.stderr));
    let out = Command::new(bin()).args(["index", "--workspace"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "index --workspace: {}", String::from_utf8_lossy(&out.stderr));
    write_footer_config(tmp.path(), "on");

    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"billing charge amount\",\"no_graph\":true}}}\n";
    let out = drive(&["mcp", "--workspace", "--dir", &tmp.path().to_string_lossy()], input);
    let resps = responses(&out);
    let text = by_id(&resps, 1)["result"]["content"][0]["text"].as_str().unwrap();
    let footer = footer_line(text).unwrap_or_else(|| panic!("no footer in: {text}"));
    assert!(footer.starts_with("cce: "), "got: {footer}");
    assert!(footer.contains(" chunks · served ~"), "got: {footer}");
}
