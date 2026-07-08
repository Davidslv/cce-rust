//! # tests/workspace — end-to-end workspace-mode CLI tests (SPEC-V2.2)
//!
//! **Why this file exists:** Workspace mode spans several fresh-process commands
//! (`workspace init`/`list`, `index --workspace`, `search --workspace`,
//! `stats --workspace`). Only spawning the real `cce` binary proves the manifest
//! is written, members are indexed into their own stores, the cross-member graph
//! is built, and federated search behaves as §6/§8 specify.
//!
//! **What it is / does:** Copies the shipped `test/fixture/workspace` ecosystem to
//! a temp dir and drives the binary against it, asserting §8's detection, edges,
//! per-member store isolation (byte-identical to a standalone index), federation,
//! `--package` scoping (+ unknown-name error), and the graph hop — plus a
//! re-assert that single-repo `conformance.json` is byte-identical.
//!
//! **Responsibilities:**
//! - Own the process-level workspace acceptance tests.
//! - It does NOT touch library internals.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

fn fixture() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"))
}

/// Recursively copy the workspace fixture into a fresh temp dir.
fn copy_fixture() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    for entry in walkdir::WalkDir::new(fixture()).into_iter().flatten() {
        let rel = entry.path().strip_prefix(fixture()).unwrap();
        let target = tmp.path().join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
    tmp
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().unwrap()
}

#[test]
fn init_writes_the_expected_manifest() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    let out = run(&["workspace", "init", root]);
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));

    let yaml = std::fs::read_to_string(tmp.path().join(".cce/workspace.yml")).unwrap();
    // Deterministic §2 shape, members sorted by path.
    assert!(yaml.starts_with("version: 1\n"));
    assert!(yaml.contains("  - name: app\n    path: app\n    type: rails-app\n    package: app\n"));
    assert!(yaml.contains(
        "  - name: billing\n    path: engines/billing\n    type: ruby-engine\n    package: billing\n"
    ));
    assert!(yaml.contains("  - name: web\n    path: web\n    type: typescript\n    package: web\n"));

    // Refuses to overwrite without --force.
    let again = run(&["workspace", "init", root]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    // --force overwrites.
    assert!(run(&["workspace", "init", root, "--force"]).status.success());
}

#[test]
fn list_shows_members_and_the_single_edge() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    let out = run(&["workspace", "list", root]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("app -> billing  (via gemfile)"), "got: {s}");
    for name in ["app", "billing", "web"] {
        assert!(s.contains(name));
    }
}

#[test]
fn index_workspace_builds_stores_and_the_graph() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    let out = run(&["index", "--workspace", root]);
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    // Each member has its own store.
    for member in ["app", "engines/billing", "web"] {
        assert!(tmp.path().join(member).join(".cce/index.json").exists(), "{member} store missing");
    }
    // The cross-member graph is exactly app -> billing (gemfile).
    let graph = std::fs::read_to_string(tmp.path().join(".cce/workspace-graph.json")).unwrap();
    assert_eq!(
        graph,
        "{\"members\":[\"app\",\"billing\",\"web\"],\"edges\":[{\"from\":\"app\",\"to\":\"billing\",\"via\":\"gemfile\"}]}"
    );
}

#[test]
fn member_store_is_byte_identical_to_standalone_index() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);

    // Standalone index of the billing member into a separate store, and compare
    // bytes with the store the federated index wrote.
    let member_dir = tmp.path().join("engines/billing");
    let standalone = tmp.path().join("billing-standalone.json");
    let out = Command::new(bin())
        .args(["index"])
        .arg(&member_dir)
        .arg("--store")
        .arg(&standalone)
        .arg("--no-metrics")
        .output()
        .unwrap();
    assert!(out.status.success());

    let federated = std::fs::read(member_dir.join(".cce/index.json")).unwrap();
    let alone = std::fs::read(&standalone).unwrap();
    assert_eq!(federated, alone, "member store must be byte-identical to a standalone index");
}

#[test]
fn federated_search_scopes_labels_and_equals_union() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);

    let out = run(&[
        "search",
        "billing charge amount",
        root,
        "--workspace",
        "--package",
        "app,billing",
        "--no-graph",
        "--json",
    ]);
    assert!(out.status.success(), "search failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let results = v["results"].as_array().unwrap();
    assert!(!results.is_empty());
    // Every result is tagged with a package and a member-relative file_path, and a
    // 6-decimal score string (SPEC-V2.2 §6).
    let packages: std::collections::BTreeSet<&str> =
        results.iter().map(|r| r["package"].as_str().unwrap()).collect();
    assert!(packages.contains("app"));
    assert!(packages.contains("billing"));
    for r in results {
        assert!(!r["file_path"].as_str().unwrap().starts_with("billing/"));
        assert_eq!(r["score"].as_str().unwrap().split('.').nth(1).unwrap().len(), 6);
    }
    assert_eq!(v["query_id"].as_str().unwrap().len(), 12);
}

#[test]
fn unknown_package_scope_exits_nonzero() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);
    let out = run(&["search", "q", root, "--workspace", "--package", "ghost"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown member/package"));
}

#[test]
fn empty_package_scope_exits_nonzero() {
    // Issue #45: `--package ""` (e.g. an unset shell variable) must error loudly,
    // never federate over zero members and silently print no results.
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);
    for empty in ["", "  ", ","] {
        let out = run(&["search", "q", root, "--workspace", "--package", empty]);
        assert!(!out.status.success(), "--package {empty:?} must exit non-zero");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(
                "--package requires at least one member or package name \
                 (e.g. --package app,billing)"
            ),
            "--package {empty:?} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn graph_hop_pulls_billing_into_an_app_result() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);

    let ids = |out: &std::process::Output| -> Vec<String> {
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["package"].as_str().unwrap().to_string())
            .collect()
    };

    let no_graph = run(&[
        "search",
        "application boot",
        root,
        "--workspace",
        "--top-k",
        "3",
        "--no-graph",
        "--json",
    ]);
    assert!(!ids(&no_graph).contains(&"billing".to_string()));

    let with_graph =
        run(&["search", "application boot", root, "--workspace", "--top-k", "3", "--json"]);
    assert!(ids(&with_graph).contains(&"billing".to_string()), "graph hop must reach billing");
}

#[test]
fn stats_workspace_reports_per_member_and_edges() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    run(&["workspace", "init", root]);
    run(&["index", "--workspace", root]);
    let out = run(&["stats", root, "--workspace"]);
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("package app"));
    assert!(s.contains("package billing"));
    assert!(s.contains("totals:"));
    assert!(s.contains("app -> billing  (via gemfile)"));
}

#[test]
fn search_workspace_without_manifest_errors_clearly() {
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    // No `workspace init` — the manifest is absent.
    let out = run(&["search", "q", root, "--workspace"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("workspace init"));
}

#[test]
fn search_workspace_with_malformed_manifest_errors_clearly() {
    // Issue #37: a syntactically broken workspace.yml (unclosed flow sequence)
    // must surface src/workspace.rs's friendly `invalid workspace.yml: …` error
    // from both federated read commands — non-zero exit, no panic.
    let tmp = copy_fixture();
    let root = tmp.path().to_str().unwrap();
    std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
    std::fs::write(tmp.path().join(".cce/workspace.yml"), "version: 1\nmembers: [\n").unwrap();

    for args in
        [["search", "q", root, "--workspace"].as_slice(), ["stats", root, "--workspace"].as_slice()]
    {
        let out = run(args);
        assert!(!out.status.success(), "{args:?} with a broken manifest must exit non-zero");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("error: invalid workspace.yml:"),
            "{args:?} must surface the friendly manifest error, got: {stderr}"
        );
        assert!(!stderr.contains("panicked"), "{args:?} must not panic, got: {stderr}");
    }
}

/// Re-assert the single-repo conformance output is byte-identical to the checked-in
/// `conformance.json` (workspace mode must not perturb it — SPEC-V2.2 §10).
#[test]
fn single_repo_conformance_is_unchanged() {
    let samples = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"));
    let committed = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/conformance.json"));
    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("conf.json");
    let r = Command::new(bin())
        .args(["conformance"])
        .arg(&samples)
        .arg("-o")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(r.status.success());
    let generated = std::fs::read(&out_path).unwrap();
    let expected = std::fs::read(committed).unwrap();
    assert_eq!(generated, expected, "single-repo conformance.json must be byte-identical");
}
