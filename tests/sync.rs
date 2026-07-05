//! # tests/sync — end-to-end CCE Sync CLI tests (SPEC-SYNC §11)
//!
//! **Why this file exists:** SPEC-SYNC §11 requires hermetic, no-network proof that
//! the whole `cce sync …` flow works across *fresh processes* against a **local bare
//! git repo** (`file://`): init → push from clone A → pull into clone B → the
//! imported `.cce/` is functionally identical and the checksum matches. Only spawning
//! the real binary proves the fresh-process guarantee and the offline-first rules.
//!
//! **What it is / does:** Builds a bare remote and two source clones in temp dirs,
//! sets `CCE_HOME` to a temp dir so working clones never touch `~/.cce`, and drives
//! the binary: init, push, pull, status, verify, `--latest`, plus the refusals
//! (dirty tree, cache miss) and the offline guarantee (index/search with no remote).
//! A separate, SKIP-if-unavailable smoke test exercises the git-LFS path.
//!
//! **Responsibilities:**
//! - Own the process-level sync acceptance tests over plain git.
//! - It does NOT require the `git-lfs` binary for the core path.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// Run `cce <args>` with `CCE_HOME` pointed at `home` (hermetic working clones).
fn cce(home: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).env("CCE_HOME", home).output().unwrap()
}

/// Run a git command in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=t@e"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A bare git repo acting as the remote; returns (tempdir, file:// URL).
fn bare_remote() -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new("git")
        .args(["init", "--bare", "-q", "-b", "main"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let url = format!("file://{}", tmp.path().to_string_lossy());
    (tmp, url)
}

/// A source repo with two committed files on branch `main`.
fn source_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join("auth.py"), "def login(user):\n    return hash(user)\n").unwrap();
    std::fs::write(d.join("app.py"), "import auth\n\ndef run(u):\n    return auth.login(u)\n")
        .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// Clone a source repo (same committed sha) into a fresh temp dir.
fn clone_of(src: &Path) -> tempfile::TempDir {
    let dst = tempfile::tempdir().unwrap();
    let url = format!("file://{}", src.to_string_lossy());
    let out = Command::new("git")
        .args(["clone", "-q", &url])
        .arg(dst.path().join("work"))
        .output()
        .unwrap();
    assert!(out.status.success(), "clone failed: {}", String::from_utf8_lossy(&out.stderr));
    dst
}

const REPO_ID: &str = "example.com__acme__demo";

#[test]
fn init_push_pull_search_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();

    // init in the source repo (LFS off so the core path needs no git-lfs binary).
    let out = cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Configured sync remote"));

    // push from the source repo.
    let out = cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);
    assert!(out.status.success(), "push failed: {}", String::from_utf8_lossy(&out.stderr));
    let push_stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(push_stdout.contains(&format!("Pushed {REPO_ID}@")));

    // pull into a fresh consumer clone.
    let dst = clone_of(src.path());
    let work = dst.path().join("work");
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            work.to_str().unwrap(),
        ],
    );
    let out = cce(home.path(), &["sync", "pull", "--dir", work.to_str().unwrap()]);
    assert!(out.status.success(), "pull failed: {}", String::from_utf8_lossy(&out.stderr));
    let pull_stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(pull_stdout.contains("matches — pulled index used as-is"), "got: {pull_stdout}");

    // The store exists and search works over the pulled index (fresh process).
    assert!(work.join(".cce/index.json").exists());
    let out = cce(
        home.path(),
        &[
            "search",
            "login user",
            "--dir",
            work.to_str().unwrap(),
            "--no-graph",
            "--json",
            "--no-metrics",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(!v["results"].as_array().unwrap().is_empty(), "pulled index should be searchable");

    // verify: re-index locally and confirm the checksum matches.
    let out = cce(home.path(), &["sync", "verify", "--dir", work.to_str().unwrap()]);
    assert!(out.status.success(), "verify failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("verify OK"));
}

#[test]
fn checksum_is_identical_across_two_independent_builders() {
    // The same repo@sha built by two separate clones yields the same checksum,
    // proving content-addressability (SPEC-SYNC §10).
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();

    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    let out = cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);
    let a = String::from_utf8_lossy(&out.stdout).to_string();
    let checksum_a =
        a.lines().find_map(|l| l.trim().strip_prefix("checksum : ")).unwrap().to_string();

    // A second independent clone at the same sha, pushing to a different remote,
    // must produce the identical checksum.
    let (_bare2, url2) = bare_remote();
    let dst = clone_of(src.path());
    let work = dst.path().join("work");
    let home2 = tempfile::tempdir().unwrap();
    cce(
        home2.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url2,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            work.to_str().unwrap(),
        ],
    );
    let out = cce(home2.path(), &["sync", "push", "--dir", work.to_str().unwrap()]);
    let b = String::from_utf8_lossy(&out.stdout).to_string();
    let checksum_b =
        b.lines().find_map(|l| l.trim().strip_prefix("checksum : ")).unwrap().to_string();

    assert_eq!(checksum_a, checksum_b, "same repo@sha must be byte-identical across builders");
    assert_eq!(checksum_a.len(), 64, "checksum is a lowercase-hex SHA-256");
}

#[test]
fn pull_latest_resolves_the_ref_pointer() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);

    let dst = clone_of(src.path());
    let work = dst.path().join("work");
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            work.to_str().unwrap(),
        ],
    );
    let out = cce(home.path(), &["sync", "pull", "--latest", "--dir", work.to_str().unwrap()]);
    assert!(out.status.success(), "pull --latest failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(work.join(".cce/index.json").exists());
}

#[test]
fn push_refuses_dirty_working_tree() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    // Introduce a real (non-.cce) change.
    std::fs::write(src.path().join("auth.py"), "def login(u):\n    return 1\n").unwrap();
    let out = cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);
    assert!(!out.status.success(), "dirty push must fail");
    assert!(String::from_utf8_lossy(&out.stderr).contains("working tree is dirty"));
}

#[test]
fn offline_commands_work_with_no_remote_configured() {
    // SPEC-SYNC §9.1: with no remote, every non-sync command behaves as today, and
    // `sync status` reports the local-only state rather than erroring.
    let home = tempfile::tempdir().unwrap();
    let src = source_repo();

    // index + search do not need a remote at all.
    let out = cce(home.path(), &["index", src.path().to_str().unwrap()]);
    assert!(out.status.success(), "index without remote failed");
    let out = cce(
        home.path(),
        &["search", "login", "--dir", src.path().to_str().unwrap(), "--no-metrics", "--json"],
    );
    assert!(out.status.success());

    let out = cce(home.path(), &["sync", "status", "--dir", src.path().to_str().unwrap()]);
    assert!(out.status.success(), "status must succeed with no remote");
    assert!(String::from_utf8_lossy(&out.stdout).contains("pure local CCE"));

    // push without a remote is a clean error, not a crash.
    let out = cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sync remote configured"));
}

#[test]
fn pull_reports_cache_miss_clearly() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    // No push happened.
    let out = cce(home.path(), &["sync", "pull", "--dir", src.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cache miss"));
}

/// git-LFS smoke test — SKIPS gracefully when the `git-lfs` binary is unavailable
/// (SPEC-SYNC §11: the core path must not require it). When present, it proves the
/// full push→pull round-trip works with `*.cce` routed through LFS.
#[test]
fn lfs_round_trip_smoke_or_skip() {
    let lfs_available = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !lfs_available {
        eprintln!("SKIP: git-lfs not installed — LFS smoke test skipped (core path is plain git)");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = source_repo();
    let out = cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            src.path().to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "lfs init failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = cce(home.path(), &["sync", "push", "--dir", src.path().to_str().unwrap()]);
    assert!(out.status.success(), "lfs push failed: {}", String::from_utf8_lossy(&out.stderr));

    let dst = clone_of(src.path());
    let work = dst.path().join("work");
    cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--lfs",
            "--repo-id",
            REPO_ID,
            "--dir",
            work.to_str().unwrap(),
        ],
    );
    let out = cce(home.path(), &["sync", "pull", "--dir", work.to_str().unwrap()]);
    assert!(out.status.success(), "lfs pull failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = cce(home.path(), &["sync", "verify", "--dir", work.to_str().unwrap()]);
    assert!(out.status.success(), "lfs verify failed: {}", String::from_utf8_lossy(&out.stderr));
    let _ = PathBuf::new();
}
