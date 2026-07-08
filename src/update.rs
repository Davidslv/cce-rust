//! # update — checksum-verified self-update from GitHub Releases (issue #75)
//!
//! **Why this file exists:** every release publishes per-platform tarballs plus a
//! `SHA256SUMS` file (the tag-driven pipeline in `.github/workflows/release.yml`).
//! Consumers running a prebuilt binary with no Rust toolchain need a client for
//! it: `cce update` replaces the running binary in place, verified, and prints
//! the CHANGELOG delta so the user sees exactly what they got.
//!
//! **What it is / does:** resolves the latest (or a pinned) release, downloads
//! the platform tarball by shelling out to `curl` (house pattern: sync shells
//! out to `git`; no HTTP-client dependency for one command), verifies the
//! tarball against `SHA256SUMS`, and atomically renames the new binary over
//! `current_exe()` (symlinks resolved first). Every step before the final
//! rename happens in a temp dir: any failure leaves the current install
//! untouched.
//!
//! **The three settled design decisions (issue #75):**
//! 1. *Offline-first posture*: `cce update` is EXPLICIT-INVOCATION network only.
//!    This module is the ONLY code path in the tree that invokes `curl` — no
//!    other command gains any network behavior because this feature exists
//!    (grep-provable: `curl` appears in no other module).
//! 2. *No HTTP client dependency*: shell out to `curl` (ships on every
//!    macOS/Linux box). Missing curl → clear error with the manual-install
//!    pointer.
//! 3. *Trust posture, stated honestly*: `SHA256SUMS` verification protects
//!    integrity (truncated/corrupt downloads), not authenticity beyond GitHub's
//!    TLS — the same posture as the README's manual install. Detached
//!    signatures are a documented future hardening, not implied to exist.
//!
//! **Version discovery (pinned choice):** fetch `releases/latest/download/
//! SHA256SUMS` and parse the release version out of the asset names
//! (`cce-vX.Y.Z-<target>.tar.gz`). Chosen over the `releases/latest` HTTP
//! redirect because ONE fetch yields both the version and the verification
//! material, it needs no JSON API and no redirect parsing, and it works
//! unchanged against the plain static-file layout the hermetic tests serve.
//!
//! **Test-only environment variables** (documented here, not user-facing):
//! - `CCE_UPDATE_BASE_URL` — replaces the `https://github.com/<repo>/releases`
//!   URL prefix so tests can serve a fixture release layout from a local HTTP
//!   server. Never set this outside tests.
//! - `CCE_UPDATE_TARGET` — overrides the compile-time platform detection so
//!   tests are deterministic across machines and can exercise the
//!   unsupported-platform error.
//!
//! **Responsibilities:**
//! - Own the whole update flow: discovery, download, verification, atomic
//!   swap, and the CHANGELOG-delta rendering.
//! - It does NOT auto-check, background-poll, or touch any retrieval surface.

use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The canonical releases URL prefix. `CCE_UPDATE_BASE_URL` (test-only)
/// replaces exactly this prefix; the `/latest/download/<asset>` and
/// `/download/vX.Y.Z/<asset>` suffixes are appended to whichever is active.
pub const RELEASES_BASE: &str = "https://github.com/davidslv/cce-rust/releases";

/// `cce update --check` exit code when an update is available (pinned; exit 0
/// means up to date, any other non-zero is an error). Documented in the CLI
/// help, README, and docs/how-to.md — scripts depend on it.
pub const EXIT_UPDATE_AVAILABLE: u8 = 10;

/// The release targets the pipeline publishes (`.github/workflows/release.yml`
/// build matrix). Asset naming is a compatibility contract: the updater
/// derives `cce-vX.Y.Z-<target>.tar.gz` from this list.
pub const SUPPORTED_TARGETS: [&str; 4] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
];

/// CHANGELOG sections printed after a multi-version jump before linking to the
/// releases page instead (issue #75: "bounded, e.g. max 5, then a link").
const MAX_DELTA_SECTIONS: usize = 5;

/// A `MAJOR.MINOR.PATCH` version. This project never publishes pre-release
/// suffixes (RELEASING.md), so three numeric fields are the whole grammar.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    /// Parse `X.Y.Z` (a leading `v` is accepted and ignored).
    pub fn parse(s: &str) -> Option<SemVer> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut it = s.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some(SemVer { major, minor, patch })
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The active releases URL prefix: `CCE_UPDATE_BASE_URL` (test-only) or the
/// canonical GitHub prefix, with any trailing `/` trimmed.
fn base_url() -> String {
    std::env::var("CCE_UPDATE_BASE_URL")
        .unwrap_or_else(|_| RELEASES_BASE.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The release-asset target triple for this machine, or a clear error naming
/// the four published targets. `CCE_UPDATE_TARGET` (test-only) overrides the
/// detection so tests are deterministic and can exercise the error path.
fn target_triple() -> Result<String, String> {
    let detected = std::env::var("CCE_UPDATE_TARGET")
        .unwrap_or_else(|_| detect_triple(std::env::consts::OS, std::env::consts::ARCH));
    validate_triple(&detected)
}

/// Map `std::env::consts` OS/arch to the release-asset triple (best effort;
/// unsupported combinations produce a descriptive placeholder for the error).
fn detect_triple(os: &str, arch: &str) -> String {
    match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin".to_string(),
        ("macos", "x86_64") => "x86_64-apple-darwin".to_string(),
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu".to_string(),
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu".to_string(),
        (os, arch) => format!("{arch}-{os}"),
    }
}

/// Accept only the four published targets; anything else errors naming them.
fn validate_triple(triple: &str) -> Result<String, String> {
    if SUPPORTED_TARGETS.contains(&triple) {
        Ok(triple.to_string())
    } else {
        Err(format!(
            "unsupported platform `{triple}`: releases are published for {}. \
             Install manually instead (README → Installation): {RELEASES_BASE}",
            SUPPORTED_TARGETS.join(", ")
        ))
    }
}

/// The error for a missing `curl` binary, with the manual-install pointer.
fn curl_missing_error() -> String {
    format!(
        "curl not found on PATH — `cce update` downloads releases with curl \
         (it makes no other network calls). Install curl, or update manually \
         from {RELEASES_BASE} (README → Installation)"
    )
}

/// Run `curl -fsSL <url>` and return its stdout bytes. `-f` turns HTTP errors
/// (404 for a missing release) into a non-zero exit instead of an HTML body.
fn curl_fetch(url: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl").args(["-fsSL", "--", url]).output();
    match out {
        Err(e) if e.kind() == ErrorKind::NotFound => Err(curl_missing_error()),
        Err(e) => Err(format!("failed to run curl: {e}")),
        Ok(out) if !out.status.success() => Err(format!(
            "download failed for {url}: {}",
            match String::from_utf8_lossy(&out.stderr).trim() {
                "" => "curl exited non-zero (is the release published?)".to_string(),
                s => s.to_string(),
            }
        )),
        Ok(out) => Ok(out.stdout),
    }
}

/// Parse `SHA256SUMS` (`shasum -a 256` format: `<hex>  <asset>` per line) into
/// `(hex, asset)` pairs, in file order.
fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let hex = it.next()?;
            let name = it.next()?;
            Some((hex.to_string(), name.trim_start_matches('*').to_string()))
        })
        .collect()
}

/// Extract the release version from the asset names in a parsed `SHA256SUMS`
/// (`cce-vX.Y.Z-<target>.tar.gz`). All assets of a release share one version,
/// so the first parseable name wins.
fn version_from_sums(sums: &[(String, String)]) -> Option<SemVer> {
    sums.iter().find_map(|(_, name)| {
        let rest = name.strip_prefix("cce-v")?;
        SemVer::parse(rest.split('-').next()?)
    })
}

/// Lowercase hex SHA-256 of `bytes`. Public so the hermetic tests build their
/// fixture `SHA256SUMS` with the exact hash the updater will compute.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

/// Render the CHANGELOG sections for every version in `(from, to]`, newest
/// first, capped at [`MAX_DELTA_SECTIONS`] with a releases-page link for the
/// remainder. Returns an empty string when nothing falls in the range (e.g. a
/// downgrade). Pure: byte-pinned by a fixture test.
pub fn changelog_delta(changelog: &str, from: SemVer, to: SemVer) -> String {
    // Split into `## [X.Y.Z]` sections; `## [Unreleased]` (unparseable) is
    // skipped along with its body.
    let mut sections: Vec<(SemVer, String)> = Vec::new();
    let mut current: Option<(SemVer, Vec<&str>)> = None;
    for line in changelog.lines() {
        if line.starts_with("## ") {
            if let Some((v, body)) = current.take() {
                sections.push((v, finish_section(body)));
            }
            let version = line
                .strip_prefix("## [")
                .and_then(|rest| rest.split(']').next())
                .and_then(SemVer::parse);
            if let Some(v) = version {
                current = Some((v, vec![line]));
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((v, body)) = current.take() {
        sections.push((v, finish_section(body)));
    }

    let mut in_range: Vec<(SemVer, String)> =
        sections.into_iter().filter(|(v, _)| *v > from && *v <= to).collect();
    // CHANGELOG.md is newest-first already; sort descending anyway so the
    // output order is a guarantee, not a file-layout accident.
    in_range.sort_by(|a, b| b.0.cmp(&a.0));

    let total = in_range.len();
    let mut out = in_range
        .iter()
        .take(MAX_DELTA_SECTIONS)
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if total > MAX_DELTA_SECTIONS {
        out.push_str(&format!(
            "\n\n... and {} more release(s): {RELEASES_BASE}",
            total - MAX_DELTA_SECTIONS
        ));
    }
    out
}

/// Join a section's lines and trim trailing blank lines.
fn finish_section(lines: Vec<&str>) -> String {
    let mut text = lines.join("\n");
    while text.ends_with('\n') || text.ends_with("\n ") {
        text.truncate(text.trim_end().len());
    }
    text.trim_end().to_string()
}

/// A temp staging directory removed on drop (best effort). Everything the
/// updater downloads or extracts lives here until the final atomic rename, so
/// any failure leaves the current install untouched.
struct StageDir(PathBuf);

impl StageDir {
    fn create() -> Result<StageDir, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("cce-update-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create temp dir: {e}"))?;
        Ok(StageDir(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StageDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `cce update [--check] [--version vX.Y.Z]`. Returns the process exit code on
/// the happy paths (0, or [`EXIT_UPDATE_AVAILABLE`] for `--check` when
/// behind); every failure is an `Err` and exits 1 via `main`.
pub fn cmd_update(check: bool, pin: Option<String>) -> Result<u8, String> {
    let current = SemVer::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is always MAJOR.MINOR.PATCH");
    // Fail fast on an unsupported platform, before any network I/O: --check on
    // a platform with no published asset would advertise an update the user
    // cannot install.
    let triple = target_triple()?;
    let base = base_url();

    // Discovery: one SHA256SUMS fetch yields both the release version and the
    // verification material (see the module docs for why this mechanism).
    let (target_version, sums) = match &pin {
        Some(v) => {
            let ver = SemVer::parse(v)
                .ok_or_else(|| format!("--version must look like vX.Y.Z (got `{v}`)"))?;
            let url = format!("{base}/download/v{ver}/SHA256SUMS");
            let text =
                curl_fetch(&url).map_err(|e| format!("cannot resolve release v{ver}: {e}"))?;
            (ver, parse_sums(&String::from_utf8_lossy(&text)))
        }
        None => {
            let url = format!("{base}/latest/download/SHA256SUMS");
            let text =
                curl_fetch(&url).map_err(|e| format!("cannot resolve the latest release: {e}"))?;
            let sums = parse_sums(&String::from_utf8_lossy(&text));
            let ver = version_from_sums(&sums).ok_or_else(|| {
                format!("no cce-vX.Y.Z asset names in SHA256SUMS at {url} — cannot determine the latest version")
            })?;
            (ver, sums)
        }
    };

    if check {
        // Machine-friendly single line + pinned exit codes: 0 = up to date,
        // EXIT_UPDATE_AVAILABLE (10) = behind, 1 = error.
        return if target_version > current {
            println!("update available: v{current} -> v{target_version}");
            Ok(EXIT_UPDATE_AVAILABLE)
        } else {
            println!("up to date: v{current} (latest: v{target_version})");
            Ok(0)
        };
    }

    if target_version == current {
        println!("cce is already v{current} — nothing to do");
        return Ok(0);
    }
    let downgrade = target_version < current;
    if downgrade {
        eprintln!(
            "warning: downgrading v{current} -> v{target_version} — older releases may expect \
             different store/format versions; this is the supported rollback path, proceeding"
        );
    }

    // Everything below stages in a temp dir; the current binary is not touched
    // until the final atomic rename.
    let asset = format!("cce-v{target_version}-{triple}.tar.gz");
    let expected =
        sums.iter().find(|(_, name)| *name == asset).map(|(hex, _)| hex.clone()).ok_or_else(
            || {
                format!(
                    "release v{target_version} publishes no asset `{asset}` (assets: {})",
                    sums.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
                )
            },
        )?;

    let stage = StageDir::create()?;
    let url = format!("{base}/download/v{target_version}/{asset}");
    let tarball_bytes = curl_fetch(&url)?;
    let actual = sha256_hex(&tarball_bytes);
    if actual != expected {
        return Err(format!(
            "CHECKSUM MISMATCH for {asset}:\n  expected {expected}\n  got      {actual}\n\
             The download is corrupt (or tampered with). Your current binary is untouched. \
             Re-run `cce update`; if it persists, install manually: {RELEASES_BASE}"
        ));
    }

    let tarball = stage.path().join(&asset);
    fs::write(&tarball, &tarball_bytes).map_err(|e| format!("cannot write {asset}: {e}"))?;
    extract_tarball(&tarball, stage.path())?;
    let release_dir = stage.path().join(format!("cce-v{target_version}-{triple}"));
    let new_binary = release_dir.join("cce");
    if !new_binary.is_file() {
        return Err(format!(
            "{asset} does not contain cce-v{target_version}-{triple}/cce — refusing to install"
        ));
    }
    // The NEW changelog knows about the versions this binary predates; render
    // the delta from it (missing/unreadable file just skips the delta).
    let delta = if downgrade {
        String::new()
    } else {
        fs::read_to_string(release_dir.join("CHANGELOG.md"))
            .map(|text| changelog_delta(&text, current, target_version))
            .unwrap_or_default()
    };

    let exe = installed_exe_path()?;
    replace_binary(&new_binary, &exe)?;

    if downgrade {
        println!(
            "installed cce v{target_version} (downgraded from v{current}) at {}",
            exe.display()
        );
    } else {
        println!("updated cce: v{current} -> v{target_version} ({})", exe.display());
    }
    if !delta.is_empty() {
        println!("\nWhat changed:\n\n{delta}");
    }
    println!(
        "\nnote: long-lived processes (`cce mcp`, `cce dashboard`) keep v{current} until restarted"
    );
    Ok(0)
}

/// The path the running binary was installed at, symlinks resolved — the
/// rename must replace the real file, not a symlink pointing at it.
fn installed_exe_path() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate the running binary: {e}"))?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

/// Extract `tarball` into `dest` by shelling out to `tar` (present wherever
/// curl is; the README's manual install already depends on it).
fn extract_tarball(tarball: &Path, dest: &Path) -> Result<(), String> {
    let out =
        Command::new("tar").arg("-xzf").arg(tarball).arg("-C").arg(dest).output().map_err(|e| {
            match e.kind() {
                ErrorKind::NotFound => {
                    "tar not found on PATH — install tar or update manually".to_string()
                }
                _ => format!("failed to run tar: {e}"),
            }
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("tar failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Atomically replace `exe` with `new_binary`: copy into a staging file IN THE
/// SAME DIRECTORY (rename is only atomic within a filesystem), then rename over
/// the target. The running process keeps its old inode (macOS/Linux). Any
/// failure cleans the staging file and leaves `exe` untouched.
fn replace_binary(new_binary: &Path, exe: &Path) -> Result<(), String> {
    let dir = exe.parent().ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let staged = dir.join(format!(".cce-update-{}", std::process::id()));
    let unwritable = |e: &std::io::Error| {
        format!(
            "cannot write to {} ({e}). cce is installed somewhere you lack write access; \
             re-run with elevated permissions (e.g. `sudo cce update`) or install manually \
             from {RELEASES_BASE} — cce never escalates privileges itself",
            dir.display()
        )
    };
    fs::copy(new_binary, &staged).map_err(|e| unwritable(&e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)) {
            let _ = fs::remove_file(&staged);
            return Err(format!("cannot mark the new binary executable: {e}"));
        }
    }
    fs::rename(&staged, exe).map_err(|e| {
        let _ = fs::remove_file(&staged);
        unwritable(&e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> SemVer {
        SemVer::parse(s).unwrap()
    }

    #[test]
    fn semver_parses_with_and_without_v() {
        assert_eq!(v("2.7.0"), SemVer { major: 2, minor: 7, patch: 0 });
        assert_eq!(v("v2.7.0"), v("2.7.0"));
        assert_eq!(SemVer::parse("2.7"), None);
        assert_eq!(SemVer::parse("2.7.0.1"), None);
        assert_eq!(SemVer::parse("abc"), None);
        assert_eq!(v("10.0.0").to_string(), "10.0.0");
    }

    #[test]
    fn semver_orders_numerically_not_lexically() {
        assert!(v("2.10.0") > v("2.9.9"));
        assert!(v("10.0.0") > v("9.99.99"));
        assert!(v("2.7.0") == v("v2.7.0"));
    }

    #[test]
    fn sums_parse_and_version_extraction() {
        let text = "abc123  cce-v2.6.9-aarch64-apple-darwin.tar.gz\n\
                    def456  cce-v2.6.9-x86_64-unknown-linux-gnu.tar.gz\n";
        let sums = parse_sums(text);
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0].0, "abc123");
        assert_eq!(sums[0].1, "cce-v2.6.9-aarch64-apple-darwin.tar.gz");
        assert_eq!(version_from_sums(&sums), Some(v("2.6.9")));
        assert_eq!(version_from_sums(&parse_sums("junk")), None);
    }

    #[test]
    fn sha256_matches_known_vector() {
        // shasum -a 256 <<< printf 'abc'
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn triple_detection_covers_the_release_matrix() {
        assert_eq!(detect_triple("macos", "aarch64"), "aarch64-apple-darwin");
        assert_eq!(detect_triple("macos", "x86_64"), "x86_64-apple-darwin");
        assert_eq!(detect_triple("linux", "x86_64"), "x86_64-unknown-linux-gnu");
        assert_eq!(detect_triple("linux", "aarch64"), "aarch64-unknown-linux-gnu");
        for t in SUPPORTED_TARGETS {
            assert_eq!(validate_triple(t).unwrap(), t);
        }
    }

    #[test]
    fn unsupported_triple_error_names_all_four_targets() {
        let err = validate_triple("riscv64-unknown-freebsd").unwrap_err();
        for t in SUPPORTED_TARGETS {
            assert!(err.contains(t), "error must name {t}: {err}");
        }
        assert!(err.contains("riscv64-unknown-freebsd"));
    }

    /// The delta rendering, byte-pinned against the fixture CHANGELOG
    /// (test/fixture/update/CHANGELOG.md).
    #[test]
    fn changelog_delta_is_byte_pinned() {
        let changelog = include_str!("../test/fixture/update/CHANGELOG.md");

        // One-version jump: exactly the newest section, headers intact.
        let one = changelog_delta(changelog, v("0.6.0"), v("0.7.0"));
        assert_eq!(
            one,
            "## [0.7.0] - 2026-07-07\n\n### Added\n- Seventh feature (#7).\n\n### Fixed\n- Seventh fix."
        );

        // Two-version jump: newest first, sections separated by a blank line.
        let two = changelog_delta(changelog, v("0.5.0"), v("0.7.0"));
        assert_eq!(
            two,
            "## [0.7.0] - 2026-07-07\n\n### Added\n- Seventh feature (#7).\n\n### Fixed\n- Seventh fix.\n\n\
             ## [0.6.0] - 2026-07-06\n\n### Added\n- Sixth feature (#6)."
        );

        // Seven-version jump: capped at five sections, then the releases link.
        let seven = changelog_delta(changelog, v("0.0.1"), v("0.7.0"));
        let expected_cap = format!(
            "## [0.7.0] - 2026-07-07\n\n### Added\n- Seventh feature (#7).\n\n### Fixed\n- Seventh fix.\n\n\
             ## [0.6.0] - 2026-07-06\n\n### Added\n- Sixth feature (#6).\n\n\
             ## [0.5.0] - 2026-07-05\n\n### Added\n- Fifth feature (#5).\n\n\
             ## [0.4.0] - 2026-07-04\n\n### Added\n- Fourth feature (#4).\n\n\
             ## [0.3.0] - 2026-07-03\n\n### Added\n- Third feature (#3).\n\n\
             ... and 2 more release(s): {RELEASES_BASE}"
        );
        assert_eq!(seven, expected_cap);

        // Downgrade / empty range: nothing to print.
        assert_eq!(changelog_delta(changelog, v("0.7.0"), v("0.6.0")), "");
        // The [Unreleased] section is never part of a delta.
        assert!(!changelog_delta(changelog, v("0.0.0"), v("99.0.0")).contains("Unreleased"));
    }
}
