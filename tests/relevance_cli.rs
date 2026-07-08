//! # tests/relevance_cli — end-to-end tests for `cce relevance` (issue #63)
//!
//! **Why this file exists:** The relevance harness gates ranking changes, so the
//! harness itself must be gated: both starter fixture sets must run green from a
//! fresh process, and the hash-path JSON report must be byte-identical to the
//! checked-in golden (the conformance pattern — a ranking-behavior change, an
//! aggregation change, or a report-grammar change must fail here first).
//!
//! **What it is / does:** Drives the built `cce` binary as a subprocess against
//! the shipped starter fixture sets, pins the `--json` report bytes, exercises
//! the comparison mode, and checks that invalid input fails cleanly.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for `cce relevance`.
//! - It does NOT test metric math (src/relevance.rs unit tests own that).

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run `cce relevance` from the repo root (fixture paths are repo-relative, so
/// the golden's corpus-path bytes are stable).
fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .arg("relevance")
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("spawn cce relevance")
}

#[test]
fn code_starter_set_runs_green() {
    let out = run(&["eval/relevance/code.jsonl"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("CCE relevance"), "{s}");
    // All three backends appear with all four metrics.
    for b in ["bm25", "vector", "hybrid"] {
        assert!(s.contains(b), "missing backend {b} in:\n{s}");
    }
    for m in ["P@k", "recall", "MRR", "F1"] {
        assert!(s.contains(m), "missing metric {m} in:\n{s}");
    }
    assert!(s.contains("queries : 6"), "{s}");
}

#[test]
fn docs_starter_set_runs_green() {
    let out = run(&["eval/relevance/docs.jsonl"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("queries : 4"), "{s}");
    assert!(s.contains("docs-corpus"), "{s}");
}

#[test]
fn hash_path_json_report_is_byte_pinned() {
    // The conformance-style golden: the full three-backend JSON report over the
    // code starter set, hash embedder, byte-identical to the checked-in file.
    // Regenerate (from the repo root) after an INTENDED change with:
    //   cargo run -- relevance eval/relevance/code.jsonl --json \
    //     > test/fixture/relevance/code.golden.json
    let out = run(&["eval/relevance/code.jsonl", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let golden = std::fs::read(repo_root().join("test/fixture/relevance/code.golden.json"))
        .expect("read golden");
    assert_eq!(
        out.stdout,
        golden,
        "cce relevance --json drifted from the golden.\n--- got ---\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn json_report_is_identical_across_runs() {
    let a = run(&["eval/relevance/docs.jsonl", "--json"]);
    let b = run(&["eval/relevance/docs.jsonl", "--json"]);
    assert!(a.status.success() && b.status.success());
    assert_eq!(a.stdout, b.stdout, "relevance --json must be deterministic");
}

#[test]
fn compare_mode_prints_per_query_deltas() {
    let out = run(&["eval/relevance/code.jsonl", "--compare", "bm25,hybrid"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("per-query deltas (bm25 → hybrid"), "{s}");
    // One delta row per fixture case, plus the mean row; signed 6-decimal cells.
    for id in [
        "code-read-config",
        "code-parse-json",
        "code-build-index",
        "code-loader-class",
        "code-sum-node",
        "code-interface-kind",
    ] {
        assert!(s.contains(id), "missing per-query row {id} in:\n{s}");
    }
    assert!(s.contains("mean"), "{s}");
    assert!(s.contains("+0.000000") || s.contains("-0."), "{s}");
}

#[test]
fn backend_flag_scopes_the_report() {
    let out = run(&["eval/relevance/code.jsonl", "--backend", "bm25", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let backends = v["backends"].as_array().unwrap();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0]["backend"], "bm25");
    assert_eq!(v["schema"], "cce.relevance.report/v1");
}

#[test]
fn dir_flag_overrides_the_header_corpus() {
    // Point the code fixture set at the docs corpus: everything misses, but the
    // run stays green (a bad score is a finding, not an error).
    let out = run(&["eval/relevance/code.jsonl", "--dir", "eval/relevance/docs-corpus"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("docs-corpus"), "{s}");
}

#[test]
fn invalid_inputs_fail_cleanly() {
    // Missing fixture file.
    let out = run(&["eval/relevance/nope.jsonl"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not read fixtures"));

    // Unknown backend.
    let out = run(&["eval/relevance/code.jsonl", "--backend", "bm42"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown backend"));

    // --compare needs exactly two backends.
    let out = run(&["eval/relevance/code.jsonl", "--compare", "bm25"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exactly two"));

    // Malformed fixture line: error names the line number.
    let tmp = tempfile::tempdir().unwrap();
    let bad = tmp.path().join("bad.jsonl");
    std::fs::write(&bad, "{\"query\":\"a\",\"expected\":[]}\n").unwrap();
    let out = Command::new(bin())
        .arg("relevance")
        .arg(&bad)
        .args(["--dir", "test/fixture/base"])
        .current_dir(repo_root())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 1"));

    // No corpus anywhere: no header corpus, no --dir.
    let nocorpus = tmp.path().join("nocorpus.jsonl");
    std::fs::write(&nocorpus, "{\"query\":\"a\",\"expected\":[\"f.py\"]}\n").unwrap();
    let out = Command::new(bin())
        .arg("relevance")
        .arg(&nocorpus)
        .current_dir(repo_root())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no corpus"));
}
