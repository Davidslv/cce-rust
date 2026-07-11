//! # tests/knowledge_search — the M4 retrieval blend, provenance & acceptance (SPEC-V2.6 §5/§9/§12)
//!
//! **Why this file exists:** Phase B makes the heading-chunked knowledge store
//! *searchable* through the SAME hybrid retrieval as code, blended by one shared
//! ranking, with byte-pinned provenance and deterministic staleness weighting. This
//! suite is the contract for that: it drives the `source` filter over piped JSON-RPC
//! (`code`/`knowledge`/`both`), pins the provenance line to the byte, proves a code
//! result's bytes are unchanged, and — the point of v2.6 — proves a big multi-section
//! document that was BURIED as a whole-file chunk now SURFACES its heading section as a
//! top hit once heading-chunked.
//!
//! **What it is / does:** Builds a hermetic, SYNTHETIC knowledge fixture (no real
//! company/project/domain names) + a tiny code repo, indexes both, and asserts the
//! blended/filter behaviour over the real `cce mcp` stdio server; plus a library-level
//! before/after acceptance on the whole-file-vs-heading-chunked store.
//!
//! **Responsibilities:**
//! - Own the `source` filter, provenance, blend, and acceptance goldens.
//! - Clean-room: synthetic data only.

use cce::knowledge::{ingest, parse_ndjson, search_knowledge};
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// A tiny self-contained Python repo (mirrors the base fixture shape).
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

/// Write a `cce.knowledge/v1` feed and index it into `<dir>/.cce/knowledge/`.
fn index_knowledge(dir: &Path, feed: &str) {
    let path = dir.join("feed.jsonl");
    std::fs::write(&path, feed).unwrap();
    let out = Command::new(bin())
        .args(["knowledge", "index"])
        .arg(&path)
        .args(["--dir"])
        .arg(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "knowledge index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Drive an MCP session with `input` on stdin, returning stdout.
fn drive(args: &[&str], input: &str) -> String {
    let mut cmd = Command::new(bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all_str(input);
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Small shim so `drive` reads cleanly without importing `std::io::Write` at the top.
trait WriteAllStr {
    fn write_all_str(&mut self, s: &str);
}
impl WriteAllStr for std::process::ChildStdin {
    fn write_all_str(&mut self, s: &str) {
        use std::io::Write;
        self.write_all(s.as_bytes()).unwrap();
    }
}

/// Extract the tool-result text for the response with the given id.
fn tool_text(stdout: &str, id: i64) -> String {
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["id"] == id {
            return v["result"]["content"][0]["text"].as_str().unwrap().to_string();
        }
    }
    panic!("no response with id {id} in:\n{stdout}");
}

/// A one-line JSON-RPC `context_search` call with an explicit `source`.
fn search_call(id: i64, query: &str, source: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"context_search\",\"arguments\":{{\"query\":\"{query}\",\"source\":\"{source}\",\"detail\":\"full\"}}}}}}\n"
    )
}

/// A two-record synthetic knowledge feed: a password-hashing policy (fully faceted, with
/// a merged-PR link) and an unrelated data-retention note. No real names.
fn feed() -> String {
    let r1 = serde_json::json!({
        "id": "kn:1",
        "title": "Password hashing policy",
        "body": "## Rule\n\nStore each password only as a salted slow hash; never keep the plaintext password.",
        "source": "handbook",
        "url": "https://example.test/1",
        "state": "closed",
        "state_reason": "completed",
        "updated_at": "2026-02-01T10:00:00Z",
        "labels": ["security"],
        "links": ["https://example.test/pull/7"],
    });
    let r2 = serde_json::json!({
        "id": "kn:2",
        "title": "Data retention window",
        "body": "## Rule\n\nPurge inactive account records after ninety days of no activity.",
        "source": "handbook",
        "url": "https://example.test/2",
        "state": "open",
        "updated_at": "2026-01-01T00:00:00Z",
        "labels": ["privacy"],
    });
    format!("{}\n{}\n", serde_json::to_string(&r1).unwrap(), serde_json::to_string(&r2).unwrap())
}

#[test]
fn source_filter_selects_the_right_pools_over_json_rpc() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "hash password", "code"),
        search_call(3, "hash password", "knowledge"),
        search_call(4, "hash password", "both"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);

    let code = tool_text(&out, 2);
    let knowledge = tool_text(&out, 3);
    let both = tool_text(&out, 4);

    // source:code — only the code pool; never a knowledge provenance line.
    assert!(code.contains("auth.py"), "code pool missing auth.py:\n{code}");
    assert!(!code.contains("[knowledge]"), "code pool leaked knowledge:\n{code}");

    // source:knowledge — only the knowledge pool; never a code path.
    assert!(
        knowledge.contains("[knowledge] Password hashing policy"),
        "knowledge pool missing hit:\n{knowledge}"
    );
    assert!(!knowledge.contains("auth.py"), "knowledge pool leaked code:\n{knowledge}");

    // source:both — both pools present.
    assert!(both.contains("auth.py"), "blend missing code:\n{both}");
    assert!(
        both.contains("[knowledge] Password hashing policy"),
        "blend missing knowledge:\n{both}"
    );
}

#[test]
fn provenance_line_is_byte_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "hash password", "knowledge"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    // The provenance line renders EXACTLY `[knowledge] <title> — <state> · <updated_at> · <url>`.
    assert!(
        text.contains(
            "[knowledge] Password hashing policy — closed · 2026-02-01T10:00:00Z · https://example.test/1"
        ),
        "provenance drifted:\n{text}"
    );
}

/// Pull the 16-hex `#chunk_id` off the first knowledge header line in `text`.
fn knowledge_chunk_id(text: &str) -> String {
    for line in text.lines() {
        if line.contains("[knowledge]") {
            if let Some(pos) = line.rfind('#') {
                let id: String =
                    line[pos + 1..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                if id.len() == 16 {
                    return id;
                }
            }
        }
    }
    panic!("no knowledge chunk_id in:\n{text}");
}

#[test]
fn expand_and_related_work_on_knowledge_chunks() {
    // SPEC-V2.6 §5: expand_chunk (body/file) and related_context (same-document
    // neighbours) resolve a knowledge chunk_id, not just code.
    let tmp = tempfile::tempdir().unwrap();
    // A single multi-section document, so a chunk HAS same-document neighbours.
    index_knowledge(tmp.path(), &big_handbook_feed());
    let dir = tmp.path().to_string_lossy().to_string();

    // 1) Find a knowledge chunk_id via a knowledge search.
    let find = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "refund window within fourteen days", "knowledge"),
    );
    let out = drive(&["mcp", "--dir", &dir], &find);
    let hit = tool_text(&out, 2);
    let id = knowledge_chunk_id(&hit);

    // 2) expand_chunk(body) recovers the section; expand_chunk(file) lists the document's
    //    sections; related_context returns same-document neighbours (not import-graph).
    let call = |id: i64, name: &str, extra: &str| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"{name}\",\"arguments\":{{\"chunk_id\":\"{}\"{extra}}}}}}}\n",
            knowledge_chunk_id(&hit)
        )
    };
    let input = format!(
        "{}{}{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        call(2, "expand_chunk", ",\"scope\":\"body\""),
        call(3, "expand_chunk", ",\"scope\":\"file\""),
        call(4, "related_context", ""),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);

    let body = tool_text(&out, 2);
    assert!(body.contains("refund"), "expand body missing the refund section:\n{body}");

    let file = tool_text(&out, 3);
    assert!(file.contains("document kn:handbook"), "expand file missing document header:\n{file}");
    // The document's OTHER sections are present (proving same-document grouping).
    assert!(
        file.contains("Password hashing") || file.contains("Rate limiting"),
        "expand file missing siblings:\n{file}"
    );

    let related = tool_text(&out, 4);
    assert!(related.contains("same document"), "related_context not same-document:\n{related}");
    // The target section must NOT list itself among its neighbours.
    assert!(
        !related.contains(&format!("#{id}")),
        "related_context included the target itself:\n{related}"
    );
}

// Secret-shaped test inputs are assembled from split fragments via `concat!` so no
// committed source file carries a contiguous secret literal (GitHub push protection);
// the redactor still sees the full value at runtime.
const SECRET_ID_AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");

/// A multi-section handbook whose RECORD ID carries a secret. The id cannot be
/// redacted at rest (chunk ids/document path derive from it), so it reaches disk
/// raw — the residual #144 targets. The title is clean, so only the served id
/// headers can leak it.
fn secret_id_feed() -> String {
    // A long overview forces the markdown chunker to split, so the document has
    // several sections and the searched section HAS same-document neighbours (so
    // related_context exercises the non-empty `related to <id>` header, not the
    // empty-neighbours fallback).
    let overview: String = std::iter::repeat_n(
        "This handbook collects the operating rules for the service across many areas. ",
        40,
    )
    .collect();
    let body = format!(
        "{overview}\n\n\
         ## Rate limiting\n\nReject more than one hundred requests per minute.\n\n\
         ## Refund windows\n\nApprove a refund only within fourteen days of the charge.\n\n\
         ## Data retention\n\nPurge inactive account records after ninety days.\n"
    );
    let rec = serde_json::json!({
        "id": format!("gh:{SECRET_ID_AWS_KEY}"),
        "title": "Service operations handbook",
        "body": body,
        "source": "handbook",
        "state": "open",
        "updated_at": "2026-03-01T00:00:00Z",
    });
    format!("{}\n", serde_json::to_string(&rec).unwrap())
}

#[test]
fn secret_in_record_id_is_redacted_in_served_expand_and_related() {
    // #144 Part A (RED on pre-fix code, which served the id verbatim): a secret in a
    // knowledge record id must not exfiltrate through the expand_chunk /
    // related_context headers — they render a REDACTED display form of the id.
    let tmp = tempfile::tempdir().unwrap();
    index_knowledge(tmp.path(), &secret_id_feed());
    let dir = tmp.path().to_string_lossy().to_string();

    // Find a knowledge chunk_id via search (context_search shows only the hashed
    // chunk_id, never the record id — the search hit itself carries no secret).
    let find = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "refund window fourteen days", "knowledge"),
    );
    let out = drive(&["mcp", "--dir", &dir], &find);
    let hit = tool_text(&out, 2);
    assert!(!hit.contains(SECRET_ID_AWS_KEY), "context_search leaked the raw id secret:\n{hit}");
    let id = knowledge_chunk_id(&hit);

    let call = |rpc_id: i64, name: &str, extra: &str| {
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{rpc_id},\"method\":\"tools/call\",\"params\":{{\"name\":\"{name}\",\"arguments\":{{\"chunk_id\":\"{id}\"{extra}}}}}}}\n"
        )
    };
    let input = format!(
        "{}{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        call(3, "expand_chunk", ",\"scope\":\"file\""),
        call(4, "related_context", ""),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);

    // expand_chunk(scope=file): the `document <id> — N section(s)` header + the
    // per-section `<id>:...` headers must show the REDACTED id, never the secret.
    let file = tool_text(&out, 3);
    assert!(!file.contains(SECRET_ID_AWS_KEY), "raw secret id leaked in expand file:\n{file}");
    assert!(
        file.contains("document gh:[REDACTED:AWS_ACCESS_KEY]"),
        "expand file header must show the redacted id:\n{file}"
    );

    // related_context: the `related to <id> in the same document` header must be
    // redacted too.
    let related = tool_text(&out, 4);
    assert!(!related.contains(SECRET_ID_AWS_KEY), "raw secret id leaked in related:\n{related}");
    assert!(
        related.contains("related to gh:[REDACTED:AWS_ACCESS_KEY]"),
        "related header must show the redacted id:\n{related}"
    );

    // The whole served transcript must be free of the raw secret.
    assert!(!out.contains(SECRET_ID_AWS_KEY), "raw secret id leaked in served output:\n{out}");
}

#[test]
fn code_result_bytes_are_unchanged_between_code_and_both() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "hash password", "code"),
        search_call(3, "hash password", "both"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let code = tool_text(&out, 2);
    let both = tool_text(&out, 3);

    // The auth.py header line, minus the leading rank (which reflects blended position),
    // is byte-identical: the code grammar carries no provenance and no new fields.
    let stable = |text: &str| -> String {
        let line = text.lines().find(|l| l.contains("auth.py") && l.contains('#')).unwrap();
        // Everything from the score bracket onwards (drop the `«rank». ` prefix).
        let at = line.find('[').unwrap();
        line[at..].to_string()
    };
    assert_eq!(stable(&code), stable(&both), "code result bytes changed under blend");
    // And a code row never gains a provenance tag.
    let code_line = both.lines().find(|l| l.contains("auth.py") && l.contains('#')).unwrap();
    assert!(!code_line.contains("[knowledge]"), "code row got a provenance tag: {code_line}");
}

#[test]
fn blend_interleaves_code_and_knowledge_by_shared_ranking() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "hash password", "both"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let both = tool_text(&out, 2);

    // Header lines are ranked `«n». [«score»] …`. Collect the (rank, score) pairs and
    // assert they are ONE list ordered by score desc — i.e. code + knowledge blended by
    // the shared ranking rather than concatenated pool-by-pool.
    let mut scores: Vec<f64> = Vec::new();
    let mut saw_code = false;
    let mut saw_knowledge = false;
    for line in both.lines() {
        if let Some(open) = line.find("[") {
            // Only result-header lines look like `«rank». [«0.dddddd»] …`.
            let rest = &line[open + 1..];
            if let Some(close) = rest.find(']') {
                if let Ok(s) = rest[..close].parse::<f64>() {
                    scores.push(s);
                    if line.contains("[knowledge]") {
                        saw_knowledge = true;
                    } else if line.contains("auth.py") || line.contains("payments.py") {
                        saw_code = true;
                    }
                }
            }
        }
    }
    assert!(saw_code && saw_knowledge, "blend must contain BOTH pools:\n{both}");
    assert!(scores.len() >= 2, "expected multiple ranked results:\n{both}");
    for w in scores.windows(2) {
        assert!(w[0] >= w[1], "results not ordered by descending score: {scores:?}\n{both}");
    }
}

// --- ACCEPTANCE (SPEC-V2.6 §12): buried whole-file chunk → surfaced heading section ---

/// A big multi-section handbook: a large generic overview plus several `##` sections on
/// DISTINCT topics. As a single whole-file chunk its embedding is diluted across every
/// topic, so a specific-topic query cannot isolate the relevant part.
fn big_handbook_feed() -> String {
    let overview: String = std::iter::repeat_n(
        "This handbook collects the operating rules for the service across many areas. ",
        40,
    )
    .collect();
    let body = format!(
        "{overview}\n\n\
         ## Rate limiting\n\nReject more than one hundred requests per minute from a single client.\n\n\
         ## Password hashing\n\nStore each password only as a salted slow hash; never the plaintext.\n\n\
         ## Session expiry\n\nExpire an idle session after thirty minutes of inactivity.\n\n\
         ## Refund windows\n\nApprove a refund only within fourteen days of the original charge.\n\n\
         ## Data retention\n\nPurge inactive account records after ninety days of no activity.\n"
    );
    // Encode as a one-line NDJSON record (escape newlines/quotes via serde).
    let rec = serde_json::json!({
        "id": "kn:handbook",
        "title": "Service operations handbook",
        "body": body,
        "source": "handbook",
        "url": "https://example.test/handbook",
        "state": "open",
        "updated_at": "2026-03-01T00:00:00Z",
    });
    format!("{}\n", serde_json::to_string(&rec).unwrap())
}

#[test]
fn acceptance_heading_chunking_surfaces_the_buried_section() {
    let feed = big_handbook_feed();
    let recs = parse_ndjson(&feed).unwrap();
    let query = "refund window within fourteen days of the charge";

    // BEFORE — whole-file chunk: a huge split budget keeps the entire handbook as ONE
    // chunk, so every topic is mixed together and the refund answer is buried.
    let before = ingest(&recs, feed.as_bytes(), 100_000);
    assert_eq!(before.chunks.len(), 1, "before: the handbook must be a single whole-file chunk");
    let before_hits = search_knowledge(&before, query, 5, 0.30);
    let before_top = &before_hits[0];
    // The one chunk is the whole document (its `kind` is the document title, not a
    // section), and its content mixes UNRELATED topics — the refund rule is buried.
    assert_eq!(before_top.kind, "Service operations handbook");
    assert!(before_top.content.contains("refund"));
    assert!(
        before_top.content.contains("password") && before_top.content.contains("Rate limiting"),
        "before: the whole-file chunk should mix every topic (buried)"
    );

    // AFTER — heading-chunked at the default budget: each `##` section is its own chunk,
    // so the refund SECTION surfaces as the top hit, focused on just its topic.
    let after = ingest(&recs, feed.as_bytes(), 400);
    assert!(after.chunks.len() > 1, "after: the handbook must split into heading sections");
    let after_hits = search_knowledge(&after, query, 5, 0.30);
    let after_top = &after_hits[0];
    assert_eq!(after_top.kind, "Refund windows", "after: the refund section must be the top hit");
    // The surfaced section is focused: it carries the refund rule and NOT the unrelated
    // password/rate-limit topics that diluted the whole-file chunk.
    assert!(after_top.content.contains("refund"));
    assert!(
        !after_top.content.contains("password") && !after_top.content.contains("Rate limiting"),
        "after: the surfaced section must be topic-focused, not the mixed blob"
    );
}

// --- issue #132: a code-index LOAD FAILURE must be visible in the blend ---

/// A `context_search` call with NO `source` argument — the true default path, which
/// resolves to the blend once a knowledge store exists (SPEC-V2.6 §5).
fn default_search_call(id: i64, query: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"context_search\",\"arguments\":{{\"query\":\"{query}\"}}}}}}\n"
    )
}

/// Whether the tool result with `id` carries the MCP `isError` flag.
fn tool_is_error(stdout: &str, id: i64) -> bool {
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["id"] == id {
            return v["result"]["isError"].as_bool().unwrap();
        }
    }
    panic!("no response with id {id} in:\n{stdout}");
}

#[test]
fn blended_default_surfaces_a_corrupt_code_index_instead_of_swallowing_it() {
    // Issue #132: the code store EXISTS on disk but cannot be loaded. The blended
    // default must NOT silently downgrade that to zero code rows and serve a
    // knowledge-only answer as if it were complete.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    // Corrupt the built store in place: present, unparseable.
    std::fs::write(tmp.path().join(".cce").join("index.json"), "{ not an index").unwrap();
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    // The degradation is VISIBLE (the issue #30 notice-channel precedent) …
    assert!(
        text.contains("NOTICE: the code index exists but could not be loaded"),
        "corrupt code index was swallowed silently:\n{text}"
    );
    // … the knowledge hits are still served (degraded, not withheld) …
    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "knowledge hits must still be served under a code-store failure:\n{text}"
    );
    // … and it is a degraded RESULT, not a tool error (`isError` stays reserved
    // for malformed calls, per the ToolOutput contract).
    assert!(!tool_is_error(&out, 2), "degraded blend must not set isError:\n{out}");
}

#[test]
fn knowledge_only_project_blends_silently_without_a_code_notice() {
    // CONTROL (issue #132): a code store that has never been built is NOT a failure —
    // knowledge-only is then the correct, complete answer, with no notice.
    let tmp = tempfile::tempdir().unwrap();
    index_knowledge(tmp.path(), &feed()); // no code index at all
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "knowledge-only project must return its knowledge hits:\n{text}"
    );
    assert!(!text.contains("NOTICE:"), "a truly absent code index must stay silent:\n{text}");
    assert!(!tool_is_error(&out, 2), "a knowledge-only project is not an error:\n{out}");
}

#[test]
fn healthy_blend_serves_both_pools_with_no_notice() {
    // CONTROL (issue #132): both stores healthy — code + knowledge rows, no
    // degradation marker anywhere in the output.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    assert!(text.contains("auth.py"), "healthy blend missing code:\n{text}");
    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "healthy blend missing knowledge:\n{text}"
    );
    assert!(!text.contains("NOTICE:"), "healthy blend must carry no notice:\n{text}");
}

/// A one-member workspace (a tiny JS member) with a knowledge store at its root.
fn write_tiny_workspace(root: &Path) {
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("auth.js"),
        "function hashPassword(pw) {\n  return pw + \"salt\";\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".cce")).unwrap();
    std::fs::write(
        root.join(".cce").join("workspace.yml"),
        "version: 1\nname: ws\nmembers:\n  - name: app\n    path: app\n    type: javascript\n    package: app\n",
    )
    .unwrap();
}

#[test]
fn workspace_blend_surfaces_a_corrupt_member_store() {
    // The workspace variant of issue #132: an indexed member whose store is then
    // corrupted in place must surface the same visible notice.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_workspace(tmp.path());
    let out = Command::new(bin()).args(["index", "--workspace"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    index_knowledge(tmp.path(), &feed());
    std::fs::write(tmp.path().join("app").join(".cce").join("index.json"), "{ nope").unwrap();
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir, "--workspace"], &input);
    let text = tool_text(&out, 2);

    assert!(
        text.contains(
            "NOTICE: one or more workspace member code indexes are missing or could not be loaded"
        ),
        "corrupt member store was swallowed silently:\n{text}"
    );
    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "knowledge hits must still be served under a member-store failure:\n{text}"
    );
    assert!(!tool_is_error(&out, 2), "degraded workspace blend must not set isError:\n{out}");
}

/// A two-member JS workspace (`app` + `lib`) with a knowledge store at its root — the
/// shape of a normal partially-indexed workspace.
fn write_two_member_workspace(root: &Path) {
    for (member, func) in [("app", "hashPassword"), ("lib", "verifyPassword")] {
        let dir = root.join(member);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auth.js"), format!("function {func}(pw) {{\n  return pw;\n}}\n"))
            .unwrap();
    }
    std::fs::create_dir_all(root.join(".cce")).unwrap();
    std::fs::write(
        root.join(".cce").join("workspace.yml"),
        "version: 1\nname: ws\nmembers:\n  - name: app\n    path: app\n    type: javascript\n    package: app\n  - name: lib\n    path: lib\n    type: javascript\n    package: lib\n",
    )
    .unwrap();
}

#[test]
fn partially_indexed_workspace_surfaces_the_incomplete_notice_without_a_false_corruption_claim() {
    // Issue #132 (reviewer NIT): the common steady state — one member indexed, another
    // never indexed. Nothing is corrupt, but code retrieval IS incomplete, so the blend
    // must surface the workspace notice. Its wording must NOT claim corruption, since
    // the federated path cannot tell "corrupt" from "not yet indexed".
    let tmp = tempfile::tempdir().unwrap();
    write_two_member_workspace(tmp.path());
    // Index ONLY `app`; leave `lib` unindexed (no `lib/.cce/index.json`).
    let out = Command::new(bin()).args(["index"]).arg(tmp.path().join("app")).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir, "--workspace"], &input);
    let text = tool_text(&out, 2);

    // The incomplete-code notice fires …
    assert!(
        text.contains(
            "NOTICE: one or more workspace member code indexes are missing or could not be loaded"
        ),
        "partially-indexed workspace did not surface the incomplete notice:\n{text}"
    );
    // … but it must NOT falsely diagnose corruption of a store that was simply
    // never built.
    assert!(
        !text.contains("corrupt or unreadable store"),
        "partially-indexed workspace got a false corruption claim:\n{text}"
    );
    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "knowledge hits must still be served:\n{text}"
    );
    assert!(!tool_is_error(&out, 2), "an incomplete workspace blend is not an error:\n{out}");
}

#[test]
fn workspace_blend_stays_silent_when_no_member_was_ever_indexed() {
    // CONTROL (issue #132, workspace variant): a workspace whose members were never
    // indexed has NO member store on disk — true absence, so the blend serves the
    // knowledge hits silently, exactly like the single-repo knowledge-only project.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_workspace(tmp.path());
    index_knowledge(tmp.path(), &feed()); // knowledge only; `index --workspace` never ran
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir, "--workspace"], &input);
    let text = tool_text(&out, 2);

    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "never-indexed workspace must still return knowledge hits:\n{text}"
    );
    assert!(!text.contains("NOTICE:"), "a truly absent member store must stay silent:\n{text}");
    assert!(!tool_is_error(&out, 2), "a never-indexed workspace is not an error:\n{out}");
}

// --- issue #143: a knowledge-store LOAD FAILURE must be visible (mirror of #132) ---

/// Corrupt the active knowledge snapshot in place: keep the `current` pointer intact,
/// scribble the snapshot artifact it names — present-but-unparseable, tagged
/// `InvalidData` by the loader (issue #143). Mirrors the code side's in-place
/// `index.json` corruption.
fn corrupt_knowledge_snapshot(root: &Path) {
    let kdir = root.join(".cce").join("knowledge");
    let snapshot = std::fs::read_to_string(kdir.join("current")).unwrap();
    std::fs::write(kdir.join(format!("{}.json", snapshot.trim())), "{ not knowledge").unwrap();
}

#[test]
fn blended_default_surfaces_a_corrupt_knowledge_store_instead_of_swallowing_it() {
    // Issue #143: the knowledge store EXISTS on disk but cannot be loaded. The blended
    // default must NOT silently downgrade that to zero knowledge rows and serve a
    // code-only answer as if it were complete — the mirror of the code-side #132 bug.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    corrupt_knowledge_snapshot(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    // The degradation is VISIBLE (the issue #30 notice-channel precedent) …
    assert!(
        text.contains("NOTICE: the knowledge store exists but could not be loaded"),
        "corrupt knowledge store was swallowed silently:\n{text}"
    );
    // … the code hits are still served (degraded, not withheld) …
    assert!(
        text.contains("auth.py"),
        "code hits must still be served under a knowledge-store failure:\n{text}"
    );
    // … and it is a degraded RESULT, not a tool error (`isError` stays reserved for
    // malformed calls, per the ToolOutput contract).
    assert!(!tool_is_error(&out, 2), "degraded blend must not set isError:\n{out}");
}

#[test]
fn explicit_knowledge_source_surfaces_a_corrupt_store_not_zero_chunks() {
    // Issue #143: even the explicit `source:"knowledge"` path masked corruption as
    // emptiness ("The index has 0 chunk(s)"). It must surface the load-failure notice.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    corrupt_knowledge_snapshot(tmp.path());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        search_call(2, "hash password", "knowledge"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    assert!(
        text.contains("NOTICE: the knowledge store exists but could not be loaded"),
        "explicit knowledge source masked corruption as emptiness:\n{text}"
    );
    assert!(!tool_is_error(&out, 2), "degraded knowledge-only path must not set isError:\n{out}");
}

#[test]
fn knowledge_absent_project_blends_silently_without_a_knowledge_notice() {
    // CONTROL (issue #143): a knowledge store that was NEVER ingested is NOT a failure —
    // code-only is then the correct, complete answer, with no knowledge notice.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path()); // healthy code; no knowledge store at all
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    assert!(
        text.contains("auth.py"),
        "knowledge-absent project must return its code hits:\n{text}"
    );
    assert!(
        !text.contains("NOTICE: the knowledge store"),
        "a truly absent knowledge store must stay silent:\n{text}"
    );
    assert!(!tool_is_error(&out, 2), "a knowledge-absent project is not an error:\n{out}");
}

#[test]
fn healthy_blend_serves_both_pools_with_no_knowledge_notice() {
    // CONTROL (issue #143): both stores healthy — code + knowledge rows, no degradation
    // marker anywhere in the output.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    index_dir(tmp.path());
    index_knowledge(tmp.path(), &feed());
    let dir = tmp.path().to_string_lossy().to_string();

    let input = format!(
        "{}{}",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        default_search_call(2, "hash password"),
    );
    let out = drive(&["mcp", "--dir", &dir], &input);
    let text = tool_text(&out, 2);

    assert!(text.contains("auth.py"), "healthy blend missing code:\n{text}");
    assert!(
        text.contains("[knowledge] Password hashing policy"),
        "healthy blend missing knowledge:\n{text}"
    );
    assert!(!text.contains("NOTICE:"), "healthy blend must carry no notice:\n{text}");
}
