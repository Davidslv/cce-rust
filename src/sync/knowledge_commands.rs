//! # sync::knowledge_commands — `cce knowledge push` / `pull` orchestration
//! (SPEC-SYNC-KNOWLEDGE §4/§5)
//!
//! **Why this file exists:** The knowledge sync CLI surface is thin argument
//! parsing; the actual work — resolve the corpus identity and remote, export or
//! import the `.cck` artifact, enforce the §5 refusals (missing store, invalid
//! corpus_id, embedding-less store, different-corpus overwrite), publish the
//! pointer + `corpus.json`, apply retention — lives here so it is unit-testable
//! against a local bare git remote without spawning the binary. The sibling of
//! `sync::commands`, on the knowledge key space.
//!
//! **What it is / does:** `cmd_knowledge_push` exports the CURRENT local store as
//! a canonical `.cck`, puts it at its content-addressed key, advances the corpus
//! `current` pointer, and publishes `corpus.json` — one commit/push (`put_many`) —
//! then applies retention (§4.5, best-effort). `cmd_knowledge_pull` fetches the
//! corpus's current (or a pinned) snapshot, verifies the manifest checksum,
//! installs it into `<root>/.cce/knowledge/` **exactly as a local ingest would**
//! (§7 byte-identity), and records the knowledge sync marker with
//! `installed_sha256` (the #55 mechanism verbatim).
//!
//! **Responsibilities:**
//! - Own the §4.1 corpus_id resolution/validation and the §4.3 remote resolution
//!   (`--remote` > `knowledge.sync.remote` > `sync.remote`).
//! - Own the `.cce/knowledge/synced.json` marker and the §5 overwrite guard.
//! - Offline-first (§10): every failure here is a clean message; local ingest and
//!   search are never touched by a failed sync.
//! - It does NOT define the `.cck` bytes (that is `knowledge_artifact`) nor parse
//!   CLI args (main.rs).

use crate::knowledge::store::KnowledgeStore;
use crate::sync::config::{KnowledgeSyncConfig, Retention, SyncConfig};
use crate::sync::knowledge_artifact::KnowledgeArtifact;
use crate::sync::remote::{GitRemote, SyncRemote};
use crate::sync::{
    hex_lower, knowledge_content_address, knowledge_contract_version,
    knowledge_corpus_meta_address, knowledge_pointer_address, valid_corpus_id,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The published corpus-metadata schema id (SPEC-SYNC-KNOWLEDGE §4.4).
pub const KNOWLEDGE_META_SCHEMA_ID: &str = "cce.knowledgemeta/v1";

/// The `.cce/knowledge/synced.json` marker recording what the local knowledge
/// store was pulled from (SPEC-SYNC-KNOWLEDGE §5) — the knowledge analogue of
/// `.cce/synced.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeSyncState {
    /// The §4.1 corpus identity the pull installed.
    pub corpus_id: String,
    /// The installed snapshot id.
    pub snapshot: String,
    /// The `.cck` manifest checksum verified at pull time.
    pub checksum: String,
    /// SHA-256 (lowercase hex) of the exact `<snapshot>.json` bytes written at
    /// install time, hashed from the file just written to disk (read back) — the
    /// #55 mechanism verbatim, version-independent by construction. `verify
    /// --checksum-only` covers knowledge with this (surface wired in M5.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

impl KnowledgeSyncState {
    /// The marker path: `<root>/.cce/knowledge/synced.json`.
    pub fn path(root: &Path) -> PathBuf {
        KnowledgeStore::dir(root).join("synced.json")
    }

    /// Load the marker, if a pull ever wrote one.
    pub fn load(root: &Path) -> Option<KnowledgeSyncState> {
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

/// Resolve the corpus_id (SPEC-SYNC-KNOWLEDGE §4.1): the explicit `--corpus`,
/// else `knowledge.sync.corpus_id` — **never derived** (knowledge has no git
/// origin to normalize; a guessed identity would silently fork a corpus). The
/// resolved id must be repo_id-valid and sanitize-stable.
fn resolve_corpus_id(
    corpus_override: Option<String>,
    kcfg: &KnowledgeSyncConfig,
) -> Result<String, String> {
    let id = corpus_override.or_else(|| kcfg.corpus_id.clone()).ok_or_else(|| {
        "cannot determine corpus_id: pass --corpus <id> or set `knowledge.sync.corpus_id` in \
         .cce/config (a corpus identity is never derived)"
            .to_string()
    })?;
    if !valid_corpus_id(&id) {
        return Err(format!(
            "invalid corpus_id `{id}`: must be non-empty, charset [A-Za-z0-9._-] \
             (sanitize-stable — it is a path segment on the cache)"
        ));
    }
    Ok(id)
}

/// Open the remote for knowledge commands (SPEC-SYNC-KNOWLEDGE §4.3): the
/// explicit `--remote`, else the per-corpus `knowledge.sync.remote` override,
/// else the project's `sync.remote` (same-remote default). Returns the opened
/// remote and whether the project has LFS enabled.
fn open_knowledge_remote(
    root: &Path,
    remote_override: Option<String>,
) -> Result<(GitRemote, bool), String> {
    let kcfg = KnowledgeSyncConfig::load(root);
    let cfg = SyncConfig::load(root);
    let url = remote_override.or(kcfg.remote).or_else(|| cfg.remote.clone()).ok_or_else(|| {
        "no sync remote configured — run `cce sync init --remote <git-url>`, set \
         `knowledge.sync.remote` in .cce/config, or pass --remote <url>"
            .to_string()
    })?;
    // LFS attribute writes only happen on push (see cmd_knowledge_push); open
    // plain here so pulls never mutate the cache.
    let remote = GitRemote::open(&url, false)?;
    Ok((remote, cfg.lfs))
}

/// Apply per-corpus retention after a push (SPEC-SYNC-KNOWLEDGE §4.5): with
/// `keep-last-<n>`, prune the oldest `<snapshot>.cck` keys beyond `n` — oldest
/// by the cache repo's commit history for the key (corpora have no sha ordering;
/// git history is the only order the cache itself carries). The snapshot named
/// by `current` is NEVER pruned regardless of `n`. Returns the pruned keys.
fn apply_retention(
    remote: &GitRemote,
    corpus_id: &str,
    current_snapshot: &str,
    retention: &Retention,
) -> Result<Vec<String>, String> {
    let Retention::KeepLast(n) = retention else {
        return Ok(Vec::new());
    };
    let ver = knowledge_contract_version();
    let prefix = format!("knowledge/{ver}/{corpus_id}");
    let keys = remote.list_keys_with_suffix(&prefix, ".cck")?;
    // Oldest first by first-added COMMIT ORDER (not timestamps — two pushes in
    // the same second still have a well-defined ancestry). A key somehow absent
    // from the walk sorts newest, so it is never pruned by a gap in history.
    let history = remote.first_added_order(&prefix)?;
    let position = |k: &str| history.iter().position(|h| h == k).unwrap_or(usize::MAX);
    let mut ordered: Vec<(usize, String)> = keys.into_iter().map(|k| (position(&k), k)).collect();
    ordered.sort();
    let keep_from = ordered.len().saturating_sub(*n);
    let current_key = knowledge_content_address(ver, corpus_id, current_snapshot);
    let prune: Vec<String> =
        ordered[..keep_from].iter().map(|(_, k)| k.clone()).filter(|k| *k != current_key).collect();
    remote.remove_many(&prune, &format!("cce knowledge sync: retention prune ({corpus_id})"))?;
    Ok(prune)
}

/// The published `corpus.json` bytes (SPEC-SYNC-KNOWLEDGE §4.4): sorted-keys,
/// pretty-printed, trailing newline — the house `--json` grammar. `pushed_at` is
/// deliberately OUTSIDE the artifact (it would break reproducibility).
fn corpus_meta_json(a: &KnowledgeArtifact, pushed_at: &str) -> String {
    let body = serde_json::json!({
        "schema": KNOWLEDGE_META_SCHEMA_ID,
        "corpus_id": a.manifest.corpus_id,
        "current": a.manifest.snapshot,
        "records": a.manifest.records,
        "chunk_count": a.manifest.chunk_count,
        "data_as_of": a.manifest.data_as_of,
        "pushed_at": pushed_at,
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// `cce knowledge push` (SPEC-SYNC-KNOWLEDGE §5): export the CURRENT local
/// knowledge store as a canonical `.cck`, put it at its content-addressed key,
/// advance the corpus `current` pointer, publish `corpus.json` — one commit/push
/// — then apply retention (best-effort: a prune failure warns, never fails the
/// push). Refuses: no local store; unresolved/invalid corpus_id; a store without
/// persisted embeddings. Best-effort and never blocks local work (§10).
pub fn cmd_knowledge_push(
    root: &Path,
    corpus_override: Option<String>,
    remote_override: Option<String>,
) -> Result<String, String> {
    let store = KnowledgeStore::load_current(root).map_err(|_| {
        format!(
            "no local knowledge store under {} (`current` missing) — run \
             `cce knowledge index <feed.jsonl>` first",
            KnowledgeStore::dir(root).display()
        )
    })?;
    let kcfg = KnowledgeSyncConfig::load(root);
    let corpus_id = resolve_corpus_id(corpus_override, &kcfg)?;
    // §2 export precondition: an embedding-less (Phase-A) store is refused here.
    let artifact = KnowledgeArtifact::from_store(&store, &corpus_id)?;
    let bytes = artifact.to_bytes();

    let (remote, lfs) = open_knowledge_remote(root, remote_override)?;
    if lfs {
        // §3: `*.cck` joins `*.cce` in the cache's `.gitattributes` (additive;
        // best-effort exactly like the code path's LFS setup).
        remote.ensure_knowledge_lfs()?;
    }

    let ver = knowledge_contract_version();
    let key = knowledge_content_address(ver, &corpus_id, &store.snapshot);
    let pointer_key = knowledge_pointer_address(ver, &corpus_id);
    let meta_key = knowledge_corpus_meta_address(ver, &corpus_id);
    let pushed_at = crate::metrics::Clock::now_iso(&crate::metrics::SystemClock);
    remote.put_many(&[
        (key.clone(), bytes),
        (pointer_key, format!("{}\n", store.snapshot).into_bytes()),
        (meta_key, corpus_meta_json(&artifact, &pushed_at).into_bytes()),
    ])?;

    let mut out = format!(
        "Pushed corpus {corpus_id}@{}\n  key        : {key}\n  checksum   : {}\n  \
         records    : {} · chunks : {}\n  data as-of : {}\n  pushed at  : {pushed_at}\n",
        store.snapshot,
        artifact.manifest.checksum,
        artifact.manifest.records,
        artifact.manifest.chunk_count,
        artifact.manifest.data_as_of.as_deref().unwrap_or("-"),
    );

    // §4.5: retention is push-side and best-effort — a prune failure warns and
    // never fails the push.
    match apply_retention(&remote, &corpus_id, &store.snapshot, &kcfg.retention) {
        Ok(pruned) if pruned.is_empty() => {}
        Ok(pruned) => out.push_str(&format!(
            "  retention  : pruned {} snapshot{} ({})\n",
            pruned.len(),
            if pruned.len() == 1 { "" } else { "s" },
            kcfg.retention.as_str()
        )),
        Err(e) => out.push_str(&format!("  warning: retention prune failed — {e}\n")),
    }
    Ok(out)
}

/// `cce knowledge pull` (SPEC-SYNC-KNOWLEDGE §5): fetch the corpus's current
/// snapshot (`--latest` is the explicit spelling of the default; `--snapshot`
/// pins one), verify the manifest checksum (a mismatch is a hard failure naming
/// the key), install into `<root>/.cce/knowledge/` exactly as a local ingest
/// would, and record the sync marker. Pulling a **different corpus** than the
/// marker records refuses without `--force`; a newer snapshot of the same corpus
/// supersedes silently (local re-ingest semantics).
pub fn cmd_knowledge_pull(
    root: &Path,
    corpus_override: Option<String>,
    snapshot_override: Option<String>,
    force: bool,
    remote_override: Option<String>,
) -> Result<String, String> {
    let kcfg = KnowledgeSyncConfig::load(root);
    let corpus_id = resolve_corpus_id(corpus_override, &kcfg)?;
    let (remote, _lfs) = open_knowledge_remote(root, remote_override)?;

    let ver = knowledge_contract_version();
    let snapshot = match snapshot_override {
        Some(s) => s,
        None => {
            let pointer = knowledge_pointer_address(ver, &corpus_id);
            remote.read_blob_text(&pointer).map_err(|_| {
                format!(
                    "no `current` pointer for corpus {corpus_id} on the remote — has anything \
                     been pushed? (`cce knowledge push --corpus {corpus_id}`)"
                )
            })?
        }
    };

    // §5 overwrite guard: never silently replace a DIFFERENT corpus. A newer
    // snapshot of the same corpus supersedes silently — local re-ingest
    // semantics, which is what makes refresh idempotent.
    if !force {
        if let Some(state) = KnowledgeSyncState::load(root) {
            if state.corpus_id != corpus_id {
                return Err(format!(
                    "the local knowledge store came from corpus `{}` but you are pulling \
                     `{corpus_id}`. Pass --force to replace it (one active corpus per root).",
                    state.corpus_id
                ));
            }
        }
    }

    let key = knowledge_content_address(ver, &corpus_id, &snapshot);
    let bytes = remote.get(&key)?;
    // Checksum verification happens inside from_bytes; name the key on failure.
    let artifact = KnowledgeArtifact::from_bytes(&bytes).map_err(|e| format!("{key}: {e}"))?;
    let checksum = artifact.manifest.checksum.clone();
    let (records, chunk_count) = (artifact.manifest.records, artifact.manifest.chunk_count);

    // Install = exactly what a local ingest writes: `<root>/.cce/knowledge/
    // <snapshot>.json` + the one-line `current` pointer (§7 byte-identity).
    let store = artifact.into_store();
    let store_path = store.save(root).map_err(|e| {
        format!("could not write the knowledge store under {}: {e}", root.display())
    })?;

    // #55 verbatim: hash the EXACT bytes just installed (read back from disk) so
    // a later checksum-only verify is version-independent. Best-effort: an
    // unreadable store leaves the field absent.
    let installed_sha256 = std::fs::read(&store_path).ok().map(|b| hex_lower(&Sha256::digest(&b)));
    KnowledgeSyncState {
        corpus_id: corpus_id.clone(),
        snapshot: snapshot.clone(),
        checksum: checksum.clone(),
        installed_sha256,
    }
    .save(root)
    .map_err(|e| format!("could not write the knowledge sync marker: {e}"))?;

    Ok(format!(
        "Pulled corpus {corpus_id}@{snapshot}\n  records  : {records} · chunks : {chunk_count}\n  \
         checksum : {checksum}\n  store    : {}\n",
        store_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{ingest_default, parse_ndjson};
    use crate::sync::git;
    use crate::sync::remote::SyncRemote;

    /// A bare git repo acting as the remote; returns (tempdir, file:// URL).
    fn bare_remote() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        git::run_commit(tmp.path(), &["init", "--bare", "-q", "-b", "main"]).unwrap();
        let url = format!("file://{}", tmp.path().to_string_lossy());
        (tmp, url)
    }

    /// Hold the env lock and point CCE_HOME at a temp dir (hermetic clones).
    struct HomeGuard {
        _home: tempfile::TempDir,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    fn with_home() -> HomeGuard {
        let lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        HomeGuard { _home: home, _lock: lock }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            std::env::remove_var("CCE_HOME");
        }
    }

    /// Ingest `feed` into a fresh project root; returns the root dir. LFS is
    /// disabled in the config so the core path needs no `git-lfs` binary (the
    /// tests/sync.rs hermeticity rule).
    fn root_with_store(feed: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
        std::fs::write(tmp.path().join(".cce").join("config"), "sync:\n  lfs: false\n").unwrap();
        let recs = parse_ndjson(feed).unwrap();
        let store = ingest_default(&recs, feed.as_bytes());
        store.save(tmp.path()).unwrap();
        tmp
    }

    fn feed(marker: &str) -> String {
        format!(
            "{{\"id\":\"kn:1\",\"title\":\"Note {marker}\",\"body\":\"Body {marker}.\",\
             \"source\":\"handbook\",\"updated_at\":\"2026-01-0{}T00:00:00Z\"}}\n",
            (marker.len() % 8) + 1
        )
    }

    #[test]
    fn push_refuses_without_a_local_store() {
        let _home = with_home();
        let tmp = tempfile::tempdir().unwrap();
        let err = cmd_knowledge_push(tmp.path(), Some("c1".into()), Some("file:///x".into()))
            .unwrap_err();
        assert!(err.contains("no local knowledge store"), "got: {err}");
    }

    #[test]
    fn push_refuses_missing_and_invalid_corpus_ids() {
        let _home = with_home();
        let root = root_with_store(&feed("a"));
        let err = cmd_knowledge_push(root.path(), None, None).unwrap_err();
        assert!(err.contains("cannot determine corpus_id"), "got: {err}");
        let err = cmd_knowledge_push(root.path(), Some("has space".into()), None).unwrap_err();
        assert!(err.contains("invalid corpus_id"), "got: {err}");
        // Validation happens before any remote is touched — no remote needed.
    }

    #[test]
    fn push_refuses_an_embedding_less_store() {
        let _home = with_home();
        let root = tempfile::tempdir().unwrap();
        let text = feed("a");
        let recs = parse_ndjson(&text).unwrap();
        let mut store = ingest_default(&recs, text.as_bytes());
        for c in &mut store.chunks {
            c.embedding = Vec::new();
        }
        store.save(root.path()).unwrap();
        let err = cmd_knowledge_push(root.path(), Some("c1".into()), None).unwrap_err();
        assert!(err.contains("Re-ingest"), "got: {err}");
    }

    #[test]
    fn push_without_a_remote_fails_cleanly_and_leaves_local_state() {
        let _home = with_home();
        let root = root_with_store(&feed("a"));
        let err = cmd_knowledge_push(root.path(), Some("c1".into()), None).unwrap_err();
        assert!(err.contains("no sync remote configured"), "got: {err}");
        // Local store untouched (§10 offline-first).
        assert!(KnowledgeStore::load_current(root.path()).is_ok());
    }

    #[test]
    fn push_lands_artifact_pointer_and_corpus_json_in_one_commit() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed("a"));
        let out = cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        assert!(out.contains("Pushed corpus c1@"), "got: {out}");

        let store = KnowledgeStore::load_current(root.path()).unwrap();
        let remote = GitRemote::open(&url, false).unwrap();
        let key = knowledge_content_address("v1", "c1", &store.snapshot);
        assert!(remote.has(&key).unwrap());
        assert_eq!(
            remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(),
            store.snapshot
        );
        let meta = remote.get(&knowledge_corpus_meta_address("v1", "c1")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&meta).unwrap();
        assert_eq!(v["schema"], KNOWLEDGE_META_SCHEMA_ID);
        assert_eq!(v["corpus_id"], "c1");
        assert_eq!(v["current"], store.snapshot);
        assert!(v["pushed_at"].as_str().unwrap().ends_with('Z'));
        // One commit: artifact + pointer + corpus.json (plus the clone's history).
        let bytes = remote.get(&key).unwrap();
        KnowledgeArtifact::from_bytes(&bytes).unwrap();
    }

    #[test]
    fn pull_installs_a_byte_identical_store_and_records_the_marker() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let producer = root_with_store(&feed("a"));
        cmd_knowledge_push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let store = KnowledgeStore::load_current(producer.path()).unwrap();
        let producer_bytes =
            std::fs::read(KnowledgeStore::snapshot_path(producer.path(), &store.snapshot)).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        let out =
            cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
                .unwrap();
        assert!(out.contains("Pulled corpus c1@"), "got: {out}");
        let consumer_bytes =
            std::fs::read(KnowledgeStore::snapshot_path(consumer.path(), &store.snapshot)).unwrap();
        assert_eq!(producer_bytes, consumer_bytes, "install must equal a local ingest");

        let marker = KnowledgeSyncState::load(consumer.path()).unwrap();
        assert_eq!(marker.corpus_id, "c1");
        assert_eq!(marker.snapshot, store.snapshot);
        assert_eq!(marker.checksum.len(), 64);
        let expected = hex_lower(&Sha256::digest(&consumer_bytes));
        assert_eq!(marker.installed_sha256.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn pull_refuses_a_different_corpus_without_force_and_supersedes_the_same() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let p1 = root_with_store(&feed("a"));
        cmd_knowledge_push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let p2 = root_with_store(&feed("bb"));
        cmd_knowledge_push(p2.path(), Some("c2".into()), Some(url.clone())).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
            .unwrap();
        // Different corpus: refused without --force.
        let err =
            cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, false, Some(url.clone()))
                .unwrap_err();
        assert!(err.contains("--force"), "got: {err}");
        // Same corpus, newer snapshot: supersedes silently.
        let newer = feed("ccc");
        let recs = parse_ndjson(&newer).unwrap();
        ingest_default(&recs, newer.as_bytes()).save(p1.path()).unwrap();
        cmd_knowledge_push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
            .unwrap();
        // --force replaces the corpus.
        cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, true, Some(url)).unwrap();
        assert_eq!(KnowledgeSyncState::load(consumer.path()).unwrap().corpus_id, "c2");
    }

    #[test]
    fn pull_pins_a_named_snapshot() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let producer = root_with_store(&feed("a"));
        cmd_knowledge_push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let first = KnowledgeStore::load_current(producer.path()).unwrap().snapshot;
        let newer = feed("bb");
        let recs = parse_ndjson(&newer).unwrap();
        ingest_default(&recs, newer.as_bytes()).save(producer.path()).unwrap();
        cmd_knowledge_push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        cmd_knowledge_pull(
            consumer.path(),
            Some("c1".into()),
            Some(first.clone()),
            false,
            Some(url),
        )
        .unwrap();
        assert_eq!(KnowledgeSyncState::load(consumer.path()).unwrap().snapshot, first);
        assert_eq!(
            KnowledgeStore::load_current(consumer.path()).unwrap().snapshot,
            first,
            "the pinned snapshot becomes current"
        );
    }

    #[test]
    fn pull_fails_loudly_on_a_tampered_artifact_naming_the_key() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let producer = root_with_store(&feed("a"));
        cmd_knowledge_push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let snapshot = KnowledgeStore::load_current(producer.path()).unwrap().snapshot;

        // Tamper with the artifact in place (flip a content byte, keep the shape).
        let remote = GitRemote::open(&url, false).unwrap();
        let key = knowledge_content_address("v1", "c1", &snapshot);
        let text = String::from_utf8(remote.get(&key).unwrap()).unwrap();
        let tampered = text.replace("Body", "Tamp");
        remote.put(&key, tampered.as_bytes()).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        let err = cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url))
            .unwrap_err();
        assert!(err.contains(&key), "the failure names the key, got: {err}");
        assert!(err.contains("checksum mismatch"), "got: {err}");
        // Nothing was installed.
        assert!(KnowledgeStore::load_current(consumer.path()).is_err());
        assert!(KnowledgeSyncState::load(consumer.path()).is_none());
    }

    #[test]
    fn retention_prunes_the_oldest_and_never_current() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = tempfile::tempdir().unwrap();
        // knowledge.sync config: keep-last-2 via the config file.
        std::fs::create_dir_all(root.path().join(".cce")).unwrap();
        std::fs::write(
            root.path().join(".cce").join("config"),
            "sync:\n  lfs: false\nknowledge:\n  sync:\n    corpus_id: c1\n    retention: keep-last-2\n",
        )
        .unwrap();

        let mut snapshots = Vec::new();
        for marker in ["a", "bb", "ccc", "dddd"] {
            let text = feed(marker);
            let recs = parse_ndjson(&text).unwrap();
            let store = ingest_default(&recs, text.as_bytes());
            store.save(root.path()).unwrap();
            snapshots.push(store.snapshot.clone());
            cmd_knowledge_push(root.path(), None, Some(url.clone())).unwrap();
        }

        let remote = GitRemote::open(&url, false).unwrap();
        let keys = remote.list_keys_with_suffix("knowledge/v1/c1", ".cck").unwrap();
        assert_eq!(keys.len(), 2, "keep-last-2 leaves two snapshots, got {keys:?}");
        let current = knowledge_content_address("v1", "c1", &snapshots[3]);
        assert!(keys.contains(&current), "current survives retention");
        let oldest = knowledge_content_address("v1", "c1", &snapshots[0]);
        assert!(!keys.contains(&oldest), "the oldest snapshot is pruned");
        // The pointer + corpus.json survive pruning.
        assert_eq!(
            remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(),
            snapshots[3]
        );
        assert!(remote.has(&knowledge_corpus_meta_address("v1", "c1")).unwrap());
    }

    #[test]
    fn corpus_meta_json_is_sorted_pretty_with_trailing_newline() {
        let text = feed("a");
        let recs = parse_ndjson(&text).unwrap();
        let store = ingest_default(&recs, text.as_bytes());
        let a = KnowledgeArtifact::from_store(&store, "c1").unwrap();
        let json = corpus_meta_json(&a, "2026-07-08T03:00:00Z");
        assert!(json.ends_with("}\n"));
        let keys: Vec<usize> = [
            "\"chunk_count\"",
            "\"corpus_id\"",
            "\"current\"",
            "\"data_as_of\"",
            "\"pushed_at\"",
            "\"records\"",
            "\"schema\"",
        ]
        .iter()
        .map(|k| json.find(k).unwrap())
        .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted, "corpus.json keys are sorted");
    }

    #[test]
    fn marker_json_shape_is_the_spec_field_order() {
        let s = KnowledgeSyncState {
            corpus_id: "c1".into(),
            snapshot: "9f1c2a3b4c5d6e7f".into(),
            checksum: "abc".into(),
            installed_sha256: Some("def".into()),
        };
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            "{\"corpus_id\":\"c1\",\"snapshot\":\"9f1c2a3b4c5d6e7f\",\"checksum\":\"abc\",\
             \"installed_sha256\":\"def\"}"
        );
        // Additive: an older marker without installed_sha256 still parses.
        let old: KnowledgeSyncState =
            serde_json::from_str("{\"corpus_id\":\"c1\",\"snapshot\":\"s\",\"checksum\":\"c\"}")
                .unwrap();
        assert_eq!(old.installed_sha256, None);
    }
}
