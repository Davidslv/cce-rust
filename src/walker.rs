//! # walker — filesystem walk with ignore rules
//!
//! **Why this file exists:** Indexing must visit source files but never descend
//! into VCS metadata, dependency trees, build output, virtualenvs, or our own
//! store, and must skip binaries and oversized files (SPEC §7.1). Centralising
//! that policy keeps indexing correct and testable.
//!
//! **What it is / does:** Recursively walks a root directory, prunes ignored
//! directories and any dotdir, skips files larger than 2 MB and files that are
//! not valid UTF-8, and yields `(root-relative path with '/' separators,
//! contents)` for everything else.
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
use std::path::Path;
use walkdir::{DirEntry, WalkDir};

/// Directory names that are always pruned.
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

/// Should this directory be pruned? True for ignore-listed names and any dotdir.
fn is_ignored_dir(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if IGNORE_DIRS.contains(&name.as_ref()) {
        return true;
    }
    // Any dotdir (but not the root ".") is ignored.
    name.starts_with('.') && name.as_ref() != "." && entry.depth() > 0
}

/// Walk `root`, applying SPEC §7.1 ignore rules. Returns eligible files.
///
/// When `protect_secrets` is true (the secure default), files whose basename is
/// sensitive (SPEC-V2.1 §1) are never read and are tallied in `sensitive_skipped`
/// instead. With `--allow-secrets` the caller passes false and those files are
/// walked like any other.
pub fn walk(root: &Path, protect_secrets: bool) -> WalkResult {
    let mut files = Vec::new();
    let mut skipped = 0usize;
    let mut sensitive_skipped = 0usize;

    let walker = WalkDir::new(root).into_iter().filter_entry(|e| !is_ignored_dir(e));

    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
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
        let rel_str = rel.to_string_lossy().replace('\\', "/");
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
}
