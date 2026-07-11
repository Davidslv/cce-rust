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
//! a canonical `.cck`, runs the §5 push guard — diff the outgoing record-id set
//! against the remote's current snapshot and refuse a push that would DROP
//! records without `--force` (`--dry-run` prints the diff and pushes nothing;
//! #90) — then puts the artifact at its content-addressed key, advances the
//! corpus `current` pointer, and publishes `corpus.json` — one commit/push
//! (`put_many`) — then applies retention (§4.5, best-effort).
//! `cmd_knowledge_pull` fetches the corpus's current (or a pinned) snapshot,
//! verifies the manifest checksum, installs it into `<root>/.cce/knowledge/`
//! **exactly as a local ingest would** (§7 byte-identity), and records the
//! knowledge sync marker with `installed_sha256` (the #55 mechanism verbatim).
//!
//! **Responsibilities:**
//! - Own the §4.1 corpus_id resolution/validation and the §4.3 remote resolution
//!   (`--remote` > `knowledge.sync.remote` > `sync.remote`).
//! - Own the `.cce/knowledge/synced.json` marker, the §5 pull overwrite guard,
//!   and the §5 push shrink guard (#90).
//! - Offline-first (§10): every failure here is a clean message; local ingest and
//!   search are never touched by a failed sync.
//! - It does NOT define the `.cck` bytes (that is `knowledge_artifact`) nor parse
//!   CLI args (main.rs).

use crate::knowledge::store::{KnowledgeChunk, KnowledgeStore};
use crate::sync::config::{KnowledgeSyncConfig, Retention, SyncConfig};
use crate::sync::knowledge_artifact::KnowledgeArtifact;
use crate::sync::remote::{GitRemote, SyncRemote};
use crate::sync::{
    hex_lower, knowledge_content_address, knowledge_contract_version,
    knowledge_corpus_meta_address, knowledge_pointer_address, valid_corpus_id,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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

    /// Load the marker, if a pull ever wrote one. Lenient: a read/parse error is
    /// treated as "no marker" — used by the best-effort freshness summary where a
    /// missing answer is acceptable. The §5 overwrite guard uses
    /// [`load_strict`](Self::load_strict) instead.
    pub fn load(root: &Path) -> Option<KnowledgeSyncState> {
        let text = std::fs::read_to_string(Self::path(root)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Load the marker for the §5 overwrite guard (#123), distinguishing a
    /// genuinely **absent** marker (`Ok(None)`, never pulled) from one that is
    /// present but **unreadable/corrupt** (`Err`). The lenient `.load()` maps a
    /// corrupt marker to `None` — indistinguishable from "never pulled" — which
    /// silently DISARMED the different-corpus guard, letting a truncated marker
    /// wave through an overwrite the guard exists to refuse. A corrupt marker is
    /// now an error the caller surfaces (bypassable only with `--force`).
    pub fn load_strict(root: &Path) -> Result<Option<KnowledgeSyncState>, String> {
        let path = Self::path(root);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => {
                Err(format!("could not read the knowledge sync marker {}: {e}", path.display()))
            }
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
                format!(
                    "the knowledge sync marker {} is unreadable/corrupt: {e}. It records which \
                     corpus is active — pass --force to overwrite it.",
                    path.display()
                )
            }),
        }
    }

    fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // #123: propagate a serialization failure rather than writing an empty
        // string (`unwrap_or_default`) that would later parse as a corrupt marker.
        let json = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

/// A read-only knowledge freshness summary for MCP `index_status`
/// (SPEC-SYNC-KNOWLEDGE §4.4) — the knowledge sibling of
/// `sync::commands::Freshness`. Pure of side effects; the remote lookup is
/// best-effort and offline-safe (any error leaves `remote_current = None`,
/// `behind_remote = false`).
#[derive(Debug, Clone)]
pub struct KnowledgeFreshness {
    /// The §4.1 corpus identity when the current store came from a pull whose
    /// marker still matches; `None` = a local ingest (a local `cce knowledge
    /// index` after a pull supersedes the pulled snapshot, §5).
    pub corpus: Option<String>,
    /// The current store's snapshot id.
    pub snapshot: String,
    /// Source records in the current store.
    pub records: usize,
    /// Chunks in the current store.
    pub chunks: usize,
    /// The deterministic data age (§2/§4.4: max `updated_at` across chunks) —
    /// computable locally from any installed store.
    pub data_as_of: Option<String>,
    /// The remote corpus pointer's snapshot, if reachable.
    pub remote_current: Option<String>,
    /// True only when both snapshots are known and differ.
    pub behind_remote: bool,
}

/// Summarise the local knowledge store's freshness for `root` (§4.4). `None`
/// when no knowledge store exists — `index_status` then stays byte-identical.
/// The remote lookup mirrors the code `freshness()` rules exactly: attempted
/// only when a remote is configured, best-effort, never blocking, never an
/// error — MCP's `index_status` must always answer.
pub fn knowledge_freshness(root: &Path) -> Option<KnowledgeFreshness> {
    let store = KnowledgeStore::load_current(root).ok()?;
    let marker = KnowledgeSyncState::load(root);
    let corpus =
        marker.as_ref().filter(|m| m.snapshot == store.snapshot).map(|m| m.corpus_id.clone());
    let data_as_of = crate::sync::knowledge_artifact::data_as_of(&store.chunks);

    // Mirror the code freshness: no remote configured ⇒ no network at all. The
    // pointer needs a corpus identity — the marker's, else the config's (never
    // derived, §4.1); without one there is nothing to look up.
    let kcfg = KnowledgeSyncConfig::load(root);
    let cfg = SyncConfig::load(root);
    let remote_current = if kcfg.remote.is_some() || cfg.remote.is_some() {
        marker.as_ref().map(|m| m.corpus_id.clone()).or_else(|| kcfg.corpus_id.clone()).and_then(
            |corpus_id| {
                open_knowledge_remote(root, None).ok().and_then(|(remote, _)| {
                    remote
                        .read_blob_text(&knowledge_pointer_address(
                            knowledge_contract_version(),
                            &corpus_id,
                        ))
                        .ok()
                        .filter(|s| !s.is_empty())
                })
            },
        )
    } else {
        None
    };
    let behind_remote = matches!(&remote_current, Some(r) if *r != store.snapshot);
    Some(KnowledgeFreshness {
        corpus,
        snapshot: store.snapshot.clone(),
        records: store.records,
        chunks: store.chunks.len(),
        data_as_of,
        remote_current,
        behind_remote,
    })
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
            "invalid corpus_id `{id}`: must be non-empty, charset [A-Za-z0-9._-], and a \
             single path segment — `.` and `..` are rejected (it is a path segment on \
             the cache, so a traversal token would escape the corpus namespace)"
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
    confine_to_corpus(&prune, ver, corpus_id)?;
    remote.remove_many(&prune, &format!("cce knowledge sync: retention prune ({corpus_id})"))?;
    Ok(prune)
}

/// Release-present confinement guard for the retention delete site (#121). This
/// is the destructive seam: every key about to be pruned MUST be a direct child
/// of `knowledge/<ver>/<corpus_id>/` — i.e. its 3rd path segment equals the
/// resolved `corpus_id`. Unlike a `debug_assert!` (compiled out of release
/// binaries) this holds in shipped builds, and it checks the actual key SET
/// rather than the id string, so it catches a broadened enumeration even if a
/// traversal id slipped past `valid_corpus_id`: under the original `..` bug the
/// enumerated keys were `knowledge/v1/prod/…` whose 3rd segment (`prod`) ≠ the
/// resolved id `..`, so this refuses the cross-corpus delete on its own. A key
/// outside the prefix is a should-never-happen invariant breach — hard error,
/// never a silent skip.
fn confine_to_corpus(keys: &[String], ver: &str, corpus_id: &str) -> Result<(), String> {
    for key in keys {
        let mut segs = key.split('/');
        let confined = segs.next() == Some("knowledge")
            && segs.next() == Some(ver)
            && segs.next() == Some(corpus_id);
        if !confined {
            return Err(format!(
                "retention refused: key `{key}` is not confined to \
                 `knowledge/{ver}/{corpus_id}/` — refusing to prune outside the corpus \
                 namespace (should-never-happen invariant breach; see #121)"
            ));
        }
    }
    Ok(())
}

/// Fetch the `.cck` at `corpus_id@snapshot` and verify the manifest checksum
/// (SPEC-SYNC-KNOWLEDGE §5) — the shared fetch machinery of `pull` and the
/// push guard. Checksum verification happens inside `from_bytes`; any failure
/// names the key.
fn fetch_verified_artifact(
    remote: &GitRemote,
    corpus_id: &str,
    snapshot: &str,
) -> Result<KnowledgeArtifact, String> {
    let key = knowledge_content_address(knowledge_contract_version(), corpus_id, snapshot);
    let bytes = remote.get(&key)?;
    KnowledgeArtifact::from_bytes(&bytes).map_err(|e| format!("{key}: {e}"))
}

/// The record-level diff between the outgoing store and the remote current
/// snapshot (§5 push guard, #90), over DISTINCT `record_id` sets: `added` = ids
/// only in the outgoing store; `removed` = ids only on the remote; `changed` =
/// ids on both sides whose rendered content (title + body, per `record_digests`)
/// differs — facet-only edits (state/labels/url/updated_at) do not render into
/// chunk content and do not register. Every list is lexicographically sorted —
/// the report is deterministic.
struct RecordDiff {
    added: Vec<String>,
    removed: Vec<String>,
    changed: Vec<String>,
}

/// `record_id` → SHA-256 over the record's chunks' FULL `content`, in store
/// (chunk) order, each chunk length-prefixed so chunk boundaries are
/// unambiguous. Deliberately not the chunk_id set: a chunk_id hashes only a
/// content PREFIX (`chunker::chunk_id`), so an edit past that prefix within an
/// unchanged line span would not re-key — the digest sees every byte. (BTree ⇒
/// sorted iteration for free.)
fn record_digests(chunks: &[KnowledgeChunk]) -> BTreeMap<&str, [u8; 32]> {
    let mut hashers: BTreeMap<&str, Sha256> = BTreeMap::new();
    for c in chunks {
        let h = hashers.entry(c.record_id.as_str()).or_default();
        h.update((c.content.len() as u64).to_le_bytes());
        h.update(c.content.as_bytes());
    }
    hashers.into_iter().map(|(id, h)| (id, h.finalize().into())).collect()
}

fn diff_records(local: &[KnowledgeChunk], remote: &[KnowledgeChunk]) -> RecordDiff {
    let l = record_digests(local);
    let r = record_digests(remote);
    let mut diff = RecordDiff { added: Vec::new(), removed: Vec::new(), changed: Vec::new() };
    for (id, digest) in &l {
        match r.get(id) {
            None => diff.added.push((*id).to_string()),
            Some(remote_digest) if remote_digest != digest => diff.changed.push((*id).to_string()),
            Some(_) => {}
        }
    }
    for id in r.keys() {
        if !l.contains_key(id) {
            diff.removed.push((*id).to_string());
        }
    }
    diff
}

/// Ids listed per diff line before eliding with "… and N more".
const DIFF_IDS_LISTED: usize = 20;

/// `"0"`, or `"<n> — id1, id2"` (elided past `DIFF_IDS_LISTED`).
fn format_id_list(ids: &[String]) -> String {
    if ids.is_empty() {
        return "0".to_string();
    }
    let shown = ids.iter().take(DIFF_IDS_LISTED).map(String::as_str).collect::<Vec<_>>().join(", ");
    if ids.len() > DIFF_IDS_LISTED {
        format!("{} — {shown} … and {} more", ids.len(), ids.len() - DIFF_IDS_LISTED)
    } else {
        format!("{} — {shown}", ids.len())
    }
}

/// The human diff report (the house aligned `key : value` grammar), shared by
/// `--dry-run` and the shrink refusal.
fn diff_report(
    corpus_id: &str,
    local_snapshot: &str,
    local_records: usize,
    remote_snapshot: &str,
    remote_records: usize,
    diff: &RecordDiff,
) -> String {
    format!(
        "Corpus {corpus_id} — outgoing {local_snapshot} vs remote current {remote_snapshot}\n  \
         records : {local_records} local · {remote_records} remote\n  \
         added   : {}\n  removed : {}\n  changed : {}\n",
        format_id_list(&diff.added),
        format_id_list(&diff.removed),
        format_id_list(&diff.changed),
    )
}

/// The §5 push guard (#90). Reads the remote `current` pointer and decides
/// whether the push may proceed:
/// - pointer absent ⇒ first publish, nothing to diff — proceed silently;
/// - pointer == the outgoing snapshot ⇒ idempotent re-publish — proceed;
/// - pointer differs ⇒ fetch + checksum-verify the remote current (the pull
///   machinery) and diff record ids: a non-empty `removed` set refuses with the
///   diff (a push must never silently drop published records); adds/changes
///   never block and produce no new output.
///
/// Push already requires a reachable remote, so a failed pointer READ is a hard
/// failure (consistent with `put_many`); so is a remote current that exists but
/// cannot be fetched or verified — `--force` (checked at the call site) is the
/// only bypass. An empty or whitespace-only pointer blob is deliberately
/// treated as absent (first publish), mirroring `knowledge_freshness`'s
/// empty-pointer filter. Returns `Ok(Some(report))` when `dry_run` (the caller
/// prints it and pushes nothing), `Ok(None)` when the push may proceed.
fn push_guard(
    remote: &GitRemote,
    corpus_id: &str,
    store: &KnowledgeStore,
    outgoing_checksum: &str,
    dry_run: bool,
) -> Result<Option<String>, String> {
    let pointer_key = knowledge_pointer_address(knowledge_contract_version(), corpus_id);
    let remote_snapshot = if remote.has(&pointer_key)? {
        Some(remote.read_blob_text(&pointer_key)?).filter(|s| !s.is_empty())
    } else {
        None
    };
    let Some(remote_snapshot) = remote_snapshot else {
        // First publish: nothing to diff, push proceeds exactly as before #90.
        return Ok(dry_run.then(|| {
            format!(
                "Dry-run: corpus {corpus_id}@{} — no remote `current` pointer; this would be \
                 the first publish ({} records · {} chunks).\nNothing pushed (--dry-run).\n",
                store.snapshot,
                store.records,
                store.chunks.len()
            )
        }));
    };
    if remote_snapshot == store.snapshot {
        // #113: the snapshot id is only a hash of the FEED bytes, but the
        // published `.cck` also depends on `markdown.max_section_tokens` and the
        // redactor version — two producers with a byte-identical feed but
        // divergent config yield the SAME snapshot id over DIFFERENT content.
        // So a matching id does NOT prove identical content: fetch + verify the
        // remote current and compare manifest checksums before treating this as
        // an unchanged re-publish. Divergent bytes at the same content-addressed
        // key would silently overwrite what consumers already recorded.
        let current =
            fetch_verified_artifact(remote, corpus_id, &remote_snapshot).map_err(|e| {
                let hint = if dry_run {
                    "there is no diff to report (--dry-run). A real `--force` push (without \
                     --dry-run) would bypass the guard."
                } else {
                    "Refusing to replace a corpus the guard cannot read. Pass --force to push \
                     without the guard."
                };
                format!(
                    "push guard: could not verify the remote's current snapshot \
                     ({corpus_id}@{remote_snapshot}) — {e}\n{hint}"
                )
            })?;
        if current.manifest.checksum == outgoing_checksum {
            // Byte-identical re-publish — truly unchanged, proceed as before.
            return Ok(dry_run.then(|| {
                format!(
                    "Dry-run: corpus {corpus_id}@{} — the remote `current` already names this \
                     snapshot; a push would re-publish it unchanged.\nNothing pushed \
                     (--dry-run).\n",
                    store.snapshot
                )
            }));
        }
        // Same snapshot id, DIFFERENT content — the #113 collision.
        let msg = format!(
            "corpus {corpus_id}@{} is already published with DIFFERENT content (remote checksum \
             {}, outgoing {outgoing_checksum}). The snapshot id hashes only the feed bytes, so a \
             different `markdown.max_section_tokens` or redactor version produced divergent \
             `.cck` bytes at the same content-addressed key",
            store.snapshot, current.manifest.checksum,
        );
        if dry_run {
            return Ok(Some(format!(
                "Dry-run: {msg}.\nA real `--force` push would overwrite it.\nNothing pushed \
                 (--dry-run).\n"
            )));
        }
        return Err(format!(
            "push guard: {msg}. Pass --force to overwrite the published snapshot."
        ));
    }
    let current = fetch_verified_artifact(remote, corpus_id, &remote_snapshot).map_err(|e| {
        // Flag-aware bypass hint: under --dry-run the user may ALREADY have
        // passed --force (dry-run still diffs), so "pass --force" would name a
        // flag they gave — point at a real push instead.
        let hint = if dry_run {
            "there is no diff to report (--dry-run). A real `--force` push (without --dry-run) \
             would bypass the guard."
        } else {
            "Refusing to replace a corpus the guard cannot read. Pass --force to push without \
             the guard."
        };
        format!(
            "push guard: could not verify the remote's current snapshot \
             ({corpus_id}@{remote_snapshot}) — {e}\n{hint}"
        )
    })?;
    let diff = diff_records(&store.chunks, &current.chunks);
    let report = diff_report(
        corpus_id,
        &store.snapshot,
        store.records,
        &remote_snapshot,
        current.manifest.records,
        &diff,
    );
    if dry_run {
        return Ok(Some(format!("{report}Nothing pushed (--dry-run).\n")));
    }
    if !diff.removed.is_empty() {
        return Err(format!(
            "{report}refusing to push: {} record{} live on the remote would be dropped (the \
             `removed` list above). Pass --force to shrink corpus {corpus_id}, or --dry-run to \
             inspect without pushing.",
            diff.removed.len(),
            if diff.removed.len() == 1 { "" } else { "s" },
        ));
    }
    Ok(None)
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
/// knowledge store as a canonical `.cck`, run the §5 push guard (#90; see
/// `push_guard` — `force` skips it entirely, `dry_run` prints the diff and
/// pushes nothing), put the artifact at its content-addressed key, advance the
/// corpus `current` pointer, publish `corpus.json` — one commit/push — then
/// apply retention (best-effort: a prune failure warns, never fails the push).
/// Refuses: no local store; unresolved/invalid corpus_id; a store without
/// persisted embeddings; a push that would drop remote-live records (without
/// `--force`). Best-effort and never blocks local work (§10).
pub fn cmd_knowledge_push(
    root: &Path,
    corpus_override: Option<String>,
    remote_override: Option<String>,
    force: bool,
    dry_run: bool,
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

    // §5 push guard (#90) — BEFORE any remote mutation (the LFS attribute write
    // included), so `--dry-run` provably touches nothing. `--force` skips the
    // diff entirely and pushes as before the guard existed.
    if dry_run || !force {
        if let Some(report) =
            push_guard(&remote, &corpus_id, &store, &artifact.manifest.checksum, dry_run)?
        {
            return Ok(report);
        }
    }

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
        // #123: a corrupt marker is an error here, NOT a silent None that would
        // disarm the guard and overwrite a different corpus without --force.
        if let Some(state) = KnowledgeSyncState::load_strict(root)? {
            if state.corpus_id != corpus_id {
                return Err(format!(
                    "the local knowledge store came from corpus `{}` but you are pulling \
                     `{corpus_id}`. Pass --force to replace it (one active corpus per root).",
                    state.corpus_id
                ));
            }
        }
    }

    let artifact = fetch_verified_artifact(&remote, &corpus_id, &snapshot)?;
    let checksum = artifact.manifest.checksum.clone();
    let (records, chunk_count) = (artifact.manifest.records, artifact.manifest.chunk_count);

    // Install = exactly what a local ingest writes: `<root>/.cce/knowledge/
    // <snapshot>.json` + the one-line `current` pointer (§7 byte-identity).
    //
    // #122: advance `current` LAST — write the snapshot artifact, then the sync
    // marker, and only then repoint `current`. Previously `store.save` moved
    // `current` (activating the new store) BEFORE the marker was written, so a
    // marker-write failure returned Err while the active store had already been
    // replaced — the guard then misfired both ways on the stale marker. With the
    // pointer moved last, any failure before it leaves the prior store active and
    // its marker consistent; a re-pull of the intended corpus simply succeeds.
    let store = artifact.into_store();
    let store_path = store.write_snapshot(root).map_err(|e| {
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

    // Marker durable — now activate the freshly written snapshot.
    KnowledgeStore::advance_current(root, &store.snapshot).map_err(|e| {
        format!("could not advance the knowledge `current` pointer under {}: {e}", root.display())
    })?;

    Ok(format!(
        "Pulled corpus {corpus_id}@{snapshot}\n  records  : {records} · chunks : {chunk_count}\n  \
         checksum : {checksum}\n  store    : {}\n",
        store_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::store::ingest;
    use crate::knowledge::{ingest_default, parse_ndjson};
    use crate::sync::git;
    use crate::sync::remote::SyncRemote;

    /// Ingest `feed` at an explicit split budget into a fresh root (LFS off), so
    /// a test can produce two stores from the SAME feed bytes but DIFFERENT
    /// `max_section_tokens` — the #113 same-snapshot-id / divergent-content case.
    fn root_with_store_budget(feed: &str, budget: usize) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".cce")).unwrap();
        std::fs::write(tmp.path().join(".cce").join("config"), "sync:\n  lfs: false\n").unwrap();
        let recs = parse_ndjson(feed).unwrap();
        ingest(&recs, feed.as_bytes(), budget).save(tmp.path()).unwrap();
        tmp
    }

    /// `cmd_knowledge_push` with the pre-#90 default flags (no force, no
    /// dry-run) — the shape every pre-guard test exercises.
    fn push(root: &Path, corpus: Option<String>, remote: Option<String>) -> Result<String, String> {
        cmd_knowledge_push(root, corpus, remote, false, false)
    }

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
        let err = push(tmp.path(), Some("c1".into()), Some("file:///x".into())).unwrap_err();
        assert!(err.contains("no local knowledge store"), "got: {err}");
    }

    #[test]
    fn push_and_pull_reject_traversal_corpus_ids_before_any_remote_operation() {
        let _home = with_home();
        let root = root_with_store(&feed("a"));
        // #121: `--corpus ..` used to pass validation and become the path
        // segment `knowledge/v1/..` on the cache — escaping the corpus
        // namespace and letting retention prune delete OTHER corpora's
        // artifacts. The rejection must fire BEFORE any filesystem or remote
        // operation: the remote URL below points nowhere, so reaching it
        // (clone attempt) would surface a different error than the one asserted.
        for id in ["..", "."] {
            let err =
                push(root.path(), Some(id.into()), Some("file:///nonexistent".into())).unwrap_err();
            assert!(err.contains("invalid corpus_id"), "push `{id}` got: {err}");
            let err = cmd_knowledge_pull(
                root.path(),
                Some(id.into()),
                None,
                false,
                Some("file:///nonexistent".into()),
            )
            .unwrap_err();
            assert!(err.contains("invalid corpus_id"), "pull `{id}` got: {err}");
        }
    }

    #[test]
    fn retention_refuses_to_prune_a_foreign_corpus_key() {
        // #121 release-present delete-site guard: even if a traversal id somehow
        // reached retention, the enumerated key set carries the real corpus's
        // path segment (`prod`), which does not equal the resolved id (`..`), so
        // the confinement check refuses the cross-corpus delete. This test pins
        // the guard in RELEASE (a debug_assert would not run here).
        let foreign = vec![
            "knowledge/v1/prod/aaaa.cck".to_string(),
            "knowledge/v1/prod/bbbb.cck".to_string(),
        ];
        let err = confine_to_corpus(&foreign, "v1", "..").unwrap_err();
        assert!(err.contains("not confined"), "got: {err}");
        assert!(err.contains("knowledge/v1/prod/aaaa.cck"), "names the offending key: {err}");
        // A key whose 3rd segment is a mere PREFIX of the id is also foreign
        // (segment-exact, not starts_with): `production` ≠ `prod`.
        let sibling = vec!["knowledge/v1/production/cccc.cck".to_string()];
        assert!(confine_to_corpus(&sibling, "v1", "prod").is_err());
        // The honest, in-namespace key set passes unchanged.
        let own = vec!["knowledge/v1/prod/dddd.cck".to_string()];
        assert!(confine_to_corpus(&own, "v1", "prod").is_ok());
        // Empty prune set is trivially confined.
        assert!(confine_to_corpus(&[], "v1", "prod").is_ok());
    }

    #[test]
    fn push_refuses_missing_and_invalid_corpus_ids() {
        let _home = with_home();
        let root = root_with_store(&feed("a"));
        let err = push(root.path(), None, None).unwrap_err();
        assert!(err.contains("cannot determine corpus_id"), "got: {err}");
        let err = push(root.path(), Some("has space".into()), None).unwrap_err();
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
        let err = push(root.path(), Some("c1".into()), None).unwrap_err();
        assert!(err.contains("Re-ingest"), "got: {err}");
    }

    #[test]
    fn push_without_a_remote_fails_cleanly_and_leaves_local_state() {
        let _home = with_home();
        let root = root_with_store(&feed("a"));
        let err = push(root.path(), Some("c1".into()), None).unwrap_err();
        assert!(err.contains("no sync remote configured"), "got: {err}");
        // Local store untouched (§10 offline-first).
        assert!(KnowledgeStore::load_current(root.path()).is_ok());
    }

    #[test]
    fn push_lands_artifact_pointer_and_corpus_json_in_one_commit() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed("a"));
        let out = push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
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
        push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
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
        push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let p2 = root_with_store(&feed("bb"));
        push(p2.path(), Some("c2".into()), Some(url.clone())).unwrap();

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
        push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
            .unwrap();
        // --force replaces the corpus.
        cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, true, Some(url)).unwrap();
        assert_eq!(KnowledgeSyncState::load(consumer.path()).unwrap().corpus_id, "c2");
    }

    #[test]
    fn pull_marker_write_failure_leaves_the_active_store_unchanged() {
        // #122: the pull used to advance `current` (activating the new corpus)
        // BEFORE writing the marker, so a marker-write failure replaced the
        // active store yet reported failure. `current` must move LAST — a marker
        // failure must leave the prior store active.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let p1 = root_with_store(&feed("a"));
        push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let snap1 = KnowledgeStore::load_current(p1.path()).unwrap().snapshot;
        let p2 = root_with_store(&feed("bb"));
        push(p2.path(), Some("c2".into()), Some(url.clone())).unwrap();
        let snap2 = KnowledgeStore::load_current(p2.path()).unwrap().snapshot;
        assert_ne!(snap1, snap2);

        let consumer = tempfile::tempdir().unwrap();
        cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
            .unwrap();
        assert_eq!(KnowledgeStore::load_current(consumer.path()).unwrap().snapshot, snap1);

        // Wedge the marker write: replace synced.json with a non-empty directory
        // (fails a bare `fs::write` AND an atomic temp+rename over it).
        let marker = KnowledgeSyncState::path(consumer.path());
        std::fs::remove_file(&marker).unwrap();
        std::fs::create_dir(&marker).unwrap();
        std::fs::write(marker.join("wedge"), b"x").unwrap();

        // Pull c2 --force: the marker write fails, so the pull errors …
        let err = cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, true, Some(url))
            .unwrap_err();
        assert!(err.contains("sync marker"), "got: {err}");
        // … but the active store is still c1's snapshot — NOT replaced.
        assert_eq!(
            KnowledgeStore::load_current(consumer.path()).unwrap().snapshot,
            snap1,
            "current must not have advanced when the marker write failed"
        );
    }

    #[test]
    fn a_corrupt_marker_does_not_disarm_the_different_corpus_guard() {
        // #123: `load` mapped any read/parse error to None, indistinguishable
        // from "never pulled", so a truncated/empty/corrupt marker silently
        // disarmed the §5 guard and let a different corpus overwrite the active
        // one without --force. A corrupt marker must now be an error.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let p1 = root_with_store(&feed("a"));
        push(p1.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let p2 = root_with_store(&feed("bb"));
        push(p2.path(), Some("c2".into()), Some(url.clone())).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        cmd_knowledge_pull(consumer.path(), Some("c1".into()), None, false, Some(url.clone()))
            .unwrap();
        let active = KnowledgeStore::load_current(consumer.path()).unwrap().snapshot;

        // Corrupt the marker (an interrupted non-atomic write / disk-full / edit).
        std::fs::write(KnowledgeSyncState::path(consumer.path()), b"").unwrap();

        // A different-corpus pull WITHOUT --force must refuse, not overwrite.
        let err =
            cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, false, Some(url.clone()))
                .unwrap_err();
        assert!(err.contains("corrupt"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
        // Nothing was overwritten — c1 is still active.
        assert_eq!(KnowledgeStore::load_current(consumer.path()).unwrap().snapshot, active);

        // --force bypasses the guard and re-writes a valid marker.
        cmd_knowledge_pull(consumer.path(), Some("c2".into()), None, true, Some(url)).unwrap();
        assert_eq!(KnowledgeSyncState::load(consumer.path()).unwrap().corpus_id, "c2");
    }

    #[test]
    fn pull_pins_a_named_snapshot() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let producer = root_with_store(&feed("a"));
        push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let first = KnowledgeStore::load_current(producer.path()).unwrap().snapshot;
        let newer = feed("bb");
        let recs = parse_ndjson(&newer).unwrap();
        ingest_default(&recs, newer.as_bytes()).save(producer.path()).unwrap();
        push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();

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
        push(producer.path(), Some("c1".into()), Some(url.clone())).unwrap();
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
            push(root.path(), None, Some(url.clone())).unwrap();
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
    fn republishing_same_snapshot_id_with_divergent_content_is_refused() {
        // #113: the snapshot id hashes only the feed bytes, so two producers with
        // a byte-identical feed but different `markdown.max_section_tokens` share
        // one id over different `.cck` content. The guard used to short-circuit on
        // pointer==snapshot and silently overwrite the content-addressed key; it
        // must now detect the checksum divergence and refuse without --force.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let sectioned = "{\"id\":\"kn:1\",\"title\":\"Note\",\"body\":\"## A\\n\\nAlpha.\\n\\n## \
             B\\n\\nBeta.\\n\\n## C\\n\\nGamma.\",\"source\":\"handbook\",\"updated_at\":\
             \"2026-01-01T00:00:00Z\"}\n";

        // Producer A: a huge budget keeps the whole doc as one chunk.
        let a = root_with_store_budget(sectioned, 100_000);
        push(a.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let snap_a = KnowledgeStore::load_current(a.path()).unwrap().snapshot;

        // Producer B: a tiny budget splits every section into its own chunk.
        let b = root_with_store_budget(sectioned, 1);
        let store_b = KnowledgeStore::load_current(b.path()).unwrap();
        assert_eq!(snap_a, store_b.snapshot, "same feed ⇒ same snapshot id (#113 collision)");
        assert!(store_b.chunks.len() > 1, "the tiny budget must produce divergent content");

        // B's push carries the same id but different bytes: refused without --force.
        let err = push(b.path(), Some("c1".into()), Some(url.clone())).unwrap_err();
        assert!(err.contains("DIFFERENT content"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");

        // The remote key is untouched — still A's single-chunk artifact.
        let remote = GitRemote::open(&url, false).unwrap();
        let key = knowledge_content_address("v1", "c1", &snap_a);
        let ra = KnowledgeArtifact::from_bytes(&remote.get(&key).unwrap()).unwrap();
        assert_eq!(ra.manifest.chunk_count, 1, "A's bytes must survive the refused push");

        // --force overwrites, as designed.
        cmd_knowledge_push(b.path(), Some("c1".into()), Some(url.clone()), true, false).unwrap();
        let rb = KnowledgeArtifact::from_bytes(&remote.get(&key).unwrap()).unwrap();
        assert!(rb.manifest.chunk_count > 1, "--force replaces it with B's content");
    }

    #[test]
    fn republishing_same_snapshot_id_with_identical_content_stays_idempotent() {
        // #113 control: a genuine re-push (same feed, same budget ⇒ same checksum)
        // still proceeds quietly — the guard only fires on real divergence.
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store_budget(
            "{\"id\":\"kn:1\",\"title\":\"N\",\"body\":\"Body.\",\
             \"source\":\"handbook\"}\n",
            400,
        );
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let out = push(root.path(), Some("c1".into()), Some(url)).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
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

    /// A multi-record feed: one NDJSON line per `(id, body)`, fixed timestamp
    /// (deterministic `data_as_of`), for the #90 push-guard diff tests.
    fn feed_of(records: &[(&str, &str)]) -> String {
        records
            .iter()
            .map(|(id, body)| {
                format!(
                    "{{\"id\":\"{id}\",\"title\":\"Note {id}\",\"body\":\"{body}\",\
                     \"source\":\"handbook\",\"updated_at\":\"2026-01-01T00:00:00Z\"}}\n"
                )
            })
            .collect()
    }

    /// Re-ingest `feed_text` into an existing root (supersedes the current
    /// store, exactly like `cce knowledge index`); returns the new snapshot id.
    fn reindex(root: &Path, feed_text: &str) -> String {
        let recs = parse_ndjson(feed_text).unwrap();
        let store = ingest_default(&recs, feed_text.as_bytes());
        store.save(root).unwrap();
        store.snapshot
    }

    #[test]
    fn first_publish_and_adds_only_and_changed_pushes_stay_quiet() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "Body one.")]));
        // First publish: no remote pointer, nothing to diff, no guard output.
        let out = push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
        assert!(!out.contains("added") && !out.contains("removed"), "guard leaked: {out}");
        // Adds + a changed record never block and stay exactly as quiet.
        reindex(root.path(), &feed_of(&[("kn:1", "Body one edited."), ("kn:2", "Body two.")]));
        let out = push(root.path(), Some("c1".into()), Some(url)).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
        assert!(!out.contains("added") && !out.contains("removed"), "guard leaked: {out}");
    }

    #[test]
    fn republishing_the_already_current_snapshot_proceeds_idempotently() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "Body one.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        // The remote `current` already names this snapshot: push as today.
        let out = push(root.path(), Some("c1".into()), Some(url)).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
    }

    #[test]
    fn shrinking_push_is_refused_naming_the_removed_ids_and_force_overrides() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root =
            root_with_store(&feed_of(&[("kn:1", "One."), ("kn:2", "Two."), ("kn:3", "Three.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let published = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // A partial rebuild (one record of three) must not silently clobber.
        let shrunk = reindex(root.path(), &feed_of(&[("kn:2", "Two.")]));
        let err = push(root.path(), Some("c1".into()), Some(url.clone())).unwrap_err();
        assert!(err.contains("removed : 2 — kn:1, kn:3"), "sorted removed ids named: {err}");
        assert!(err.contains("2 records live on the remote would be dropped"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
        // Nothing moved on the remote: pointer intact, no new artifact key.
        let remote = GitRemote::open(&url, false).unwrap();
        assert_eq!(
            remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(),
            published
        );
        assert!(!remote.has(&knowledge_content_address("v1", "c1", &shrunk)).unwrap());

        // --force pushes the shrink.
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), true, false)
                .unwrap();
        assert!(out.contains(&format!("Pushed corpus c1@{shrunk}")), "got: {out}");
        assert_eq!(remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(), shrunk);
    }

    #[test]
    fn dry_run_reports_the_full_diff_and_pushes_nothing() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "Body A."), ("kn:2", "Two.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let published = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // Local: kn:1 changed, kn:2 dropped, kn:3 added.
        let local = reindex(root.path(), &feed_of(&[("kn:1", "Body B."), ("kn:3", "Three.")]));
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), false, true)
                .unwrap();
        assert!(out.contains(&format!("outgoing {local} vs remote current {published}")), "{out}");
        assert!(out.contains("records : 2 local · 2 remote"), "got: {out}");
        assert!(out.contains("added   : 1 — kn:3"), "got: {out}");
        assert!(out.contains("removed : 1 — kn:2"), "got: {out}");
        assert!(out.contains("changed : 1 — kn:1"), "got: {out}");
        assert!(out.contains("Nothing pushed (--dry-run)."), "got: {out}");
        // The remote is untouched: pointer unmoved, no new artifact key.
        let remote = GitRemote::open(&url, false).unwrap();
        assert_eq!(
            remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(),
            published
        );
        assert!(!remote.has(&knowledge_content_address("v1", "c1", &local)).unwrap());
    }

    #[test]
    fn dry_run_against_an_absent_remote_reports_first_publish_and_pushes_nothing() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One."), ("kn:2", "Two.")]));
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), false, true)
                .unwrap();
        assert!(out.contains("first publish"), "got: {out}");
        assert!(out.contains("2 records"), "got: {out}");
        assert!(out.contains("Nothing pushed (--dry-run)."), "got: {out}");
        let remote = GitRemote::open(&url, false).unwrap();
        assert!(!remote.has(&knowledge_pointer_address("v1", "c1")).unwrap(), "pointer appeared");
    }

    #[test]
    fn dry_run_on_the_already_current_snapshot_reports_unchanged() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url), false, true).unwrap();
        assert!(out.contains("already names this snapshot"), "got: {out}");
        assert!(out.contains("Nothing pushed (--dry-run)."), "got: {out}");
    }

    #[test]
    fn guard_refuses_an_unverifiable_remote_current_and_force_bypasses() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let published = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // Tamper with the remote current in place (flip a content byte).
        let remote = GitRemote::open(&url, false).unwrap();
        let key = knowledge_content_address("v1", "c1", &published);
        let text = String::from_utf8(remote.get(&key).unwrap()).unwrap();
        remote.put(&key, text.replace("One", "Two").as_bytes()).unwrap();

        // A guarded push cannot verify what it would replace: refuse, not proceed.
        reindex(root.path(), &feed_of(&[("kn:1", "One."), ("kn:2", "Two.")]));
        let err = push(root.path(), Some("c1".into()), Some(url.clone())).unwrap_err();
        assert!(err.contains("could not verify the remote's current snapshot"), "got: {err}");
        assert!(err.contains(&key), "the failure names the key, got: {err}");
        assert!(err.contains("--force"), "got: {err}");

        // --force skips the guard entirely and pushes as today.
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url), true, false).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
    }

    #[test]
    fn diff_id_lists_elide_past_twenty_ids() {
        let ids: Vec<String> = (0..25).map(|i| format!("kn:{i:02}")).collect();
        let s = format_id_list(&ids);
        assert!(s.starts_with("25 — kn:00, kn:01"), "got: {s}");
        assert!(s.contains("kn:19") && !s.contains("kn:20"), "got: {s}");
        assert!(s.ends_with("… and 5 more"), "got: {s}");
        assert_eq!(format_id_list(&[]), "0");
        assert_eq!(format_id_list(&["kn:1".to_string()]), "1 — kn:1");
    }

    /// The chunk_id blind spot (the reason `changed` uses a full-content
    /// digest): chunk ids hash only a content PREFIX, so an edit past it
    /// within an unchanged line span leaves every chunk_id identical — the
    /// digest must still see the record as changed.
    #[test]
    fn changed_detects_a_content_edit_past_the_chunk_id_prefix() {
        let long = "A".repeat(150);
        let f1 = feed_of(&[("kn:1", &format!("{long} tail-one."))]);
        let f2 = feed_of(&[("kn:1", &format!("{long} tail-two."))]);
        let s1 = {
            let recs = parse_ndjson(&f1).unwrap();
            ingest_default(&recs, f1.as_bytes())
        };
        let s2 = {
            let recs = parse_ndjson(&f2).unwrap();
            ingest_default(&recs, f2.as_bytes())
        };
        // Same line span, same 100-byte prefix ⇒ the chunk_ids are identical…
        let ids =
            |s: &KnowledgeStore| s.chunks.iter().map(|c| c.chunk_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&s1), ids(&s2), "the test must exercise the prefix blind spot");
        assert_ne!(s1.chunks[0].content, s2.chunks[0].content);
        // …but the guard still reports the record as changed.
        let diff = diff_records(&s2.chunks, &s1.chunks);
        assert!(diff.added.is_empty() && diff.removed.is_empty());
        assert_eq!(diff.changed, vec!["kn:1".to_string()]);
    }

    #[test]
    fn dry_run_with_force_still_diffs_and_pushes_nothing() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One."), ("kn:2", "Two.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let published = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // Both flags: dry-run wins — the diff prints, nothing is written.
        let local = reindex(root.path(), &feed_of(&[("kn:1", "One.")]));
        let out = cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), true, true)
            .unwrap();
        assert!(out.contains("removed : 1 — kn:2"), "got: {out}");
        assert!(out.contains("Nothing pushed (--dry-run)."), "got: {out}");
        let remote = GitRemote::open(&url, false).unwrap();
        assert_eq!(
            remote.read_blob_text(&knowledge_pointer_address("v1", "c1")).unwrap(),
            published
        );
        assert!(!remote.has(&knowledge_content_address("v1", "c1", &local)).unwrap());
    }

    #[test]
    fn dry_run_on_an_unverifiable_remote_names_the_real_push_bypass() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let published = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // Tamper with the remote current, then ask for a dry-run diff.
        let remote = GitRemote::open(&url, false).unwrap();
        let key = knowledge_content_address("v1", "c1", &published);
        let text = String::from_utf8(remote.get(&key).unwrap()).unwrap();
        remote.put(&key, text.replace("One", "Two").as_bytes()).unwrap();
        reindex(root.path(), &feed_of(&[("kn:1", "One."), ("kn:2", "Two.")]));

        // Under --dry-run the bypass hint must NOT say "pass --force" — the
        // user may already have passed it (dry-run still diffs with --force).
        for force in [false, true] {
            let err =
                cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), force, true)
                    .unwrap_err();
            assert!(err.contains("could not verify the remote's current snapshot"), "got: {err}");
            assert!(err.contains("no diff to report (--dry-run)"), "got: {err}");
            assert!(err.contains("without --dry-run"), "got: {err}");
            assert!(!err.contains("Pass --force to push without the guard"), "got: {err}");
        }
    }

    #[test]
    fn empty_or_whitespace_remote_pointer_is_treated_as_first_publish() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();
        let snapshot = KnowledgeStore::load_current(root.path()).unwrap().snapshot;

        // Blank the pointer in place (whitespace-only blob).
        let remote = GitRemote::open(&url, false).unwrap();
        let pointer_key = knowledge_pointer_address("v1", "c1");
        remote.put(&pointer_key, b"  \n").unwrap();

        // Deliberate: an empty pointer is ABSENT to the guard (mirroring
        // knowledge_freshness) — a dry-run reports a first publish…
        let out =
            cmd_knowledge_push(root.path(), Some("c1".into()), Some(url.clone()), false, true)
                .unwrap();
        assert!(out.contains("first publish"), "got: {out}");
        // …and a real push proceeds silently, restoring the pointer.
        let out = push(root.path(), Some("c1".into()), Some(url)).unwrap();
        assert!(out.starts_with("Pushed corpus c1@"), "got: {out}");
        assert!(!out.contains("removed"), "guard leaked: {out}");
        assert_eq!(remote.read_blob_text(&pointer_key).unwrap(), snapshot);
    }

    #[test]
    fn empty_local_store_push_over_a_nonempty_remote_is_refused() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();

        // A zero-record local store (empty feed) would erase the corpus.
        reindex(root.path(), "");
        let err = push(root.path(), Some("c1".into()), Some(url)).unwrap_err();
        assert!(err.contains("records : 0 local · 1 remote"), "got: {err}");
        assert!(err.contains("removed : 1 — kn:1"), "got: {err}");
        assert!(err.contains("--force"), "got: {err}");
    }

    #[test]
    fn dangling_pointer_is_refused_naming_the_missing_artifact_key() {
        let _home = with_home();
        let (_bare, url) = bare_remote();
        let root = root_with_store(&feed_of(&[("kn:1", "One.")]));
        push(root.path(), Some("c1".into()), Some(url.clone())).unwrap();

        // Point `current` at a snapshot whose .cck does not exist.
        let remote = GitRemote::open(&url, false).unwrap();
        remote.put(&knowledge_pointer_address("v1", "c1"), b"aaaaaaaaaaaaaaaa\n").unwrap();

        let err = push(root.path(), Some("c1".into()), Some(url)).unwrap_err();
        let missing = knowledge_content_address("v1", "c1", "aaaaaaaaaaaaaaaa");
        assert!(err.contains("could not verify the remote's current snapshot"), "got: {err}");
        assert!(err.contains(&missing), "the failure names the missing key, got: {err}");
        assert!(err.contains("--force"), "got: {err}");
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
