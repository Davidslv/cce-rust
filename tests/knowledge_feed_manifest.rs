//! # tests/knowledge_feed_manifest — the U6.2 feed-manifest gate, end to end (G16)
//!
//! **Why this file exists:** U6.2 turns a silent failure into a loud one — a truncated
//! or misdirected `cce.knowledge/v1` feed must NOT index. The unit tests in
//! `knowledge::manifest` pin the verification math; this suite proves the whole path
//! through the built `cce` binary: `--manifest` on a matching feed indexes, on a
//! truncated feed exits non-zero and writes no store, and on a misdirected feed the
//! same — and that omitting `--manifest` is byte-for-byte the old behaviour (additive).
//!
//! **What it is / does:** Writes a feed + a `cce.feed-manifest/v1` sidecar to a temp
//! dir, drives `cce knowledge index` against them, and asserts exit status + whether
//! `.cce/knowledge/current` was created.
//!
//! **Responsibilities:**
//! - Own the process-level fail-loud acceptance for the feed-manifest check.
//! - It does NOT re-test verification math (src/knowledge/manifest.rs owns that).

use cce::knowledge::feed_sha256;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

const FEED: &str =
    "{\"id\":\"gh:o/r#1\",\"title\":\"One\",\"body\":\"alpha\",\"source\":\"github-issues\"}\n\
     {\"id\":\"gh:o/r#2\",\"title\":\"Two\",\"body\":\"beta\",\"source\":\"github-issues\"}\n";

/// A project root with `knowledge.enabled = true` so `cce knowledge index` runs.
fn make_root(tmp: &Path) -> PathBuf {
    let root = tmp.join("proj");
    std::fs::create_dir_all(root.join(".cce")).unwrap();
    std::fs::write(root.join(".cce").join("config"), "knowledge:\n  enabled: true\n").unwrap();
    root
}

fn write_manifest(path: &Path, records: usize, sha256: &str) {
    let doc = format!(
        "{{\"schema\":\"cce.feed-manifest/v1\",\"records\":{records},\"sha256\":\"{sha256}\"}}\n"
    );
    std::fs::write(path, doc).unwrap();
}

fn index(feed: &Path, manifest: Option<&Path>, root: &Path) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args(["knowledge", "index"]).arg(feed).arg("--dir").arg(root);
    if let Some(m) = manifest {
        cmd.arg("--manifest").arg(m);
    }
    cmd.output().expect("spawn cce knowledge index")
}

fn indexed(root: &Path) -> bool {
    root.join(".cce").join("knowledge").join("current").is_file()
}

#[test]
fn a_matching_manifest_indexes_and_reports_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let root = make_root(tmp.path());
    let feed = tmp.path().join("feed.jsonl");
    std::fs::write(&feed, FEED).unwrap();
    let manifest = tmp.path().join("MANIFEST.json");
    write_manifest(&manifest, 2, &feed_sha256(FEED.as_bytes()));

    let out = index(&feed, Some(&manifest), &root);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("manifest  : verified"));
    assert!(indexed(&root), "the store must be written when the manifest matches");
}

#[test]
fn a_truncated_feed_fails_loudly_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = make_root(tmp.path());
    // The manifest describes the FULL two-record feed…
    let full_sha = feed_sha256(FEED.as_bytes());
    let manifest = tmp.path().join("MANIFEST.json");
    write_manifest(&manifest, 2, &full_sha);
    // …but the feed on disk has lost its second record.
    let truncated =
        "{\"id\":\"gh:o/r#1\",\"title\":\"One\",\"body\":\"alpha\",\"source\":\"github-issues\"}\n";
    let feed = tmp.path().join("feed.jsonl");
    std::fs::write(&feed, truncated).unwrap();

    let out = index(&feed, Some(&manifest), &root);
    assert!(!out.status.success(), "a truncated feed must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("truncated or incomplete"), "stderr: {stderr}");
    assert!(!indexed(&root), "no store may be written for a feed that fails the manifest check");
}

#[test]
fn a_misdirected_feed_fails_loudly_on_checksum() {
    let tmp = tempfile::tempdir().unwrap();
    let root = make_root(tmp.path());
    // The manifest is for FEED, but a different same-count feed is pointed at it.
    let manifest = tmp.path().join("MANIFEST.json");
    write_manifest(&manifest, 2, &feed_sha256(FEED.as_bytes()));
    let other = "{\"id\":\"gh:o/r#9\",\"title\":\"Nine\",\"body\":\"nine\",\"source\":\"github-issues\"}\n\
                 {\"id\":\"gh:o/r#8\",\"title\":\"Eight\",\"body\":\"eight\",\"source\":\"github-issues\"}\n";
    let feed = tmp.path().join("feed.jsonl");
    std::fs::write(&feed, other).unwrap();

    let out = index(&feed, Some(&manifest), &root);
    assert!(!out.status.success(), "a misdirected feed must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("checksum") && stderr.contains("misdirected"), "stderr: {stderr}");
    assert!(!indexed(&root), "no store may be written for a misdirected feed");
}

#[test]
fn without_a_manifest_the_feed_still_indexes_unchanged() {
    // The check is opt-in: no `--manifest` is exactly today's behaviour.
    let tmp = tempfile::tempdir().unwrap();
    let root = make_root(tmp.path());
    let feed = tmp.path().join("feed.jsonl");
    std::fs::write(&feed, FEED).unwrap();

    let out = index(&feed, None, &root);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("manifest"));
    assert!(indexed(&root));
}
