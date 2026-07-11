//! # walker — gitignore-aware filesystem walk with ignore rules
//!
//! **Why this file exists:** Indexing must visit source files but never descend
//! into VCS metadata, dependency trees, build output, virtualenvs, or our own
//! store, and must skip binaries and oversized files (SPEC §7.1). It must ALSO
//! skip files ignored by the repository's committed `.gitignore` — otherwise a
//! developer with a gitignored-but-present file on disk (Next's `next-env.d.ts`,
//! build output, coverage, generated code) indexes a file that a clean CI
//! checkout at the same sha does not, so the two build DIFFERENT artifacts and
//! `cce sync verify` false-fails (issue #24). Centralising that policy keeps
//! indexing correct, deterministic, and testable.
//!
//! **What it is / does:** Recursively walks a root directory with ripgrep's
//! `ignore` crate, prunes ignored directories and any dotdir, honors committed
//! `.gitignore` files, skips files larger than 2 MB and files that are not valid
//! UTF-8, and yields `(root-relative path with '/' separators, contents)` for
//! everything else, in a deterministic order.
//!
//! **Builder-independence (`artifact == build(sha)` across machines).** Committed
//! `.gitignore` files are part of the tree at a sha, so honoring them is
//! machine-independent. Machine-LOCAL ignore sources are NOT part of the sha and
//! MUST be ignored, or two machines diverge again. The walker therefore honors
//! ONLY committed `.gitignore` files at/below the walk root:
//! - `git_ignore(true)`   — honor `.gitignore` files in the tree ✅
//! - `git_exclude(false)` — do NOT honor `.git/info/exclude` (machine-local)
//! - `git_global(false)`  — do NOT honor the global `core.excludesfile` (machine-local)
//! - `ignore(false)`      — do NOT honor bare `.ignore` files (non-git convention)
//! - `parents(false)`     — do NOT honor `.gitignore` files ABOVE the walk root
//! - `require_git(false)` — apply `.gitignore` even outside a `.git` checkout, so
//!   the rule set is a pure function of the tree (and hermetic tests need no `git init`)
//! - `hidden(false)`      — keep indexing dotfiles that are NOT gitignored
//!
//! `.git/` and **`.cce/`** (cce's own cache) are hard-pruned regardless of
//! gitignore state, so cce never indexes its own artifacts on any machine.
//!
//! **Responsibilities:**
//! - Own the ignore policy and the read/UTF-8/size checks.
//! - Report how many files were skipped.
//! - It does NOT chunk or embed.
//!
//! Note (dashboard, SPEC v1.1): files with a `.jsonl` extension are skipped.
//! `.jsonl` is a runtime data/log format (the metrics event log lives in
//! `metrics.jsonl`), never source to be chunked. Skipping it keeps the metrics
//! sample fixture (`test/fixture/base/metrics_sample.jsonl`) out of the conformance
//! corpus, so `conformance.json` stays byte-identical. See docs/DECISIONS.md.

use crate::config::MAX_FILE_SIZE;
use ignore::{DirEntry, WalkBuilder};
use std::path::Path;

/// Directory names that are always pruned regardless of gitignore state. `.git`
/// and `.cce` (cce's own cache) MUST be here so cce never indexes VCS metadata or
/// its own artifacts on any machine; the rest match the historical SPEC §7.1 set.
const IGNORE_DIRS: [&str; 8] =
    [".git", ".cce", "node_modules", ".venv", "venv", "__pycache__", "dist", "build"];

/// The result of walking: eligible files and a count of skipped ones.
pub struct WalkResult {
    /// `(relative_path, content)` for each indexable file.
    pub files: Vec<(String, String)>,
    /// Number of files that existed but were skipped (size / non-UTF-8 / `.jsonl`).
    pub skipped: usize,
    /// Number of files skipped by the Layer-1 sensitive-file policy (SPEC-V2.1
    /// §2) — tallied separately from `skipped` and never read.
    pub sensitive_skipped: usize,
}

/// `filter_entry` predicate: keep this entry (and, for a directory, descend into
/// it)? Prunes the ignore-listed directory names and any dotdir at depth > 0.
/// Files always pass — the per-file checks below decide their fate. The walk root
/// (depth 0) is never pruned, since its own name may legitimately start with '.'
/// (e.g. a `.tmpXXXX` tempdir).
fn keep_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
    if !is_dir {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !(IGNORE_DIRS.contains(&name.as_ref()) || name.starts_with('.'))
}

/// Walk `root`, applying SPEC §7.1 ignore rules plus committed `.gitignore`.
/// Returns eligible files in a deterministic order.
///
/// When `protect_secrets` is true (the secure default), files whose basename is
/// sensitive (SPEC-V2.1 §1) are never read and are tallied in `sensitive_skipped`
/// instead. With `--allow-secrets` the caller passes false and those files are
/// walked like any other.
pub fn walk(root: &Path, protect_secrets: bool) -> WalkResult {
    let mut files = Vec::new();
    let mut skipped = 0usize;
    let mut sensitive_skipped = 0usize;

    // Honor ONLY committed `.gitignore` files at/below the walk root; ignore every
    // machine-local source so `artifact == build(sha)` holds across machines. See
    // the module docs for the rationale behind each flag.
    let walker = WalkBuilder::new(root)
        .git_ignore(true)
        .git_exclude(false)
        .git_global(false)
        .ignore(false)
        .parents(false)
        .require_git(false)
        .hidden(false)
        .filter_entry(keep_entry)
        .build();

    for entry in walker.flatten() {
        // Directories were pruned/handled by `filter_entry`; only files are indexed.
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        // Layer 1 (SPEC-V2.1 §2): never even read a sensitive file. Tested on the
        // basename alone, before the size/read below, and counted separately.
        if protect_secrets {
            let basename = entry.file_name().to_string_lossy();
            if crate::sensitive::is_sensitive(&basename) {
                sensitive_skipped += 1;
                continue;
            }
        }
        // Skip runtime data logs (`.jsonl`); they are never source. Keeps the
        // metrics sample fixture out of the conformance corpus (SPEC v1.1).
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            skipped += 1;
            continue;
        }
        // Size check.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if meta.len() > MAX_FILE_SIZE {
            skipped += 1;
            continue;
        }
        // Read + UTF-8 check.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let content = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let rel = match path.strip_prefix(root) {
            Ok(p) => p,
            Err(_) => path,
        };
        // Normalise ONLY the platform path separator to '/'. A literal backslash
        // is a legal filename byte on Unix, so a blanket `replace('\\', "/")` would
        // rewrite a file named `a\b.py` to `a/b.py` and collide it with the nested
        // `a/b.py` (issue #105). `MAIN_SEPARATOR` is '/' on Unix (a no-op that keeps
        // backslashes) and '\' on Windows (where '\' is never a filename byte).
        let rel_str = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        files.push((rel_str, content));
    }

    // Deterministic order regardless of filesystem traversal order.
    files.sort_by(|a, b| a.0.cmp(&b.0));
    WalkResult { files, skipped, sensitive_skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.py"), "def f():\n    pass\n").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        fs::create_dir_all(root.join("node_modules/foo")).unwrap();
        fs::write(root.join("node_modules/foo/a.js"), "1").unwrap();
        fs::create_dir_all(root.join("__pycache__")).unwrap();
        fs::write(root.join("__pycache__/c.pyc"), "1").unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden/secret.py"), "1").unwrap();
        // oversized file
        fs::write(root.join("big.py"), vec![b'a'; (MAX_FILE_SIZE + 1) as usize]).unwrap();
        // non-utf8 file
        fs::write(root.join("bin.dat"), vec![0xff, 0xfe, 0x00]).unwrap();

        let res = walk(root, true);
        let paths: Vec<&str> = res.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["keep.py"]);
        // big.py + bin.dat skipped
        assert!(res.skipped >= 2);
    }

    #[test]
    fn jsonl_logs_are_skipped() {
        // The metrics event log format (`.jsonl`) is runtime data, not source,
        // and must never be chunked — this keeps the metrics fixture out of the
        // conformance corpus (SPEC v1.1).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.py"), "def f():\n    pass\n").unwrap();
        fs::write(root.join("metrics_sample.jsonl"), "{\"event\":\"search\"}\n").unwrap();

        let res = walk(root, true);
        let paths: Vec<&str> = res.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["keep.py"]);
        assert!(res.skipped >= 1);
    }

    #[test]
    fn sensitive_files_are_skipped_and_tallied_separately() {
        // SPEC-V2.1 §2 Layer 1: `.env` / `id_rsa` / `*.pem` are never read and are
        // counted in `sensitive_skipped`, distinct from the size/UTF-8 `skipped`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.py"), "def f():\n    pass\n").unwrap();
        fs::write(root.join(".env"), "SECRET=live\n").unwrap();
        // Body is never read (Layer 1 skips by basename); the marker is split so
        // this source carries no contiguous "PRIVATE KEY" literal.
        fs::write(root.join("id_rsa"), concat!("-----BEGIN OPENSSH PRIVATE ", "KEY-----\n"))
            .unwrap();
        fs::write(root.join("server.pem"), "-----BEGIN CERTIFICATE-----\n").unwrap();
        fs::write(root.join(".env.example"), "SECRET=your-secret\n").unwrap();

        let res = walk(root, true);
        let paths: Vec<&str> = res.files.iter().map(|(p, _)| p.as_str()).collect();
        // The safe template IS indexed; the three sensitive files are not.
        assert_eq!(paths, vec![".env.example", "keep.py"]);
        assert_eq!(res.sensitive_skipped, 3);
    }

    #[test]
    fn allow_secrets_disables_layer_one() {
        // With protection off, sensitive files are walked like any other file.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".env"), "SECRET=live\n").unwrap();
        fs::write(root.join("id_rsa"), "key\n").unwrap();

        let res = walk(root, false);
        let mut paths: Vec<&str> = res.files.iter().map(|(p, _)| p.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec![".env", "id_rsa"]);
        assert_eq!(res.sensitive_skipped, 0);
    }

    /// Convenience: the sorted list of indexed relative paths for `root`.
    fn walked_paths(root: &Path) -> Vec<String> {
        walk(root, true).files.into_iter().map(|(p, _)| p).collect()
    }

    #[test]
    fn committed_gitignore_excludes_a_source_file_but_keeps_the_rest() {
        // The heart of issue #24: a gitignored-but-present SOURCE file (Next's
        // generated `generated.ts`, analogous to `next-env.d.ts`) must NOT be
        // indexed, while its non-ignored neighbours must be. A nested `.gitignore`
        // in a subdir is respected too.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "generated.ts\ncoverage/\n").unwrap();
        fs::write(root.join("app.ts"), "export const x = 1;\n").unwrap();
        fs::write(root.join("generated.ts"), "// auto-generated, git-ignored\n").unwrap();
        // A gitignored directory is pruned wholesale.
        fs::create_dir_all(root.join("coverage")).unwrap();
        fs::write(root.join("coverage/lcov.ts"), "export const cov = 1;\n").unwrap();
        // Nested `.gitignore` in a subdir excludes only within that subtree.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/.gitignore"), "local.ts\n").unwrap();
        fs::write(root.join("src/main.ts"), "export const m = 1;\n").unwrap();
        fs::write(root.join("src/local.ts"), "export const l = 1;\n").unwrap();

        // `.gitignore` files themselves are non-source config and carry no code, so
        // they never surface as chunks; assert on the source set that matters.
        let paths = walked_paths(root);
        assert!(paths.contains(&"app.ts".to_string()), "non-ignored file must be indexed");
        assert!(paths.contains(&"src/main.ts".to_string()), "nested non-ignored must be indexed");
        assert!(!paths.contains(&"generated.ts".to_string()), "gitignored source must be excluded");
        assert!(!paths.iter().any(|p| p.starts_with("coverage/")), "gitignored dir must be pruned");
        assert!(!paths.contains(&"src/local.ts".to_string()), "nested `.gitignore` must apply");
    }

    #[test]
    fn builder_independence_ignored_file_present_or_absent_is_identical() {
        // Generalised repro (issue #24): a machine that has the gitignored file on
        // disk and a clean checkout that does not must produce the IDENTICAL walk
        // (same paths AND same contents) — the walk output is a pure function of
        // the sha, never of what stray ignored files happen to sit on disk.
        let make = |with_ignored: bool| -> Vec<(String, String)> {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::write(root.join(".gitignore"), "next-env.d.ts\n").unwrap();
            fs::write(root.join("app.ts"), "export const x = 1;\n").unwrap();
            if with_ignored {
                // The "dev machine after `next build`" state.
                fs::write(root.join("next-env.d.ts"), "/// <reference types=\"next\" />\n")
                    .unwrap();
            }
            walk(root, true).files
        };
        assert_eq!(make(true), make(false), "ignored file on disk must not change the walk");
    }

    #[test]
    fn machine_local_git_exclude_does_not_affect_the_walk() {
        // `.git/info/exclude` and the global `core.excludesfile` are machine-LOCAL,
        // not part of the tree at a sha. Honoring them would let two machines
        // diverge again, so the walker must IGNORE them: a file listed ONLY in
        // `.git/info/exclude` is STILL indexed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".git/info/exclude"), "machine_local.ts\n").unwrap();
        fs::write(root.join("machine_local.ts"), "export const ml = 1;\n").unwrap();
        fs::write(root.join("app.ts"), "export const x = 1;\n").unwrap();

        let paths = walked_paths(root);
        assert!(
            paths.contains(&"machine_local.ts".to_string()),
            "a file excluded only via machine-local .git/info/exclude must still be indexed"
        );
        assert!(paths.contains(&"app.ts".to_string()));
        // And `.git/` itself is never walked.
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
    }

    #[test]
    fn cce_cache_dir_is_never_walked_even_when_not_gitignored() {
        // cce's own cache holds built artifacts; indexing them would be circular and
        // machine-dependent. It is hard-pruned regardless of gitignore state.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // No `.gitignore` at all — the hard-skip must still apply.
        fs::create_dir_all(root.join(".cce")).unwrap();
        fs::write(root.join(".cce/store.ts"), "export const cache = 1;\n").unwrap();
        fs::write(root.join("app.ts"), "export const x = 1;\n").unwrap();

        let paths = walked_paths(root);
        assert_eq!(paths, vec!["app.ts".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn backslash_in_unix_filename_is_not_conflated_with_a_slash_path() {
        // Issue #105: a literal backslash is a legal filename byte on Unix. A root
        // file named `a\b.py` and a nested file `a/b.py` are TWO distinct real
        // files and must map to TWO distinct root-relative `file_path`s — the
        // separator normalisation must not rewrite the backslash and collapse them
        // onto the same path (which gives colliding, nondeterministic provenance).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join("a").join("b.py"), "nested = 1\n").unwrap();
        // A DISTINCT root file whose name literally contains a backslash.
        fs::write(root.join("a\\b.py"), "backslash = 2\n").unwrap();

        let res = walk(root, true);
        let paths: Vec<&str> = res.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(res.files.len(), 2, "two distinct files; got {paths:?}");
        assert!(paths.contains(&"a/b.py"), "nested slash path preserved; got {paths:?}");
        assert!(
            paths.contains(&"a\\b.py"),
            "literal-backslash filename must NOT be rewritten to a slash; got {paths:?}"
        );
    }

    #[test]
    fn walk_is_deterministic_across_runs() {
        // Two walks of the same tree yield identical order and results — the
        // `ignore` crate's native order is not guaranteed, so we sort.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("z")).unwrap();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("z/b.py"), "b = 1\n").unwrap();
        fs::write(root.join("a/c.py"), "c = 1\n").unwrap();
        fs::write(root.join("m.py"), "m = 1\n").unwrap();

        let first = walk(root, true).files;
        let second = walk(root, true).files;
        assert_eq!(first, second);
        let paths: Vec<&str> = first.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["a/c.py", "m.py", "z/b.py"], "sorted, deterministic order");
    }
}
