//! # tests/usage_cli — the `cce usage` command end-to-end (SPEC-USAGE-VISIBILITY §2)
//!
//! **Why this file exists:** v2.8's acceptance bar requires the byte-pinned
//! `cce usage` surfaces to be proven over the REAL binary — the human block, the
//! `cce.usage/v1` JSON, the `--since`/`--source` flags, the malformed-`--since`
//! error, and the workspace federation (member logs + the workspace-root log,
//! the issue-#28 rule) — AND that the numbers equal the dashboard's for the same
//! log. The pure renders are unit-tested in `usage.rs`; only driving the binary
//! proves the command wiring and the log resolution.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for `cce usage`.
//! - Prove dashboard parity by running BOTH real paths (the dashboard's
//!   aggregate body and `cce usage --json`) over one fixture log.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// The committed, pinned usage fixture log: one index event, one human (cli)
/// search, and two agent (mcp) searches — with sources and latencies.
fn fixture_log() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixture/usage/metrics_usage.jsonl")
}

#[test]
fn usage_human_all_time_is_byte_pinned() {
    // "all time" carries no wall-clock text, so the whole block is byte-pinnable
    // at the process level over the committed fixture log.
    let out = Command::new(bin()).args(["usage", "--metrics"]).arg(fixture_log()).output().unwrap();
    assert!(out.status.success(), "usage failed: {}", String::from_utf8_lossy(&out.stderr));
    let got = String::from_utf8(out.stdout).unwrap();
    let want = "CCE usage — all time\n\
                \x20 agent (mcp) : 2 searches · saved ~16,500 tok (88%) · quality 0.79 · 58 ms avg\n\
                \x20 human (cli) : 1 searches · saved ~ 2,100 tok (81%) · quality 0.74 · 12 ms avg\n\
                \x20 recent (newest first)\n\
                \x20   mcp  09:58  \"how does the payment flow create a new case\"  5 hits  ~8.6k saved\n\
                \x20   mcp  09:52  \"where is the retry idempotency boundary\"      5 hits  ~7.9k saved\n\
                \x20   cli  09:00  \"rrf fusion constant\"                          3 hits  ~2.1k saved\n";
    assert_eq!(got, want);
}

#[test]
fn usage_since_iso_filters_and_source_narrows_the_display() {
    // An ISO --since is deterministic at the process level (the cutoff is the
    // given instant, not derived from now).
    let out = Command::new(bin())
        .args(["usage", "--since", "2026-07-05", "--source", "mcp", "--metrics"])
        .arg(fixture_log())
        .output()
        .unwrap();
    assert!(out.status.success(), "usage failed: {}", String::from_utf8_lossy(&out.stderr));
    let got = String::from_utf8(out.stdout).unwrap();
    let want = "CCE usage — since 2026-07-05T00:00:00Z\n\
                \x20 agent (mcp) : 2 searches · saved ~16,500 tok (88%) · quality 0.79 · 58 ms avg\n\
                \x20 recent (newest first)\n\
                \x20   mcp  09:58  \"how does the payment flow create a new case\"  5 hits  ~8.6k saved\n\
                \x20   mcp  09:52  \"where is the retry idempotency boundary\"      5 hits  ~7.9k saved\n";
    assert_eq!(got, want);
}

#[test]
fn usage_empty_window_is_friendly_and_exits_zero() {
    let out = Command::new(bin())
        .args(["usage", "--since", "2027-01-01", "--metrics"])
        .arg(fixture_log())
        .output()
        .unwrap();
    assert!(out.status.success(), "an empty window must exit 0");
    let got = String::from_utf8(out.stdout).unwrap();
    assert_eq!(got, "CCE usage — since 2027-01-01T00:00:00Z\n  no searches in this window\n");
}

#[test]
fn usage_malformed_since_is_a_clear_error() {
    let out = Command::new(bin())
        .args(["usage", "--since", "yesterday", "--metrics"])
        .arg(fixture_log())
        .output()
        .unwrap();
    assert!(!out.status.success(), "malformed --since must exit non-zero");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("invalid --since"), "got: {err}");
    assert!(err.contains("90m, 24h, 7d, 4w"), "must list the accepted forms: {err}");
}

#[test]
fn usage_unknown_source_is_a_clear_error() {
    let out = Command::new(bin())
        .args(["usage", "--source", "agents", "--metrics"])
        .arg(fixture_log())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mcp, cli, or all"));
}

#[test]
fn usage_json_is_the_versioned_projection_and_matches_the_dashboard() {
    // Dashboard parity, both REAL paths over one log: the dashboard's
    // `/api/metrics` body (the same `metrics_body` the server serves) vs the
    // `cce usage --json` projection from the spawned binary.
    let out = Command::new(bin())
        .args(["usage", "--json", "--metrics"])
        .arg(fixture_log())
        .output()
        .unwrap();
    assert!(out.status.success(), "usage --json failed: {}", String::from_utf8_lossy(&out.stderr));
    let usage: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(usage["schema"], "cce.usage/v1");
    assert_eq!(usage["source_filter"], "all");
    assert_eq!(usage["window"]["since"], Value::Null);

    let dash: Value =
        serde_json::from_str(&cce::dashboard::metrics_body(&fixture_log(), 3.00)).unwrap();
    // Totals + the whole by_source split agree field-for-field.
    for k in ["searches", "tokens_saved", "mean_savings_ratio", "mean_top_score"] {
        assert_eq!(usage["totals"][k], dash["totals"][k], "totals.{k} diverged");
    }
    assert_eq!(usage["by_source"], dash["by_source"], "by_source diverged");
    // The recent list is the aggregate's recent_searches, re-shaped.
    let recent = usage["recent"].as_array().unwrap();
    let dash_recent = dash["recent_searches"].as_array().unwrap();
    assert_eq!(recent.len(), dash_recent.len());
    for (u, d) in recent.iter().zip(dash_recent) {
        for k in ["ts", "source", "query", "result_count", "tokens_saved"] {
            assert_eq!(u[k], d[k], "recent.{k} diverged");
        }
    }
    // Pinned spot values off the fixture.
    assert_eq!(usage["by_source"]["mcp"]["searches"], 2);
    assert_eq!(usage["by_source"]["mcp"]["tokens_saved"], 16500);
    assert_eq!(usage["by_source"]["mcp"]["mean_latency_ms"], 58.0);
    assert_eq!(usage["by_source"]["cli"]["searches"], 1);
    // Single-repo: no by_package key.
    assert!(usage.get("by_package").is_none());
}

// --- workspace federation (the #28 workspace-root log rule) ---

/// One pinned search-event line.
fn search_line(id: &str, ts: &str, query: &str, tokens: u64, source: &str) -> String {
    format!(
        "{{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"{ts}\",\"id\":\"{id}\",\"query\":\"{query}\",\"result_count\":2,\"tokens_saved\":{tokens},\"savings_ratio\":0.5,\"top_score\":0.9,\"empty\":false,\"low_confidence\":false,\"latency_ms\":10.0,\"source\":\"{source}\"}}\n"
    )
}

/// Copy the committed workspace fixture into a temp dir, detect its members,
/// and plant member metrics logs + a workspace-root log with agent searches.
fn workspace_with_logs() -> tempfile::TempDir {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixture/workspace");
    let tmp = tempfile::tempdir().unwrap();
    for entry in walkdir::WalkDir::new(&src).into_iter().flatten() {
        let rel = entry.path().strip_prefix(&src).unwrap();
        let target = tmp.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
    let out = Command::new(bin()).args(["workspace", "init"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "workspace init: {}", String::from_utf8_lossy(&out.stderr));

    // One human search in each of two members.
    let app = tmp.path().join("app").join(".cce");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("metrics.jsonl"),
        search_line("app000000001", "2026-07-04T10:00:00Z", "boot sequence", 1000, "cli"),
    )
    .unwrap();
    let billing = tmp.path().join("engines").join("billing").join(".cce");
    std::fs::create_dir_all(&billing).unwrap();
    std::fs::write(
        billing.join("metrics.jsonl"),
        search_line("bil000000001", "2026-07-04T11:00:00Z", "charge amount", 3000, "cli"),
    )
    .unwrap();
    // Two agent searches at the workspace root (where `cce mcp --workspace` writes).
    let root = tmp.path().join(".cce");
    std::fs::write(
        root.join("metrics.jsonl"),
        format!(
            "{}{}",
            search_line("root00000001", "2026-07-04T12:00:00Z", "cross member flow", 500, "mcp"),
            search_line("root00000002", "2026-07-04T13:00:00Z", "invoice totals", 700, "mcp"),
        ),
    )
    .unwrap();
    tmp
}

#[test]
fn usage_workspace_folds_the_root_log_and_matches_the_dashboard() {
    let tmp = workspace_with_logs();

    let out = Command::new(bin())
        .args(["usage", "--workspace", "--json"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "usage --workspace: {}", String::from_utf8_lossy(&out.stderr));
    let usage: Value = serde_json::from_slice(&out.stdout).unwrap();

    // The roll-up spans member logs AND the workspace-root log (the #28 rule).
    assert_eq!(usage["totals"]["searches"], 4);
    assert_eq!(usage["totals"]["tokens_saved"], 5200);
    assert_eq!(usage["by_source"]["cli"]["searches"], 2);
    assert_eq!(usage["by_source"]["mcp"]["searches"], 2);
    assert_eq!(usage["by_source"]["mcp"]["tokens_saved"], 1200);
    // by_package stays members-only: federated agent searches are NOT attributed.
    let by = usage["by_package"].as_array().unwrap();
    let names: Vec<&str> = by.iter().map(|p| p["package"].as_str().unwrap()).collect();
    let app = by.iter().find(|p| p["package"] == "app").unwrap();
    assert_eq!(app["searches"], 1);
    assert_eq!(app["tokens_saved"], 1000);
    let member_total: u64 = by.iter().map(|p| p["searches"].as_u64().unwrap()).sum();
    assert_eq!(member_total, 2, "root-log searches must not appear in by_package: {names:?}");

    // Dashboard parity over the SAME workspace: the federated `/api/metrics`
    // body (what `cce dashboard --workspace` serves) agrees on every shared field.
    let manifest = cce::workspace::Manifest::load(tmp.path()).unwrap();
    let members = cce::federation::member_metrics(tmp.path(), &manifest);
    let root_log = cce::store::default_metrics_path(tmp.path());
    let dash: Value = serde_json::from_str(&cce::dashboard::workspace_metrics_body(
        &members,
        Some(&root_log),
        3.00,
    ))
    .unwrap();
    for k in ["searches", "tokens_saved", "mean_savings_ratio", "mean_top_score"] {
        assert_eq!(usage["totals"][k], dash["totals"][k], "totals.{k} diverged");
    }
    assert_eq!(usage["by_source"], dash["by_source"]);
    assert_eq!(usage["by_package"], dash["by_package"]);
}

#[test]
fn usage_workspace_human_shows_the_by_package_table() {
    let tmp = workspace_with_logs();
    let out = Command::new(bin()).args(["usage", "--workspace"]).arg(tmp.path()).output().unwrap();
    assert!(out.status.success(), "usage --workspace: {}", String::from_utf8_lossy(&out.stderr));
    let got = String::from_utf8(out.stdout).unwrap();
    assert!(got.contains("  by package\n"), "missing by-package table: {got}");
    assert!(got.contains("app"), "{got}");
    assert!(got.contains("billing"), "{got}");
    // The split counts the root-log agent searches even though by_package doesn't.
    assert!(got.contains("agent (mcp) : 2 searches"), "{got}");
    assert!(got.contains("human (cli) : 2 searches"), "{got}");
}
