//! # tests/corpus_serve — the corpus bridge over a real loopback socket (ADR-CORPUS-SERVE; #11)
//!
//! **Why this file exists:** `route` is unit-tested in-process, but U1.3's done-when
//! (G1 / G-BRIDGE-01) is an *acceptance* claim: a live `cce corpus serve` over a real
//! `.cce/knowledge/` store answers `GET /docs?service=` with a decision-usable doc set,
//! and refuses an unauthenticated request with `401` — in the same run. Only a real
//! `TcpListener`/`TcpStream` round-trip against the shipped binary proves the socket path
//! (request parse, `Authorization` header, status codes, `{"docs":[…]}` body) end-to-end.
//! This is the producer-side half of the seam; teaching signal-engine's client to send
//! the bearer token (replacing its corpus mocks) is the consumer-side ticket #12/U1.4.
//!
//! **What it is / does:** Indexes a synthetic knowledge feed through the real binary,
//! starts `cce corpus serve --port 0 --token-file …` as a child, then issues real GETs:
//! authenticated `service=checkout` → 200 with non-empty `docs` carrying truthy `id`s
//! (the consumer's `corpus_doc_ids`); the same request with no/ wrong bearer → 401.
//!
//! **Responsibilities:**
//! - Own the process/socket-level acceptance test for the bridge.
//! - Hermetic: loopback only, synthetic data, bounded startup, kill-on-drop child.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// A one-record synthetic knowledge feed (no real names): a checkout-service runbook the
/// query `service=checkout` must surface.
fn feed() -> String {
    let r = serde_json::json!({
        "id": "kn:checkout",
        "title": "Checkout service runbook",
        "body": "## Restart\n\nThe checkout service restarts cleanly; drain connections first.",
        "source": "handbook",
        "url": "https://example.test/checkout",
        "updated_at": "2026-02-01T10:00:00Z",
        "labels": ["checkout"],
    });
    format!("{}\n", serde_json::to_string(&r).unwrap())
}

/// Index a `cce.knowledge/v1` feed into `<dir>/.cce/knowledge/` through the real binary.
fn index_knowledge(dir: &Path, feed: &str) {
    let path = dir.join("feed.jsonl");
    std::fs::write(&path, feed).unwrap();
    let out = std::process::Command::new(bin())
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

/// Issue one HTTP/1.1 GET on a fresh connection, optionally with a bearer token; return
/// (status_line, body).
fn http_get(port: u16, path: &str, bearer: Option<&str>) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = match bearer {
        Some(t) => format!("Authorization: Bearer {t}\r\n"),
        None => String::new(),
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head.lines().next().unwrap_or("").to_string();
    (status, body.to_string())
}

/// Issue one GET and return the FULL raw HTTP/1.1 response (head + body), so a test can
/// assert on the response head — used to prove a hostile `q` injects no header.
fn raw_get(port: u16, path: &str, bearer: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {bearer}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    raw
}

/// Kill-on-drop guard: the serve child never outlives the test.
struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Read the child's stdout until the `serving http://…` line appears; return the port.
fn wait_for_bound_port(child: &mut std::process::Child) -> u16 {
    use std::io::BufRead;
    let stdout = child.stdout.take().expect("child stdout must be piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let Some(rest) = line.split("serving http://").nth(1) else { continue };
                let addr = rest.split('/').next().unwrap_or_default();
                // Loopback only by construction (ADR-CORPUS-SERVE OD2) — pinned here.
                assert!(addr.starts_with("127.0.0.1:"), "must bind loopback, got: {line}");
                return addr.rsplit(':').next().unwrap().parse().expect("port parses");
            }
            Err(_) => panic!("corpus serve did not print its serving line within 30s"),
        }
    }
}

#[test]
fn live_bridge_serves_authenticated_docs_and_401s_the_unauthenticated() {
    let tmp = tempfile::tempdir().unwrap();
    index_knowledge(tmp.path(), &feed());

    // The bearer token is a per-instance secret, delivered via a file (off the argv).
    let token = "test-corpus-token-abc123";
    let token_path = tmp.path().join("corpus.token");
    std::fs::write(&token_path, format!("{token}\n")).unwrap();

    let child = std::process::Command::new(bin())
        .args(["corpus", "serve", "--dir"])
        .arg(tmp.path())
        .args(["--port", "0", "--token-file"])
        .arg(&token_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = ChildGuard(child);
    let port = wait_for_bound_port(&mut guard.0);

    // Authenticated: a real decision-usable doc set with a non-empty corpus_doc_ids.
    let (status, body) = http_get(port, "/docs?service=checkout", Some(token));
    assert!(status.contains("200"), "status: {status} body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let docs = v["docs"].as_array().expect("docs is an array");
    assert!(!docs.is_empty(), "checkout must surface at least one doc: {body}");
    // Each doc carries the truthy id the consumer records as corpus_doc_ids, plus body.
    assert!(docs[0]["id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(docs[0]["title"], "Checkout service runbook");
    assert!(docs[0]["body"].as_str().unwrap().contains("checkout service restarts"));

    // Unauthenticated: same request, no bearer → 401, no docs leaked.
    let (status, body) = http_get(port, "/docs?service=checkout", None);
    assert!(status.contains("401"), "unauth must be 401, got: {status} body: {body}");
    assert!(!body.contains("Checkout service runbook"), "401 must not leak docs: {body}");

    // Wrong token → 401 too.
    let (status, _) = http_get(port, "/docs?service=checkout", Some("not-the-token"));
    assert!(status.contains("401"), "wrong token must be 401, got: {status}");
}

#[test]
fn live_bridge_fences_a_hostile_q_and_still_answers() {
    // U2.3 done-when, over a real socket: an incident-derived `q=` is untrusted input. A
    // hostile q — CRLF header-injection, a shell substitution, NULs, and a length flood —
    // must not steer the live server into an error or an injected header; it degrades to a
    // well-formed 200 answer. Only a real round-trip proves the wire response is unpolluted.
    let tmp = tempfile::tempdir().unwrap();
    index_knowledge(tmp.path(), &feed());

    let token = "test-corpus-token-abc123";
    let token_path = tmp.path().join("corpus.token");
    std::fs::write(&token_path, format!("{token}\n")).unwrap();

    let child = std::process::Command::new(bin())
        .args(["corpus", "serve", "--dir"])
        .arg(tmp.path())
        .args(["--port", "0", "--token-file"])
        .arg(&token_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = ChildGuard(child);
    let port = wait_for_bound_port(&mut guard.0);

    // A CRLF-injection attempt smuggled through q: `%0d%0a` decodes to CRLF server-side.
    // The fence neutralises it, so no `X-Injected` header can appear in the response head.
    let (status, body) =
        http_get(port, "/docs?service=checkout&q=timeout%0d%0aX-Injected:+evil", Some(token));
    assert!(status.contains("200"), "hostile q must still answer 200: {status} {body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["docs"].is_array(), "docs must be an array: {body}");

    // Re-issue and read the RAW response so we can assert the header block is unpolluted.
    let raw = raw_get(port, "/docs?service=checkout&q=a%0d%0aX-Injected:+evil", token);
    let head = raw.split("\r\n\r\n").next().unwrap_or("");
    assert!(
        !head.to_ascii_lowercase().contains("x-injected"),
        "no injected header may appear in the response head: {head:?}"
    );

    // A shell substitution, NULs, and a 2k-char flood (over the fence's 500-char cap, but
    // within the 8 KiB request-head bound) each still return a clean 200 — the server never
    // errors on a hostile q (degrade-never-block); the fence caps the over-long term.
    for q in ["%24%28rm+-rf+%2F%29", "%00%00%00", &"z".repeat(2_000)] {
        let (status, body) = http_get(port, &format!("/docs?service=checkout&q={q}"), Some(token));
        assert!(status.contains("200"), "hostile q={q} must answer 200: {status} {body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["docs"].is_array(), "docs must be an array for q={q}: {body}");
    }
}

#[test]
fn serve_refuses_to_start_without_a_token() {
    // Authenticated by construction (OD1): no CCE_CORPUS_TOKEN, no --token-file → the
    // command fails loud rather than serving an open corpus.
    let tmp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(bin())
        .args(["corpus", "serve", "--dir"])
        .arg(tmp.path())
        .args(["--port", "0"])
        .env_remove("CCE_CORPUS_TOKEN")
        .output()
        .unwrap();
    assert!(!out.status.success(), "must fail without a token");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("requires a bearer token"), "stderr: {stderr}");
}

#[test]
fn serve_fails_loud_on_a_broken_store_not_silently_empty() {
    // A dangling `current` pointer (present, but its snapshot is missing — a half-written
    // deploy) is a BROKEN store, not an un-provisioned one. It must fail the command loud,
    // never be served as `{"docs":[]}` — that silent zero-context degradation is exactly
    // the failure the bridge exists to eliminate. (An absent store is the empty path,
    // proven live green in the acceptance test above.)
    let tmp = tempfile::tempdir().unwrap();
    let store_dir = tmp.path().join(".cce").join("knowledge");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::write(store_dir.join("current"), "deadbeefdeadbeef\n").unwrap(); // names no snapshot

    let token_path = tmp.path().join("corpus.token");
    std::fs::write(&token_path, "tok\n").unwrap();

    let out = std::process::Command::new(bin())
        .args(["corpus", "serve", "--dir"])
        .arg(tmp.path())
        .args(["--port", "0", "--token-file"])
        .arg(&token_path)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a dangling store pointer must fail loud, not serve empty");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("corpus serve failed"), "stderr: {stderr}");
}
