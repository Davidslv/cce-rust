//! # tests/savings_cli — the `cce savings` + `cce eval` commands end-to-end
//!
//! **Why this file exists:** SPEC-V2.5 §3/§7 add two CLI surfaces: `cce savings`
//! (the seven-bucket ledger + $ estimate) and `cce eval` (the real-world A/B
//! harness over recorded runs). The pure logic is unit-tested in `savings.rs`,
//! `pricing.rs`, and `eval.rs`; only driving the real binary proves the command
//! wiring, the honesty label on the output, and the JSON shapes.
//!
//! **Responsibilities:**
//! - Prove `cce savings` sums the `retrieval` bucket from a real log and labels it.
//! - Prove `cce eval` grades the canned runs and reports the paired, cost-primary
//!   delta both as text and JSON.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn manifest_path(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

#[test]
fn savings_reports_retrieval_bucket_and_labels_it() {
    let log = manifest_path("test/fixture/base/metrics_sample.jsonl");
    let out = Command::new(bin()).args(["savings", "--metrics"]).arg(&log).output().unwrap();
    assert!(out.status.success(), "savings failed: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    // The mandatory honesty label is present.
    assert!(text.contains("vs full-file baseline — not your real end-to-end agent cost"));
    // The retrieval bucket sums the sample log (53000 saved / 70000 baseline).
    assert!(text.contains("retrieval"));
    assert!(text.contains("53000"));
    assert!(text.contains("70000"));
    // $ estimate at the default $3/Mtok input rate: 53000 -> $0.16.
    assert!(text.contains("$0.16"), "expected $ estimate, got:\n{text}");
}

#[test]
fn savings_json_matches_api_shape() {
    let log = manifest_path("test/fixture/base/metrics_sample.jsonl");
    let out =
        Command::new(bin()).args(["savings", "--json", "--metrics"]).arg(&log).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ledger = &v["savings_by_layer"];
    assert_eq!(ledger["retrieval"]["saved_tokens"], 53000);
    assert_eq!(ledger["retrieval"]["baseline_tokens"], 70000);
    assert_eq!(ledger["total"]["saved_tokens"], 53000);
    // All seven buckets are present (even the zero ones), ready for later stages.
    for bucket in [
        "retrieval",
        "chunk_compression",
        "grammar",
        "output",
        "memory",
        "turn_summarization",
        "progressive_disclosure",
    ] {
        assert!(ledger.get(bucket).is_some(), "missing bucket {bucket}");
    }
    assert_eq!(v["pricing_id"], "cce.pricing/builtin-v1");
    assert_eq!(v["estimated_dollars_saved"], "0.16");
}

#[test]
fn savings_on_empty_log_is_a_clean_zero_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("metrics.jsonl"); // does not exist
    let out =
        Command::new(bin()).args(["savings", "--json", "--metrics"]).arg(&log).output().unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["savings_by_layer"]["total"]["saved_tokens"], 0);
    assert_eq!(v["estimated_dollars_saved"], "0.00");
}

#[test]
fn eval_grades_canned_runs_and_reports_paired_delta() {
    let questions = manifest_path("eval/questions.jsonl");
    let runs = manifest_path("eval/runs.example.jsonl");
    let out = Command::new(bin())
        .args(["eval"])
        .arg(&runs)
        .arg("--questions")
        .arg(&questions)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success(), "eval failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["questions"], 6);
    // q4 punts under `off`, so it is excluded from the paired set: 5 pairs.
    assert_eq!(v["paired_correct"], 5);
    assert_eq!(v["off"]["punts"], 1);
    assert_eq!(v["on"]["correct"], 6);
    // On is cheaper across the paired set -> a positive saving ratio.
    let ratio = v["cost_saved_ratio"].as_f64().unwrap();
    assert!(ratio > 0.0 && ratio < 1.0, "ratio was {ratio}");
    assert_eq!(v["note"], "real end-to-end A/B (cost-primary, correctness-gated, paired)");
}

#[test]
fn eval_text_output_names_the_method() {
    let questions = manifest_path("eval/questions.jsonl");
    let runs = manifest_path("eval/runs.example.jsonl");
    let out = Command::new(bin())
        .args(["eval"])
        .arg(&runs)
        .arg("--questions")
        .arg(&questions)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("cost-primary, correctness-gated, paired"));
    assert!(text.contains("paired-correct (both arms): 5"));
}
