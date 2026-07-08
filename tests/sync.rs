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

// --- `cce sync list` (#53): enumerate what a cache holds ---

/// Seed a cache repo with arbitrary `(path, bytes)` entries via plain git — a
/// hermetic stand-in for pushes from several repos (no CCE_HOME, no env races).
fn seed_cache(url: &str, files: &[(&str, &[u8])]) {
    let work = tempfile::tempdir().unwrap();
    let d = work.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["remote", "add", "origin", url]);
    // Base on whatever the remote already holds (no-op fetch on an empty cache).
    let _ = Command::new("git").args(["-C"]).arg(d).args(["fetch", "-q", "origin"]).output();
    let _ = Command::new("git")
        .arg("-C")
        .arg(d)
        .args(["reset", "-q", "--hard", "origin/main"])
        .output();
    for (path, bytes) in files {
        let p = d.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "seed"]);
    git(d, &["push", "-q", "origin", "HEAD:main"]);
}

/// The sha a cache branch tip is at (to prove `list` never mutates the cache).
fn cache_tip(bare: &Path) -> String {
    let out = Command::new("git").arg("-C").arg(bare).args(["rev-parse", "main"]).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn sync_list_enumerates_a_populated_cache_byte_pinned_json() {
    // A populated cache: repo `aaa` has two artifacts + a `refs/main` latest
    // pointer plus junk entries (the #37 fixture, raised to the CLI level: a
    // README, an extensionless file, a nested artifact); repo `bbb` has one
    // artifact and NO pointer (shown with a `-`/null latest, never hidden).
    let home = tempfile::tempdir().unwrap();
    let (bare, url) = bare_remote();
    seed_cache(
        &url,
        &[
            ("hash/2.3/aaa__one/1111111.cce", b"AA\n"),
            ("hash/2.3/aaa__one/2222222.cce", b"BBBB\n"),
            ("hash/2.3/aaa__one/refs/main", b"2222222\n"),
            ("hash/2.3/aaa__one/README.md", b"not an artifact\n"),
            ("hash/2.3/aaa__one/no-extension", b"junk\n"),
            ("hash/2.3/aaa__one/nested/deadbee.cce", b"CCCCC\n"),
            ("hash/2.3/bbb__two/3333333.cce", b"D\n"),
        ],
    );
    let tip_before = cache_tip(bare.path());

    // The repo-less consumer case: a bare directory with only `--remote`.
    let consumer = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &["sync", "list", "--remote", &url, "--dir", consumer.path().to_str().unwrap()],
    );
    assert!(out.status.success(), "list failed: {}", String::from_utf8_lossy(&out.stderr));
    let human = String::from_utf8_lossy(&out.stdout).to_string();
    let golden_human = format!(
        "remote        : {url}\n\
         \n\
         repo_id   latest   artifacts  bytes\n\
         aaa__one  2222222          3     14\n\
         bbb__two  -                1      2\n\
         \n\
         total         : 2 repos, 4 artifacts, 16 bytes\n"
    );
    assert_eq!(human, golden_human);

    // `--json`: the stable cce.synclist/v1 shape, byte-pinned end to end.
    let out = cce(
        home.path(),
        &["sync", "list", "--remote", &url, "--json", "--dir", consumer.path().to_str().unwrap()],
    );
    assert!(out.status.success(), "list --json failed: {}", String::from_utf8_lossy(&out.stderr));
    let json = String::from_utf8_lossy(&out.stdout).to_string();
    let golden_json = format!(
        r#"{{
  "remote": "{url}",
  "repos": [
    {{
      "artifacts": 3,
      "bytes": 14,
      "latest_sha": "2222222",
      "repo_id": "aaa__one"
    }},
    {{
      "artifacts": 1,
      "bytes": 2,
      "latest_sha": null,
      "repo_id": "bbb__two"
    }}
  ],
  "schema": "cce.synclist/v1"
}}
"#
    );
    assert_eq!(json, golden_json);

    // Read-only, twice over: the cache branch did not move, and the consumer
    // directory gained no `.cce/`.
    assert_eq!(cache_tip(bare.path()), tip_before, "list must never mutate the cache");
    assert!(!consumer.path().join(".cce").exists(), "list must not create a local .cce/");
}

#[test]
fn sync_list_reflects_a_real_push_via_the_latest_pointer() {
    // After a real `cce sync push`, `list` reports the pushed repo with its
    // latest sha equal to the pushed HEAD — the same refs/<ref> pointer
    // `pull --latest` resolves, not a heuristic.
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
    assert!(out.status.success(), "push failed: {}", String::from_utf8_lossy(&out.stderr));
    let head =
        Command::new("git").arg("-C").arg(src.path()).args(["rev-parse", "HEAD"]).output().unwrap();
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();

    // With no --remote, list resolves the configured `.cce/config` remote —
    // exactly as `cce sync status` does.
    let out = cce(home.path(), &["sync", "list", "--json", "--dir", src.path().to_str().unwrap()]);
    assert!(out.status.success(), "list failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(v["schema"], "cce.synclist/v1");
    assert_eq!(v["remote"], url.as_str());
    let repos = v["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["repo_id"], REPO_ID);
    assert_eq!(repos[0]["latest_sha"], head.as_str());
    assert_eq!(repos[0]["artifacts"], 1);
    assert!(repos[0]["bytes"].as_u64().unwrap() > 0);
}

#[test]
fn sync_list_empty_cache_is_friendly_and_exit_zero() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let consumer = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &["sync", "list", "--remote", &url, "--dir", consumer.path().to_str().unwrap()],
    );
    assert!(out.status.success(), "an empty cache is not an error");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("The cache is empty — nothing has been pushed yet."), "got: {stdout}");
}

#[test]
fn sync_list_unreachable_remote_errors_clearly() {
    let home = tempfile::tempdir().unwrap();
    let consumer = tempfile::tempdir().unwrap();
    let out = cce(
        home.path(),
        &[
            "sync",
            "list",
            "--remote",
            "file:///definitely/not/a/repo/here.git",
            "--dir",
            consumer.path().to_str().unwrap(),
        ],
    );
    assert!(!out.status.success(), "an unreachable remote must be a non-zero error");
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not clone"));
}

#[test]
fn sync_list_without_a_remote_gives_the_friendly_guidance() {
    let home = tempfile::tempdir().unwrap();
    let consumer = tempfile::tempdir().unwrap();
    let out = cce(home.path(), &["sync", "list", "--dir", consumer.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sync remote configured"));
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

// --- `cce sync pull --all --into <dir>` (#54): the repo-less consumer workspace ---

/// A tiny one-file git repo with the given content, committed on `main`.
fn tiny_repo(file: &str, content: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::write(d.join(file), content).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// `cce sync init` + `cce sync push` for `dir` under `repo_id`, asserting success.
fn init_and_push(home: &Path, url: &str, dir: &Path, repo_id: &str) {
    let out = cce(
        home,
        &[
            "sync",
            "init",
            "--remote",
            url,
            "--no-lfs",
            "--repo-id",
            repo_id,
            "--dir",
            dir.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "init failed: {}", String::from_utf8_lossy(&out.stderr));
    let out = cce(home, &["sync", "push", "--dir", dir.to_str().unwrap()]);
    assert!(out.status.success(), "push failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Drive an MCP session over stdio (the tests/mcp.rs pattern): feed newline-
/// delimited JSON-RPC on stdin, return stdout after EOF-driven exit.
fn drive_mcp(home: &Path, args: &[&str], input: &str) -> String {
    use std::io::Write;
    let mut child = Command::new(bin())
        .args(args)
        .env("CCE_HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// The `modified` time of a member's installed store — refresh proof.
fn store_mtime(ctx: &Path, member: &str) -> std::time::SystemTime {
    std::fs::metadata(ctx.join(member).join(".cce/index.json")).unwrap().modified().unwrap()
}

#[test]
fn pull_all_end_to_end_federated_search_and_mcp_from_a_bare_directory() {
    // Three independent tiny repos with distinct content, pushed to ONE cache…
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("rocket.py", "def alpha_rocket_launch(thrust):\n    return thrust * 2\n");
    let beta = tiny_repo("submarine.py", "def beta_submarine_dive(depth):\n    return depth + 1\n");
    let gamma = tiny_repo("glacier.py", "def gamma_glacier_melt(rate):\n    return rate - 1\n");
    init_and_push(home.path(), &url, alpha.path(), "example.com__team__alpha");
    init_and_push(home.path(), &url, beta.path(), "example.com__team__beta");
    init_and_push(home.path(), &url, gamma.path(), "example.com__team__gamma");

    // …then one command from a bare directory: no source checkout anywhere.
    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success(), "pull --all failed: {}", String::from_utf8_lossy(&out.stderr));
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("summary       : 3 pulled · 0 up-to-date · 0 skipped"),
        "got: {report}"
    );

    // Short member names; each member has a store + a config carrying the repo_id.
    for m in ["alpha", "beta", "gamma"] {
        assert!(ctx.join(m).join(".cce/index.json").exists(), "{m} store missing");
        let cfg = std::fs::read_to_string(ctx.join(m).join(".cce/config")).unwrap();
        assert!(cfg.contains(&format!("repo_id: example.com__team__{m}")), "got: {cfg}");
    }
    // The synthesized manifest declares store-only members and parses via the
    // ordinary parser (`cce workspace list` loads it).
    let yaml = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert!(yaml.contains("    type: store-only\n"), "got: {yaml}");
    let out = cce(home.path(), &["workspace", "list", ctx.to_str().unwrap()]);
    assert!(out.status.success(), "workspace list: {}", String::from_utf8_lossy(&out.stderr));

    // Federated search returns member-tagged hits from ALL three members.
    for (query, member) in [
        ("alpha rocket launch thrust", "alpha"),
        ("beta submarine dive depth", "beta"),
        ("gamma glacier melt rate", "gamma"),
    ] {
        let out = cce(
            home.path(),
            &["search", query, ctx.to_str().unwrap(), "--workspace", "--json", "--no-graph"],
        );
        assert!(out.status.success(), "search failed: {}", String::from_utf8_lossy(&out.stderr));
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
        let results = v["results"].as_array().unwrap();
        assert!(!results.is_empty(), "no federated results for {query}");
        assert_eq!(results[0]["package"], member, "top hit for {query} should be {member}");
    }

    // `cce mcp --workspace` serves a federated context_search over stdio.
    let input = "{\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"context_search\",\"arguments\":{\"query\":\"beta submarine dive depth\",\"no_graph\":true}}}\n";
    let stdout =
        drive_mcp(home.path(), &["mcp", "--workspace", "--dir", ctx.to_str().unwrap()], input);
    let resp: serde_json::Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["id"] == 1)
        .expect("no MCP response with id 1");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("beta · "), "expected a beta-tagged federated row, got: {text}");
    assert!(text.contains("submarine.py"), "got: {text}");
}

#[test]
fn pull_all_warns_and_skips_repos_without_a_latest_pointer() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    let beta = tiny_repo("b.py", "def beta_two():\n    return 2\n");
    init_and_push(home.path(), &url, alpha.path(), "example.com__team__alpha");
    init_and_push(home.path(), &url, beta.path(), "example.com__team__beta");
    // A repo_id with an artifact but NO latest pointer — real caches have these
    // (`cce sync list` renders them `-`). It must be warned + skipped, not fatal.
    seed_cache(&url, &[("hash/2.3/example.com__team__nolatest/1234567.cce", b"raw\n")]);

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success(), "one unpullable repo must not fail the run");
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("warning: skipped example.com__team__nolatest — no latest pointer"),
        "got: {report}"
    );
    assert!(
        report.contains("summary       : 2 pulled · 0 up-to-date · 1 skipped"),
        "got: {report}"
    );
    assert!(ctx.join("alpha/.cce/index.json").exists());
    assert!(ctx.join("beta/.cce/index.json").exists());
    assert!(!ctx.join("nolatest").exists(), "a skipped repo must leave no member dir");
}

#[test]
fn pull_all_is_idempotent_and_refreshes_exactly_the_moved_member() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    let beta = tiny_repo("b.py", "def beta_two():\n    return 2\n");
    init_and_push(home.path(), &url, alpha.path(), "example.com__team__alpha");
    init_and_push(home.path(), &url, beta.path(), "example.com__team__beta");

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let run = || {
        cce(
            home.path(),
            &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
        )
    };
    let out = run();
    assert!(out.status.success());
    let (alpha_t1, beta_t1) = (store_mtime(&ctx, "alpha"), store_mtime(&ctx, "beta"));

    // Second run: nothing moved — all up-to-date, nothing re-written.
    let out = run();
    assert!(out.status.success());
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("summary       : 0 pulled · 2 up-to-date · 0 skipped"),
        "got: {report}"
    );
    assert!(report.contains("alpha            up-to-date"), "got: {report}");
    assert_eq!(store_mtime(&ctx, "alpha"), alpha_t1, "an up-to-date member must not be re-written");
    assert_eq!(store_mtime(&ctx, "beta"), beta_t1, "an up-to-date member must not be re-written");

    // Move alpha's latest pointer (new commit + push) and add a brand-NEW repo.
    std::fs::write(alpha.path().join("a.py"), "def alpha_one_v2():\n    return 11\n").unwrap();
    git(alpha.path(), &["add", "-A"]);
    git(alpha.path(), &["commit", "-q", "-m", "v2"]);
    let out = cce(home.path(), &["sync", "push", "--dir", alpha.path().to_str().unwrap()]);
    assert!(out.status.success(), "re-push failed: {}", String::from_utf8_lossy(&out.stderr));
    let gamma = tiny_repo("g.py", "def gamma_three():\n    return 3\n");
    init_and_push(home.path(), &url, gamma.path(), "example.com__team__gamma");

    // Third run: exactly alpha refreshed, gamma picked up, beta untouched.
    let out = run();
    assert!(out.status.success());
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("summary       : 2 pulled · 1 up-to-date · 0 skipped"),
        "got: {report}"
    );
    assert!(report.contains("alpha            pulled"), "got: {report}");
    assert!(report.contains("gamma            pulled"), "got: {report}");
    assert!(report.contains("beta             up-to-date"), "got: {report}");
    assert_ne!(store_mtime(&ctx, "alpha"), alpha_t1, "a moved pointer must refresh the member");
    assert_eq!(store_mtime(&ctx, "beta"), beta_t1, "an unmoved member must not be re-written");
    assert!(ctx.join("gamma/.cce/index.json").exists(), "new repo_ids join the workspace");
    let yaml = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert!(yaml.contains("  - name: gamma\n"), "got: {yaml}");
}

#[test]
fn pull_all_synthesized_manifest_round_trips_and_hand_written_golden_is_untouched() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push(home.path(), &url, alpha.path(), "example.com__team__alpha");

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success());
    // The synthesized manifest is byte-stable across a refresh run (parse →
    // re-serialize is the identity: the round-trip proof at the file level).
    let yaml1 = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success());
    let yaml2 = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert_eq!(yaml1, yaml2, "a refresh must round-trip the manifest byte-identically");

    // Golden: a hand-written manifest is untouched by consumer mode — `cce
    // workspace list` (the ordinary parser) accepts it byte-for-byte as written.
    let hand = consumer.path().join("hand");
    std::fs::create_dir_all(hand.join(".cce")).unwrap();
    std::fs::create_dir_all(hand.join("api")).unwrap();
    let golden = "version: 1\nname: hand\nmembers:\n  - name: api\n    path: api\n    type: rails-app\n    package: api\n";
    std::fs::write(hand.join(".cce/workspace.yml"), golden).unwrap();
    let out = cce(home.path(), &["workspace", "list", hand.to_str().unwrap()]);
    assert!(out.status.success(), "hand-written manifest must still parse");
    assert_eq!(
        std::fs::read_to_string(hand.join(".cce/workspace.yml")).unwrap(),
        golden,
        "hand-written manifests must stay byte-identical"
    );
}

#[test]
fn pull_all_name_collision_gets_the_dash2_suffix() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    // Two repo_ids whose last segment collides: `…acme__demo` and `…zeta__demo`.
    let a = tiny_repo("a.py", "def acme_demo():\n    return 1\n");
    let z = tiny_repo("z.py", "def zeta_demo():\n    return 2\n");
    init_and_push(home.path(), &url, a.path(), "example.com__acme__demo");
    init_and_push(home.path(), &url, z.path(), "example.com__zeta__demo");

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success(), "pull --all failed: {}", String::from_utf8_lossy(&out.stderr));

    // repo_id order (acme < zeta): acme keeps `demo`, zeta gets `demo-2`.
    let demo = std::fs::read_to_string(ctx.join("demo/.cce/config")).unwrap();
    assert!(demo.contains("repo_id: example.com__acme__demo"), "got: {demo}");
    let demo2 = std::fs::read_to_string(ctx.join("demo-2/.cce/config")).unwrap();
    assert!(demo2.contains("repo_id: example.com__zeta__demo"), "got: {demo2}");
    let yaml = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert!(yaml.contains("  - name: demo\n    path: demo\n"), "got: {yaml}");
    assert!(yaml.contains("  - name: demo-2\n    path: demo-2\n"), "got: {yaml}");

    // A refresh keeps the mapping stable (config repo_id, not name re-derivation).
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success());
    let report = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        report.contains("summary       : 0 pulled · 2 up-to-date · 0 skipped"),
        "got: {report}"
    );
}

// --- #55: the self-describing cache — published workspace metadata + checksum-only verify ---

/// A committed JS workspace where member `alpha` DEPENDS ON member `beta`
/// (`package.json` dependency → one cross-member edge). Beta's content shares
/// no vocabulary with the alpha query, so only the graph edge can pull it in.
fn dep_workspace() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(d.join("alpha/src")).unwrap();
    std::fs::write(
        d.join("alpha/package.json"),
        "{\"name\":\"alpha\",\"dependencies\":{\"beta\":\"1.0.0\"}}",
    )
    .unwrap();
    // Three query-relevant functions: with `--top-k 3` the base ranking is all
    // alpha, so a beta row can ONLY come from cross-member graph expansion.
    std::fs::write(
        d.join("alpha/src/launch.js"),
        "function alphaRocketLaunchThrust(thrust) {\n  return thrust * 2;\n}\n\n\
         function alphaRocketLaunchWindow(thrust) {\n  return thrust + 1;\n}\n\n\
         function alphaRocketLaunchAbort(thrust) {\n  return thrust - 1;\n}\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.join("beta/src")).unwrap();
    std::fs::write(d.join("beta/package.json"), "{\"name\":\"beta\"}").unwrap();
    std::fs::write(
        d.join("beta/src/dive.js"),
        "function betaSubmarineDive(depth) {\n  return depth + 1;\n}\n",
    )
    .unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

const BASE_ID: &str = "example.com__acme__mono";

/// `cce workspace init` + `cce sync init` + `cce sync push --workspace`,
/// asserting the metadata publication line.
fn init_and_push_workspace(home: &Path, url: &str, dir: &Path) {
    let out = cce(home, &["workspace", "init", dir.to_str().unwrap()]);
    assert!(out.status.success(), "workspace init: {}", String::from_utf8_lossy(&out.stderr));
    let out = cce(
        home,
        &[
            "sync",
            "init",
            "--remote",
            url,
            "--no-lfs",
            "--repo-id",
            BASE_ID,
            "--dir",
            dir.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "sync init: {}", String::from_utf8_lossy(&out.stderr));
    let out = cce(home, &["sync", "push", "--workspace", "--dir", dir.to_str().unwrap()]);
    assert!(out.status.success(), "push --workspace: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("workspace.yml + workspace-graph.json"), "got: {stdout}");
}

/// Run a federated `--json` search (`--top-k 3`) and return the parsed
/// `results` array.
fn fed_results(home: &Path, dir: &Path, query: &str, graph: bool) -> serde_json::Value {
    let mut args =
        vec!["search", query, dir.to_str().unwrap(), "--workspace", "--json", "--top-k", "3"];
    if !graph {
        args.push("--no-graph");
    }
    let out = cce(home, &args);
    assert!(out.status.success(), "search failed: {}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    v["results"].clone()
}

/// The decisive #55 test: A depends on B → `push --workspace` publishes the
/// metadata → a REPO-LESS `pull --workspace` and a repo-less `pull --all` both
/// regain cross-member graph expansion — a federated search hitting alpha pulls
/// beta context, byte-identical to the same search on the source-side workspace.
#[test]
fn published_metadata_gives_repo_less_consumers_cross_member_expansion_byte_identical() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = dep_workspace();
    init_and_push_workspace(home.path(), &url, src.path());

    // Source-side truth: index the workspace (member stores + derived graph),
    // then search WITH graph expansion.
    let out = cce(home.path(), &["index", src.path().to_str().unwrap(), "--workspace"]);
    assert!(out.status.success(), "index --workspace: {}", String::from_utf8_lossy(&out.stderr));
    let query = "alpha rocket launch thrust";
    let source_results = fed_results(home.path(), src.path(), query, true);
    let members_of = |results: &serde_json::Value| -> Vec<String> {
        results
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["package"].as_str().unwrap().to_string())
            .collect()
    };
    // The source-side search proves the expansion premise: beta appears only
    // because of the alpha -> beta edge (absent under --no-graph).
    assert!(members_of(&source_results).contains(&"beta".to_string()));
    assert!(!members_of(&fed_results(home.path(), src.path(), query, false))
        .contains(&"beta".to_string()));

    // Consumer 1: repo-less `pull --workspace --latest` (bare dir + config only).
    let c1 = tempfile::tempdir().unwrap();
    let ctx1 = c1.path().join("ctx");
    std::fs::create_dir_all(&ctx1).unwrap();
    let out = cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            BASE_ID,
            "--dir",
            ctx1.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    let out = cce(
        home.path(),
        &["sync", "pull", "--workspace", "--latest", "--dir", ctx1.to_str().unwrap()],
    );
    assert!(out.status.success(), "pull --workspace: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("manifest         installed from the published metadata"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("workspace-graph.json installed (1 cross-member edge)"),
        "got: {stdout}"
    );
    let r1 = fed_results(home.path(), &ctx1, query, true);
    assert_eq!(r1, source_results, "pull --workspace consumer must match the source search");
    assert_eq!(r1.to_string(), source_results.to_string(), "byte-identical results JSON");

    // Consumer 2: repo-less `pull --all`.
    let c2 = tempfile::tempdir().unwrap();
    let ctx2 = c2.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx2.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success(), "pull --all: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "metadata      : 1 published workspace manifest applied · 1 cross-member edge installed"
        ),
        "got: {stdout}"
    );
    let r2 = fed_results(home.path(), &ctx2, query, true);
    assert_eq!(r2, source_results, "pull --all consumer must match the source search");
    assert_eq!(r2.to_string(), source_results.to_string(), "byte-identical results JSON");
}

/// #55 stability: `pull --all` against a cache with NO published manifest is
/// exactly the #54 behaviour — synthesized store-only members, no metadata
/// line, and no workspace-graph.json is written.
#[test]
fn pull_all_without_published_metadata_keeps_the_54_shape() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let alpha = tiny_repo("a.py", "def alpha_one():\n    return 1\n");
    init_and_push(home.path(), &url, alpha.path(), "example.com__team__alpha");

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success());
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(!report.contains("metadata      :"), "got: {report}");
    assert!(!ctx.join(".cce/workspace-graph.json").exists(), "no graph without a manifest");
    let yaml = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert!(yaml.contains("    type: store-only\n"), "got: {yaml}");
}

/// #55, the documented multi-workspace rule at the CLI level: two published
/// workspaces whose member names collide → the first (repo_id order) keeps the
/// bare name, the later one stays at its `-2` name — warned — and each manifest
/// enriches only its OWN member.
#[test]
fn two_published_workspaces_with_a_member_name_collision_first_wins_and_warns() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    for (base, package) in
        [("example.com__acme__mono", "demo_acme"), ("example.com__zeta__mono", "demo_zeta")]
    {
        let src = tempfile::tempdir().unwrap();
        let d = src.path();
        git(d, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(d.join("demo/src")).unwrap();
        std::fs::write(d.join("demo/package.json"), format!("{{\"name\":\"{package}\"}}")).unwrap();
        std::fs::write(
            d.join("demo/src/index.js"),
            format!("function {package}() {{ return 1; }}\n"),
        )
        .unwrap();
        git(d, &["add", "-A"]);
        git(d, &["commit", "-q", "-m", "init"]);
        let out = cce(home.path(), &["workspace", "init", d.to_str().unwrap()]);
        assert!(out.status.success());
        let out = cce(
            home.path(),
            &[
                "sync",
                "init",
                "--remote",
                &url,
                "--no-lfs",
                "--repo-id",
                base,
                "--dir",
                d.to_str().unwrap(),
            ],
        );
        assert!(out.status.success());
        let out = cce(home.path(), &["sync", "push", "--workspace", "--dir", d.to_str().unwrap()]);
        assert!(out.status.success(), "push: {}", String::from_utf8_lossy(&out.stderr));
    }

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success(), "pull --all: {}", String::from_utf8_lossy(&out.stderr));
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(
        report.contains(
            "warning: workspace example.com__zeta__mono: member name `demo` was taken by an \
             earlier repo — kept as `demo-2` (first in repo_id order wins)"
        ),
        "got: {report}"
    );
    let yaml = std::fs::read_to_string(ctx.join(".cce/workspace.yml")).unwrap();
    assert!(
        yaml.contains(
            "  - name: demo\n    path: demo\n    type: javascript\n    package: demo_acme\n"
        ),
        "got: {yaml}"
    );
    assert!(
        yaml.contains(
            "  - name: demo-2\n    path: demo-2\n    type: javascript\n    package: demo_zeta\n"
        ),
        "got: {yaml}"
    );
}

/// #55: `verify --checksum-only` at the CLI level — passes on an intact
/// repo-less consumer workspace, fails loudly NAMING the member after one byte
/// flips in a pulled store, with no source checkout anywhere.
#[test]
fn verify_checksum_only_passes_intact_and_names_the_corrupted_member() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = dep_workspace();
    init_and_push_workspace(home.path(), &url, src.path());

    let consumer = tempfile::tempdir().unwrap();
    let ctx = consumer.path().join("ctx");
    let out = cce(
        home.path(),
        &["sync", "pull", "--all", "--into", ctx.to_str().unwrap(), "--remote", &url],
    );
    assert!(out.status.success());

    let out =
        cce(home.path(), &["sync", "verify", "--checksum-only", "--dir", ctx.to_str().unwrap()]);
    assert!(out.status.success(), "intact verify: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("verify OK (checksum-only): 2 members"), "got: {stdout}");

    // Flip one byte in beta's pulled store.
    let store = ctx.join("beta/.cce/index.json");
    let mut bytes = std::fs::read(&store).unwrap();
    let idx = bytes.iter().position(|&b| b == b'u').unwrap();
    bytes[idx] = b'v';
    std::fs::write(&store, bytes).unwrap();

    let out =
        cce(home.path(), &["sync", "verify", "--checksum-only", "--dir", ctx.to_str().unwrap()]);
    assert!(!out.status.success(), "corrupted store must fail verify");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("verify FAILED (checksum-only) for member `beta`"), "got: {stderr}");
}

/// #55 additivity, the old-client shape: a plain single-member `pull --latest`
/// against a METADATA-CARRYING cache is byte-for-byte the pre-#55 experience —
/// the published workspace keys are invisible to it.
#[test]
fn plain_single_member_pull_latest_is_unchanged_on_a_metadata_carrying_cache() {
    let home = tempfile::tempdir().unwrap();
    let (_bare, url) = bare_remote();
    let src = dep_workspace();
    init_and_push_workspace(home.path(), &url, src.path());

    // A repo-less single-member consumer of just alpha.
    let consumer = tempfile::tempdir().unwrap();
    let dir = consumer.path().join("alpha");
    std::fs::create_dir_all(&dir).unwrap();
    let repo_id = format!("{BASE_ID}__alpha");
    let out = cce(
        home.path(),
        &[
            "sync",
            "init",
            "--remote",
            &url,
            "--no-lfs",
            "--repo-id",
            &repo_id,
            "--dir",
            dir.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    let out = cce(home.path(), &["sync", "pull", "--latest", "--dir", dir.to_str().unwrap()]);
    assert!(out.status.success(), "pull --latest: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("Pulled {repo_id}@")), "got: {stdout}");
    assert!(
        stdout.contains("(no source checkout — consumer mode; the pulled index is the corpus)"),
        "got: {stdout}"
    );
    // The pull touched only the member's own store — no workspace metadata
    // appears for a plain pull.
    assert!(dir.join(".cce/index.json").exists());
    assert!(!dir.join(".cce/workspace.yml").exists());
    assert!(!dir.join(".cce/workspace-graph.json").exists());
    // And a plain search over it works exactly as before.
    let out = cce(
        home.path(),
        &[
            "search",
            "rocket launch thrust",
            "--dir",
            dir.to_str().unwrap(),
            "--json",
            "--no-metrics",
        ],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(!v["results"].as_array().unwrap().is_empty());
}
