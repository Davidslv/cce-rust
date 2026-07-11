//! # sync::git — thin, deterministic wrappers over the `git` CLI
//!
//! **Why this file exists:** SPEC-SYNC §4 makes a git repository the remote and a
//! local working clone the transport. The rest of sync needs a handful of git
//! facts (HEAD sha, branch, is-the-tree-dirty, a commit's date, the origin URL) and
//! a handful of git actions (clone, add/commit/push with fetch-rebase-retry, read a
//! file out of a ref). Centralizing them here keeps `std::process::Command` noise
//! out of the command logic and makes the git contract testable in one place.
//!
//! **What it is / does:** Invokes `git` via `std::process`, returning `Result` so
//! every failure is graceful (offline-first: a failed git call never panics). Commit
//! calls carry a fixed identity so they work in a bare CI/test environment with no
//! global git config.
//!
//! **Responsibilities:**
//! - Own the read helpers (`head_sha`, `current_branch`, `is_dirty`, `commit_date`,
//!   `origin_url`) and the porcelain filter that ignores `.cce/` churn.
//! - Own the process runners (`run`, `run_bytes`) used by `GitRemote`.
//! - It does NOT know about artifacts, content addresses, or config.

use std::path::Path;
use std::process::Command;

/// A fixed committer identity so commits succeed with no global git config.
const IDENTITY: [&str; 4] = ["-c", "user.name=cce-sync", "-c", "user.email=cce-sync@localhost"];

/// Run `git <args>` in `dir`, returning trimmed stdout on success or an error
/// carrying git's stderr.
pub fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Like [`run`] but returns raw stdout bytes (for `git show` of a file blob).
pub fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run a git command that commits, carrying the fixed identity.
pub fn run_commit(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut full: Vec<&str> = IDENTITY.to_vec();
    full.extend_from_slice(args);
    run(dir, &full)
}

/// The HEAD commit sha of the repo at `dir`, if it is a git checkout with commits.
pub fn head_sha(dir: &Path) -> Option<String> {
    run(dir, &["rev-parse", "HEAD"]).ok().filter(|s| !s.is_empty())
}

/// Resolve a commit-ish (`<sha>`, short sha, tag, `HEAD~1`, …) to its canonical
/// 40-char commit sha, or `None` if it is not a valid commit in `dir`. Uses
/// `rev-parse --verify <rev>^{commit}` so a tree/blob/tag-to-non-commit and a
/// nonexistent/garbage ref both resolve to `None` rather than a bogus sha.
pub fn resolve_commit(dir: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    run(dir, &["rev-parse", "--verify", "--quiet", &spec]).ok().filter(|s| !s.is_empty())
}

/// The current branch name (`main`/`master`/…), resolving even an unborn HEAD.
pub fn current_branch(dir: &Path) -> Option<String> {
    // `symbolic-ref --short HEAD` works before the first commit too.
    run(dir, &["symbolic-ref", "--short", "HEAD"]).ok().filter(|s| !s.is_empty())
}

/// Is the working tree dirty, ignoring CCE's own `.cce/` store churn? A repo with
/// only `.cce/` changes counts as clean, so `cce index` before `cce sync push`
/// does not spuriously block the push.
pub fn is_dirty(dir: &Path) -> bool {
    match run(dir, &["status", "--porcelain"]) {
        Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).any(|l| !status_line_is_cce(l)),
        // If git cannot report status, treat as clean is unsafe; treat as dirty so
        // we refuse rather than push a possibly-uncommitted tree.
        Err(_) => true,
    }
}

/// True when a porcelain status line refers only to the `.cce/` store directory.
fn status_line_is_cce(line: &str) -> bool {
    // Porcelain lines are `XY <path>` (or `XY <old> -> <new>` for renames). Take
    // the path portion after the 3-char status prefix.
    let path = line.get(3..).unwrap_or("").trim();
    let path = path.rsplit(" -> ").next().unwrap_or(path);
    let path = path.trim_matches('"');
    path == ".cce" || path.starts_with(".cce/")
}

/// The committer date of `sha` (RFC 3339 strict, `%cI`), deterministic per commit.
/// (Retained as a general git helper; the reconciled artifact carries no
/// provenance, so it is no longer part of the manifest.)
pub fn commit_date(dir: &Path, sha: &str) -> Option<String> {
    run(dir, &["show", "-s", "--format=%cI", sha]).ok().filter(|s| !s.is_empty())
}

/// The configured `origin` remote URL of the repo at `dir`, if any.
pub fn origin_url(dir: &Path) -> Option<String> {
    run(dir, &["config", "--get", "remote.origin.url"]).ok().filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Create a git repo in a temp dir with one committed file, return its dir.
    fn repo_with_commit() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        run_commit(d, &["init", "-q"]).unwrap();
        std::fs::write(d.join("a.txt"), "hello\n").unwrap();
        run_commit(d, &["add", "a.txt"]).unwrap();
        run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    #[test]
    fn head_sha_and_branch_and_clean_tree() {
        let tmp = repo_with_commit();
        let d = tmp.path();
        assert_eq!(head_sha(d).unwrap().len(), 40);
        let branch = current_branch(d).unwrap();
        assert!(branch == "main" || branch == "master", "branch was {branch}");
        assert!(!is_dirty(d), "a freshly committed tree is clean");
    }

    #[test]
    fn dirty_tree_is_detected_but_cce_churn_is_ignored() {
        let tmp = repo_with_commit();
        let d = tmp.path();
        // A .cce/ store does not count as dirty.
        std::fs::create_dir_all(d.join(".cce")).unwrap();
        std::fs::write(d.join(".cce/index.json"), "{}").unwrap();
        assert!(!is_dirty(d), ".cce churn must not mark the tree dirty");
        // A real source change does.
        std::fs::write(d.join("a.txt"), "changed\n").unwrap();
        assert!(is_dirty(d), "a modified tracked file marks the tree dirty");
    }

    #[test]
    fn resolve_commit_normalizes_and_rejects_garbage() {
        let tmp = repo_with_commit();
        let d = tmp.path();
        let head = head_sha(d).unwrap();
        // Full sha, "HEAD", and a short sha all resolve to the canonical 40-char sha.
        assert_eq!(resolve_commit(d, &head).unwrap(), head);
        assert_eq!(resolve_commit(d, "HEAD").unwrap(), head);
        assert_eq!(resolve_commit(d, &head[..8]).unwrap(), head);
        // Nonexistent and garbage revs resolve to None (never a bogus sha).
        assert!(resolve_commit(d, &"0".repeat(40)).is_none());
        assert!(resolve_commit(d, "not-a-sha").is_none());
    }

    #[test]
    fn commit_date_is_present() {
        let tmp = repo_with_commit();
        let d = tmp.path();
        let sha = head_sha(d).unwrap();
        let date = commit_date(d, &sha).unwrap();
        // RFC 3339-ish: starts with a 4-digit year and contains 'T'.
        assert!(date.contains('T'), "date was {date}");
    }

    #[test]
    fn origin_url_absent_without_remote() {
        let tmp = repo_with_commit();
        assert!(origin_url(tmp.path()).is_none());
    }

    #[test]
    fn run_reports_error_on_bad_command() {
        let tmp = repo_with_commit();
        let err = run(tmp.path(), &["not-a-real-subcommand"]).unwrap_err();
        assert!(err.contains("failed"), "got {err}");
    }

    #[test]
    fn head_sha_none_for_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(head_sha(tmp.path()).is_none());
        // A non-repo also reports "dirty" (fail-safe: refuse rather than push).
        assert!(is_dirty(tmp.path()));
        let _ = PathBuf::new();
    }
}
