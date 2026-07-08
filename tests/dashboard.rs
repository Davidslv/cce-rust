//! # tests/dashboard — the metrics dashboard over a real loopback socket
//!
//! **Why this file exists:** `route` is unit-tested in-process, but DASHBOARD-SPEC
//! §6/§8 require the actual HTTP server to answer on an ephemeral port. Only a
//! real `TcpListener` + `TcpStream` round-trip proves the socket path (request
//! parsing, headers, status codes) works end-to-end.
//!
//! **What it is / does:** Binds `127.0.0.1:0`, serves a bounded number of
//! connections on a background thread, and issues real `GET` requests for `/`,
//! `/api/metrics`, `/api/health`, and an unknown path, asserting on the responses.
//!
//! **Responsibilities:**
//! - Own the process/socket-level acceptance test for the dashboard.
//! - It is hermetic: loopback only, no external network, bounded shutdown.

use cce::dashboard::serve;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

fn fixture_metrics() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base/metrics_sample.jsonl"))
}

/// Issue one HTTP/1.1 GET on a fresh connection; return (status_line, body).
fn http_get(port: u16, path: &str) -> (String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head.lines().next().unwrap_or("").to_string();
    (status, body.to_string())
}

#[test]
fn serves_page_api_and_health_on_ephemeral_port() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    // Serve exactly the four connections this test makes, then the thread ends.
    let handle = std::thread::spawn(move || {
        serve(listener, fixture_metrics(), 3.00, Some(4));
    });

    // GET / -> HTML page
    let (status, body) = http_get(port, "/");
    assert!(status.contains("200"), "status: {status}");
    assert!(body.contains("<title>CCE Dashboard</title>"));

    // GET /api/health -> event/skipped counts
    let (status, body) = http_get(port, "/api/health");
    assert!(status.contains("200"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["events"], 7);
    assert_eq!(v["skipped"], 0);

    // GET /api/metrics -> the aggregate (with the anchor numbers)
    let (status, body) = http_get(port, "/api/metrics");
    assert!(status.contains("200"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema"], "cce.metrics/v1");
    assert_eq!(v["totals"]["searches"], 4);
    assert_eq!(v["totals"]["tokens_saved"], 53000);
    assert!(v.get("generated_ts").is_some());
    // v2.4.1 refreshed panels are served over the real socket, purely log-derived.
    assert_eq!(v["by_source"]["cli"]["searches"], 4);
    assert_eq!(v["secret_safety"]["sensitive_skipped"], 0);
    assert_eq!(v["index_freshness"]["source"], "local");
    assert!(v["index_freshness"].get("behind_remote").is_none());
    // SPEC-V2.5 §3: the seven-bucket ledger is served, purely log-derived, labelled.
    let ledger = &v["savings_by_layer"];
    assert_eq!(ledger["retrieval"]["saved_tokens"], 53000);
    assert_eq!(ledger["retrieval"]["baseline_tokens"], 70000);
    assert_eq!(ledger["total"]["saved_tokens"], 53000);
    assert_eq!(ledger["chunk_compression"]["saved_tokens"], 0);
    assert_eq!(ledger["note"], "vs full-file baseline — not your real end-to-end agent cost");

    // Unknown path -> 404
    let (status, body) = http_get(port, "/nope");
    assert!(status.contains("404"), "status: {status}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("error").is_some());

    handle.join().unwrap();
}

// --- `cce dashboard` driven through the real binary (issue #37) ---

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// Kill-on-drop guard: the dashboard child never outlives the test, even when
/// an assertion (or the startup timeout) panics.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Read the child's stdout until the `serving http://…` line appears and return
/// the bound port. Robust to slow startup: a reader thread feeds a channel and
/// the wait is bounded by a 30s deadline rather than blocking forever.
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
                // Loopback only (DASHBOARD-SPEC §6) — pinned at the binary level.
                assert!(addr.starts_with("127.0.0.1:"), "must bind loopback, got: {line}");
                return addr.rsplit(':').next().unwrap().parse().expect("port parses");
            }
            Err(_) => panic!("dashboard did not print its serving line within 30s"),
        }
    }
}

#[test]
fn dashboard_cli_serves_health_on_an_ephemeral_port() {
    // Issue #37: drive `cce dashboard --port 0 --no-open` through the real
    // binary — port 0 binds an ephemeral port, the URL is printed to stdout,
    // and /api/health answers 200 with valid JSON. The guard kills the child
    // even if an assertion fails.
    let child = std::process::Command::new(bin())
        .args(["dashboard", "--metrics"])
        .arg(fixture_metrics())
        .args(["--port", "0", "--no-open"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = ChildGuard(child);
    let port = wait_for_bound_port(&mut guard.0);

    let (status, body) = http_get(port, "/api/health");
    assert!(status.contains("200"), "status: {status}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["events"], 7, "fixture log has 7 events: {body}");
}

#[test]
fn dashboard_cli_workspace_variant_serves_federated_health() {
    // Issue #37: the `--workspace` wiring of cmd_dashboard — manifest load,
    // member metrics federation, ephemeral port — driven through the binary.
    let fixture = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"));
    let tmp = tempfile::tempdir().unwrap();
    for entry in walkdir::WalkDir::new(&fixture).into_iter().flatten() {
        let rel = entry.path().strip_prefix(&fixture).unwrap();
        let target = tmp.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
    let root = tmp.path().to_str().unwrap();
    let out = std::process::Command::new(bin()).args(["workspace", "init", root]).output().unwrap();
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));

    let child = std::process::Command::new(bin())
        .args(["dashboard", root, "--workspace", "--port", "0", "--no-open"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut guard = ChildGuard(child);
    let port = wait_for_bound_port(&mut guard.0);

    let (status, body) = http_get(port, "/api/health");
    assert!(status.contains("200"), "status: {status}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["members"], 3, "fixture workspace has 3 members: {body}");
}
