//! # sync::remote — the `SyncRemote` trait and its git backend (SPEC-SYNC §4)
//!
//! **Why this file exists:** SPEC-SYNC §4 defines a pluggable remote so S3/HTTP
//! backends stay possible without CLI changes, and picks a git repository as the
//! first, recommended backend. The content-addressed cache lives in a git repo; a
//! local working clone under `~/.cce/sync/<remote-id>/` is the transport. `put`
//! writes the artifact at its content path and pushes (a lost ref race is retried
//! by RE-APPLYING the write on the fresh remote state — fixed-path pointer keys
//! make racing commits genuinely conflict, so a rebase would wedge; issue #92);
//! `get` fetches and reads it back.
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

/// The number of fetch-and-re-apply retry attempts on a push ref race
/// (SPEC-SYNC §4).
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
    /// The full keys under `prefix` (recursive) whose basename ends with
    /// `suffix`, sorted. The knowledge walk (SPEC-SYNC-KNOWLEDGE §3/§4.5) uses
    /// it with `.cck`; the #72 ref fallback uses it with `""` to enumerate a
    /// repo's `refs/*` pointers in ONE listing call (never N pointer reads).
    fn list_keys_with_suffix(&self, prefix: &str, suffix: &str) -> Result<Vec<String>, String>;
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
        } else {
            // Reusing an existing clone: self-heal one left mid-rebase by an
            // interrupted run or the pre-#92 retry path, BEFORE resolving the
            // branch (a rebase detaches HEAD, so `symbolic-ref` would lie).
            Self::heal_interrupted_rebase(&dir)?;
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

    /// Recover a working clone left with a rebase in progress (issue #92: the
    /// old retry path could conflict mid-rebase and leave the clone poisoned —
    /// every later push silently no-opped until the clone was deleted by
    /// hand). Abort the rebase and restore a clean tree; every operation then
    /// re-bases itself on `origin/<branch>` before writing, so nothing of the
    /// half-applied state can leak into a future commit.
    fn heal_interrupted_rebase(dir: &Path) -> Result<(), String> {
        let git_dir = dir.join(".git");
        if !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists() {
            return Ok(());
        }
        // `--abort` restores the pre-rebase HEAD and tree; fall back to
        // `--quit` (clears the rebase state, keeps HEAD) if abort cannot run.
        if git::run(dir, &["rebase", "--abort"]).is_err() {
            git::run(dir, &["rebase", "--quit"]).map_err(|e| {
                format!(
                    "the sync clone at {} was left mid-rebase and could not recover \
                     (delete the directory to reset it): {e}",
                    dir.display()
                )
            })?;
            // `--quit` leaves HEAD wherever the rebase parked it — usually
            // DETACHED. Re-attach it to the real cache branch, or `open()`
            // would resolve no branch and fall back to the default name,
            // forking a cache that lives on any other branch.
            Self::reattach_head(dir)?;
        }
        // Drop whatever the aborted rebase left half-applied in the tree.
        git::run(dir, &["reset", "--hard", "--quiet"])
            .map_err(|e| format!("could not restore a clean sync clone at {}: {e}", dir.display()))
            .map(|_| ())
    }

    /// Re-attach a detached HEAD to the cache branch (resolved from the
    /// remote's recorded default, falling back to the sole local branch, then
    /// to [`crate::sync::DEFAULT_REF`]), resetting the branch onto
    /// `origin/<branch>` when that ref exists.
    fn reattach_head(dir: &Path) -> Result<(), String> {
        if git::current_branch(dir).is_some() {
            return Ok(());
        }
        let branch = Self::default_remote_branch(dir);
        let onto = format!("origin/{branch}");
        let mut args = vec!["checkout", "--quiet", "--force", "-B", &branch];
        if git::run(dir, &["rev-parse", "--verify", "--quiet", &onto]).is_ok() {
            args.push(&onto);
        } // else: no fetched remote branch to reset onto — attach at HEAD.
        git::run(dir, &args)
            .map(|_| ())
            .map_err(|e| format!("could not re-attach the sync clone at {}: {e}", dir.display()))
    }

    /// The cache branch a clone with a detached HEAD should re-attach to: the
    /// remote's recorded default branch (`refs/remotes/origin/HEAD`), else the
    /// clone's sole local branch, else [`crate::sync::DEFAULT_REF`].
    fn default_remote_branch(dir: &Path) -> String {
        if let Ok(sym) = git::run(dir, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
            if let Some(name) = sym.trim().strip_prefix("origin/") {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
        if let Ok(list) =
            git::run(dir, &["for-each-ref", "--format=%(refname:short)", "refs/heads/"])
        {
            let mut branches = list.lines().map(str::trim).filter(|l| !l.is_empty());
            if let (Some(only), None) = (branches.next(), branches.next()) {
                return only.to_string();
            }
        }
        crate::sync::DEFAULT_REF.to_string()
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
        let already = std::fs::read_to_string(&attrs).map(|s| s.contains(pattern)).unwrap_or(false);
        if !already {
            // `git lfs install` is best-effort: absent git-lfs must not abort init.
            let _ = git::run(&self.dir, &["lfs", "install", "--local"]);
            let msg = format!("cce sync: enable git-LFS for {pattern}");
            // Idempotent (re-)apply: a push race resets the clone onto the new
            // origin state, so the retry must re-check and re-append there.
            let apply = || -> Result<(), String> {
                let mut content = std::fs::read_to_string(&attrs).unwrap_or_default();
                if !content.contains(pattern) {
                    if !content.is_empty() && !content.ends_with('\n') {
                        content.push('\n');
                    }
                    content.push_str(line);
                    std::fs::write(&attrs, content)
                        .map_err(|e| format!("cannot write .gitattributes: {e}"))?;
                }
                git::run_commit(&self.dir, &["add", ".gitattributes"])?;
                // Attrs already tracked and identical stages nothing: fine.
                self.commit_staged(&msg)
            };
            apply()?;
            self.push_with_retry(&apply)?;
        }
        Ok(())
    }

    /// Fetch the cache branch into `origin/<branch>`. Best effort by design: a
    /// fresh empty remote has nothing to fetch, and an offline fetch simply
    /// leaves `origin/<branch>` at its last-known state — reads then see stale
    /// data and pushes surface the failure loudly at push time (SPEC-SYNC §9).
    fn fetch(&self) -> Result<(), String> {
        let _ = git::run(&self.dir, &["fetch", "--quiet", "origin"]);
        Ok(())
    }

    /// Does `origin/<branch>` exist locally (i.e. has the remote branch been
    /// born and fetched)? A fresh empty remote has not.
    fn origin_branch_exists(&self) -> bool {
        let onto = format!("origin/{}", self.branch);
        git::run(&self.dir, &["rev-parse", "--verify", "--quiet", &onto]).is_ok()
    }

    /// Force the working clone onto `origin/<branch>` (discarding any local
    /// commits or tree state), so the next commit descends from the latest
    /// fetched remote state. A no-op when the remote branch is unborn — the
    /// first commit will create it.
    fn checkout_branch_at_origin(&self) -> Result<(), String> {
        if !self.origin_branch_exists() {
            return Ok(());
        }
        let onto = format!("origin/{}", self.branch);
        git::run(&self.dir, &["checkout", "--quiet", "--force", "-B", &self.branch, &onto])
            .map(|_| ())
            .map_err(|e| format!("could not reset the sync clone onto {onto}: {e}"))
    }

    /// Commit whatever is staged with `msg`. Nothing staged is success, not
    /// failure — it means HEAD (the state about to be pushed) already carries
    /// the change: an identical re-push, or a race the other writer resolved
    /// to the same bytes. Detected via `diff --cached`, never by parsing
    /// git's output; any REAL commit failure propagates (issue #92 — the old
    /// swallow here turned commit failures into silent publish no-ops).
    fn commit_staged(&self, msg: &str) -> Result<(), String> {
        if git::run(&self.dir, &["diff", "--cached", "--quiet"]).is_ok() {
            return Ok(());
        }
        git::run_commit(&self.dir, &["commit", "-q", "-m", msg]).map(|_| ())
    }

    /// Push HEAD to `origin/<branch>`, retrying on a ref race by REBUILDING the
    /// change on top of the freshly fetched remote state.
    ///
    /// Why re-apply instead of rebase (issue #92): fixed-path keys — the
    /// `refs/<ref>` and knowledge `current` pointers, `corpus.json`, the
    /// workspace metadata — are rewritten by every push, so two racing pushes
    /// genuinely conflict in content and a rebase stops mid-way. Cache keys
    /// are whole-file last-writer-wins, and the SPEC-SYNC-KNOWLEDGE §5 push
    /// guard is read-then-publish, not a transaction — so the documented race
    /// semantic is exactly: reset onto the new `origin/<branch>`, `reapply`
    /// the change (re-write, re-stage, re-commit), push again. `reapply` must
    /// leave HEAD carrying the change (tolerating nothing-to-commit when the
    /// new origin state already has it). Exhausted retries return a real
    /// `Err` — never `Ok` without our change published.
    fn push_with_retry(&self, reapply: &dyn Fn() -> Result<(), String>) -> Result<(), String> {
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        let mut last_err = String::new();
        for _ in 0..PUSH_RETRIES {
            match git::run_commit(&self.dir, &["push", "--quiet", "origin", &refspec]) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last_err = e;
                    self.fetch()?;
                    // Only re-apply when there is a remote state to rebuild on;
                    // a push refused with the remote branch still unborn is not
                    // a ref race (network/auth/hook) — just retry.
                    if self.origin_branch_exists() {
                        self.checkout_branch_at_origin()?;
                        reapply()?;
                    }
                }
            }
        }
        Err(format!(
            "push to origin/{} failed after {PUSH_RETRIES} attempts (lost a push race {PUSH_RETRIES} \
             times or the remote kept refusing): {last_err}",
            self.branch
        ))
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
        self.list_sizes_with_suffix(prefix, ".cce")
    }

    /// The same LFS-aware `(path, bytes)` walk for an arbitrary artifact suffix.
    /// The knowledge listing (SPEC-SYNC-KNOWLEDGE §6) uses it with `.cck`, so a
    /// corpus's bytes reflect the real artifact on an LFS-enabled cache too.
    pub fn list_sizes_with_suffix(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> Result<Vec<(String, u64)>, String> {
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
            if !path.ends_with(suffix) {
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

    /// The keys under `prefix` in FIRST-ADDED commit order, oldest first
    /// (SPEC-SYNC-KNOWLEDGE §4.5): corpora have no sha ordering, so the cache
    /// repo's commit history is the only order the cache itself carries. Commit
    /// ORDER, not commit timestamps — two pushes in the same second still have a
    /// well-defined ancestry. An unborn branch or a missing prefix is empty.
    pub fn first_added_order(&self, prefix: &str) -> Result<Vec<String>, String> {
        self.fetch()?;
        let treeish = format!("origin/{}", self.branch);
        // `--reverse` walks oldest→newest; `--diff-filter=A --name-only` prints
        // the paths each commit ADDED under the pathspec (the `--format=` keeps
        // commit headers out of the listing).
        let listing = match git::run(
            &self.dir,
            &[
                "log",
                "--reverse",
                "--format=",
                "--name-only",
                "--diff-filter=A",
                &treeish,
                "--",
                prefix,
            ],
        ) {
            Ok(l) => l,
            Err(_) => return Ok(Vec::new()),
        };
        let mut keys: Vec<String> = Vec::new();
        for line in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !keys.iter().any(|k| k == line) {
                keys.push(line.to_string());
            }
        }
        Ok(keys)
    }

    /// Remove `keys` from the cache in a single commit + push (retention pruning,
    /// SPEC-SYNC-KNOWLEDGE §4.5). The caller decides WHAT to prune; this only
    /// executes it. A no-op on an empty list.
    pub fn remove_many(&self, keys: &[String], message: &str) -> Result<(), String> {
        if keys.is_empty() {
            return Ok(());
        }
        // Start from the latest remote state so the prune descends from it.
        self.fetch()?;
        self.checkout_branch_at_origin()?;
        // Re-appliable on a push race: `--ignore-unmatch` makes re-running the
        // removal against the new origin state a no-op for keys another writer
        // already pruned (nothing staged ⇒ nothing to prune ⇒ success).
        let apply = || -> Result<(), String> {
            let mut args: Vec<&str> = vec!["rm", "-q", "--ignore-unmatch", "--"];
            args.extend(keys.iter().map(String::as_str));
            git::run_commit(&self.dir, &args)?;
            self.commit_staged(message)
        };
        apply()?;
        self.push_with_retry(&apply)
    }

    /// Read a small *non-artifact* text blob (e.g. a `refs/<ref>` latest pointer)
    /// straight out of the fetched branch — no working-tree checkout, so no LFS
    /// smudge is involved and nothing on disk or on the remote is touched.
    pub fn read_blob_text(&self, key: &str) -> Result<String, String> {
        self.fetch()?;
        git::run(&self.dir, &["cat-file", "blob", &self.ref_path(key)])
            .map(|s| s.trim().to_string())
    }

    /// Post-push verification (issue #92): confirm the publish actually
    /// happened, converting any residual silent failure into a loud error.
    ///
    /// Fast path: read every just-written key back from the TIP of
    /// `origin/<branch>`. The preceding fetch is best-effort: when it fails,
    /// the comparison runs against the remote-tracking ref our own successful
    /// push just updated — still safe, but a real remote round-trip only when
    /// the fetch works.
    ///
    /// Supersede path: a competitor may legitimately advance `origin/<branch>`
    /// past our commit between our push and this check — high contention is
    /// exactly this code's territory, and being superseded is NOT a lost
    /// publish. When the tip no longer carries an entry, the publish is
    /// verified iff BOTH (a) `pushed_sha` — the commit our push landed —
    /// itself carries the entry (guarding against any future commit-swallow
    /// regression, where ancestry alone would be vacuously true because HEAD
    /// never left origin's own commit), AND (b) `pushed_sha` is an ancestor of
    /// `origin/<branch>`, i.e. our commit really landed on the remote branch
    /// and whatever replaced our bytes built on top of them.
    fn verify_published(
        &self,
        entries: &[(String, Vec<u8>)],
        pushed_sha: &str,
    ) -> Result<(), String> {
        self.fetch()?;
        let tip = format!("origin/{}", self.branch);
        let next_step = format!(
            "fetch and inspect origin/{}; local stores are unaffected; re-run the push to retry",
            self.branch
        );
        let mut ancestry_verified = false;
        for (key, bytes) in entries {
            if self.commit_carries(&tip, key, bytes) {
                continue;
            }
            // The tip moved past us: superseded (fine) or lost (loud).
            if !self.commit_carries(pushed_sha, key, bytes) {
                return Err(format!(
                    "push verification failed: neither origin/{} nor the pushed commit \
                     {pushed_sha} carries {key} with the just-pushed content — {next_step}",
                    self.branch
                ));
            }
            if !ancestry_verified {
                if git::run(&self.dir, &["merge-base", "--is-ancestor", pushed_sha, &tip]).is_err()
                {
                    return Err(format!(
                        "push verification failed: the pushed commit {pushed_sha} carrying \
                         {key} never landed on origin/{} — nothing was published; {next_step}",
                        self.branch
                    ));
                }
                ancestry_verified = true;
            }
        }
        Ok(())
    }

    /// Does `treeish` carry `key`? Artifact keys (`.cce`/`.cck`) may be git-LFS
    /// *pointers* on the remote, so they are checked for existence; every other
    /// key — exactly the fixed-path pointer files issue #92 lost — is compared
    /// byte-for-byte against `bytes`.
    fn commit_carries(&self, treeish: &str, key: &str, bytes: &[u8]) -> bool {
        let spec = format!("{treeish}:{key}");
        if key.ends_with(".cce") || key.ends_with(".cck") {
            return git::run(&self.dir, &["cat-file", "-e", &spec]).is_ok();
        }
        git::run_bytes(&self.dir, &["cat-file", "blob", &spec])
            .map(|got| got == bytes)
            .unwrap_or(false)
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
        self.checkout_branch_at_origin()?;
        let msg = if entries.len() == 1 {
            format!("cce sync: {}", entries[0].0)
        } else {
            format!("cce sync: {} artifacts", entries.len())
        };
        // Write + stage + commit the entries, as a closure so a lost push race
        // can re-run it on top of the freshly fetched origin state (issue #92:
        // fixed-path pointer keys make racing commits genuinely conflict, so
        // the retry rebuilds the whole-file last-writer-wins result instead of
        // rebasing into a conflict).
        let apply = || -> Result<(), String> {
            for (key, bytes) in entries {
                let path = self.dir.join(key);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create {key} dir: {e}"))?;
                }
                std::fs::write(&path, bytes).map_err(|e| format!("cannot write {key}: {e}"))?;
                git::run_commit(&self.dir, &["add", "--", key])?;
            }
            // Nothing staged (an identical re-push) is success, not failure.
            self.commit_staged(&msg)
        };
        apply()?;
        self.push_with_retry(&apply)?;
        // The exact commit the successful push landed (HEAD after the final
        // apply): verification must never confuse "superseded by a competitor
        // right after our push" with "never published".
        let pushed_sha = git::head_sha(&self.dir).ok_or_else(|| {
            "push verification failed: cannot resolve the just-pushed commit".to_string()
        })?;
        // Belt and braces (#92): a push that "succeeded" without publishing our
        // content must error loudly, never report success.
        self.verify_published(entries, &pushed_sha)
    }

    /// Keys ending in `suffix` under `prefix` (moved verbatim from the former
    /// inherent method — SPEC-SYNC-KNOWLEDGE §3/§4.5 corpus walk, #72 ref walk).
    /// Junk entries are skipped silently (the #37 graceful-skip rule); an unborn
    /// branch or a missing prefix lists as empty.
    fn list_keys_with_suffix(&self, prefix: &str, suffix: &str) -> Result<Vec<String>, String> {
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
    fn first_added_order_walks_cache_history_oldest_first() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        // Deliberately push in NON-lexicographic order, back-to-back (same
        // second): commit order must win, never timestamps or key names.
        remote.put("knowledge/v1/c/zzz.cck", b"Z\n").unwrap();
        remote.put("knowledge/v1/c/aaa.cck", b"A\n").unwrap();
        remote.put("knowledge/v1/c/mmm.cck", b"M\n").unwrap();
        let order = remote.first_added_order("knowledge/v1/c").unwrap();
        assert_eq!(
            order,
            vec![
                "knowledge/v1/c/zzz.cck".to_string(),
                "knowledge/v1/c/aaa.cck".to_string(),
                "knowledge/v1/c/mmm.cck".to_string(),
            ]
        );
        // A missing prefix is empty, not an error.
        assert!(remote.first_added_order("knowledge/v1/nope").unwrap().is_empty());
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

    // ---- issue #92: the conflicted push race -------------------------------
    //
    // The race is simulated deterministically with a pre-receive hook on the
    // bare remote: the FIRST push triggers a conflicting out-of-band push from
    // a separate racer clone and is rejected — exactly a lost ref race, landed
    // mid-flight between our fetch and our push. Later pushes pass.

    /// A separate clone of `url` with `key` = `content` committed but NOT
    /// pushed — the conflicting commit the race hook publishes mid-flight.
    #[cfg(unix)]
    fn racer_clone(url: &str, key: &str, content: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("racer");
        let dir_str = dir.to_string_lossy().to_string();
        git::run_commit(Path::new("."), &["clone", "--quiet", url, &dir_str]).unwrap();
        let path = dir.join(key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        git::run_commit(&dir, &["add", "--", key]).unwrap();
        git::run_commit(&dir, &["commit", "-q", "-m", "racer"]).unwrap();
        (tmp, dir)
    }

    /// Install a pre-receive hook on the bare remote: the first push publishes
    /// `racer`'s HEAD (a mid-flight ref race) and is rejected; later pushes
    /// pass. With `racer` = None every push is rejected (retries exhausted).
    #[cfg(unix)]
    fn arm_race_hook(bare: &Path, racer: Option<&Path>) {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var("PATH").unwrap_or_default();
        let script = match racer {
            Some(r) => format!(
                "#!/bin/sh\nif [ -f \"$GIT_DIR/race-done\" ]; then exit 0; fi\n\
                 touch \"$GIT_DIR/race-done\"\n\
                 env -i PATH=\"{path}\" git -C \"{}\" push --quiet origin \
                 HEAD:refs/heads/main >&2\n\
                 echo 'simulated ref race' >&2\nexit 1\n",
                r.display()
            ),
            None => "#!/bin/sh\necho 'rejected by test hook' >&2\nexit 1\n".to_string(),
        };
        let hook = bare.join("hooks").join("pre-receive");
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Is the working clone free of any in-progress rebase?
    fn no_rebase_in_progress(remote: &GitRemote) -> bool {
        let git_dir = remote.dir().join(".git");
        !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists()
    }

    #[cfg(unix)]
    #[test]
    fn conflicting_fixed_path_race_republishes_last_writer_wins() {
        // Issue #92: two pushes rewriting the SAME fixed-path key (a knowledge
        // `current` pointer) genuinely conflict. The old rebase-based retry
        // swallowed the conflict, reported success while publishing nothing,
        // and left the clone mid-rebase. Pinned now: the lost race re-applies
        // our write on the new origin state (last-writer-wins) and the remote
        // really carries our bytes afterward.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let current = "knowledge/v1/corpus/current";
        let meta = "knowledge/v1/corpus/corpus.json";
        remote
            .put_many(&[
                (current.to_string(), b"aaaa1111\n".to_vec()),
                (meta.to_string(), b"{\"current\":\"aaaa1111\"}\n".to_vec()),
            ])
            .unwrap();

        let (_racer_tmp, racer) = racer_clone(&url, current, "raced9999\n");
        arm_race_hook(bare.path(), Some(&racer));
        remote
            .put_many(&[
                (current.to_string(), b"bbbb2222\n".to_vec()),
                (meta.to_string(), b"{\"current\":\"bbbb2222\"}\n".to_vec()),
            ])
            .unwrap();

        // The remote carries OUR bytes — success was not a lie.
        let published = git::run(bare.path(), &["show", &format!("main:{current}")]).unwrap();
        assert_eq!(published, "bbbb2222");
        // The clone is clean (no rebase in progress) and still usable.
        assert!(no_rebase_in_progress(&remote), "clone left mid-rebase");
        remote.put(current, b"cccc3333\n").unwrap();
        let published = git::run(bare.path(), &["show", &format!("main:{current}")]).unwrap();
        assert_eq!(published, "cccc3333");
        std::env::remove_var("CCE_HOME");
    }

    #[cfg(unix)]
    #[test]
    fn raced_push_with_different_artifact_keys_lands_both() {
        // The NON-conflicting race (two distinct content-addressed keys) must
        // still resolve automatically: the racer's artifact survives and ours
        // lands beside it.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put("hash/2.3/x/sha_a.cce", b"A\n").unwrap();

        let (_racer_tmp, racer) = racer_clone(&url, "hash/2.3/x/sha_r.cce", "R\n");
        arm_race_hook(bare.path(), Some(&racer));
        remote.put("hash/2.3/x/sha_b.cce", b"B\n").unwrap();

        let mut shas = remote.list("hash/2.3/x").unwrap();
        shas.sort();
        assert_eq!(shas, vec!["sha_a".to_string(), "sha_b".to_string(), "sha_r".to_string()]);
        assert!(no_rebase_in_progress(&remote), "clone left mid-rebase");
        std::env::remove_var("CCE_HOME");
    }

    #[cfg(unix)]
    #[test]
    fn exhausted_push_retries_return_err_and_leave_a_clean_clone() {
        // A publish that cannot land must be a REAL error — never Ok with
        // nothing published — and must not poison the clone for later pushes.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put("knowledge/v1/corpus/current", b"aaaa1111\n").unwrap();

        arm_race_hook(bare.path(), None);
        let err = remote.put("knowledge/v1/corpus/current", b"bbbb2222\n").unwrap_err();
        assert!(err.contains("push"), "got {err}");
        assert!(no_rebase_in_progress(&remote), "clone left mid-rebase");
        // The remote kept the pre-race state, and the clone recovers as soon
        // as the remote accepts pushes again.
        let kept = git::run(bare.path(), &["show", "main:knowledge/v1/corpus/current"]).unwrap();
        assert_eq!(kept, "aaaa1111");
        std::fs::remove_file(bare.path().join("hooks").join("pre-receive")).unwrap();
        remote.put("knowledge/v1/corpus/current", b"cccc3333\n").unwrap();
        let now = git::run(bare.path(), &["show", "main:knowledge/v1/corpus/current"]).unwrap();
        assert_eq!(now, "cccc3333");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn poisoned_clone_self_heals_on_open() {
        // A clone left mid-rebase by the pre-fix retry path (issue #92) must
        // recover transparently on the next open — no manual deletion.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let key = "knowledge/v1/corpus/current";
        remote.put(key, b"aaaa1111\n").unwrap();

        // Poison the clone exactly as the old code did: a local commit to the
        // fixed path, a conflicting commit on the remote, then a rebase that
        // stops on the conflict and is abandoned.
        let dir = remote.dir().to_path_buf();
        std::fs::write(dir.join(key), b"local5555\n").unwrap();
        git::run_commit(&dir, &["add", "--", key]).unwrap();
        git::run_commit(&dir, &["commit", "-q", "-m", "doomed local commit"]).unwrap();
        let other_home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", other_home.path());
        let other = GitRemote::open(&url, false).unwrap();
        other.put(key, b"remote7777\n").unwrap();
        std::env::set_var("CCE_HOME", _home.path());
        git::run(&dir, &["fetch", "--quiet", "origin"]).unwrap();
        assert!(git::run_commit(&dir, &["rebase", "--quiet", "origin/main"]).is_err());
        assert!(
            dir.join(".git").join("rebase-merge").exists()
                || dir.join(".git").join("rebase-apply").exists(),
            "test setup failed to leave a rebase in progress"
        );

        // Re-open heals the clone, and the next push publishes for real.
        let healed = GitRemote::open(&url, false).unwrap();
        assert!(no_rebase_in_progress(&healed), "open did not heal the mid-rebase clone");
        healed.put(key, b"bbbb2222\n").unwrap();
        let published = git::run(bare.path(), &["show", &format!("main:{key}")]).unwrap();
        assert_eq!(published, "bbbb2222");
        std::env::remove_var("CCE_HOME");
    }

    /// Install an arbitrary post-receive hook on the bare remote.
    #[cfg(unix)]
    fn arm_post_receive_hook(bare: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let hook = bare.join("hooks").join("post-receive");
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn superseding_competitor_push_after_ours_is_not_a_lost_publish() {
        // A competitor can legitimately advance origin/<branch> past our
        // commit BETWEEN our accepted push and the verification fetch — high
        // contention is exactly what #92 targets. Being superseded is not a
        // lost publish: our commit landed and the competitor built on top of
        // it, so put_many must return Ok (an Err here would steer the
        // operator into a retry that clobbers the competitor). Simulated with
        // a post-receive hook that commits a conflicting rewrite of the same
        // fixed path on top of our just-accepted push.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let key = "knowledge/v1/corpus/current";
        remote.put(key, b"aaaa1111\n").unwrap();

        arm_post_receive_hook(
            bare.path(),
            "#!/bin/sh\n\
             [ -f \"$GIT_DIR/superseded\" ] && exit 0\n\
             touch \"$GIT_DIR/superseded\"\n\
             blob=$(printf 'competitor\\n' | git hash-object -w --stdin)\n\
             export GIT_INDEX_FILE=\"$GIT_DIR/tmp-index\"\n\
             git read-tree main\n\
             git update-index --add --cacheinfo \"100644,$blob,knowledge/v1/corpus/current\"\n\
             tree=$(git write-tree)\n\
             commit=$(GIT_AUTHOR_NAME=r GIT_AUTHOR_EMAIL=r@e GIT_COMMITTER_NAME=r \
             GIT_COMMITTER_EMAIL=r@e git commit-tree \"$tree\" -p main -m supersede)\n\
             git update-ref refs/heads/main \"$commit\"\n\
             exit 0\n",
        );
        // Our push is ACCEPTED, then immediately superseded: still a success.
        remote.put(key, b"bbbb2222\n").unwrap();

        // The remote tip is the competitor's, with our commit as its parent.
        let tip = git::run(bare.path(), &["show", &format!("main:{key}")]).unwrap();
        assert_eq!(tip, "competitor");
        let ours = git::run(bare.path(), &["show", &format!("main~1:{key}")]).unwrap();
        assert_eq!(ours, "bbbb2222");
        std::env::remove_var("CCE_HOME");
    }

    #[cfg(unix)]
    #[test]
    fn push_accepted_but_never_landing_on_the_ref_is_a_loud_error() {
        // The other half of the supersede fix, and the mutation-test pin for
        // verify_published (deleting the verify call must fail THIS test): a
        // push git reports as accepted whose ref never carries our commit —
        // post-receive resets the ref to its old value, discarding ours — is
        // a lost publish and must be a real Err, never Ok.
        let _home = with_home();
        let (bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        let key = "knowledge/v1/corpus/current";
        remote.put(key, b"aaaa1111\n").unwrap();

        arm_post_receive_hook(
            bare.path(),
            "#!/bin/sh\n\
             while read old new ref; do git update-ref \"$ref\" \"$old\"; done\n\
             exit 0\n",
        );
        let err = remote.put(key, b"bbbb2222\n").unwrap_err();
        assert!(err.contains("push verification failed"), "got {err}");
        assert!(err.contains("re-run the push"), "the error must name a next step: {err}");
        // The remote really kept the old state; the clone is clean.
        let kept = git::run(bare.path(), &["show", &format!("main:{key}")]).unwrap();
        assert_eq!(kept, "aaaa1111");
        assert!(no_rebase_in_progress(&remote), "clone left mid-rebase");
        std::env::remove_var("CCE_HOME");
    }

    #[cfg(unix)]
    #[test]
    fn commit_failure_in_put_many_is_a_real_error_not_a_silent_no_op() {
        // The mutation-test pin for commit-error propagation: restoring the
        // old `Err(_) => { /* fall through */ }` swallow must fail THIS test.
        // A pre-commit hook inside the clone vetoes the commit; put_many must
        // surface THAT failure (the veto marker), not blunder on to a push of
        // stale HEAD (which post-push verification would report differently).
        use std::os::unix::fs::PermissionsExt;
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put("knowledge/v1/corpus/current", b"aaaa1111\n").unwrap();

        let hook = remote.dir().join(".git").join("hooks").join("pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\necho 'pre-commit veto' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = remote.put("knowledge/v1/corpus/current", b"bbbb2222\n").unwrap_err();
        assert!(err.contains("pre-commit veto"), "commit failure must propagate, got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn heal_reattaches_a_detached_head_on_a_non_default_branch() {
        // When `rebase --abort` cannot run and the heal falls back to
        // `rebase --quit`, HEAD stays detached. The heal must re-attach it to
        // the REAL cache branch (from origin/HEAD) — falling back to the
        // default branch name on a cache living on another branch would push
        // refs/heads/main and fork the cache.
        let _home = with_home();
        let bare = tempfile::tempdir().unwrap();
        git::run_commit(bare.path(), &["init", "--bare", "-q", "-b", "trunk"]).unwrap();
        let url = format!("file://{}", bare.path().to_string_lossy());
        let remote = GitRemote::open(&url, false).unwrap();
        assert_eq!(remote.branch(), "trunk");
        let key = "knowledge/v1/corpus/current";
        remote.put(key, b"aaaa1111\n").unwrap();

        // Poison: doomed local commit + conflicting remote commit + a rebase
        // that stops on the conflict, then break `--abort` by removing the
        // rebase bookkeeping it restores from (the fall-back `--quit` path).
        let dir = remote.dir().to_path_buf();
        std::fs::write(dir.join(key), b"local5555\n").unwrap();
        git::run_commit(&dir, &["add", "--", key]).unwrap();
        git::run_commit(&dir, &["commit", "-q", "-m", "doomed local commit"]).unwrap();
        let other_home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", other_home.path());
        let other = GitRemote::open(&url, false).unwrap();
        other.put(key, b"remote7777\n").unwrap();
        std::env::set_var("CCE_HOME", _home.path());
        git::run(&dir, &["fetch", "--quiet", "origin"]).unwrap();
        assert!(git::run_commit(&dir, &["rebase", "--quiet", "origin/trunk"]).is_err());
        let state = dir.join(".git").join("rebase-merge");
        assert!(state.exists(), "test setup failed to leave a rebase in progress");
        let _ = std::fs::remove_file(state.join("orig-head"));
        let _ = std::fs::remove_file(state.join("head"));
        assert!(
            git::run(&dir, &["rebase", "--abort"]).is_err(),
            "test setup: --abort should be broken so the --quit path is exercised"
        );

        // Re-open: heal quits the rebase, re-attaches HEAD to trunk (never the
        // DEFAULT_REF fallback), and the next push publishes to trunk.
        let healed = GitRemote::open(&url, false).unwrap();
        assert!(no_rebase_in_progress(&healed), "open did not heal the mid-rebase clone");
        assert_eq!(healed.branch(), "trunk");
        assert_eq!(git::current_branch(healed.dir()).as_deref(), Some("trunk"));
        healed.put(key, b"bbbb2222\n").unwrap();
        let published = git::run(bare.path(), &["show", &format!("trunk:{key}")]).unwrap();
        assert_eq!(published, "bbbb2222");
        // No forked default branch was created on the remote.
        assert!(git::run(bare.path(), &["show-ref", "--verify", "refs/heads/main"]).is_err());
        std::env::remove_var("CCE_HOME");
    }
}
