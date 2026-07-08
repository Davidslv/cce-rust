//! # tests/ollama — the Ollama loud-failure policy, proven hermetically (#30)
//!
//! **Why this file exists:** Issue #30: the opt-in Ollama embedder path used to
//! degrade *silently* — batch failures became empty vectors, index time
//! persisted them forever, and query time silently cosined a hash query vector
//! against ollama-built embeddings. These tests pin the loud replacements:
//! index time aborts non-zero persisting nothing, CLI search refuses a space
//! mismatch with guidance, and MCP `context_search` degrades to BM25-only with
//! a pinned, visible notice.
//!
//! **What it is / does:** Drives the real `cce` binary against a local HTTP
//! stub of `POST /api/embed` (loopback only — succeeds N times, then fails) or
//! a closed port, via the `CCE_OLLAMA_URL` override. No real Ollama server is
//! ever contacted; the default suite stays fully hermetic. The one test that
//! needs a live server remains `#[ignore]`.
//!
//! **Responsibilities:**
//! - Own the process-level Ollama failure-policy tests (SPEC §11 + issue #30).
//! - Own the opt-in live-server integration test.

use cce::embedder::{Embedder, OllamaEmbedder};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// A URL no server listens on: port 1 refuses connections instantly.
const CLOSED_PORT_URL: &str = "http://127.0.0.1:1";

/// Write a tiny self-contained Python repo into `dir`.
fn write_tiny_repo(dir: &Path) {
    std::fs::write(
        dir.join("auth.py"),
        "def hash_password(password):\n    return password + 'salt'\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("payments.py"),
        "import auth\n\ndef process_payment(amount):\n    return amount\n",
    )
    .unwrap();
}

/// Read one HTTP request (headers + Content-Length body) off `stream`.
fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    // Headers, byte-by-byte until the blank line (tiny requests; test-only).
    while !buf.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => buf.push(byte[0]),
            _ => return Vec::new(),
        }
    }
    let headers = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 && stream.read_exact(&mut body).is_err() {
        return Vec::new();
    }
    body
}

/// Spawn a loopback-only stub of Ollama's `POST /api/embed`. The first
/// `ok_requests` requests succeed (one 3-dim embedding per input); every later
/// request is a 500. Returns the stub's base URL. The serving thread is
/// detached; it dies with the test process.
fn spawn_embed_stub(ok_requests: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut served = 0usize;
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_http_request(&mut s);
            let inputs = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("input")?.as_array().map(|a| a.len()))
                .unwrap_or(1);
            if served < ok_requests {
                let embs: Vec<Vec<f64>> = (0..inputs).map(|_| vec![0.1, 0.2, 0.3]).collect();
                let json = serde_json::json!({ "embeddings": embs }).to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{}",
                    json.len(),
                    json
                );
            } else {
                let _ = write!(
                    s,
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: \
                     close\r\n\r\n"
                );
            }
            served += 1;
        }
    });
    format!("http://{addr}")
}

/// Write `files` tiny single-function Python files into `dir` — enough chunks
/// to cross several `EMBED_BATCH_SIZE` boundaries (#38).
fn write_many_file_repo(dir: &Path, files: usize) {
    for i in 0..files {
        std::fs::write(
            dir.join(format!("mod_{i:03}.py")),
            format!("def func_{i:03}(x):\n    return x + {i}\n"),
        )
        .unwrap();
    }
}

/// The distinguishable per-input vector the batching stub returns: a pure
/// function of the input text, so a test can recompute it per chunk and prove
/// each vector landed on the chunk whose content produced it.
fn stub_vector(text: &str) -> Vec<f64> {
    let bytes = text.as_bytes();
    vec![
        bytes.len() as f64,
        bytes.first().copied().unwrap_or(0) as f64,
        bytes.last().copied().unwrap_or(0) as f64,
    ]
}

/// What the batching stub does once `ok_requests` requests have been served.
enum StubAfter {
    /// Respond 500 — Ollama "dies mid-index".
    Fail500,
    /// Respond 200 but with one embedding fewer than there were inputs —
    /// a count-mismatched batch that must never be zipped silently (#30/#38).
    WrongCount,
}

/// Spawn a loopback `POST /api/embed` stub for the batching tests (#38): the
/// first `ok_requests` requests succeed with one `stub_vector` per input; later
/// requests do `after`. Returns the base URL and a live served-request counter.
fn spawn_batch_stub(
    ok_requests: usize,
    after: StubAfter,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let served_counter = counter.clone();
    std::thread::spawn(move || {
        let mut served = 0usize;
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let body = read_http_request(&mut s);
            let inputs: Vec<String> = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| serde_json::from_value(v.get("input")?.clone()).ok())
                .unwrap_or_default();
            served_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let respond_ok = |s: &mut TcpStream, embs: Vec<Vec<f64>>| {
                let json = serde_json::json!({ "embeddings": embs }).to_string();
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{}",
                    json.len(),
                    json
                );
            };
            if served < ok_requests {
                respond_ok(&mut s, inputs.iter().map(|t| stub_vector(t)).collect());
            } else {
                match after {
                    StubAfter::Fail500 => {
                        let _ = write!(
                            s,
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: \
                             0\r\nConnection: close\r\n\r\n"
                        );
                    }
                    StubAfter::WrongCount => {
                        let short = inputs.len().saturating_sub(1);
                        respond_ok(
                            &mut s,
                            inputs.iter().take(short).map(|t| stub_vector(t)).collect(),
                        );
                    }
                }
            }
            served += 1;
        }
    });
    (format!("http://{addr}"), counter)
}

/// `cce index <dir> --embedder ollama` against the given Ollama URL.
fn run_index(dir: &Path, ollama_url: &str) -> std::process::Output {
    Command::new(bin())
        .args(["index"])
        .arg(dir)
        .args(["--embedder", "ollama"])
        .env("CCE_OLLAMA_URL", ollama_url)
        .output()
        .unwrap()
}

#[test]
fn mid_index_embedding_failure_aborts_nonzero_and_persists_nothing() {
    // The stub passes the health check (1 OK request), then fails: Ollama
    // "dies mid-index". The build must abort non-zero and write NO store —
    // never a store with empty embeddings (#30).
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    let stub_url = spawn_embed_stub(1);

    let out = run_index(tmp.path(), &stub_url);
    assert!(!out.status.success(), "a mid-index embedding failure must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("embedding failed"), "stderr must name the failure: {stderr}");
    assert!(stderr.contains("Aborting the index"), "stderr must say it aborted: {stderr}");
    assert!(
        !tmp.path().join(".cce").join("index.json").exists(),
        "no store may be persisted after an embedding failure"
    );
}

#[test]
fn ollama_built_store_has_no_empty_embeddings_and_search_refuses_when_down() {
    // Index against an always-healthy stub: the store is genuinely
    // ollama-built (exercising the HTTP success parse path), with a non-empty
    // embedding for every chunk. Then query it with Ollama "down": the CLI
    // must refuse with guidance (#30) — never silently embed the query with
    // the hash backend into a different vector space.
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    let stub_url = spawn_embed_stub(usize::MAX);

    let out = run_index(tmp.path(), &stub_url);
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    let store = tmp.path().join(".cce").join("index.json");
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    assert_eq!(data["embedder"], "ollama");
    let chunks = data["chunks"].as_array().unwrap();
    assert!(!chunks.is_empty());
    for c in chunks {
        assert!(
            !c["embedding"].as_array().unwrap().is_empty(),
            "an ollama-built store must never contain an empty embedding"
        );
    }

    let out = Command::new(bin())
        .args(["search", "hash password", "--store"])
        .arg(&store)
        .env("CCE_OLLAMA_URL", CLOSED_PORT_URL)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a hash-vs-ollama space mismatch must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("built with the ollama embedder") && stderr.contains("unreachable"),
        "stderr must explain the mismatch: {stderr}"
    );
    assert!(
        stderr.contains("re-index with the default hash embedder"),
        "stderr must say how to recover: {stderr}"
    );
}

#[test]
fn mcp_context_search_on_ollama_store_with_ollama_down_is_bm25_with_notice() {
    // The MCP session must NOT crash (the friendly-error pattern) and must NOT
    // silently cosine across spaces: it degrades to BM25-only keyword results
    // under the pinned, visible notice line (#30).
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    let stub_url = spawn_embed_stub(usize::MAX);
    let out = run_index(tmp.path(), &stub_url);
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\
                 \"arguments\":{\"query\":\"hash password\"}}}\n";
    let mut child = Command::new(bin())
        .args(["mcp", "--dir"])
        .arg(tmp.path())
        .env("CCE_OLLAMA_URL", CLOSED_PORT_URL)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let resp: serde_json::Value = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .find(|r: &serde_json::Value| r["id"] == 1)
        .expect("no response with id 1");

    assert_eq!(resp["result"]["isError"], false, "must be a friendly result, not a crash");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("NOTICE: this index was built with the ollama embedder"),
        "the pinned degradation notice must lead the result: {text}"
    );
    assert!(text.contains("BM25"), "the notice must name the BM25-only mode: {text}");
    assert!(text.contains("auth.py"), "keyword recall must still serve results: {text}");
}

// --- Index-time batching (#38): request-count collapse, order, fail-loud ---

/// How many files the batching tests index: enough chunks to cross several
/// `EMBED_BATCH_SIZE` (64) boundaries.
const MANY_FILES: usize = 130;

#[test]
fn indexing_issues_one_request_per_batch_not_per_chunk() {
    // #38: N chunks must cost ceil(N / EMBED_BATCH_SIZE) embed requests
    // (+ 1 health check), not N.
    let tmp = tempfile::tempdir().unwrap();
    write_many_file_repo(tmp.path(), MANY_FILES);
    let (stub_url, requests) = spawn_batch_stub(usize::MAX, StubAfter::Fail500);

    let out = run_index(tmp.path(), &stub_url);
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    let store = tmp.path().join(".cce").join("index.json");
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    let n_chunks = data["chunks"].as_array().unwrap().len();
    assert!(
        n_chunks > cce::config::EMBED_BATCH_SIZE,
        "need >1 batch for this test to prove anything; got {n_chunks} chunks"
    );

    let expected = 1 + n_chunks.div_ceil(cce::config::EMBED_BATCH_SIZE); // health + batches
    let served = requests.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        served,
        expected,
        "{n_chunks} chunks must cost {expected} requests (1 health + \
         ceil({n_chunks}/{})), not {served}",
        cce::config::EMBED_BATCH_SIZE
    );
}

#[test]
fn batched_vectors_land_on_the_chunks_whose_content_produced_them() {
    // #38: the stub returns a vector that is a pure function of each input
    // text, so a misalignment anywhere (within a batch or across batch
    // boundaries) would leave some chunk with the wrong vector.
    let tmp = tempfile::tempdir().unwrap();
    write_many_file_repo(tmp.path(), MANY_FILES);
    let (stub_url, _) = spawn_batch_stub(usize::MAX, StubAfter::Fail500);

    let out = run_index(tmp.path(), &stub_url);
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    let store = tmp.path().join(".cce").join("index.json");
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
    let chunks = data["chunks"].as_array().unwrap();
    assert!(chunks.len() > cce::config::EMBED_BATCH_SIZE);
    for c in chunks {
        // The embedder truncates each input to 2000 chars before sending.
        let sent: String = c["content"].as_str().unwrap().chars().take(2000).collect();
        let got: Vec<f64> =
            c["embedding"].as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect();
        assert_eq!(
            got,
            stub_vector(&sent),
            "chunk {} carries a vector produced by some other chunk's content",
            c["chunk_id"]
        );
    }
}

#[test]
fn mid_index_batch_failure_aborts_nonzero_and_persists_nothing() {
    // The #30 invariant at batch granularity (#38): health check and the first
    // batch succeed, the second batch 500s. The build must abort non-zero,
    // name the failing batch's file span, and write NO store — never the
    // first batch's vectors.
    let tmp = tempfile::tempdir().unwrap();
    write_many_file_repo(tmp.path(), MANY_FILES);
    let (stub_url, _) = spawn_batch_stub(2, StubAfter::Fail500);

    let out = run_index(tmp.path(), &stub_url);
    assert!(!out.status.success(), "a mid-index batch failure must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("embedding failed"), "stderr must name the failure: {stderr}");
    assert!(stderr.contains("a batch of"), "stderr must name the batch's span: {stderr}");
    assert!(stderr.contains("Aborting the index"), "stderr must say it aborted: {stderr}");
    assert!(
        !tmp.path().join(".cce").join("index.json").exists(),
        "no store may be persisted after a batch failure"
    );
}

#[test]
fn count_mismatched_batch_is_an_error_not_a_silent_misalignment() {
    // #30's count guard at batch granularity (#38): a 200 response carrying
    // the wrong number of embeddings must abort the index, never be zipped
    // onto the wrong chunks.
    let tmp = tempfile::tempdir().unwrap();
    write_many_file_repo(tmp.path(), MANY_FILES);
    let (stub_url, _) = spawn_batch_stub(1, StubAfter::WrongCount); // health OK, then short

    let out = run_index(tmp.path(), &stub_url);
    assert!(!out.status.success(), "a count-mismatched batch must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("embedding failed"), "stderr must name the failure: {stderr}");
    assert!(
        stderr.contains("embedding(s) for") && stderr.contains("input(s)"),
        "stderr must name the count mismatch: {stderr}"
    );
    assert!(
        !tmp.path().join(".cce").join("index.json").exists(),
        "no store may be persisted after a count-mismatched batch"
    );
}

#[test]
#[ignore = "requires a local Ollama server; run with --ignored"]
fn ollama_embeds_when_available() {
    let oll = OllamaEmbedder::default();
    if !oll.healthy() {
        eprintln!("skipping: no Ollama server at {}", oll.base_url);
        return;
    }
    let vecs = oll.try_embed_batch(&["hello world".to_string(), "goodbye".to_string()]).unwrap();
    assert_eq!(vecs.len(), 2);
    assert!(!vecs[0].is_empty());
    assert_eq!(vecs[0].len(), vecs[1].len());
    // single embed path
    assert!(!oll.embed("single").is_empty());
}
