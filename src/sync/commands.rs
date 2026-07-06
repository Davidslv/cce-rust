//! # sync::commands — the `cce sync …` command orchestration (SPEC-SYNC §5)
//!
//! **Why this file exists:** The CLI surface (`init`, `push`, `pull`, `status`,
//! `verify`) is thin argument parsing; the actual work — resolve config, read git
//! facts, export/import the artifact, drive the remote, and enforce the safety
//! rules (refuse a dirty tree or a non-hash index; never break local work) — lives
//! here so it is unit-testable against a local bare git remote without spawning the
//! binary.
//!
//! **What it is / does:** Each `cmd_*` returns `Result<String, String>` (a
//! human-readable report, or a clear error). It composes `config`, `git`, `remote`,
//! and `artifact`, and keeps a small `.cce/synced.json` marker so `status`/`verify`
//! know which sha the local cache came from (SPEC-SYNC §9.4).
//!
//! **Responsibilities:**
//! - Own `cmd_init/push/pull/status/verify` and the `--workspace` fan-out.
//! - Enforce §5 rules and §9 offline-first guarantees (best-effort, never fatal to
//!   local state).
//! - It does NOT parse CLI args (main.rs) nor define the artifact bytes (artifact).

use crate::embedder::HashEmbedder;
use crate::store::{default_store_path, Index};
use crate::sync::artifact::{Artifact, ManifestMeta};
use crate::sync::config::SyncConfig;
use crate::sync::remote::{GitRemote, SyncRemote};
use crate::sync::{
    content_address, git, normalize_repo_id, pointer_address, HASH_EMBEDDER, SYNC_FORMAT_VERSION,
};
use crate::workspace::Manifest;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which sha a `pull` targets (SPEC-SYNC §5).
#[derive(Debug, Clone)]
pub enum PullTarget {
    /// The working tree's current HEAD (the default).
    Head,
    /// An explicit `--commit <sha>`.
    Commit(String),
    /// The remote's latest pushed sha for the default ref (`--latest`).
    Latest,
}

/// The `.cce/synced.json` marker recording what the local cache was pulled from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncState {
    pub repo_id: String,
    pub sha: String,
    pub checksum: String,
}

impl SyncState {
    fn path(root: &Path) -> PathBuf {
        root.join(".cce").join("synced.json")
    }
    fn load(root: &Path) -> Option<SyncState> {
        let text = std::fs::read_to_string(Self::path(root)).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(self).unwrap_or_default())
    }
}

/// Resolve the `repo_id` for `root`: the config override, else the normalized git
/// origin, else an error (we cannot address the cache without one).
fn resolve_repo_id(root: &Path, cfg: &SyncConfig) -> Result<String, String> {
    if let Some(id) = &cfg.repo_id {
        return Ok(id.clone());
    }
    match git::origin_url(root) {
        Some(url) => Ok(normalize_repo_id(&url)),
        None => {
            Err("cannot determine repo_id: no `sync.repo_id` configured and no git origin remote. \
             Set one with `cce sync init --repo-id <id>`."
                .to_string())
        }
    }
}

/// Open the configured remote, or a clear "no remote" error (offline-first: this is
/// the only place that can fail for lack of a remote, and it fails cleanly).
fn open_remote(cfg: &SyncConfig) -> Result<GitRemote, String> {
    let url = cfg.remote.as_deref().ok_or_else(|| {
        "no sync remote configured — run `cce sync init --remote <git-url>`".to_string()
    })?;
    GitRemote::open(url, cfg.lfs)
}

/// Build a fresh hash-embedder index for `root`'s working tree, for export at `sha`.
///
/// **Content-address invariant (SPEC-SYNC §1/§3):** the cache is keyed by sha and the
/// invariant is `artifact == build(sha)`. Push therefore MUST rebuild from the current
/// source tree and export THAT — it must **never** re-export a pre-existing
/// `.cce/index.json`. That file may be stale, foreign, or (after `cce sync pull`) a
/// cache built by an older or sibling engine; re-publishing its bytes under the sha key
/// would launder a non-`build(sha)` index into the content-addressed cache and make
/// `cce sync verify` (which correctly rebuilds) fail against it.
///
/// If a store already exists we still read it to enforce the non-hash refusal
/// (SPEC-SYNC §1: only the deterministic hash embedder produces shareable caches), but
/// we never reuse its bytes — we always rebuild.
fn ensure_hash_index(root: &Path, sha: &str) -> Result<Index, String> {
    let store = default_store_path(root);
    if store.exists() {
        let idx =
            Index::load(&store).map_err(|e| format!("could not load {}: {e}", store.display()))?;
        if idx.embedder_name != HASH_EMBEDDER {
            return Err(format!(
                "refusing to push a non-hash index (embedder = `{}`). Only the deterministic \
                 hash embedder produces shareable caches; re-index with `cce index <dir>`.",
                idx.embedder_name
            ));
        }
    }
    // Always rebuild from the working tree so the export is exactly build(sha), never a
    // stale/foreign/pulled index.json. Use the same build path `cce index`/verify use.
    eprintln!("rebuilding index for {sha}");
    let (idx, _) = Index::build_protected(root, &HashEmbedder, |_| true, true)?;
    Ok(idx)
}

/// `cce sync init` (SPEC-SYNC §5): write `sync.*` config and set up the local clone.
pub fn cmd_init(
    root: &Path,
    remote_url: &str,
    lfs: bool,
    repo_id_override: Option<String>,
) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let mut cfg = SyncConfig::load(root);
    cfg.remote = Some(remote_url.to_string());
    cfg.lfs = lfs;
    if let Some(id) = repo_id_override {
        cfg.repo_id = Some(id);
    } else if cfg.repo_id.is_none() {
        // Best effort: record the normalized origin so the id is visible/stable.
        if let Some(url) = git::origin_url(root) {
            cfg.repo_id = Some(normalize_repo_id(&url));
        }
    }
    cfg.save(root).map_err(|e| format!("could not write config: {e}"))?;

    // Set up (or reuse) the working clone; this is where LFS attributes are written.
    let remote = open_remote(&cfg)?;

    let mut out = String::new();
    out.push_str(&format!("Configured sync remote: {remote_url}\n"));
    out.push_str(&format!(
        "  git-LFS       : {}\n",
        if lfs { "enabled (*.cce)" } else { "disabled" }
    ));
    if let Some(id) = &cfg.repo_id {
        out.push_str(&format!("  repo_id       : {id}\n"));
    } else {
        out.push_str("  repo_id       : (resolved from git origin at push time)\n");
    }
    out.push_str(&format!("  working clone : {}\n", remote.dir().display()));
    out.push_str(&format!(
        "  config        : {}\n",
        crate::sync::config::config_path(root).display()
    ));
    Ok(out)
}

/// Resolve the sha to push: `--commit` or HEAD; refuse a dirty tree (SPEC-SYNC §5).
fn resolve_push_sha(root: &Path, commit: Option<String>) -> Result<String, String> {
    if git::is_dirty(root) {
        return Err(
            "refusing to push: the working tree is dirty. Commit your changes and push a clean \
             sha (a cache is content-addressed by commit)."
                .to_string(),
        );
    }
    match commit {
        Some(sha) => Ok(sha),
        None => git::head_sha(root).ok_or_else(|| {
            "cannot determine HEAD sha — is this a git repository with a commit?".to_string()
        }),
    }
}

/// Export a single repo's index at `sha` to an artifact and put it on the remote,
/// updating the ref pointer. Returns the artifact checksum.
fn push_one(
    root: &Path,
    remote: &dyn SyncRemote,
    repo_id: &str,
    sha: &str,
) -> Result<(String, String), String> {
    let index = ensure_hash_index(root, sha)?;
    let meta = ManifestMeta { repo_id: repo_id.to_string(), sha: sha.to_string() };
    let artifact = Artifact::from_index(&index, meta);
    let bytes = artifact.to_bytes();
    let ver = SYNC_FORMAT_VERSION.to_string();
    let key = content_address(HASH_EMBEDDER, &ver, repo_id, sha);

    // Update the ref pointer to this sha alongside the artifact, in one commit/push.
    let branch = git::current_branch(root).unwrap_or_else(|| crate::sync::DEFAULT_REF.to_string());
    let pointer_key = pointer_address(HASH_EMBEDDER, &ver, repo_id, &branch);
    remote.put_many(&[(key.clone(), bytes), (pointer_key, format!("{sha}\n").into_bytes())])?;
    Ok((key, artifact.manifest.checksum))
}

/// `cce sync push` (SPEC-SYNC §5): export the hash-index for HEAD/sha and put it.
pub fn cmd_push(root: &Path, commit: Option<String>, workspace: bool) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let remote = open_remote(&cfg)?;

    if workspace {
        return push_workspace(root, &cfg, &remote);
    }

    let sha = resolve_push_sha(root, commit)?;
    let repo_id = resolve_repo_id(root, &cfg)?;
    let (key, checksum) = push_one(root, &remote, &repo_id, &sha)?;
    Ok(format!("Pushed {repo_id}@{sha}\n  key      : {key}\n  checksum : {checksum}\n"))
}

/// Push every workspace member, each keyed by its own `repo_id@sha`.
fn push_workspace(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
) -> Result<String, String> {
    let manifest = Manifest::load(root)?;
    let base = resolve_repo_id(root, cfg)?;
    let sha = resolve_push_sha(root, None)?;
    let mut out = format!("Pushing workspace {} @ {sha}\n", manifest.name);
    for m in &manifest.members {
        let member_dir = root.join(&m.path);
        let repo_id = format!("{base}__{}", m.name);
        let (key, checksum) = push_one(&member_dir, remote, &repo_id, &sha)?;
        out.push_str(&format!("  {:<16} {key}  ({})\n", m.name, &checksum[..12]));
    }
    Ok(out)
}

/// Resolve the sha a pull should install.
fn resolve_pull_sha(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
    repo_id: &str,
    target: &PullTarget,
) -> Result<String, String> {
    match target {
        PullTarget::Commit(sha) => Ok(sha.clone()),
        PullTarget::Head => git::head_sha(root).ok_or_else(|| {
            "cannot determine HEAD sha — pass --commit <sha> or --latest".to_string()
        }),
        PullTarget::Latest => {
            let ver = SYNC_FORMAT_VERSION.to_string();
            let pointer = pointer_address(HASH_EMBEDDER, &ver, repo_id, crate::sync::DEFAULT_REF);
            let bytes = remote.get(&pointer).map_err(|_| {
                format!("no `--latest` pointer for {repo_id} on `{}`", crate::sync::DEFAULT_REF)
            })?;
            let _ = cfg;
            Ok(String::from_utf8_lossy(&bytes).trim().to_string())
        }
    }
}

/// Install an artifact's bytes into `root`'s `.cce/` store; record the sync marker.
fn install_artifact(root: &Path, bytes: &[u8]) -> Result<Artifact, String> {
    let artifact = Artifact::from_bytes(bytes)?;
    let index = artifact.clone().into_index();
    let store = default_store_path(root);
    index.save(&store).map_err(|e| format!("could not write {}: {e}", store.display()))?;
    SyncState {
        repo_id: artifact.manifest.repo_id.clone(),
        sha: artifact.manifest.sha.clone(),
        checksum: artifact.manifest.checksum.clone(),
    }
    .save(root)
    .map_err(|e| format!("could not write sync marker: {e}"))?;

    // Best-effort: record a `sync-pull` index event so the dashboard's freshness
    // panel shows the pulled provenance (purely log-derived — the dashboard makes no
    // live lookup). Never fatal to the pull.
    let clock = crate::metrics::SystemClock;
    let ids = crate::metrics::HexIdSource::default();
    let writer = crate::metrics::MetricsWriter::new(
        crate::store::default_metrics_path(root),
        &clock,
        &ids,
        true,
    );
    writer.log_index(&crate::metrics::IndexRecord {
        files_indexed: index.files().len(),
        chunks: index.chunks.len(),
        index_bytes: bytes.len() as u64,
        duration_ms: 0.0,
        embedder: HASH_EMBEDDER.to_string(),
        full: true,
        sha: Some(artifact.manifest.sha.clone()),
        source: "sync-pull".to_string(),
        sensitive_skipped: 0,
    });

    Ok(artifact)
}

/// `cce sync pull` (SPEC-SYNC §5): fetch the cache for a sha and install it.
pub fn cmd_pull(
    root: &Path,
    target: PullTarget,
    force: bool,
    workspace: bool,
) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let remote = open_remote(&cfg)?;

    if workspace {
        return pull_workspace(root, &cfg, &remote, &target);
    }

    let repo_id = resolve_repo_id(root, &cfg)?;
    let sha = resolve_pull_sha(root, &cfg, &remote, &repo_id, &target)?;

    // §9.4: do not silently overwrite a newer local index for a different sha.
    if !force {
        if let Some(state) = SyncState::load(root) {
            if state.sha != sha {
                return Err(format!(
                    "local cache is at {} but you are pulling {sha}. Pass --force to overwrite.",
                    state.sha
                ));
            }
        }
    }

    let ver = SYNC_FORMAT_VERSION.to_string();
    let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
    let bytes = remote.get(&key)?;
    let artifact = install_artifact(root, &bytes)?;

    let mut out = format!(
        "Pulled {repo_id}@{sha}\n  chunks   : {}\n  checksum : {}\n  store    : {}\n",
        artifact.manifest.chunk_count,
        artifact.manifest.checksum,
        default_store_path(root).display()
    );
    // §7 v1: if the working tree differs from the pulled sha, note the fallback.
    match git::head_sha(root) {
        Some(head) if head == sha && !git::is_dirty(root) => {
            out.push_str("  tree     : matches — pulled index used as-is\n");
        }
        Some(_) => {
            out.push_str(
                "  tree     : differs from the pulled sha — `cce index` locally for a WIP index\n",
            );
        }
        None => {}
    }
    Ok(out)
}

/// Pull every workspace member from its own `repo_id@sha` cache.
fn pull_workspace(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
    target: &PullTarget,
) -> Result<String, String> {
    let manifest = Manifest::load(root)?;
    let base = resolve_repo_id(root, cfg)?;
    let ver = SYNC_FORMAT_VERSION.to_string();
    let mut out = format!("Pulling workspace {}\n", manifest.name);
    for m in &manifest.members {
        let member_dir = root.join(&m.path);
        let repo_id = format!("{base}__{}", m.name);
        let sha = resolve_pull_sha(&member_dir, cfg, remote, &repo_id, target)?;
        let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
        let bytes = remote.get(&key)?;
        let artifact = install_artifact(&member_dir, &bytes)?;
        out.push_str(&format!(
            "  {:<16} {sha}  chunks {}  ({})\n",
            m.name,
            artifact.manifest.chunk_count,
            &artifact.manifest.checksum[..12]
        ));
    }
    Ok(out)
}

/// `cce sync status` (SPEC-SYNC §5): remote, local cache sha, remote latest, match.
pub fn cmd_status(root: &Path) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let mut out = String::new();
    match &cfg.remote {
        Some(r) => out.push_str(&format!("remote        : {r}\n")),
        None => {
            out.push_str("remote        : (none — pure local CCE)\n");
            return Ok(out);
        }
    }
    out.push_str(&format!("git-LFS       : {}\n", if cfg.lfs { "on" } else { "off" }));

    let repo_id = match resolve_repo_id(root, &cfg) {
        Ok(id) => id,
        Err(e) => {
            out.push_str(&format!("repo_id       : (unresolved: {e})\n"));
            return Ok(out);
        }
    };
    out.push_str(&format!("repo_id       : {repo_id}\n"));

    match SyncState::load(root) {
        Some(state) => {
            out.push_str(&format!("local cache   : {} ({})\n", state.sha, &state.checksum[..12]))
        }
        None => out.push_str("local cache   : (none pulled yet)\n"),
    }

    // Remote latest is best-effort: offline ⇒ we still print everything else.
    match open_remote(&cfg) {
        Ok(remote) => {
            let ver = SYNC_FORMAT_VERSION.to_string();
            let pointer = pointer_address(HASH_EMBEDDER, &ver, &repo_id, crate::sync::DEFAULT_REF);
            match remote.get(&pointer) {
                Ok(bytes) => out.push_str(&format!(
                    "remote latest : {} (ref {})\n",
                    String::from_utf8_lossy(&bytes).trim(),
                    crate::sync::DEFAULT_REF
                )),
                Err(_) => out.push_str("remote latest : (no pointer yet)\n"),
            }
        }
        Err(e) => out.push_str(&format!("remote latest : (unreachable: {e})\n")),
    }

    match git::head_sha(root) {
        Some(head) => {
            let dirty = if git::is_dirty(root) { " (dirty)" } else { "" };
            out.push_str(&format!("working tree  : {head}{dirty}\n"));
        }
        None => out.push_str("working tree  : (not a git checkout)\n"),
    }
    Ok(out)
}

/// `cce sync verify` (SPEC-SYNC §5): re-index locally and confirm the pulled
/// artifact's checksum by rebuilding it byte-for-byte.
pub fn cmd_verify(root: &Path, commit: Option<String>) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let repo_id = resolve_repo_id(root, &cfg)?;

    // The expected checksum: from the local sync marker, else fetched from the remote.
    let sha = match &commit {
        Some(s) => s.clone(),
        None => match SyncState::load(root) {
            Some(state) => state.sha,
            None => git::head_sha(root)
                .ok_or_else(|| "nothing to verify: no pulled cache and no HEAD sha".to_string())?,
        },
    };

    let expected = match SyncState::load(root) {
        Some(state) if state.sha == sha => state.checksum,
        _ => {
            // Fetch the artifact from the remote to learn its checksum.
            let remote = open_remote(&cfg)?;
            let ver = SYNC_FORMAT_VERSION.to_string();
            let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
            let bytes = remote.get(&key)?;
            Artifact::from_bytes(&bytes)?.manifest.checksum
        }
    };

    // Rebuild locally from the working tree and export at the same identity.
    let (index, _) = Index::build_protected(root, &HashEmbedder, |_| true, true)?;
    let meta = ManifestMeta { repo_id: repo_id.clone(), sha: sha.clone() };
    let rebuilt = Artifact::from_index(&index, meta);
    let actual = rebuilt.manifest.checksum;

    if actual == expected {
        Ok(format!("verify OK: {repo_id}@{sha}\n  checksum : {actual}\n"))
    } else {
        Err(format!(
            "verify FAILED for {repo_id}@{sha}\n  expected : {expected}\n  rebuilt  : {actual}\n\
             The pulled cache does not match a local rebuild (working tree differs from the sha, \
             or the cache is not trustworthy)."
        ))
    }
}

/// Where the local index came from (SPEC-MCP: `index_status` freshness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSource {
    /// Built locally by `cce index` — no sync marker present.
    Local,
    /// Installed by `cce sync pull` — a `.cce/synced.json` marker is present.
    Pulled,
}

/// A read-only freshness summary of the local index (SPEC-MCP §"Freshness is
/// observable"). Pure of side effects; the remote lookup is best-effort and
/// offline-safe (any error leaves `remote_latest = None`, `behind_remote = false`).
#[derive(Debug, Clone)]
pub struct Freshness {
    /// Local vs pulled.
    pub source: IndexSource,
    /// The pulled sha, when the index came from the cache.
    pub sha: Option<String>,
    /// The remote's latest pushed sha for the default ref, if reachable.
    pub remote_latest: Option<String>,
    /// True only when both shas are known and differ (the local index is stale).
    pub behind_remote: bool,
}

/// Summarise the local index's freshness for `root` (SPEC-MCP). Reads the sync
/// marker for source/sha, then best-effort resolves the remote's latest sha to
/// decide "behind remote." With no remote configured this touches no network and
/// reports `Local`. Never errors — MCP's `index_status` must always answer.
pub fn freshness(root: &Path) -> Freshness {
    let state = SyncState::load(root);
    let (source, sha) = match &state {
        Some(s) => (IndexSource::Pulled, Some(s.sha.clone())),
        None => (IndexSource::Local, None),
    };

    let cfg = SyncConfig::load(root);
    let remote_latest = if cfg.remote.is_some() {
        resolve_repo_id(root, &cfg).ok().and_then(|repo_id| {
            open_remote(&cfg).ok().and_then(|remote| {
                let ver = SYNC_FORMAT_VERSION.to_string();
                let pointer =
                    pointer_address(HASH_EMBEDDER, &ver, &repo_id, crate::sync::DEFAULT_REF);
                remote.get(&pointer).ok().map(|b| String::from_utf8_lossy(&b).trim().to_string())
            })
        })
    } else {
        None
    };

    let behind_remote = match (&sha, &remote_latest) {
        (Some(local), Some(remote)) => local != remote,
        _ => false,
    };

    Freshness { source, sha, remote_latest, behind_remote }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::remote::GitRemote;

    /// A source repo with committed content, on branch `main`.
    fn source_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run_commit(d, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(d.join("auth.py"), "def login(u):\n    return hash(u)\n").unwrap();
        std::fs::write(d.join("app.py"), "import auth\n\ndef run():\n    return auth.login('x')\n")
            .unwrap();
        git::run_commit(d, &["add", "-A"]).unwrap();
        git::run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    fn bare_remote() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        git::run_commit(tmp.path(), &["init", "--bare", "-q", "-b", "main"]).unwrap();
        let url = format!("file://{}", tmp.path().to_string_lossy());
        (tmp, url)
    }

    #[allow(dead_code)]
    struct HomeGuard {
        home: tempfile::TempDir,
        lock: std::sync::MutexGuard<'static, ()>,
    }
    /// Point CCE_HOME at a temp dir under the process-wide env lock.
    fn set_home() -> HomeGuard {
        let lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        HomeGuard { home, lock }
    }

    fn init_cfg(root: &Path, url: &str) {
        SyncConfig {
            remote: Some(url.to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(root)
        .unwrap();
    }

    #[test]
    fn push_then_pull_round_trip_is_functionally_identical() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);

        // Push from the source repo.
        let report = cmd_push(src.path(), None, false).unwrap();
        assert!(report.contains("Pushed example.com__acme__demo@"));

        // A fresh consumer checkout (same sha) pulls it.
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        let out = cmd_pull(dst.path(), PullTarget::Head, false, false).unwrap();
        assert!(out.contains("Pulled example.com__acme__demo@"));
        assert!(out.contains("matches — pulled index used as-is"), "got: {out}");

        // The pulled store is byte-identical to a fresh local hash index of src.
        let (local, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let pulled = Index::load(&default_store_path(dst.path())).unwrap();
        assert_eq!(pulled.chunks.len(), local.chunks.len());
        assert_eq!(pulled.file_imports, local.file_imports);
        std::env::remove_var("CCE_HOME");
    }

    /// Clone the source repo's committed tree into a new dir at the same sha.
    fn source_repo_clone(src: &tempfile::TempDir) -> tempfile::TempDir {
        let dst = tempfile::tempdir().unwrap();
        let src_url = format!("file://{}", src.path().to_string_lossy());
        git::run_commit(Path::new("."), &["clone", "-q", &src_url, &dst.path().to_string_lossy()])
            .unwrap();
        dst
    }

    #[test]
    fn pull_latest_uses_the_ref_pointer() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        let out = cmd_pull(dst.path(), PullTarget::Latest, false, false).unwrap();
        assert!(out.contains(&sha), "latest should resolve to {sha}: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_matches_after_pull() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();

        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false).unwrap();
        let out = cmd_verify(dst.path(), None).unwrap();
        assert!(out.contains("verify OK"), "got: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_refuses_dirty_tree() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // Make a real (non-.cce) change.
        std::fs::write(src.path().join("auth.py"), "def login(u):\n    return 0\n").unwrap();
        let err = cmd_push(src.path(), None, false).unwrap_err();
        assert!(err.contains("working tree is dirty"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_refuses_non_hash_index() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // Plant an ollama-embedder store so ensure_hash_index refuses.
        let (mut idx, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        idx.embedder_name = "ollama".to_string();
        idx.save(&default_store_path(src.path())).unwrap();
        let err = cmd_push(src.path(), None, false).unwrap_err();
        assert!(err.contains("refusing to push a non-hash index"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn pull_refuses_overwriting_a_different_sha_without_force() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false).unwrap();

        // Now pretend to pull a different sha: should refuse without --force.
        let err = cmd_pull(dst.path(), PullTarget::Commit("deadbeef".to_string()), false, false)
            .unwrap_err();
        assert!(err.contains("--force to overwrite"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn no_remote_configured_errors_clearly() {
        let _home = set_home();
        let work = tempfile::tempdir().unwrap();
        let err = cmd_push(work.path(), None, false).unwrap_err();
        assert!(err.contains("no sync remote configured"), "got: {err}");
        // Status with no remote is not an error — it reports the local-only state.
        let s = cmd_status(work.path()).unwrap();
        assert!(s.contains("pure local CCE"), "got: {s}");
    }

    #[test]
    fn cache_miss_on_pull_reports_clearly() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // No push happened, so the artifact is absent.
        let err = cmd_pull(src.path(), PullTarget::Head, false, false).unwrap_err();
        assert!(err.contains("cache miss"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_remote_and_local_cache() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false).unwrap();
        let s = cmd_status(dst.path()).unwrap();
        assert!(s.contains("remote        : file://"));
        assert!(s.contains("local cache   :"));
        assert!(s.contains("remote latest :"));
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn init_writes_config_and_clone() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        let out =
            cmd_init(src.path(), &url, false, Some("example.com__acme__demo".to_string())).unwrap();
        assert!(out.contains("Configured sync remote"));
        let cfg = SyncConfig::load(src.path());
        assert_eq!(cfg.remote.as_deref(), Some(url.as_str()));
        assert_eq!(cfg.repo_id.as_deref(), Some("example.com__acme__demo"));
        assert!(GitRemote::clone_dir(&url).join(".git").is_dir());
        std::env::remove_var("CCE_HOME");
    }

    /// A git workspace with two detectable JS members, committed on `main`.
    fn workspace_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run_commit(d, &["init", "-q", "-b", "main"]).unwrap();
        for name in ["alpha", "beta"] {
            let m = d.join(name);
            std::fs::create_dir_all(m.join("src")).unwrap();
            std::fs::write(m.join("package.json"), format!("{{\"name\":\"{name}\"}}")).unwrap();
            std::fs::write(m.join("src/index.js"), format!("function {name}() {{ return 1; }}\n"))
                .unwrap();
        }
        // Write the workspace manifest and commit everything.
        let manifest = crate::workspace::build_manifest(d);
        manifest.save(d).unwrap();
        git::run_commit(d, &["add", "-A"]).unwrap();
        git::run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    #[test]
    fn workspace_push_then_pull_over_members() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        // Config with a base repo_id; members are keyed <base>__<member>.
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();

        let report = cmd_push(src.path(), None, true).unwrap();
        assert!(report.contains("Pushing workspace"));
        assert!(report.contains("alpha"));
        assert!(report.contains("beta"));

        // Clone the whole workspace and pull every member.
        let dst = source_repo_clone(&src);
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(dst.path())
        .unwrap();
        let out = cmd_pull(dst.path(), PullTarget::Head, false, true).unwrap();
        assert!(out.contains("Pulling workspace"));
        // Each member now has its own store.
        assert!(dst.path().join("alpha/.cce/index.json").exists());
        assert!(dst.path().join("beta/.cce/index.json").exists());
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_fails_when_working_tree_differs() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false).unwrap();

        // Mutate a tracked file so the local rebuild no longer matches the cache.
        std::fs::write(dst.path().join("auth.py"), "def login(u):\n    return 999\n").unwrap();
        let err = cmd_verify(dst.path(), None).unwrap_err();
        assert!(err.contains("verify FAILED"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_fetches_checksum_from_remote_when_no_marker() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        // A clean clone with no synced.json: verify must fetch the artifact to learn
        // its checksum, then confirm the local rebuild matches.
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        assert!(!SyncState::path(dst.path()).exists());
        let out = cmd_verify(dst.path(), Some(sha)).unwrap();
        assert!(out.contains("verify OK"), "got: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_unreachable_remote() {
        let _home = set_home();
        let src = source_repo();
        // A configured but non-existent remote: status still succeeds and flags it.
        SyncConfig {
            remote: Some("file:///definitely/not/here.git".to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();
        let s = cmd_status(src.path()).unwrap();
        assert!(s.contains("remote latest : (unreachable"), "got: {s}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_unresolved_repo_id() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        // A non-git directory with a remote but no repo_id and no git origin.
        let dir = tempfile::tempdir().unwrap();
        SyncConfig {
            remote: Some(url),
            lfs: false,
            repo_id: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(dir.path())
        .unwrap();
        let s = cmd_status(dir.path()).unwrap();
        assert!(s.contains("repo_id       : (unresolved"), "got: {s}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_with_explicit_commit_flag() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let report = cmd_push(src.path(), Some(sha.clone()), false).unwrap();
        assert!(report.contains(&sha), "got: {report}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn init_rejects_non_directory() {
        let _home = set_home();
        let err = cmd_init(Path::new("/no/such/dir/here"), "file:///x", false, None).unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    /// Regression (fix/sync-push-rebuild): push must REBUILD the index from the
    /// working tree and export `build(sha)`, never re-export a pre-existing (stale or
    /// foreign) `.cce/index.json`. We plant an index whose bytes differ from a real
    /// build and assert the *published* artifact's checksum equals a fresh rebuild's,
    /// not the planted file's.
    #[test]
    fn push_rebuilds_and_ignores_a_stale_planted_index() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let repo_id = "example.com__acme__demo";
        let ver = SYNC_FORMAT_VERSION.to_string();

        // The correct artifact checksum: a fresh build of the working tree.
        let (fresh, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let fresh_checksum = Artifact::from_index(
            &fresh,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;

        // Plant a FOREIGN/stale index.json whose bytes differ from a real build
        // (mutate a chunk's content so its content-address checksum differs), but keep
        // it a *hash* index so the non-hash refusal does not short-circuit the push.
        // (The deterministic hash build reproduces `fresh` exactly, so this is a
        // faithful copy with one chunk perturbed.)
        let (mut foreign, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        assert!(!foreign.chunks.is_empty());
        foreign.chunks[0].content.push_str("\n# stale foreign bytes from an older engine\n");
        let planted_checksum = Artifact::from_index(
            &foreign,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;
        assert_ne!(planted_checksum, fresh_checksum, "planted bytes must differ from build(sha)");
        assert_eq!(foreign.embedder_name, HASH_EMBEDDER);
        foreign.save(&default_store_path(src.path())).unwrap();

        // Push: must rebuild from the tree, NOT re-export the planted file.
        let report = cmd_push(src.path(), None, false).unwrap();

        // The PUBLISHED artifact's checksum equals a fresh rebuild, not the planted file.
        let key = content_address(HASH_EMBEDDER, &ver, repo_id, &sha);
        let bytes = GitRemote::open(&url, false).unwrap().get(&key).unwrap();
        let published = Artifact::from_bytes(&bytes).unwrap().manifest.checksum;
        assert_eq!(
            published, fresh_checksum,
            "push must publish build(sha), not the planted index"
        );
        assert_ne!(published, planted_checksum, "push must NOT launder the stale planted bytes");
        assert!(
            report.contains(&fresh_checksum),
            "report should show the rebuilt checksum: {report}"
        );
        std::env::remove_var("CCE_HOME");
    }

    /// Regression: the reproduction from the bug report — a consumer `pull`s a stale
    /// cache (built by an older engine, bytes ≠ build(sha)), then `push`es. Push must
    /// republish `build(sha)`; a fresh consumer that then `pull`s the republished
    /// artifact must `verify` GREEN. Proves pull → push → verify is laundering-proof.
    #[test]
    fn pull_then_push_then_verify_is_green_after_stale_cache() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let repo_id = "example.com__acme__demo";
        let ver = SYNC_FORMAT_VERSION.to_string();
        let key = content_address(HASH_EMBEDDER, &ver, repo_id, &sha);
        let pointer = pointer_address(HASH_EMBEDDER, &ver, repo_id, crate::sync::DEFAULT_REF);

        // The correct build(sha) checksum.
        let (fresh, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let correct = Artifact::from_index(
            &fresh,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;

        // Seed the remote with a STALE artifact under the sha key (simulating a cache
        // built by an older/sibling engine: bytes that differ from build(sha)). The
        // deterministic hash build reproduces `fresh`; we perturb one chunk.
        let (mut stale_idx, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        stale_idx.chunks[0].content.push_str("\n# stale cache from an older cce version\n");
        let stale_artifact = Artifact::from_index(
            &stale_idx,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        );
        let stale_checksum = stale_artifact.manifest.checksum.clone();
        assert_ne!(stale_checksum, correct);
        GitRemote::open(&url, false)
            .unwrap()
            .put_many(&[
                (key.clone(), stale_artifact.to_bytes()),
                (pointer, format!("{sha}\n").into_bytes()),
            ])
            .unwrap();

        // Consumer B pulls the stale cache (installs the stale index + marker)...
        let b = source_repo_clone(&src);
        init_cfg(b.path(), &url);
        let pulled = cmd_pull(b.path(), PullTarget::Head, false, false).unwrap();
        assert!(pulled.contains(&stale_checksum), "B pulled the stale cache: {pulled}");
        // ...then B pushes: must REBUILD and republish build(sha), not the stale file.
        cmd_push(b.path(), None, false).unwrap();

        // The remote now holds build(sha), not the stale bytes.
        let republished =
            Artifact::from_bytes(&GitRemote::open(&url, false).unwrap().get(&key).unwrap())
                .unwrap()
                .manifest
                .checksum;
        assert_eq!(republished, correct, "push must republish build(sha)");
        assert_ne!(republished, stale_checksum, "the stale bytes must not survive a push");

        // A fresh consumer C pulls the republished artifact → verify is GREEN.
        let c = source_repo_clone(&src);
        init_cfg(c.path(), &url);
        cmd_pull(c.path(), PullTarget::Head, false, false).unwrap();
        let out = cmd_verify(c.path(), None).unwrap();
        assert!(out.contains("verify OK"), "pull→push→verify must be green: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn resolve_repo_id_uses_git_origin_when_unconfigured() {
        let _home = set_home();
        let src = source_repo();
        // Give the source repo an origin so repo_id can be derived.
        git::run_commit(
            src.path(),
            &["remote", "add", "origin", "https://github.com/acme/demo.git"],
        )
        .unwrap();
        let cfg = SyncConfig { repo_id: None, ..SyncConfig::default() };
        assert_eq!(resolve_repo_id(src.path(), &cfg).unwrap(), "github.com__acme__demo");
        std::env::remove_var("CCE_HOME");
    }
}
