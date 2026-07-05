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
//! facets. The result is deterministic and byte-pinned; save/load round-trips JSON.
//!
//! **Responsibilities:**
//! - Own `KnowledgeChunk`, `KnowledgeStore`, the snapshot id, and JSON persistence.
//! - Own the redact-before-chunk order and facet attachment.
//! - It does NOT parse the contract (that is `contract`) nor retrieve (that is M4).

use crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS;
use crate::knowledge::contract::{render_document, KnowledgeRecord, KNOWLEDGE_SCHEMA_ID};
use crate::markdown::chunk_markdown;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

/// One indexed knowledge chunk: a markdown heading section plus the source record's
/// metadata as facets (SPEC-V2.6 §4). Field order is the persisted, byte-pinned order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// A persisted knowledge store for one extraction snapshot (SPEC-V2.6 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    let mut chunks: Vec<KnowledgeChunk> = Vec::new();
    for rec in records {
        // Render, then redact the WHOLE document BEFORE chunking (SPEC-V2.6 §4 /
        // SPEC-V2.1 §2): the store never sees a secret and chunk ids/token counts
        // derive from redacted text.
        let doc = render_document(rec);
        let redacted = crate::redactor::redact(&doc);
        let md_chunks = chunk_markdown(&rec.id, &redacted, max_section_tokens);
        for mc in md_chunks {
            chunks.push(KnowledgeChunk {
                chunk_id: mc.chunk_id,
                record_id: rec.id.clone(),
                kind: mc.kind,
                name: mc.name,
                start_line: mc.start_line,
                end_line: mc.end_line,
                token_count: mc.token_count,
                content: mc.content,
                source: rec.source.clone(),
                url: rec.url.clone(),
                state: rec.state.clone(),
                state_reason: rec.state_reason.clone(),
                updated_at: rec.updated_at.clone(),
                group: rec.group.clone(),
                labels: rec.labels.clone(),
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

    /// Persist this store under `root`: write the snapshot artifact and update the
    /// `current` pointer to name it (superseding any prior snapshot). Deterministic
    /// (pretty JSON, declaration-order fields). Returns the artifact path.
    pub fn save(&self, root: &Path) -> io::Result<PathBuf> {
        let dir = Self::dir(root);
        std::fs::create_dir_all(&dir)?;
        let path = Self::snapshot_path(root, &self.snapshot);
        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&path, format!("{json}\n"))?;
        std::fs::write(Self::current_pointer_path(root), format!("{}\n", self.snapshot))?;
        Ok(path)
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
    let text = std::fs::read_to_string(input_path)
        .map_err(|e| format!("could not read {}: {e}", input_path.display()))?;
    let records = crate::knowledge::contract::parse_ndjson(&text)?;
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
