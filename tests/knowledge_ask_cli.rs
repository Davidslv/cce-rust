//! # tests/knowledge_ask_cli — end-to-end tests for `cce knowledge ask` (Epic U5.4)
//!
//! **Why this file exists:** The knowledge-ask suite is a standing regression check
//! for the knowledge host — so the check itself must be gated. The committed suite
//! must prove every query against the committed fixture corpus from a fresh process,
//! and the `--json` report must be byte-identical to the checked-in golden (the
//! conformance pattern: a retrieval-behavior change, a corpus change, or a report-
//! grammar change fails here first). This is the "prove the suite + runner" evidence
//! U5.4 ships before the production-corpus tail (which parks).
//!
//! **What it is / does:** Drives the built `cce` binary against the shipped fixture
//! corpus + suite, pins the report bytes, and checks that a suite whose expected
//! record cannot surface fails cleanly with a non-zero exit.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for `cce knowledge ask`.
//! - It does NOT test metric math (src/knowledge/ask.rs unit tests own that).

use std::path::PathBuf;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Run `cce knowledge ask` from the repo root (the suite's `corpus` header is
/// resolved relative to the suite file, and the golden's corpus bytes are the
/// header string — both path-independent, so the pin is stable on any machine).
fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(["knowledge", "ask"])
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("spawn cce knowledge ask")
}

#[test]
fn golden_suite_proves_every_query() {
    let out = run(&["eval/knowledge/ask.jsonl"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("CCE knowledge-ask"), "{s}");
    // Seven proven queries is the U5.4 "≥5 proven queries" evidence.
    assert!(s.contains("proven   : 7/7"), "expected 7/7 proven in:\n{s}");
    assert!(!s.contains("  NO"), "a query regressed (NO in the proven column):\n{s}");
}

#[test]
fn json_report_is_byte_identical_to_golden() {
    let out = run(&["eval/knowledge/ask.jsonl", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let got = String::from_utf8_lossy(&out.stdout);
    let golden =
        std::fs::read_to_string(repo_root().join("test/fixture/knowledge/ask.golden.json"))
            .expect("read ask.golden.json");
    assert_eq!(
        got, golden,
        "knowledge-ask report drifted from the golden. If intended, regenerate:\n  \
         cargo run -- knowledge ask eval/knowledge/ask.jsonl --json > test/fixture/knowledge/ask.golden.json"
    );
}

#[test]
fn a_query_that_cannot_be_answered_fails_nonzero() {
    // A suite over the same corpus but expecting a record that does not exist must
    // fail (recall < 1.0 → not proven → non-zero exit): the regression gate bites.
    let dir = std::env::temp_dir().join("cce-ask-cli-miss");
    std::fs::create_dir_all(&dir).unwrap();
    let suite = dir.join("miss.jsonl");
    // The corpus header points back at the shipped fixture feed (relative to the
    // suite file), so we only need to write the suite.
    let feed = repo_root().join("eval/knowledge/corpus.knowledge.jsonl");
    std::fs::write(
        &suite,
        format!(
            "{{\"schema\":\"cce.knowledge.ask/v1\",\"corpus\":\"{}\"}}\n\
             {{\"id\":\"impossible\",\"query\":\"quarterly revenue in the finance ledger\",\"expect\":[\"gh:acme/shop#999\"],\"k\":5}}\n",
            feed.display()
        ),
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["knowledge", "ask"])
        .arg(&suite)
        .output()
        .expect("spawn cce knowledge ask");
    assert!(
        !out.status.success(),
        "an unanswerable suite must fail; stdout:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("regression"), "expected a regression message, got:\n{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
