//! # sync::remote — the `SyncRemote` trait and its git backend (SPEC-SYNC §4)
//!
//! **Why this file exists:** SPEC-SYNC §4 defines a pluggable remote so S3/HTTP
//! backends stay possible without CLI changes, and picks a git repository as the
//! first, recommended backend. The content-addressed cache lives in a git repo; a
//! local working clone under `~/.cce/sync/<remote-id>/` is the transport. `put`
//! writes the artifact at its content path and pushes (fetch-rebase-retry on a ref
//! race); `get` fetches and reads it back.
//!
//! **What it is / does:** Declares `SyncRemote` and implements `GitRemote` over the
//! `git` CLI. Blobs use git-LFS for `*.cce` when enabled (a `.gitattributes` written
//! by `init`); the core path works over plain git so tests need no `git-lfs` binary.
//! Every operation is fail-graceful — a network/auth failure returns `Err`, never a
//! panic, so local work is unaffected (SPEC-SYNC §9).
//!
//! **Responsibilities:**
//! - Own `SyncRemote`, `GitRemote`, the working-clone lifecycle, and push retries.
//! - Own the LFS `.gitattributes` and the ref-pointer read/write.
//! - It does NOT build/parse artifacts (that is `artifact`) nor pick keys (that is
//!   the `sync` root's `content_address`).

use crate::sync::{git, remote_slug, sync_home};
use std::path::{Path, PathBuf};

/// The number of fetch-rebase-retry attempts on a push ref race (SPEC-SYNC §4).
const PUSH_RETRIES: usize = 5;

/// The `.gitattributes` line that routes `*.cce` blobs through git-LFS.
pub const LFS_ATTRIBUTES: &str = "*.cce filter=lfs diff=lfs merge=lfs -text\n";

/// The `.gitattributes` line that routes `*.cck` corpus blobs through git-LFS
/// (SPEC-SYNC-KNOWLEDGE §3: `*.cck` joins `*.cce` — corpora carry embeddings).
pub const KNOWLEDGE_LFS_ATTRIBUTES: &str = "*.cck filter=lfs diff=lfs merge=lfs -text\n";

/// A pluggable cache backend (SPEC-SYNC §4). The git backend is the only impl in
/// v1; the trait keeps S3/HTTP possible without CLI changes.
pub trait SyncRemote {
    /// Does an artifact exist at `key`?
    fn has(&self, key: &str) -> Result<bool, String>;
    /// Read the artifact bytes at `key` (cache miss ⇒ `Err`).
    fn get(&self, key: &str) -> Result<Vec<u8>, String>;
    /// Write `bytes` at `key` (commit + push, retrying on a ref race).
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    /// Write several `(key, bytes)` pairs in a single commit + push.
    fn put_many(&self, entries: &[(String, Vec<u8>)]) -> Result<(), String>;
    /// List the shas cached under `<prefix>` (the `<embedder>/<ver>/<repo_id>/` dir).
    fn list(&self, prefix: &str) -> Result<Vec<String>, String>;
}

/// The git-backed remote: a local working clone that mirrors the cache repo.
#[derive(Debug)]
pub struct GitRemote {
    /// The working-clone directory (`~/.cce/sync/<remote-id>/`).
    dir: PathBuf,
    /// The branch the cache lives on (resolved once at open).
    branch: String,
}

impl GitRemote {
    /// The working-clone directory for `url` (without touching the filesystem).
    pub fn clone_dir(url: &str) -> PathBuf {
        sync_home().join(remote_slug(url))
    }

    /// Open the working clone for `url`, cloning it if absent. When `lfs` is true a
    /// `.gitattributes` routing `*.cce` through LFS is written and committed (best
    /// effort — a missing `git-lfs` binary does not fail plain-git operation, but a
    /// committed LFS attribute does require `git-lfs` to smudge on `get`).
    pub fn open(url: &str, lfs: bool) -> Result<GitRemote, String> {
        let dir = Self::clone_dir(url);
        if !dir.join(".git").is_dir() {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create sync home: {e}"))?;
            }
            // Clone works even for an empty bare remote (yields an initialized repo
            // with `origin` set and an unborn default branch).
            let dir_str = dir.to_string_lossy().to_string();
            git::run_commit(Path::new("."), &["clone", "--quiet", url, &dir_str])
                .map_err(|e| format!("could not clone remote {url}: {e}"))?;
        }
        let branch =
            git::current_branch(&dir).unwrap_or_else(|| crate::sync::DEFAULT_REF.to_string());
        let remote = GitRemote { dir, branch };
        if lfs {
            remote.ensure_lfs()?;
        }
        Ok(remote)
    }

    /// The working-clone directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The cache branch.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Write and commit `.gitattributes` for LFS if it is not already present, and
    /// run `git lfs install` (best effort).
    fn ensure_lfs(&self) -> Result<(), String> {
        self.ensure_lfs_pattern("*.cce", LFS_ATTRIBUTES)
    }

    /// Route `*.cck` corpus blobs through git-LFS too (SPEC-SYNC-KNOWLEDGE §3).
    /// Called by `knowledge push` when the project has LFS enabled; `cce sync
    /// init` and every code-path write stay byte-identical (additive).
    pub fn ensure_knowledge_lfs(&self) -> Result<(), String> {
        self.ensure_lfs_pattern("*.cck", KNOWLEDGE_LFS_ATTRIBUTES)
    }

    /// Idempotently append one LFS attribute line (keyed by `pattern`) to the
    /// cache's `.gitattributes`, commit, and push.
    fn ensure_lfs_pattern(&self, pattern: &str, line: &str) -> Result<(), String> {
        let attrs = self.dir.join(".gitattributes");
        let already =
            std::fs::read_to_string(&attrs).map(|s| s.contains(pattern)).unwrap_or(false);
        if !already {
            let mut content = std::fs::read_to_string(&attrs).unwrap_or_default();
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(line);
            std::fs::write(&attrs, content)
                .map_err(|e| format!("cannot write .gitattributes: {e}"))?;
            // `git lfs install` is best-effort: absent git-lfs must not abort init.
            let _ = git::run(&self.dir, &["lfs", "install", "--local"]);
            git::run_commit(&self.dir, &["add", ".gitattributes"])?;
            // Commit may be empty if attrs already tracked; ignore that specific case.
            let msg = format!("cce sync: enable git-LFS for {pattern}");
            let _ = git::run_commit(&self.dir, &["commit", "-q", "-m", &msg]);
            self.push_with_retry()?;
        }
        Ok(())
    }

    /// Fetch the cache branch into `origin/<branch>` (best effort; a fresh empty
    /// remote has nothing to fetch).
    fn fetch(&self) -> Result<(), String> {
        // `--` guards against the branch name being read as a path; ignore the
        // "couldn't find remote ref" case an empty remote produces.
        let _ = git::run(&self.dir, &["fetch", "--quiet", "origin"]);
        Ok(())
    }

    /// Push HEAD to `origin/<branch>`, retrying with fetch+rebase on a ref race.
    fn push_with_retry(&self) -> Result<(), String> {
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        let mut last_err = String::new();
        for attempt in 0..PUSH_RETRIES {
            match git::run_commit(&self.dir, &["push", "--quiet", "origin", &refspec]) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    // Someone advanced the ref first: fetch and rebase our commit on
                    // top, then retry. Different shas never conflict in content, so
                    // the rebase is clean.
                    let _ = git::run(&self.dir, &["fetch", "--quiet", "origin"]);
                    let onto = format!("origin/{}", self.branch);
                    let _ = git::run_commit(&self.dir, &["rebase", "--quiet", &onto]);
                    if attempt + 1 == PUSH_RETRIES {
                        break;
                    }
                }
            }
        }
        Err(format!("push failed after {PUSH_RETRIES} attempts: {last_err}"))
    }

    /// The `origin/<branch>:<key>` ref-path used to read a file out of the fetched
    /// branch.
    fn ref_path(&self, key: &str) -> String {
        format!("origin/{}:{}", self.branch, key)
    }

    /// The immediate child *directories* of `prefix` on the fetched cache branch,
    /// by basename, sorted. `cce sync list` uses this to enumerate the `repo_id`
    /// directories under `hash/<SYNC_FORMAT_VERSION>/`. An unborn branch or a
    /// missing prefix lists as empty — nothing cached — mirroring `list`.
    pub fn list_dirs(&self, prefix: &str) -> Result<Vec<String>, String> {
        self.fetch()?;
        let treeish = format!("origin/{}", self.branch);
        // The trailing `/` asks ls-tree for the entries *inside* the prefix; `-d`
        // keeps only trees (a stray blob beside the repo dirs is not a repo_id).
        let arg = format!("{}/", prefix.trim_end_matches('/'));
        let listing = match git::run(&self.dir, &["ls-tree", "-d", "--name-only", &treeish, &arg]) {
            Ok(l) => l,
            Err(_) => return Ok(Vec::new()),
        };
        let mut dirs: Vec<String> = listing
            .lines()
            .filter_map(|l| l.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        dirs.sort();
        dirs.dedup();
        Ok(dirs)
    }

    /// The `.cce` artifacts under `prefix` (recursive) as `(path, bytes)` pairs.
    /// Non-artifact entries are skipped silently — the same graceful-skip rule the
    /// #37 tests pin for `list`. When a blob is a git-LFS *pointer* the pointer's
    /// recorded `size` is reported, so bytes reflect the real artifact on an
    /// LFS-enabled cache, not the ~130-byte pointer file.
    pub fn list_artifact_sizes(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        self.fetch()?;
        let treeish = format!("origin/{}", self.branch);
        // `-l` (long) adds the object size: `<mode> <type> <object> <size>\t<path>`.
        let listing = match git::run(&self.dir, &["ls-tree", "-r", "-l", &treeish, prefix]) {
            Ok(l) => l,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out: Vec<(String, u64)> = Vec::new();
        for line in listing.lines() {
            let Some((meta, path)) = line.split_once('\t') else { continue };
            if !path.ends_with(".cce") {
                continue;
            }
            let mut fields = meta.split_whitespace();
            let (_mode, kind, _object, size) =
                (fields.next(), fields.next(), fields.next(), fields.next());
            if kind != Some("blob") {
                continue;
            }
            let Some(mut bytes) = size.and_then(|s| s.parse::<u64>().ok()) else { continue };
            // A git-LFS pointer is a tiny text stanza; only bother reading small blobs.
            if bytes <= LFS_POINTER_MAX_BYTES {
                if let Some(real) = self.lfs_pointer_size(path) {
                    bytes = real;
                }
            }
            out.push((path.to_string(), bytes));
        }
        Ok(out)
    }

    /// If the blob at `key` is a git-LFS pointer, its recorded artifact `size`.
    fn lfs_pointer_size(&self, key: &str) -> Option<u64> {
        let text = git::run(&self.dir, &["cat-file", "blob", &self.ref_path(key)]).ok()?;
        if !text.starts_with("version https://git-lfs") {
            return None;
        }
        text.lines().find_map(|l| l.strip_prefix("size ")).and_then(|s| s.trim().parse().ok())
    }

    /// The full keys under `prefix` (recursive) whose basename ends with
    /// `suffix`, sorted. The knowledge walk (SPEC-SYNC-KNOWLEDGE §3/§4.5) uses
    /// this to enumerate a corpus's `<snapshot>.cck` keys; junk entries are
    /// skipped silently (the #37 graceful-skip rule). An unborn branch or a
    /// missing prefix lists as empty.
    pub fn list_keys_with_suffix(&self, prefix: &str, suffix: &str) -> Result<Vec<String>, String> {
        self.fetch()?;
        let treeish = format!("origin/{}", self.branch);
        let listing = match git::run(&self.dir, &["ls-tree", "-r", "--name-only", &treeish, prefix])
        {
            Ok(l) => l,
            Err(_) => return Ok(Vec::new()),
        };
        let mut keys: Vec<String> =
            listing.lines().filter(|l| l.ends_with(suffix)).map(str::to_string).collect();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// The unix timestamp of the commit that FIRST ADDED `key` on the cache
    /// branch (SPEC-SYNC-KNOWLEDGE §4.5): corpora have no sha ordering, so git
    /// history is the only order the cache itself carries. `None` when the key
    /// has no add commit (never existed, or an unborn branch).
    pub fn first_added_epoch(&self, key: &str) -> Option<i64> {
        let treeish = format!("origin/{}", self.branch);
        // `--diff-filter=A` keeps only the commit(s) that added the path; the
        // LAST line of the log is the earliest such commit.
        let log = git::run(
            &self.dir,
            &["log", "--format=%ct", "--diff-filter=A", &treeish, "--", key],
        )
        .ok()?;
        log.lines().last().and_then(|l| l.trim().parse().ok())
    }

    /// Remove `keys` from the cache in a single commit + push (retention pruning,
    /// SPEC-SYNC-KNOWLEDGE §4.5). The caller decides WHAT to prune; this only
    /// executes it. A no-op on an empty list.
    pub fn remove_many(&self, keys: &[String], message: &str) -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        self.fetch()?;
        let onto = format!("origin/{}", self.branch);
        let _ = git::run_commit(&self.dir, &["checkout", "-q", "-B", &self.branch, &onto]);
        let mut args: Vec<&str> = vec!["rm", "-q", "--ignore-unmatch", "--"];
        args.extend(keys.iter().map(String::as_str));
        git::run_commit(&self.dir, &args)?;
        // Every key already absent ⇒ nothing staged ⇒ nothing to prune (success).
        if git::run(&self.dir, &["diff", "--cached", "--quiet"]).is_ok() {
            return Ok(());
        }
        git::run_commit(&self.dir, &["commit", "-q", "-m", message])?;
        self.push_with_retry()
    }

    /// Read a small *non-artifact* text blob (e.g. a `refs/<ref>` latest pointer)
    /// straight out of the fetched branch — no working-tree checkout, so no LFS
    /// smudge is involved and nothing on disk or on the remote is touched.
    pub fn read_blob_text(&self, key: &str) -> Result<String, String> {
        self.fetch()?;
        git::run(&self.dir, &["cat-file", "blob", &self.ref_path(key)])
            .map(|s| s.trim().to_string())
    }
}

/// Blobs at or under this size are sniffed for a git-LFS pointer stanza (real
/// pointers are ~130 bytes; real artifacts are far larger).
const LFS_POINTER_MAX_BYTES: u64 = 512;

impl SyncRemote for GitRemote {
    fn has(&self, key: &str) -> Result<bool, String> {
        self.fetch()?;
        Ok(git::run(&self.dir, &["cat-file", "-e", &self.ref_path(key)]).is_ok())
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, String> {
        self.fetch()?;
        if git::run(&self.dir, &["cat-file", "-e", &self.ref_path(key)]).is_err() {
            return Err(format!("cache miss: {key} not found on the remote"));
        }
        // Reset the working tree to the fetched branch so LFS smudge (if any) runs,
        // then read the real bytes from disk. For plain git the blob is already the
        // artifact, so this also works with no git-lfs binary.
        let onto = format!("origin/{}", self.branch);
        git::run_commit(&self.dir, &["checkout", "-q", "-B", &self.branch, &onto])?;
        let path = self.dir.join(key);
        std::fs::read(&path).map_err(|e| format!("could not read {key} after checkout: {e}"))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_many(&[(key.to_string(), bytes.to_vec())])
    }

    fn put_many(&self, entries: &[(String, Vec<u8>)]) -> Result<(), String> {
        if entries.is_empty() {
            return Ok(());
        }
        // Start from the latest remote state so our commit descends from it.
        self.fetch()?;
        let onto = format!("origin/{}", self.branch);
        // If the branch already exists remotely, base our work on it.
        let _ = git::run_commit(&self.dir, &["checkout", "-q", "-B", &self.branch, &onto]);

        for (key, bytes) in entries {
            let path = self.dir.join(key);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {key} dir: {e}"))?;
            }
            std::fs::write(&path, bytes).map_err(|e| format!("cannot write {key}: {e}"))?;
            git::run_commit(&self.dir, &["add", "--", key])?;
        }
        let msg = if entries.len() == 1 {
            format!("cce sync: {}", entries[0].0)
        } else {
            format!("cce sync: {} artifacts", entries.len())
        };
        // Nothing-to-commit (an identical re-push) is success, not failure.
        match git::run_commit(&self.dir, &["commit", "-q", "-m", &msg]) {
            Ok(_) => {}
            Err(e) if e.contains("nothing to commit") => {}
            Err(_) => { /* fall through: still attempt push in case of prior commit */ }
        }
        self.push_with_retry()
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        self.fetch()?;
        let treeish = format!("origin/{}", self.branch);
        let listing = match git::run(&self.dir, &["ls-tree", "-r", "--name-only", &treeish, prefix])
        {
            Ok(l) => l,
            // No branch yet / empty prefix ⇒ nothing cached.
            Err(_) => return Ok(Vec::new()),
        };
        let mut shas: Vec<String> = Vec::new();
        for line in listing.lines() {
            if let Some(name) = line.rsplit('/').next() {
                if let Some(sha) = name.strip_suffix(".cce") {
                    shas.push(sha.to_string());
                }
            }
        }
        shas.sort();
        shas.dedup();
        Ok(shas)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Make a bare git repo to act as the remote, return its `file://` URL + dir.
    fn bare_remote() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        git::run_commit(tmp.path(), &["init", "--bare", "-q", "-b", "main"]).unwrap();
        let url = format!("file://{}", tmp.path().to_string_lossy());
        (tmp, url)
    }

    /// Point CCE_HOME at a temp dir so working clones never touch the real ~/.cce,
    /// while holding the process-wide env lock for the test's duration.
    #[allow(dead_code)]
    struct HomeGuard {
        home: tempfile::TempDir,
        lock: std::sync::MutexGuard<'static, ()>,
    }
    impl HomeGuard {
        fn path(&self) -> &std::path::Path {
            self.home.path()
        }
    }
    fn with_home() -> HomeGuard {
        let lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        HomeGuard { home, lock }
    }

    #[test]
    fn put_get_has_list_over_plain_git() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        assert_eq!(remote.branch(), "main");

        let key = "hash/2.3/example.com__acme__demo/abc123.cce";
        assert!(!remote.has(key).unwrap());
        remote.put(key, b"artifact-bytes\n").unwrap();
        assert!(remote.has(key).unwrap());
        assert_eq!(remote.get(key).unwrap(), b"artifact-bytes\n");

        let shas = remote.list("hash/2.3/example.com__acme__demo").unwrap();
        assert_eq!(shas, vec!["abc123".to_string()]);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn get_reports_cache_miss() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let err = remote.get("hash/2.3/x/nope.cce").unwrap_err();
        assert!(err.contains("cache miss"), "got {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn second_open_reuses_the_existing_clone() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let r1 = GitRemote::open(&url, false).unwrap();
        r1.put("hash/2.3/x/a.cce", b"one\n").unwrap();
        // Re-open: same dir, existing clone; the data is still visible.
        let r2 = GitRemote::open(&url, false).unwrap();
        assert_eq!(r2.dir(), r1.dir());
        assert!(r2.has("hash/2.3/x/a.cce").unwrap());
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_race_retry_lands_both_shas() {
        // Two independent clones of one remote push different shas; the second push
        // races (its ref is stale) and must fetch-rebase-retry, ending with both.
        let _home = with_home();
        let (_bare, url) = bare_remote();

        let a = GitRemote::open(&url, false).unwrap();
        a.put("hash/2.3/x/sha_a.cce", b"A\n").unwrap();

        // A second working clone in a different home dir, same remote.
        let home_b = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home_b.path());
        let b = GitRemote::open(&url, false).unwrap();
        // b has not yet fetched A's commit; putting a new sha forces the retry path.
        b.put("hash/2.3/x/sha_b.cce", b"B\n").unwrap();

        // A re-opened view sees both shas.
        std::env::set_var("CCE_HOME", _home.path());
        let checker = GitRemote::open(&url, false).unwrap();
        let mut shas = checker.list("hash/2.3/x").unwrap();
        shas.sort();
        assert_eq!(shas, vec!["sha_a".to_string(), "sha_b".to_string()]);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn list_skips_non_artifact_listing_entries_gracefully() {
        // Issue #37: a cache repo can accumulate entries that are not artifacts
        // (a README, a file with no extension) beside the `<sha>.cce` blobs, so
        // the real `ls-tree` listing contains lines the parser must treat as
        // malformed. Pinned behavior: those lines are skipped silently — the
        // listing still succeeds and returns every real artifact (graceful
        // skip, not an error). Also pinned: a `.cce` blob in a nested
        // subdirectory is listed by its basename (`ls-tree -r` recurses).
        // Unit-level on purpose: no CLI command calls `SyncRemote::list` today,
        // so the binary cannot reach this parser.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("hash/2.3/x/abc123.cce".to_string(), b"A\n".to_vec()),
                ("hash/2.3/x/README.md".to_string(), b"not an artifact\n".to_vec()),
                ("hash/2.3/x/no-extension".to_string(), b"junk\n".to_vec()),
                ("hash/2.3/x/nested/deadbeef.cce".to_string(), b"B\n".to_vec()),
            ])
            .unwrap();
        let shas = remote.list("hash/2.3/x").unwrap();
        assert_eq!(shas, vec!["abc123".to_string(), "deadbeef".to_string()]);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn list_dirs_enumerates_repo_id_directories_sorted() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("hash/2.3/zzz__last/a.cce".to_string(), b"Z\n".to_vec()),
                ("hash/2.3/aaa__first/b.cce".to_string(), b"A\n".to_vec()),
                // A stray blob beside the repo dirs is not a repo_id.
                ("hash/2.3/README.md".to_string(), b"junk\n".to_vec()),
            ])
            .unwrap();
        let dirs = remote.list_dirs("hash/2.3").unwrap();
        assert_eq!(dirs, vec!["aaa__first".to_string(), "zzz__last".to_string()]);
        // An absent prefix (or unborn branch) lists as empty, not an error.
        assert!(remote.list_dirs("hash/9.9").unwrap().is_empty());
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn list_artifact_sizes_skips_junk_and_reports_bytes() {
        // The #37 fixture, extended with sizes: only `.cce` blobs are counted, by
        // their byte size; junk entries are skipped silently.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("hash/2.3/x/abc123.cce".to_string(), b"A\n".to_vec()),
                ("hash/2.3/x/README.md".to_string(), b"not an artifact\n".to_vec()),
                ("hash/2.3/x/no-extension".to_string(), b"junk\n".to_vec()),
                ("hash/2.3/x/nested/deadbeef.cce".to_string(), b"BBBB\n".to_vec()),
            ])
            .unwrap();
        let mut sizes = remote.list_artifact_sizes("hash/2.3/x").unwrap();
        sizes.sort();
        assert_eq!(
            sizes,
            vec![
                ("hash/2.3/x/abc123.cce".to_string(), 2),
                ("hash/2.3/x/nested/deadbeef.cce".to_string(), 5),
            ]
        );
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn list_artifact_sizes_reports_the_lfs_pointer_recorded_size() {
        // A `.cce` blob that is a git-LFS *pointer* must report the pointer's
        // recorded artifact size, not the ~130-byte pointer file size. Hermetic:
        // the pointer stanza is plain text, so no git-lfs binary is needed.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let pointer = b"version https://git-lfs.github.com/spec/v1\n\
                        oid sha256:0000000000000000000000000000000000000000000000000000000000000000\n\
                        size 123456\n"
            .to_vec();
        remote.put("hash/2.3/x/abc123.cce", &pointer).unwrap();
        let sizes = remote.list_artifact_sizes("hash/2.3/x").unwrap();
        assert_eq!(sizes, vec![("hash/2.3/x/abc123.cce".to_string(), 123456)]);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn read_blob_text_reads_a_pointer_without_checkout() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put("hash/2.3/x/refs/main", b"abc123\n").unwrap();
        assert_eq!(remote.read_blob_text("hash/2.3/x/refs/main").unwrap(), "abc123");
        assert!(remote.read_blob_text("hash/2.3/x/refs/nope").is_err());
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn first_added_epoch_orders_keys_by_cache_history() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put("knowledge/v1/c/aaa.cck", b"A\n").unwrap();
        remote.put("knowledge/v1/c/bbb.cck", b"B\n").unwrap();
        let a = remote.first_added_epoch("knowledge/v1/c/aaa.cck").unwrap();
        let b = remote.first_added_epoch("knowledge/v1/c/bbb.cck").unwrap();
        assert!(a <= b, "the earlier push is not later in history");
        assert_eq!(remote.first_added_epoch("knowledge/v1/c/nope.cck"), None);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn remove_many_prunes_keys_in_one_commit() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("knowledge/v1/c/aaa.cck".to_string(), b"A\n".to_vec()),
                ("knowledge/v1/c/bbb.cck".to_string(), b"B\n".to_vec()),
                ("knowledge/v1/c/current".to_string(), b"bbb\n".to_vec()),
            ])
            .unwrap();
        remote.remove_many(&["knowledge/v1/c/aaa.cck".to_string()], "prune").unwrap();
        assert!(!remote.has("knowledge/v1/c/aaa.cck").unwrap());
        assert!(remote.has("knowledge/v1/c/bbb.cck").unwrap());
        assert!(remote.has("knowledge/v1/c/current").unwrap());
        // Empty list and an already-absent key are both no-ops, not errors.
        remote.remove_many(&[], "noop").unwrap();
        remote.remove_many(&["knowledge/v1/c/aaa.cck".to_string()], "again").unwrap();
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn open_fails_on_unreachable_remote() {
        let _home = with_home();
        // A file:// URL to a path that is not a repo fails to clone.
        let err = GitRemote::open("file:///definitely/not/a/repo/here.git", false).unwrap_err();
        assert!(err.contains("could not clone"), "got {err}");
        std::env::remove_var("CCE_HOME");
    }
}
