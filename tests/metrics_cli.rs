//! # tests/metrics_cli — the metrics/feedback CLI flow end-to-end
//!
//! **Why this file exists:** DASHBOARD-SPEC §5/§8 require that `cce search`
//! appends a search event and prints a query-id, that `--no-metrics` suppresses
//! the write, and that `cce feedback` records a feedback event that resolves into
//! the aggregate's recent-searches view. Only driving the real binary proves the
//! command wiring, and reading the log back proves persistence.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for search-metrics + feedback.
//! - It reuses the library aggregator to assert the log resolves correctly.

use cce::aggregator::aggregate;
use cce::metrics::{parse_log, Event};
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Index the tiny repo into `store`, returning the sibling metrics-log path.
fn index_repo(repo: &Path, store: &Path) -> PathBuf {
    let out =
        Command::new(bin()).args(["index"]).arg(repo).arg("--store").arg(store).output().unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    store.parent().unwrap().join("metrics.jsonl")
}

#[test]
fn search_appends_event_prints_query_id_and_feedback_resolves() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_tiny_repo(&repo);
    let store = tmp.path().join("index.json");
    let metrics = index_repo(&repo, &store);

    // The index event was recorded beside the store.
    assert!(metrics.exists(), "index should create the metrics log");

    // Search with --json: the object carries a top-level query_id.
    let out = Command::new(bin())
        .args(["search", "hash password", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let query_id = v["query_id"].as_str().unwrap().to_string();
    assert_eq!(query_id.len(), 12);
    assert!(!v["results"].as_array().unwrap().is_empty());

    // Human search prints the query-id line.
    let out = Command::new(bin())
        .args(["search", "hash password", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("query-id:"), "expected query-id line, got: {stdout}");

    // Give feedback on the first search's id.
    let out = Command::new(bin())
        .args(["feedback", &query_id, "--helpful", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "feedback failed: {}", String::from_utf8_lossy(&out.stderr));

    // Read the log back and confirm the feedback resolves into recent_searches.
    let text = std::fs::read_to_string(&metrics).unwrap();
    let log = parse_log(&text);
    let searches = log.events.iter().filter(|e| matches!(e, Event::Search(_))).count();
    let feedback = log.events.iter().filter(|e| matches!(e, Event::Feedback(_))).count();
    assert_eq!(searches, 2, "two searches were run");
    assert_eq!(feedback, 1, "one feedback was given");

    let agg = aggregate(&log.events, 0, 3.00);
    let entry = agg.recent_searches.iter().find(|r| r.id == query_id).unwrap();
    assert_eq!(entry.feedback, "helpful");
}

#[test]
fn no_metrics_suppresses_the_search_event() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_tiny_repo(&repo);
    let store = tmp.path().join("index.json");
    let metrics = index_repo(&repo, &store);

    let before = std::fs::read_to_string(&metrics).unwrap();
    let before_searches =
        parse_log(&before).events.iter().filter(|e| matches!(e, Event::Search(_))).count();

    // Search with --no-metrics writes no event and prints no query-id line.
    let out = Command::new(bin())
        .args(["search", "hash password", "--store"])
        .arg(&store)
        .arg("--no-metrics")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("query-id:"));

    let after = std::fs::read_to_string(&metrics).unwrap();
    let after_searches =
        parse_log(&after).events.iter().filter(|e| matches!(e, Event::Search(_))).count();
    assert_eq!(before_searches, after_searches, "--no-metrics must not append a search event");
}

#[test]
fn index_no_metrics_writes_no_log() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    write_tiny_repo(&repo);
    let store = tmp.path().join("index.json");

    let out = Command::new(bin())
        .args(["index"])
        .arg(&repo)
        .arg("--store")
        .arg(&store)
        .arg("--no-metrics")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!store.parent().unwrap().join("metrics.jsonl").exists());
}

#[test]
fn feedback_requires_exactly_one_verdict() {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("index.json");

    // Neither verdict.
    let out = Command::new(bin())
        .args(["feedback", "aaaaaaaaaaaa", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exactly one"));

    // Both verdicts.
    let out = Command::new(bin())
        .args(["feedback", "aaaaaaaaaaaa", "--helpful", "--not-helpful", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn feedback_for_unknown_id_warns_but_records() {
    let tmp = tempfile::tempdir().unwrap();
    let metrics = tmp.path().join("metrics.jsonl");

    let out = Command::new(bin())
        .args(["feedback", "deadbeefdead", "--not-helpful", "--metrics"])
        .arg(&metrics)
        .output()
        .unwrap();
    assert!(out.status.success(), "feedback should still succeed");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no search event"));
    // The event was recorded despite the unknown target.
    let log = parse_log(&std::fs::read_to_string(&metrics).unwrap());
    assert_eq!(log.events.iter().filter(|e| matches!(e, Event::Feedback(_))).count(), 1);
}
