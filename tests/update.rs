//! # tests/update — hermetic end-to-end tests for `cce update` (issue #75)
//!
//! **Why this file exists:** the self-updater's guarantees — atomic in-place
//! replacement, checksum refusal that leaves the install untouched, pinned
//! `--check` exit codes — are process-level contracts. Only spawning the real
//! binary against a real release layout can prove them.
//!
//! **What it is / does:** builds a fixture GitHub-Releases layout on disk
//! (`latest/download/SHA256SUMS`, `download/vX.Y.Z/<tarballs>`), serves it from
//! a tiny local HTTP server (a `TcpListener` thread — the same hand-rolled
//! pattern as `src/dashboard.rs`; chosen over `file://` so the tests exercise
//! curl over the protocol production uses), and drives `cce update` against it
//! via the test-only `CCE_UPDATE_BASE_URL` / `CCE_UPDATE_TARGET` overrides.
//! No test ever touches the live GitHub, the developer's installed cce, or the
//! cargo-built test binary itself: the updater replaces `current_exe()` of the
//! process that runs, so every mutating test copies the built binary into a
//! tempdir and runs THAT copy.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for update/upgrade/--check/--version.
//! - It does NOT test delta rendering byte-exactness (unit-pinned in src/update.rs).

use cce::update::{sha256_hex, EXIT_UPDATE_AVAILABLE};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

/// The target triple every test pins via `CCE_UPDATE_TARGET`, so fixtures and
/// assertions are identical on any development or CI machine.
const TEST_TRIPLE: &str = "aarch64-apple-darwin";

/// The version the built binary reports (the tests' "current" version).
const CURRENT: &str = env!("CARGO_PKG_VERSION");

fn built_bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

/// Copy the built cce into `dir` and return the copy's path. The updater takes
/// its replacement target from `current_exe()`, so running this staged copy
/// confines every mutation to the tempdir.
fn stage_binary(dir: &Path) -> PathBuf {
    let staged = dir.join("cce");
    fs::copy(built_bin(), &staged).expect("stage the built binary");
    staged
}

/// Add a release to the fixture layout: a tarball named per the pipeline
/// contract (`cce-vX.Y.Z-<triple>/{cce,CHANGELOG.md}` inside
/// `cce-vX.Y.Z-<triple>.tar.gz`) plus its `SHA256SUMS`. With `latest`, the
/// sums are mirrored at `latest/download/SHA256SUMS` (the discovery fetch).
fn add_release(root: &Path, version: &str, bin_content: &[u8], changelog: &str, latest: bool) {
    let name = format!("cce-v{version}-{TEST_TRIPLE}");
    let build = root.join(format!(".build-{version}"));
    let pkg = build.join(&name);
    fs::create_dir_all(&pkg).unwrap();
    fs::write(pkg.join("cce"), bin_content).unwrap();
    fs::write(pkg.join("CHANGELOG.md"), changelog).unwrap();

    let release_dir = root.join("download").join(format!("v{version}"));
    fs::create_dir_all(&release_dir).unwrap();
    let tarball = release_dir.join(format!("{name}.tar.gz"));
    let out = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&build)
        .arg(&name)
        .output()
        .expect("tar available");
    assert!(out.status.success(), "tar failed: {}", String::from_utf8_lossy(&out.stderr));

    let sums = format!("{}  {name}.tar.gz\n", sha256_hex(&fs::read(&tarball).unwrap()));
    fs::write(release_dir.join("SHA256SUMS"), &sums).unwrap();
    if latest {
        let latest_dir = root.join("latest").join("download");
        fs::create_dir_all(&latest_dir).unwrap();
        fs::write(latest_dir.join("SHA256SUMS"), &sums).unwrap();
    }
}

/// Serve `root` over HTTP on an ephemeral local port; returns the base URL.
/// GET-only static files, 404 otherwise — exactly what the updater needs from
/// the GitHub release-asset endpoints. The thread leaks until the test process
/// exits, which is fine for a test server.
fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let root = root.clone();
            thread::spawn(move || handle(stream, &root));
        }
    });
    format!("http://{addr}")
}

fn handle(mut stream: TcpStream, root: &Path) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    // Drain the headers so curl sees a well-behaved server.
    let mut line = String::new();
    while reader.read_line(&mut line).is_ok() && line != "\r\n" && !line.is_empty() {
        line.clear();
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let rel = path.trim_start_matches('/');
    let file = root.join(rel);
    let response = if !rel.contains("..") && file.is_file() {
        let body = fs::read(&file).unwrap();
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body);
        resp
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

/// Run `bin` with the update env overrides pointing at `base`.
///
/// Retries on `ETXTBSY` (#78): a sibling test's fork can momentarily inherit
/// the write fd from `stage_binary`'s copy, so the first exec of a freshly
/// staged binary is racy under a parallel run. The inherited fd closes as soon
/// as that child execs, so a short retry always clears it.
fn run_update(bin: &Path, base: &str, args: &[&str]) -> Output {
    let mut attempts = 0;
    loop {
        let result = Command::new(bin)
            .args(args)
            .env("CCE_UPDATE_BASE_URL", base)
            .env("CCE_UPDATE_TARGET", TEST_TRIPLE)
            .output();
        match result {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 10 => {
                attempts += 1;
                thread::sleep(std::time::Duration::from_millis(50));
            }
            other => return other.expect("run cce"),
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn update_replaces_staged_binary_and_prints_the_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_root = tmp.path().join("releases");
    let new_bin = b"fake cce v99.0.0 binary\n".to_vec();
    let changelog = format!(
        "# Changelog\n\n## [Unreleased]\n\n### Added\n- Never printed.\n\n\
         ## [99.0.0] - 2099-01-01\n\n### Added\n- A brand new thing (#999).\n\n\
         ## [{CURRENT}] - 2026-01-01\n\n### Added\n- What you already have.\n"
    );
    add_release(&serve_root, "99.0.0", &new_bin, &changelog, true);
    let base = serve(serve_root);

    let staged = stage_binary(tmp.path());
    let out = run_update(&staged, &base, &["update"]);
    assert!(out.status.success(), "update failed: {}", stderr(&out));

    // The staged binary IS the downloaded one now, byte for byte.
    assert_eq!(fs::read(&staged).unwrap(), new_bin, "staged binary was not replaced");

    let text = stdout(&out);
    assert!(
        text.contains(&format!("updated cce: v{CURRENT} -> v99.0.0")),
        "missing summary: {text}"
    );
    // The CHANGELOG delta: the new version's section, not what the user had.
    assert!(text.contains("## [99.0.0] - 2099-01-01"), "missing delta section: {text}");
    assert!(text.contains("A brand new thing (#999)"), "missing delta body: {text}");
    assert!(!text.contains("What you already have"), "delta leaked the old version: {text}");
    assert!(!text.contains("Never printed"), "delta leaked [Unreleased]: {text}");
    // The long-lived-process note.
    assert!(text.contains("`cce mcp`"), "missing restart note: {text}");
}

#[test]
fn check_reports_behind_with_the_pinned_exit_code() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_root = tmp.path().join("releases");
    add_release(&serve_root, "99.0.0", b"unused", "# Changelog\n", true);
    let base = serve(serve_root);

    // --check never modifies anything, so the built binary can run directly.
    let out = run_update(Path::new(built_bin()), &base, &["update", "--check"]);
    assert_eq!(
        out.status.code(),
        Some(EXIT_UPDATE_AVAILABLE as i32),
        "behind must exit {EXIT_UPDATE_AVAILABLE}: {}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), format!("update available: v{CURRENT} -> v99.0.0\n"));
}

#[test]
fn check_reports_up_to_date_with_exit_zero_via_the_upgrade_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_root = tmp.path().join("releases");
    add_release(&serve_root, CURRENT, b"unused", "# Changelog\n", true);
    let base = serve(serve_root);

    // Exercises the `upgrade` alias on the same code path.
    let out = run_update(Path::new(built_bin()), &base, &["upgrade", "--check"]);
    assert_eq!(out.status.code(), Some(0), "up to date must exit 0: {}", stderr(&out));
    assert_eq!(stdout(&out), format!("up to date: v{CURRENT} (latest: v{CURRENT})\n"));
}

#[test]
fn version_pin_downgrades_with_a_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_root = tmp.path().join("releases");
    let old_bin = b"fake cce v0.0.1 binary\n".to_vec();
    // Deliberately NOT the latest: the pin path resolves download/v0.0.1/ directly.
    add_release(&serve_root, "0.0.1", &old_bin, "# Changelog\n\n## [0.0.1] - 2025-01-01\n", false);
    let base = serve(serve_root);

    let staged = stage_binary(tmp.path());
    let out = run_update(&staged, &base, &["update", "--version", "v0.0.1"]);
    assert!(out.status.success(), "rollback failed: {}", stderr(&out));
    assert!(stderr(&out).contains("downgrading"), "missing downgrade warning: {}", stderr(&out));
    assert_eq!(fs::read(&staged).unwrap(), old_bin, "staged binary was not rolled back");
    assert!(stdout(&out).contains("downgraded from"), "missing summary: {}", stdout(&out));
}

#[test]
fn checksum_mismatch_refuses_and_leaves_the_binary_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let serve_root = tmp.path().join("releases");
    add_release(&serve_root, "99.0.0", b"evil bytes", "# Changelog\n", true);
    // Corrupt the published checksum (both the latest mirror and the release's
    // own copy) so the downloaded tarball can never verify.
    let bogus = format!("{}  cce-v99.0.0-{TEST_TRIPLE}.tar.gz\n", "0".repeat(64));
    fs::write(serve_root.join("latest/download/SHA256SUMS"), &bogus).unwrap();
    fs::write(serve_root.join("download/v99.0.0/SHA256SUMS"), &bogus).unwrap();
    let base = serve(serve_root);

    let staged = stage_binary(tmp.path());
    let before = fs::read(&staged).unwrap();
    let out = run_update(&staged, &base, &["update"]);
    assert_eq!(out.status.code(), Some(1), "mismatch must exit 1");
    assert!(stderr(&out).contains("CHECKSUM MISMATCH"), "not loud enough: {}", stderr(&out));
    assert_eq!(fs::read(&staged).unwrap(), before, "binary must be byte-identical after refusal");
}

#[test]
fn unsupported_platform_error_names_the_published_targets() {
    // Fails before any network I/O; the base URL points at a closed port so a
    // regression (fetch-before-platform-check) fails loudly rather than
    // touching the live GitHub.
    let out = Command::new(built_bin())
        .args(["update", "--check"])
        .env("CCE_UPDATE_BASE_URL", "http://127.0.0.1:1")
        .env("CCE_UPDATE_TARGET", "riscv64-unknown-freebsd")
        .output()
        .expect("run cce");
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("unsupported platform `riscv64-unknown-freebsd`"), "{err}");
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ] {
        assert!(err.contains(target), "error must name {target}: {err}");
    }
}

#[test]
fn missing_curl_points_at_the_manual_install() {
    let tmp = tempfile::tempdir().unwrap();
    let empty_path = tmp.path().join("empty-path");
    fs::create_dir_all(&empty_path).unwrap();
    // An empty PATH makes `curl` unresolvable for the child process; the
    // closed-port base URL guarantees no live network even on a regression.
    let out = Command::new(built_bin())
        .args(["update", "--check"])
        .env("CCE_UPDATE_BASE_URL", "http://127.0.0.1:1")
        .env("CCE_UPDATE_TARGET", TEST_TRIPLE)
        .env("PATH", &empty_path)
        .output()
        .expect("run cce");
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("curl not found"), "{err}");
    assert!(err.contains("releases"), "must point at the releases page: {err}");
}
