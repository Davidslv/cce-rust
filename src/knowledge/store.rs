//! # knowledge::store — the snapshot-keyed knowledge index (SPEC-V2.6 §4, M3)
//!
//! **Why this file exists:** Knowledge (epics, issues, policy docs) is *mutable*, so
//! it can never enter the byte-identical `repo@sha` code cache. It lives in a
//! SEPARATE store under `.cce/knowledge/`, keyed by an extraction **snapshot id** (a
//! deterministic hash of the input contract file); a newer snapshot supersedes the
//! old. This module owns that store: ingest a `cce.knowledge/v1` feed → render each
//! record → M1 heading-chunk it → redact → persist with per-chunk facets.
//!
//! **What it is / does:** [`ingest`] renders each record to `# <title>\n\n<body>`,
//! runs the **v2.1 redactor** over that document BEFORE chunking (so a secret never
//! reaches the store and chunk ids derive from redacted text — mirroring the code
//! index's Layer 2), heading-chunks it with M1, and attaches the record's metadata
//! (`state`, `state_reason`, `updated_at`, `group`, `url`, `labels`, `source`, id) as
//! facets. EVERY facet except the record id (`title`, `state`, `state_reason`,
//! `updated_at`, `source`, `group`, `url`, `labels`, `links`) passes through the SAME
//! redactor before attachment (#111), so no facet carries a raw secret either — the
//! schema validates none of them, and `state`/`updated_at` are served in provenance.
//! The `record_id` is the one exception (an addressing key, documented residual —
//! #144). The result is deterministic and byte-pinned; save/load round-trips JSON.
//!
//! **Responsibilities:**
//! - Own `KnowledgeChunk`, `KnowledgeStore`, the snapshot id, and JSON persistence.
//! - Own the redact-before-chunk order and facet attachment.
//! - It does NOT parse the contract (that is `contract`) nor retrieve (that is M4).

use crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS;
use crate::embedder::{Embedder, HashEmbedder};
use crate::knowledge::contract::{render_document, KnowledgeRecord, KNOWLEDGE_SCHEMA_ID};
use crate::markdown::chunk_markdown;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

/// One indexed knowledge chunk: a markdown heading section plus the source record's
/// metadata as facets (SPEC-V2.6 §4). Field order is the persisted, byte-pinned order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    /// `SHA-256(record_id:start:end:prefix)`, the shared content-addressed scheme.
    pub chunk_id: String,
    /// The source record's stable id (also the synthetic document path).
    pub record_id: String,
    /// The heading text (or `(preamble)` for pre-heading content).
    pub kind: String,
    /// The breadcrumb name, e.g. `# Title › ## Section`.
    pub name: String,
    /// 1-based first line of the section within the rendered document.
    pub start_line: usize,
    /// 1-based last content line of the section.
    pub end_line: usize,
    /// `token_count` per the shared `cce.tokens/v1` estimator.
    pub token_count: usize,
    /// The redacted markdown of this section (what the store holds and retrieval serves).
    pub content: String,
    // --- facets (SPEC-V2.6 §4) ---
    pub source: String,
    pub url: Option<String>,
    pub state: Option<String>,
    pub state_reason: Option<String>,
    pub updated_at: Option<String>,
    pub group: Option<String>,
    pub labels: Vec<String>,
    // --- M4 retrieval facets (SPEC-V2.6 §5) ---
    /// The source record's title (redacted at ingest, #111), carried on every chunk so
    /// a knowledge hit can render its byte-pinned provenance line without re-reading
    /// the record.
    #[serde(default)]
    pub title: String,
    /// The record's related links (a merged-PR reference is the "decided + implemented"
    /// staleness signal, SPEC-V2.6 §5). Empty for older Phase-A snapshots.
    #[serde(default)]
    pub links: Vec<String>,
    /// The deterministic hash embedding of `content`, persisted at index time so
    /// knowledge search uses the SAME hybrid retrieval as code (SPEC-V2.6 §5). Older
    /// Phase-A snapshots omit it (serde default `[]`); retrieval recomputes then.
    #[serde(default)]
    pub embedding: Vec<f64>,
}

/// A persisted knowledge store for one extraction snapshot (SPEC-V2.6 §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeStore {
    /// The pinned contract schema id (`cce.knowledge/v1`).
    pub schema: String,
    /// The extraction snapshot id (a hash of the input file); supersedes older snapshots.
    pub snapshot: String,
    /// Number of source records ingested.
    pub records: usize,
    /// The heading-chunked, redacted, faceted chunks in deterministic order.
    pub chunks: Vec<KnowledgeChunk>,
}

/// Summary of an ingest run (SPEC-V2.6 §4).
#[derive(Debug, Clone)]
pub struct IngestSummary {
    pub records: usize,
    pub chunks: usize,
    pub snapshot: String,
    pub store_path: PathBuf,
}

/// The extraction snapshot id: the first 16 lowercase-hex chars of the SHA-256 of the
/// raw input contract bytes. Location-independent and deterministic — the same feed
/// always yields the same id, so the store is reproducible and supersedable.
pub fn snapshot_id(input_bytes: &[u8]) -> String {
    let digest = Sha256::digest(input_bytes);
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Ingest parsed records (from an input whose raw bytes are `input_bytes`) into a
/// `KnowledgeStore`, splitting oversized sections at `max_section_tokens`. Pure and
/// deterministic: render → redact → heading-chunk → attach facets, in record order.
pub fn ingest(
    records: &[KnowledgeRecord],
    input_bytes: &[u8],
    max_section_tokens: usize,
) -> KnowledgeStore {
    // The deterministic hash embedder (SPEC §5.1) — the SAME backend the code index
    // uses — so a knowledge chunk's persisted embedding is byte-reproducible and its
    // retrieval ranking is identical to code's (no network, no wall-clock).
    let embedder = HashEmbedder;
    let mut chunks: Vec<KnowledgeChunk> = Vec::new();
    for rec in records {
        // Render, then redact the WHOLE document BEFORE chunking (SPEC-V2.6 §4 /
        // SPEC-V2.1 §2): the store never sees a secret and chunk ids/token counts
        // derive from redacted text.
        let doc = render_document(rec);
        let redacted = crate::redactor::redact(&doc);
        let md_chunks = chunk_markdown(&rec.id, &redacted, max_section_tokens);
        // The facets (#111): EVERY facet except the record id arrives raw from the
        // adapter and is persisted, exported by `knowledge push`, and — for `title`,
        // `state`, `updated_at`, `url` — SERVED in the provenance line. The
        // `cce.knowledge/v1` schema enforces no enum/format on any of them (`state`,
        // `updated_at`, `source` are plain `Option<String>`/`String`, NOT a validated
        // vocabulary), so each gets the SAME Layer-2 pass as the document, once per
        // record, BEFORE attachment. Redaction is the identity on clean text (an
        // `open`/`closed` state, an ISO timestamp, a `github-issues` tag are not
        // secret-shaped), so secret-free stores are byte-unchanged.
        //
        // `record_id` (from `rec.id`) is DELIBERATELY not redacted: it is the
        // addressing key — chunk ids and the synthetic document path derive from it,
        // so scrubbing it would break lookup/round-trip. A secret in a record id can
        // therefore still surface via `expand_chunk`/`related_context` headers; the
        // `cce.knowledge/v1` contract requires ids to be secret-free (see
        // docs/knowledge.md), and the redacted-display mitigation is tracked as #144.
        let title = crate::redactor::redact(rec.title.trim());
        let url = rec.url.as_deref().map(crate::redactor::redact);
        let state = rec.state.as_deref().map(crate::redactor::redact);
        let state_reason = rec.state_reason.as_deref().map(crate::redactor::redact);
        let updated_at = rec.updated_at.as_deref().map(crate::redactor::redact);
        let source = crate::redactor::redact(&rec.source);
        let group = rec.group.as_deref().map(crate::redactor::redact);
        let labels: Vec<String> = rec.labels.iter().map(|l| crate::redactor::redact(l)).collect();
        let links: Vec<String> = rec.links.iter().map(|l| crate::redactor::redact(l)).collect();
        for mc in md_chunks {
            let embedding = embedder.embed(&mc.content);
            chunks.push(KnowledgeChunk {
                chunk_id: mc.chunk_id,
                record_id: rec.id.clone(),
                kind: mc.kind,
                name: mc.name,
                start_line: mc.start_line,
                end_line: mc.end_line,
                token_count: mc.token_count,
                content: mc.content,
                source: source.clone(),
                url: url.clone(),
                state: state.clone(),
                state_reason: state_reason.clone(),
                updated_at: updated_at.clone(),
                group: group.clone(),
                labels: labels.clone(),
                title: title.clone(),
                links: links.clone(),
                embedding,
            });
        }
    }
    KnowledgeStore {
        schema: KNOWLEDGE_SCHEMA_ID.to_string(),
        snapshot: snapshot_id(input_bytes),
        records: records.len(),
        chunks,
    }
}

impl KnowledgeStore {
    /// The knowledge store directory for a project root: `<root>/.cce/knowledge`.
    pub fn dir(root: &Path) -> PathBuf {
        root.join(".cce").join("knowledge")
    }

    /// The snapshot artifact path: `<root>/.cce/knowledge/<snapshot>.json`.
    pub fn snapshot_path(root: &Path, snapshot: &str) -> PathBuf {
        Self::dir(root).join(format!("{snapshot}.json"))
    }

    /// The `current` pointer path: `<root>/.cce/knowledge/current` — a one-line file
    /// naming the active snapshot, so a newer ingest supersedes the old.
    pub fn current_pointer_path(root: &Path) -> PathBuf {
        Self::dir(root).join("current")
    }

    /// Persist this store under `root`: write the snapshot artifact and then
    /// advance the `current` pointer to name it (superseding any prior snapshot).
    /// Deterministic (pretty JSON, declaration-order fields). Returns the artifact
    /// path.
    ///
    /// The two steps are split ([`write_snapshot`](Self::write_snapshot) +
    /// [`advance_current`](Self::advance_current)) so a caller that must record
    /// another marker in between (the sync pull, #122) can make the `current`
    /// move the LAST durable step — never advancing the active store before the
    /// marker that describes it is on disk.
    pub fn save(&self, root: &Path) -> io::Result<PathBuf> {
        let path = self.write_snapshot(root)?;
        Self::advance_current(root, &self.snapshot)?;
        Ok(path)
    }

    /// Write ONLY the `<snapshot>.json` artifact (atomically), WITHOUT touching
    /// the `current` pointer — so the snapshot exists on disk but is not yet the
    /// active store. Returns the artifact path.
    pub fn write_snapshot(&self, root: &Path) -> io::Result<PathBuf> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir)?;
        let path = Self::snapshot_path(root, &self.snapshot);
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        // #101: atomic temp-file + rename, so an interrupted ingest can never
        // destroy a prior snapshot.
        crate::atomic::atomic_write(&path, format!("{json}\n").as_bytes())?;
        Ok(path)
    }

    /// Advance the `current` pointer to `snapshot` (atomically) — the step that
    /// makes a written snapshot the ACTIVE store. Must run after its snapshot
    /// artifact is on disk. Then prunes superseded local snapshots (#114).
    pub fn advance_current(root: &Path, snapshot: &str) -> io::Result<()> {
        // #101: atomic temp-file + rename, so a torn pointer a concurrent reader
        // would misresolve can never occur.
        crate::atomic::atomic_write(
            &Self::current_pointer_path(root),
            format!("{snapshot}\n").as_bytes(),
        )?;
        // #114: local retention — a newer snapshot supersedes the old, so the
        // superseded `<snapshot>.json` artifacts (each carrying a per-chunk
        // embedding, easily multi-MB) are dropped here; without this, routine
        // re-ingestion / pull leaks unbounded disk (there is no other local
        // prune). Runs only AFTER `current` is durably repointed, and is
        // best-effort: a prune failure never fails the save.
        Self::prune_superseded_snapshots(root, snapshot);
        Ok(())
    }

    /// Remove every `<snapshot>.json` artifact under the store dir except the one
    /// named `keep` (#114). Scoped tightly to snapshot artifacts — a name must be
    /// a 16-char lowercase-hex snapshot id — so the `synced.json` sync marker, the
    /// `current` pointer, and any other file are never touched. Best-effort.
    fn prune_superseded_snapshots(root: &Path, keep: &str) {
        let Ok(entries) = std::fs::read_dir(Self::dir(root)) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let is_snapshot_id = stem.len() == 16
                && stem.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !is_snapshot_id || stem == keep {
                continue;
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Load the store for a snapshot artifact path.
    pub fn load(path: &Path) -> io::Result<KnowledgeStore> {
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Load the active (current-pointer) store under `root`, if one exists.
    pub fn load_current(root: &Path) -> io::Result<KnowledgeStore> {
        let snapshot = std::fs::read_to_string(Self::current_pointer_path(root))?;
        Self::load(&Self::snapshot_path(root, snapshot.trim()))
    }
}

/// Ingest a contract file at `input_path` into the knowledge store under `root`,
/// using `max_section_tokens` for the split budget. Reads the file, parses it,
/// ingests, and persists (superseding any prior snapshot). Returns a summary.
pub fn ingest_file(
    input_path: &Path,
    root: &Path,
    max_section_tokens: usize,
) -> Result<IngestSummary, String> {
    ingest_file_verified(input_path, None, root, max_section_tokens)
}

/// Like [`ingest_file`], but optionally verifies the feed against a neutral
/// `cce.feed-manifest/v1` sidecar (U6.2) BEFORE anything is written. When
/// `manifest_path` is `Some`, a record-count or checksum mismatch is a loud `Err` and
/// no store is persisted — a truncated or misdirected feed can never index silently
/// (gap G16). When `None`, behaviour is byte-identical to a plain ingest (the check is
/// opt-in and additive). The feed bytes are read exactly once, so the bytes verified
/// are the bytes ingested.
pub fn ingest_file_verified(
    input_path: &Path,
    manifest_path: Option<&Path>,
    root: &Path,
    max_section_tokens: usize,
) -> Result<IngestSummary, String> {
    let text = std::fs::read_to_string(input_path)
        .map_err(|e| format!("could not read {}: {e}", input_path.display()))?;
    let records = crate::knowledge::contract::parse_ndjson(&text)?;
    if let Some(mpath) = manifest_path {
        let mtext = std::fs::read_to_string(mpath)
            .map_err(|e| format!("could not read feed manifest {}: {e}", mpath.display()))?;
        let manifest = crate::knowledge::manifest::FeedManifest::parse(&mtext)?;
        manifest.verify(text.as_bytes(), records.len())?;
    }
    let store = ingest(&records, text.as_bytes(), max_section_tokens);
    let store_path =
        store.save(root).map_err(|e| format!("could not write knowledge store: {e}"))?;
    Ok(IngestSummary {
        records: store.records,
        chunks: store.chunks.len(),
        snapshot: store.snapshot,
        store_path,
    })
}

/// Convenience: ingest with the default `markdown.max_section_tokens`.
pub fn ingest_default(records: &[KnowledgeRecord], input_bytes: &[u8]) -> KnowledgeStore {
    ingest(records, input_bytes, DEFAULT_MARKDOWN_MAX_SECTION_TOKENS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::contract::parse_ndjson;

    fn rec(id: &str, title: &str, body: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.into(),
            title: title.into(),
            body: body.into(),
            source: "github-issues".into(),
            url: Some(format!("https://x/{id}")),
            state: Some("open".into()),
            state_reason: None,
            updated_at: Some("2026-01-02T03:04:05Z".into()),
            labels: vec!["bug".into()],
            group: Some("Checkout".into()),
            links: vec![],
            extra: None,
        }
    }

    #[test]
    fn snapshot_id_is_deterministic_16_hex() {
        let a = snapshot_id(b"hello");
        let b = snapshot_id(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(snapshot_id(b"hello"), snapshot_id(b"world"));
    }

    #[test]
    fn ingest_attaches_facets_to_every_chunk() {
        let r = rec("gh:1", "Login policy", "## Why\n\nBecause reasons.\n\n## How\n\nSteps.");
        let store = ingest(std::slice::from_ref(&r), b"input", 1);
        assert!(store.chunks.len() >= 2);
        for c in &store.chunks {
            assert_eq!(c.record_id, "gh:1");
            assert_eq!(c.source, "github-issues");
            assert_eq!(c.state.as_deref(), Some("open"));
            assert_eq!(c.updated_at.as_deref(), Some("2026-01-02T03:04:05Z"));
            assert_eq!(c.group.as_deref(), Some("Checkout"));
            assert_eq!(c.url.as_deref(), Some("https://x/gh:1"));
            assert_eq!(c.labels, vec!["bug".to_string()]);
        }
        // The title becomes the top-level `# Login policy` heading chunk.
        assert_eq!(store.chunks[0].kind, "Login policy");
        assert_eq!(store.schema, "cce.knowledge/v1");
        assert_eq!(store.records, 1);
    }

    #[test]
    fn secret_in_body_is_redacted_before_write() {
        // A secret-shaped assignment in the body must be [REDACTED:…] in the store.
        let r = rec("gh:2", "Config", "The key is api_key = s3cr3tvalue123 in prod.");
        let store = ingest(&[r], b"x", 400);
        let joined: String = store.chunks.iter().map(|c| c.content.clone()).collect();
        assert!(joined.contains("[REDACTED:SECRET]"), "{joined}");
        assert!(!joined.contains("s3cr3tvalue123"), "{joined}");
    }

    // Secret-shaped test inputs are assembled from split fragments via `concat!`
    // so no committed source file contains a contiguous secret literal (GitHub
    // push protection); the redactor still sees the full value at runtime.
    const AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");
    const GH_TOKEN: &str = concat!("ghp", "_", "0123456789abcdefghijklmnopqrstuvwx01");

    #[test]
    fn secret_in_title_facet_is_redacted_before_write() {
        // #111: the title facet is carried on every chunk, served in provenance
        // lines, and exported by `knowledge push` — it must get the SAME Layer-2
        // redaction as the rendered document.
        let r =
            rec("gh:3", &format!("Rotate leaked key {AWS_KEY} in prod"), "## Fix\n\nRotate it.");
        let store = ingest(&[r], b"x", 400);
        let json = serde_json::to_string_pretty(&store).unwrap();
        assert!(!json.contains(AWS_KEY), "raw title secret leaked into the store: {json}");
        for c in &store.chunks {
            assert_eq!(c.title, "Rotate leaked key [REDACTED:AWS_ACCESS_KEY] in prod");
        }
    }

    #[test]
    fn secrets_in_free_text_facets_are_redacted_before_write() {
        // #111 audit: every persisted/served facet EXCEPT the record id is adapter
        // free text — the `cce.knowledge/v1` schema enforces no enum/format on any
        // of them (`state`/`updated_at`/`source` are `Option<String>`/`String`, not
        // a validated vocabulary), so each gets the same Layer-2 pass as the body.
        // (`state` + `updated_at` are additionally SERVED in the provenance line —
        // the exact leak class of #111.) Only `id`/`record_id` stays raw: it is the
        // addressing key (see the seam's rustdoc note + #144).
        let mut r = rec("gh:4", "Clean title", "body");
        r.url = Some(format!("https://example.test/1?token={GH_TOKEN}"));
        r.labels = vec![format!("leak-{AWS_KEY}"), "bug".into()];
        r.group = Some(format!("Ops {AWS_KEY}"));
        r.state = Some(format!("open; leaked {AWS_KEY}"));
        r.updated_at = Some(format!("2026-01-02 token={GH_TOKEN}"));
        r.source = format!("github-issues {AWS_KEY}");
        r.state_reason = Some("rotated; old api_key = s3cr3tvalue123".into());
        r.links = vec![format!("https://example.test/pull/7?auth_token={GH_TOKEN}")];
        let store = ingest(&[r], b"x", 400);
        let json = serde_json::to_string_pretty(&store).unwrap();
        assert!(!json.contains(AWS_KEY), "raw facet secret leaked into the store: {json}");
        assert!(!json.contains(GH_TOKEN), "raw facet secret leaked into the store: {json}");
        assert!(!json.contains("s3cr3tvalue123"), "raw facet secret leaked into the store: {json}");
        let c = &store.chunks[0];
        assert_eq!(c.url.as_deref(), Some("https://example.test/1?token=[REDACTED:GITHUB_TOKEN]"));
        assert_eq!(c.labels, vec!["leak-[REDACTED:AWS_ACCESS_KEY]".to_string(), "bug".to_string()]);
        assert_eq!(c.group.as_deref(), Some("Ops [REDACTED:AWS_ACCESS_KEY]"));
        assert_eq!(c.state.as_deref(), Some("open; leaked [REDACTED:AWS_ACCESS_KEY]"));
        assert_eq!(c.updated_at.as_deref(), Some("2026-01-02 token=[REDACTED:GITHUB_TOKEN]"));
        assert_eq!(c.source, "github-issues [REDACTED:AWS_ACCESS_KEY]");
        assert_eq!(c.state_reason.as_deref(), Some("rotated; old api_key = [REDACTED:SECRET]"));
        assert_eq!(
            c.links,
            vec!["https://example.test/pull/7?auth_token=[REDACTED:GITHUB_TOKEN]".to_string()]
        );
    }

    #[test]
    fn clean_facets_pass_through_byte_identically() {
        // Determinism control (#111): redaction is identity on clean text, so a
        // secret-free record's facets persist byte-for-byte as the adapter sent
        // them (the pinned-store checksums in tests/knowledge_ingest.rs rely on it).
        let mut r = rec("gh:5", "Login policy", "## Why\n\nBecause.");
        r.state_reason = Some("completed".into());
        r.links = vec!["https://example.test/pull/40".into()];
        let store = ingest(&[r], b"x", 400);
        for c in &store.chunks {
            assert_eq!(c.title, "Login policy");
            assert_eq!(c.url.as_deref(), Some("https://x/gh:5"));
            assert_eq!(c.group.as_deref(), Some("Checkout"));
            assert_eq!(c.labels, vec!["bug".to_string()]);
            // Legitimate state/updated_at/source values are not secret-shaped, so
            // redaction is the identity on them — they persist byte-for-byte.
            assert_eq!(c.state.as_deref(), Some("open"));
            assert_eq!(c.updated_at.as_deref(), Some("2026-01-02T03:04:05Z"));
            assert_eq!(c.source, "github-issues");
            assert_eq!(c.state_reason.as_deref(), Some("completed"));
            assert_eq!(c.links, vec!["https://example.test/pull/40".to_string()]);
        }
    }

    #[test]
    fn ingest_is_deterministic_byte_for_byte() {
        let text = "{\"id\":\"a\",\"title\":\"A\",\"body\":\"x\",\"source\":\"s\"}\n{\"id\":\"b\",\"title\":\"B\",\"body\":\"y\",\"source\":\"s\"}\n";
        let recs = parse_ndjson(text).unwrap();
        let a = ingest(&recs, text.as_bytes(), 400);
        let b = ingest(&recs, text.as_bytes(), 400);
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string_pretty(&a).unwrap(),
            serde_json::to_string_pretty(&b).unwrap()
        );
    }

    #[test]
    fn save_load_round_trip_and_current_pointer() {
        let recs = vec![rec("gh:1", "T", "body")];
        let store = ingest(&recs, b"input-bytes", 400);
        let tmp = tempfile::tempdir().unwrap();
        let path = store.save(tmp.path()).unwrap();
        assert!(path.exists());
        // The current pointer names the snapshot.
        let ptr =
            std::fs::read_to_string(KnowledgeStore::current_pointer_path(tmp.path())).unwrap();
        assert_eq!(ptr.trim(), store.snapshot);
        // Round-trips via the pointer.
        let loaded = KnowledgeStore::load_current(tmp.path()).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn newer_snapshot_supersedes_via_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let s1 = ingest(&[rec("a", "One", "x")], b"feed-v1", 400);
        s1.save(tmp.path()).unwrap();
        let s2 = ingest(&[rec("a", "One", "x"), rec("b", "Two", "y")], b"feed-v2", 400);
        s2.save(tmp.path()).unwrap();
        // The pointer now names the second snapshot; loading current returns it.
        let loaded = KnowledgeStore::load_current(tmp.path()).unwrap();
        assert_eq!(loaded, s2);
        assert_ne!(s1.snapshot, s2.snapshot);
    }

    #[test]
    fn saving_a_new_snapshot_prunes_the_superseded_local_artifact() {
        // #114: a newer snapshot supersedes the old, so the superseded
        // `<snapshot>.json` must not accumulate forever under `.cce/knowledge/`.
        let tmp = tempfile::tempdir().unwrap();
        // A sync marker in the same dir must SURVIVE pruning.
        std::fs::create_dir_all(KnowledgeStore::dir(tmp.path())).unwrap();
        std::fs::write(KnowledgeStore::dir(tmp.path()).join("synced.json"), b"{}").unwrap();

        let s1 = ingest(&[rec("a", "One", "x")], b"feed-v1", 400);
        s1.save(tmp.path()).unwrap();
        let s2 = ingest(&[rec("a", "One", "x"), rec("b", "Two", "y")], b"feed-v2", 400);
        s2.save(tmp.path()).unwrap();
        assert_ne!(s1.snapshot, s2.snapshot);

        assert!(
            !KnowledgeStore::snapshot_path(tmp.path(), &s1.snapshot).exists(),
            "the superseded snapshot artifact leaked"
        );
        assert!(KnowledgeStore::snapshot_path(tmp.path(), &s2.snapshot).exists());
        // Exactly one `<snapshot>.json` remains, and the marker is untouched.
        let snapshots = std::fs::read_dir(KnowledgeStore::dir(tmp.path()))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                p.extension().and_then(|x| x.to_str()) == Some("json")
                    && p.file_name().and_then(|n| n.to_str()) != Some("synced.json")
            })
            .count();
        assert_eq!(snapshots, 1, "only the current snapshot json remains");
        assert!(KnowledgeStore::dir(tmp.path()).join("synced.json").exists(), "marker pruned");
        assert_eq!(KnowledgeStore::load_current(tmp.path()).unwrap(), s2);
    }

    #[test]
    fn ingest_file_reads_parses_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let feed = tmp.path().join("feed.jsonl");
        std::fs::write(&feed, "{\"id\":\"a\",\"title\":\"T\",\"body\":\"b\",\"source\":\"s\"}\n")
            .unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let summary = ingest_file(&feed, &root, 400).unwrap();
        assert_eq!(summary.records, 1);
        assert!(summary.chunks >= 1);
        assert!(summary.store_path.exists());
    }
}
