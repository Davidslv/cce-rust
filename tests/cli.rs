//! # tests/cli — end-to-end CLI integration tests
//!
//! **Why this file exists:** SPEC §9/§12 require that `index` then `search` work
//! across separate process runs, that `conformance` is deterministic, and that
//! invalid input fails cleanly. Unit tests cannot prove the "fresh process"
//! guarantee — only spawning the real binary can.
//!
//! **What it is / does:** Drives the built `cce` binary as a subprocess against
//! the fixture, checking the index→search round-trip across processes, stats,
//! twice-identical conformance output, and non-zero exit on bad input.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests.
//! - It does NOT touch library internals.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn fixture() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
}

fn samples() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"))
}

#[test]
fn index_then_search_in_fresh_process() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("index.json");

    // Process 1: index.
    let out = Command::new(bin())
        .args(["index"])
        .arg(fixture())
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(store.exists(), "store not written");

    // Process 2: search, fresh process, JSON output.
    let out = Command::new(bin())
        .args(["search", "hash password", "--store"])
        .arg(&store)
        .args(["--no-graph", "--json", "--top-k", "5"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // DASHBOARD-SPEC §5: --json is now an object with a top-level `query_id`
    // field wrapping the `results` array.
    let arr = v["results"].as_array().unwrap();
    assert!(!arr.is_empty());
    assert_eq!(arr[0]["file_path"], "auth.py");
    // score is a fixed 6-decimal string
    let score = arr[0]["score"].as_str().unwrap();
    assert_eq!(score.split('.').nth(1).unwrap().len(), 6);
    // A query-id was assigned (metrics enabled by default) and is 12 hex chars.
    let qid = v["query_id"].as_str().unwrap();
    assert_eq!(qid.len(), 12);
}

#[test]
fn stats_reports_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("index.json");
    Command::new(bin()).args(["index"]).arg(fixture()).arg("--store").arg(&store).output().unwrap();

    let out = Command::new(bin()).args(["stats", "--store"]).arg(&store).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("chunks"));
    assert!(s.contains("python"));
}

#[test]
fn conformance_is_deterministic() {
    let tmp = tempfile::tempdir().unwrap();
    let out1 = tmp.path().join("c1.json");
    let out2 = tmp.path().join("c2.json");

    for out in [&out1, &out2] {
        let r = Command::new(bin())
            .args(["conformance"])
            .arg(samples())
            .arg("-o")
            .arg(out)
            .output()
            .unwrap();
        assert!(r.status.success(), "conformance failed: {}", String::from_utf8_lossy(&r.stderr));
    }

    let a = std::fs::read(&out1).unwrap();
    let b = std::fs::read(&out2).unwrap();
    assert_eq!(a, b, "conformance.json must be byte-identical across runs");

    // v2 shape (SPEC-V2 §7): spec_version 2.0, per-chunk `kind`, no queries.
    let v: serde_json::Value = serde_json::from_slice(&a).unwrap();
    assert_eq!(v["impl_language"], "rust");
    assert_eq!(v["spec_version"], "2.0");
    assert!(v.get("queries").is_none());
    let chunks = v["chunks"].as_array().unwrap();
    assert_eq!(chunks.len(), 21);
    assert!(chunks.iter().all(|c| !c["kind"].as_str().unwrap().is_empty()));
}

#[test]
fn packs_lists_the_six_registered_packs() {
    let out = Command::new(bin()).args(["packs"]).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for name in ["python", "javascript", "ruby", "rust", "typescript", "c"] {
        assert!(s.contains(name), "expected {name} in packs output: {s}");
    }
}

#[test]
fn packs_validate_passes_for_all_packs() {
    let out = Command::new(bin()).args(["packs", "--validate"]).output().unwrap();
    assert!(
        out.status.success(),
        "packs --validate failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("passed validation"), "got: {s}");
}

#[test]
fn invalid_index_dir_exits_nonzero() {
    let out =
        Command::new(bin()).args(["index", "/definitely/not/a/real/dir/xyz"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}

#[test]
fn search_missing_store_exits_nonzero() {
    let out = Command::new(bin())
        .args(["search", "x", "--store", "/no/such/store.json"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// Write a tiny self-contained Python repo into `dir` for indexing.
fn write_tiny_repo(dir: &std::path::Path) {
    std::fs::write(dir.join("auth.py"), "def hash_password(pw):\n    return pw + 'salt'\n")
        .unwrap();
    std::fs::write(
        dir.join("payments.py"),
        "import auth\n\ndef process_payment(amount):\n    return amount\n",
    )
    .unwrap();
}

#[test]
fn index_without_store_uses_default_path_and_search_resolves_it() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());

    // index with no --store: store defaults to <dir>/.cce/index.json.
    let out = Command::new(bin()).args(["index"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("files indexed"));
    assert!(s.contains("embedder"));
    assert!(tmp.path().join(".cce").join("index.json").exists());

    // search resolving the store via --dir (default_store_path branch).
    let out = Command::new(bin())
        .args(["search", "hash password", "--dir"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    // Human (non-JSON) output includes the ranked line and file.
    assert!(s.contains("auth.py"), "expected auth.py in output, got: {s}");

    // search resolving the store from the current working directory (./.cce).
    let out = Command::new(bin())
        .args(["search", "hash password"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("auth.py"));
}

#[test]
fn search_with_no_matches_prints_no_results() {
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    let store = tmp.path().join("index.json");
    Command::new(bin())
        .args(["index"])
        .arg(tmp.path())
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();

    // A query with no lexical/vector overlap yields an empty result set,
    // which the human formatter renders as "(no results)".
    let out = Command::new(bin()).args(["search", "", "--store"]).arg(&store).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("(no results)"));
}

#[test]
fn index_with_ollama_embedder_falls_back_gracefully() {
    // Requesting the ollama backend must never crash: with no reachable server
    // it health-checks, warns, and falls back to the hash embedder. (If a local
    // Ollama happens to be up, it is used; either way indexing succeeds.)
    let tmp = tempfile::tempdir().unwrap();
    write_tiny_repo(tmp.path());
    let store = tmp.path().join("index.json");
    let out = Command::new(bin())
        .args(["index"])
        .arg(tmp.path())
        .args(["--embedder", "ollama", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(store.exists());
}

#[test]
fn stats_on_empty_index_reports_zero_averages() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty_src");
    std::fs::create_dir(&empty).unwrap();
    let store = tmp.path().join("index.json");
    Command::new(bin()).args(["index"]).arg(&empty).arg("--store").arg(&store).output().unwrap();

    let out = Command::new(bin()).args(["stats", "--store"]).arg(&store).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("avg token/chunk: 0.0"), "got: {s}");
    assert!(s.contains("chunks         : 0"));
}

#[test]
fn stats_missing_store_exits_nonzero() {
    let out =
        Command::new(bin()).args(["stats", "--store", "/no/such/store.json"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}

#[test]
fn bench_runs_on_tiny_local_repo() {
    // Run bench against a tiny temp repo (NOT the flask corpus). cmd_bench writes
    // docs/BENCHMARKS.md relative to the cwd, so run it inside the temp dir.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_tiny_repo(&repo);

    let out =
        Command::new(bin()).args(["bench"]).arg(&repo).current_dir(tmp.path()).output().unwrap();
    assert!(out.status.success(), "bench failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Benchmark complete"));
    // Not a git checkout, so the commit is detected as "unknown".
    assert!(s.contains("unknown"), "expected unknown commit, got: {s}");
    assert!(tmp.path().join("docs").join("BENCHMARKS.md").exists());
}

#[test]
fn bench_with_explicit_commit_and_name() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_tiny_repo(&repo);

    let out = Command::new(bin())
        .args(["bench"])
        .arg(&repo)
        .args(["--commit", "deadbeef", "--name", "tiny/local@test"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "bench failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("deadbeef"));
    assert!(s.contains("tiny/local@test"));
}

#[test]
fn bench_invalid_dir_exits_nonzero() {
    let out =
        Command::new(bin()).args(["bench", "/definitely/not/a/real/dir/xyz"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}

#[test]
fn conformance_invalid_dir_exits_nonzero() {
    let out = Command::new(bin())
        .args(["conformance", "/definitely/not/a/real/dir/xyz"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}

#[test]
fn knowledge_index_ingests_a_feed_in_a_fresh_process() {
    // SPEC-V2.6 §4: `cce knowledge index` reads a cce.knowledge/v1 feed and writes
    // a snapshot-keyed store under <dir>/.cce/knowledge/, never the code cache.
    let tmp = tempfile::tempdir().unwrap();
    let feed = tmp.path().join("curated.jsonl");
    std::fs::write(
        &feed,
        "{\"id\":\"gh:1\",\"title\":\"Policy\",\"body\":\"## Why\\n\\nBecause.\",\"source\":\"github-issues\",\"state\":\"open\"}\n",
    )
    .unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();

    let out = Command::new(bin())
        .args(["knowledge", "index"])
        .arg(&feed)
        .arg("--dir")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "knowledge index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("cce.knowledge/v1"));
    assert!(stdout.contains("records   : 1"));

    // The store landed under .cce/knowledge/ (NOT the code index.json), with a
    // `current` pointer naming the snapshot.
    let kdir = root.join(".cce").join("knowledge");
    assert!(kdir.join("current").exists());
    assert!(!root.join(".cce").join("index.json").exists());
    let ptr = std::fs::read_to_string(kdir.join("current")).unwrap();
    assert!(kdir.join(format!("{}.json", ptr.trim())).exists());
}

#[test]
fn knowledge_index_missing_file_exits_nonzero() {
    let out = Command::new(bin())
        .args(["knowledge", "index", "/definitely/not/a/real/file.jsonl"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("error"));
}
