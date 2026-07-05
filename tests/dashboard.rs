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

    // Unknown path -> 404
    let (status, body) = http_get(port, "/nope");
    assert!(status.contains("404"), "status: {status}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("error").is_some());

    handle.join().unwrap();
}
