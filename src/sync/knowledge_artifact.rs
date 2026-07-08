//! # sync::knowledge_artifact — the portable, byte-exact `.cck` corpus container
//! (SPEC-SYNC-KNOWLEDGE §2)
//!
//! **Why this file exists:** Knowledge corpora travel through the same dumb,
//! content-addressed cache as code indexes, so they need the same discipline: a
//! *canonical, engine-neutral* serialization that is **byte-identical across
//! people and across both engines** for the same `(feed, corpus_id)`. What is
//! uploaded is the **built store, never the feed** — redaction happens at index
//! time, so pushing the feed would put pre-redaction bytes on the remote. This
//! file owns the `.cck` format down to the last byte, mirroring `sync::artifact`.
//!
//! **What it is / does:** A UTF-8 stream with an LF after **every** line
//! (including the last):
//!   line 1        = the manifest JSON,
//!   lines 2..N+1  = one JSON object per chunk, in **store order** (feed record
//!                   order, then section order — chunks are NOT re-sorted, because
//!                   document order is meaningful and the store's order is the
//!                   canonical one).
//! There is NO graph line — knowledge has no import graph. Every object uses
//! **sorted keys and compact separators**. Embeddings reuse the `.cce` codec:
//! standard base64 (with padding) of little-endian IEEE-754 `f64` bytes.
//! **Provenance is absent entirely** (no `built_at`/`built_by`/host/user): the
//! artifact is a pure function of `(feed bytes, corpus_id)` — reproducible or it
//! is nothing. `data_as_of` is the lexicographic max `updated_at` across chunks (a
//! content property, not a push property). `checksum` = lowercase-hex SHA-256 over
//! the ENTIRE stream built with the manifest's `checksum` value set to `""`.
//!
//! **Responsibilities:**
//! - Own the `.cck` manifest/container types, canonical (de)serialization, checksum.
//! - Own the lossless `KnowledgeStore` <-> `.cck` map + the embedding-less refusal
//!   (§2 export precondition).
//! - It does NOT know about git, remotes, keys, or the CLI (that is
//!   `knowledge_commands`).

use crate::knowledge::store::{KnowledgeChunk, KnowledgeStore};
use crate::sync::artifact::{decode_embedding, encode_embedding};
use crate::sync::hex_lower;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The `.cck` manifest (SPEC-SYNC-KNOWLEDGE §2, line 1). Every field is
/// deterministic for a given `(feed, corpus_id)`. Serialized keys are sorted:
/// `checksum, chunk_count, contract, corpus_id, data_as_of, records, snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeManifest {
    /// The pinned schema id of the feed the store was built from (`cce.knowledge/v1`).
    pub contract: String,
    /// The §4.1 identity the artifact is keyed under.
    pub corpus_id: String,
    /// The M3 snapshot id (hash of the raw feed bytes) — pushing never re-keys.
    pub snapshot: String,
    /// Source records ingested.
    pub records: usize,
    /// Chunk lines that follow the manifest.
    pub chunk_count: usize,
    /// Lexicographic max `updated_at` across all chunks (ISO-8601 strings compare
    /// correctly lexicographically), or `None` when no record carries one.
    pub data_as_of: Option<String>,
    /// Lowercase-hex SHA-256 (filled in by `KnowledgeArtifact::from_store`).
    pub checksum: String,
}

/// A fully-materialized `.cck` artifact: its manifest plus the chunks it carries,
/// in store order.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeArtifact {
    pub manifest: KnowledgeManifest,
    pub chunks: Vec<KnowledgeChunk>,
}

/// The deterministic data age (SPEC-SYNC-KNOWLEDGE §2/§4.4): the lexicographic
/// maximum `updated_at` across chunks, or `None` when no record carries one.
/// Computable locally from any installed store.
pub fn data_as_of(chunks: &[KnowledgeChunk]) -> Option<String> {
    chunks.iter().filter_map(|c| c.updated_at.as_deref()).max().map(str::to_string)
}

/// Canonical JSON `Value` for one chunk (keys sorted by serde_json's `BTreeMap`).
/// Every `KnowledgeChunk` field is always present — absent optionals serialize as
/// JSON `null`, empty lists as `[]` — so the line shape is fixed.
fn chunk_value(c: &KnowledgeChunk) -> Value {
    json!({
        "chunk_id": c.chunk_id,
        "content": c.content,
        "embedding": encode_embedding(&c.embedding),
        "end_line": c.end_line,
        "group": c.group,
        "kind": c.kind,
        "labels": c.labels,
        "links": c.links,
        "name": c.name,
        "record_id": c.record_id,
        "source": c.source,
        "start_line": c.start_line,
        "state": c.state,
        "state_reason": c.state_reason,
        "title": c.title,
        "token_count": c.token_count,
        "updated_at": c.updated_at,
        "url": c.url,
    })
}

/// Reconstruct a `KnowledgeChunk` from its canonical JSON `Value` (lossless
/// inverse of `chunk_value`).
fn chunk_from_value(v: &Value) -> Result<KnowledgeChunk, String> {
    let get_str = |k: &str| -> Result<String, String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("chunk missing string field `{k}`"))
    };
    let get_usize = |k: &str| -> Result<usize, String> {
        v.get(k)
            .and_then(|x| x.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| format!("chunk missing integer field `{k}`"))
    };
    let get_opt = |k: &str| -> Result<Option<String>, String> {
        match v.get(k) {
            Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            _ => Err(format!("chunk missing nullable string field `{k}`")),
        }
    };
    let get_list = |k: &str| -> Result<Vec<String>, String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .ok_or_else(|| format!("chunk missing array field `{k}`"))
    };
    let embedding = decode_embedding(&get_str("embedding")?)?;
    Ok(KnowledgeChunk {
        chunk_id: get_str("chunk_id")?,
        record_id: get_str("record_id")?,
        kind: get_str("kind")?,
        name: get_str("name")?,
        start_line: get_usize("start_line")?,
        end_line: get_usize("end_line")?,
        token_count: get_usize("token_count")?,
        content: get_str("content")?,
        source: get_str("source")?,
        url: get_opt("url")?,
        state: get_opt("state")?,
        state_reason: get_opt("state_reason")?,
        updated_at: get_opt("updated_at")?,
        group: get_opt("group")?,
        labels: get_list("labels")?,
        title: get_str("title")?,
        links: get_list("links")?,
        embedding,
    })
}

/// The manifest JSON `Value`. Keys are sorted by serde_json's `BTreeMap` backing.
fn manifest_value(m: &KnowledgeManifest) -> Value {
    json!({
        "checksum": m.checksum,
        "chunk_count": m.chunk_count,
        "contract": m.contract,
        "corpus_id": m.corpus_id,
        "data_as_of": m.data_as_of,
        "records": m.records,
        "snapshot": m.snapshot,
    })
}

/// Parse the manifest line into a `KnowledgeManifest`.
fn parse_manifest(line: &str) -> Result<KnowledgeManifest, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad manifest JSON: {e}"))?;
    let s = |k: &str| -> Result<String, String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|x| x.to_string())
            .ok_or_else(|| format!("manifest missing string field `{k}`"))
    };
    let n = |k: &str| -> Result<usize, String> {
        v.get(k)
            .and_then(|x| x.as_u64())
            .map(|x| x as usize)
            .ok_or_else(|| format!("manifest missing integer field `{k}`"))
    };
    let data_as_of = match v.get("data_as_of") {
        Some(Value::Null) => None,
        Some(Value::String(x)) => Some(x.clone()),
        _ => return Err("manifest missing nullable string field `data_as_of`".to_string()),
    };
    Ok(KnowledgeManifest {
        contract: s("contract")?,
        corpus_id: s("corpus_id")?,
        snapshot: s("snapshot")?,
        records: n("records")?,
        chunk_count: n("chunk_count")?,
        data_as_of,
        checksum: s("checksum")?,
    })
}

impl KnowledgeArtifact {
    /// The canonical stream: manifest line + one line per chunk, LF after every
    /// line (including the last). No graph line — the container ends after the
    /// last chunk.
    fn stream(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&manifest_value(&self.manifest).to_string());
        out.push('\n');
        for c in &self.chunks {
            out.push_str(&chunk_value(c).to_string());
            out.push('\n');
        }
        out.into_bytes()
    }

    /// Export a built knowledge store to a canonical `.cck` artifact
    /// (SPEC-SYNC-KNOWLEDGE §2). **Export precondition:** refuses a store whose
    /// chunks lack persisted embeddings (a pre-v2.6.1 Phase-A snapshot) — an
    /// artifact whose consumer would have to recompute embeddings is not the
    /// byte-pinned store contract.
    pub fn from_store(store: &KnowledgeStore, corpus_id: &str) -> Result<KnowledgeArtifact, String> {
        if store.chunks.iter().any(|c| c.embedding.is_empty()) {
            return Err(
                "refusing to export a knowledge store without persisted embeddings (a Phase-A \
                 snapshot). Re-ingest the feed with this version: `cce knowledge index <feed>`."
                    .to_string(),
            );
        }
        let manifest = KnowledgeManifest {
            contract: store.schema.clone(),
            corpus_id: corpus_id.to_string(),
            snapshot: store.snapshot.clone(),
            records: store.records,
            chunk_count: store.chunks.len(),
            data_as_of: data_as_of(&store.chunks),
            checksum: String::new(),
        };
        let mut artifact = KnowledgeArtifact { manifest, chunks: store.chunks.clone() };
        // Checksum = SHA-256 over the whole stream with checksum == "" (the exact
        // `.cce` rule).
        artifact.manifest.checksum = artifact.computed_checksum();
        Ok(artifact)
    }

    /// The canonical artifact bytes (line 1 carries the real checksum).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.stream()
    }

    /// Recompute the checksum from the current content: SHA-256 over the whole
    /// canonical stream serialized with the manifest's `checksum` set to `""`.
    pub fn computed_checksum(&self) -> String {
        let mut probe = self.clone();
        probe.manifest.checksum = String::new();
        hex_lower(&Sha256::digest(probe.stream()))
    }

    /// Parse a canonical `.cck` artifact from bytes, validating the structure and
    /// the stored checksum (§4.2: import recomputes and verifies on every pull —
    /// a corrupted or tampered-in-transit artifact fails loudly).
    pub fn from_bytes(bytes: &[u8]) -> Result<KnowledgeArtifact, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| format!("artifact is not UTF-8: {e}"))?;
        let mut lines: Vec<&str> = text.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        if lines.is_empty() {
            return Err("artifact too short: need at least a manifest line".to_string());
        }
        let manifest = parse_manifest(lines[0])?;
        let n = manifest.chunk_count;
        if lines.len() != n + 1 {
            return Err(format!(
                "artifact line count {} does not match chunk_count {} (expected {})",
                lines.len(),
                n,
                n + 1
            ));
        }
        let mut chunks = Vec::with_capacity(n);
        for line in &lines[1..=n] {
            let v: Value =
                serde_json::from_str(line).map_err(|e| format!("bad chunk JSON: {e}"))?;
            chunks.push(chunk_from_value(&v)?);
        }
        let artifact = KnowledgeArtifact { manifest, chunks };
        let recomputed = artifact.computed_checksum();
        if recomputed != artifact.manifest.checksum {
            return Err(format!(
                "checksum mismatch: manifest says {}, recomputed {}",
                artifact.manifest.checksum, recomputed
            ));
        }
        Ok(artifact)
    }

    /// Materialize a ready-to-persist `KnowledgeStore` (the lossless inverse of
    /// `from_store`): installing it writes native-store bytes identical to what a
    /// local `cce knowledge index` of the same feed writes (§2 round-trip bar).
    pub fn into_store(self) -> KnowledgeStore {
        KnowledgeStore {
            schema: self.manifest.contract,
            snapshot: self.manifest.snapshot,
            records: self.manifest.records,
            chunks: self.chunks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{ingest_default, parse_ndjson};
    use std::path::PathBuf;

    /// The shared fixture feed (the same one the ingest goldens pin).
    fn fixture_feed() -> String {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/fixture/knowledge/curated.jsonl"
        ));
        std::fs::read_to_string(path).unwrap()
    }

    fn fixture_store() -> KnowledgeStore {
        let text = fixture_feed();
        let recs = parse_ndjson(&text).unwrap();
        ingest_default(&recs, text.as_bytes())
    }

    fn built() -> KnowledgeArtifact {
        KnowledgeArtifact::from_store(&fixture_store(), "fixture").unwrap()
    }

    #[test]
    fn manifest_line_has_exactly_the_canonical_keys_sorted_and_no_provenance() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("{\"checksum\":\""));
        assert!(!first.contains("built_at"));
        assert!(!first.contains("built_by"));
        let keys = [
            "\"checksum\"",
            "\"chunk_count\"",
            "\"contract\"",
            "\"corpus_id\"",
            "\"data_as_of\"",
            "\"records\"",
            "\"snapshot\"",
        ];
        let mut last = 0usize;
        for k in keys {
            let idx = first.find(k).unwrap_or_else(|| panic!("missing key {k}"));
            assert!(idx >= last, "key {k} out of order");
            last = idx;
        }
        // Compact separators.
        assert!(!first.contains(", "));
        assert!(!first.contains(": "));
    }

    #[test]
    fn stream_is_manifest_plus_chunks_with_trailing_lf_and_no_graph() {
        let a = built();
        let bytes = a.to_bytes();
        assert_eq!(bytes.last(), Some(&b'\n'), "the last line is LF-terminated");
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        // manifest + N chunks + trailing empty (from the final LF) — NO graph line.
        assert_eq!(lines.len(), a.manifest.chunk_count + 1 + 1);
        assert!(!text.contains("\"edges\""));
    }

    #[test]
    fn chunk_lines_are_in_store_order_not_resorted() {
        let a = built();
        let store = fixture_store();
        assert_eq!(a.chunks, store.chunks, "store order is the canonical order");
    }

    #[test]
    fn data_as_of_is_the_lexicographic_max_updated_at() {
        let a = built();
        // Fixture: record 1 carries 2026-02-01T10:00:00Z; record 2 carries none.
        assert_eq!(a.manifest.data_as_of.as_deref(), Some("2026-02-01T10:00:00Z"));
        // No updated_at anywhere ⇒ null.
        let text = "{\"id\":\"a\",\"title\":\"T\",\"body\":\"b\",\"source\":\"s\"}\n";
        let recs = parse_ndjson(text).unwrap();
        let store = ingest_default(&recs, text.as_bytes());
        let bare = KnowledgeArtifact::from_store(&store, "fixture").unwrap();
        assert_eq!(bare.manifest.data_as_of, None);
        assert!(String::from_utf8(bare.to_bytes()).unwrap().contains("\"data_as_of\":null"));
    }

    #[test]
    fn checksum_is_deterministic_and_recomputes() {
        let a = built();
        let b = built();
        assert_eq!(a.manifest.checksum, b.manifest.checksum);
        assert_eq!(a.manifest.checksum, a.computed_checksum());
        assert_eq!(a.to_bytes(), b.to_bytes(), "artifact bytes are byte-identical");
        assert_eq!(a.manifest.checksum.len(), 64);
    }

    #[test]
    fn round_trips_bytes_and_store_losslessly() {
        let a = built();
        let bytes = a.to_bytes();
        let parsed = KnowledgeArtifact::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.manifest, a.manifest);
        assert_eq!(parsed.to_bytes(), bytes);
        // import(export(store)) == store (§2 round-trip, normative).
        let restored = parsed.into_store();
        assert_eq!(restored, fixture_store());
    }

    #[test]
    fn installed_store_bytes_equal_a_local_ingest() {
        // Installing an imported store writes native-store bytes identical to what
        // a local `cce knowledge index` of the same feed writes (§2/§7 byte bar).
        let store = fixture_store();
        let local = tempfile::tempdir().unwrap();
        let local_path = store.save(local.path()).unwrap();

        let pulled = KnowledgeArtifact::from_bytes(&built().to_bytes()).unwrap().into_store();
        let consumer = tempfile::tempdir().unwrap();
        let pulled_path = pulled.save(consumer.path()).unwrap();

        assert_eq!(
            std::fs::read(&local_path).unwrap(),
            std::fs::read(&pulled_path).unwrap(),
            "installed store must be byte-identical to a local ingest"
        );
        assert_eq!(
            std::fs::read_to_string(KnowledgeStore::current_pointer_path(local.path())).unwrap(),
            std::fs::read_to_string(KnowledgeStore::current_pointer_path(consumer.path()))
                .unwrap()
        );
    }

    #[test]
    fn refuses_an_embedding_less_store() {
        // A pre-v2.6.1 Phase-A snapshot: embeddings default to [] (§2 precondition).
        let mut store = fixture_store();
        for c in &mut store.chunks {
            c.embedding = Vec::new();
        }
        let err = KnowledgeArtifact::from_store(&store, "fixture").unwrap_err();
        assert!(err.contains("Re-ingest"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_tampered_checksum() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let tampered = text.replacen(&a.manifest.checksum, &"0".repeat(64), 1);
        let err = KnowledgeArtifact::from_bytes(tampered.as_bytes()).unwrap_err();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_a_flipped_content_byte() {
        let a = built();
        let mut bytes = a.to_bytes();
        // Flip one byte INSIDE a chunk's content value (past the manifest line),
        // keeping valid UTF-8 and valid JSON, so only the checksum can catch it.
        let text = std::str::from_utf8(&bytes).unwrap();
        let manifest_end = text.find('\n').unwrap();
        let marker = "\"content\":\"";
        let start = text[manifest_end..].find(marker).unwrap() + manifest_end + marker.len();
        let pos = bytes[start..]
            .iter()
            .position(|b| b.is_ascii_lowercase())
            .map(|p| p + start)
            .unwrap();
        bytes[pos] = bytes[pos].to_ascii_uppercase();
        let err = KnowledgeArtifact::from_bytes(&bytes).unwrap_err();
        assert!(err.contains("checksum mismatch"), "a flipped byte must fail loudly, got: {err}");
    }

    #[test]
    fn from_bytes_rejects_truncated_stream() {
        let err = KnowledgeArtifact::from_bytes(b"").unwrap_err();
        assert!(err.contains("too short") || err.contains("manifest"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_wrong_chunk_count() {
        let a = built();
        let mut lines: Vec<String> =
            String::from_utf8(a.to_bytes()).unwrap().lines().map(str::to_string).collect();
        lines.remove(1);
        let text = lines.join("\n") + "\n";
        let err = KnowledgeArtifact::from_bytes(text.as_bytes()).unwrap_err();
        assert!(err.contains("line count"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_non_utf8() {
        let err = KnowledgeArtifact::from_bytes(&[0xff, 0xfe, 0x00]).unwrap_err();
        assert!(err.contains("not UTF-8"), "got: {err}");
    }

    #[test]
    fn parse_manifest_rejects_missing_field() {
        // Missing `data_as_of`.
        let line = "{\"checksum\":\"x\",\"chunk_count\":0,\"contract\":\"cce.knowledge/v1\",\"corpus_id\":\"c\",\"records\":0,\"snapshot\":\"s\"}";
        let err = parse_manifest(line).unwrap_err();
        assert!(err.contains("data_as_of"), "got: {err}");
    }

    #[test]
    fn chunk_from_value_rejects_missing_field() {
        let v: Value = serde_json::from_str("{\"chunk_id\":\"x\"}").unwrap();
        assert!(chunk_from_value(&v).is_err());
    }

    #[test]
    fn optionals_serialize_as_null_and_empty_lists_as_brackets() {
        let text = "{\"id\":\"a\",\"title\":\"T\",\"body\":\"b\",\"source\":\"s\"}\n";
        let recs = parse_ndjson(text).unwrap();
        let store = ingest_default(&recs, text.as_bytes());
        let a = KnowledgeArtifact::from_store(&store, "fixture").unwrap();
        let line = String::from_utf8(a.to_bytes()).unwrap().lines().nth(1).unwrap().to_string();
        assert!(line.contains("\"url\":null"), "{line}");
        assert!(line.contains("\"state\":null"), "{line}");
        assert!(line.contains("\"updated_at\":null"), "{line}");
        assert!(line.contains("\"labels\":[]"), "{line}");
        assert!(line.contains("\"links\":[]"), "{line}");
    }

    /// The **shared golden** (SPEC-SYNC-KNOWLEDGE §2, the SPEC-SYNC §10 way): the
    /// committed fixture feed at the default budget, `corpus_id = "fixture"`. Both
    /// engines MUST reproduce this checksum, and the raw bytes are written to
    /// `/tmp/cce_knowledge_artifact_rust.cck` for a byte-for-byte diff against
    /// Ruby. A diff here is a breaking-format decision, not a test to "fix".
    #[test]
    fn shared_golden_checksum_for_the_fixture_corpus() {
        let a = built();
        let _ = std::fs::write("/tmp/cce_knowledge_artifact_rust.cck", a.to_bytes());
        assert_eq!(a.manifest.corpus_id, "fixture");
        assert_eq!(a.manifest.snapshot, "598e1b3891572bbb");
        assert_eq!(a.manifest.records, 2);
        assert_eq!(
            a.manifest.checksum,
            "84284a0ad6981d3c40c4dffd6dbb67d7e000cf6e64ff6e75b321d439ee9f452d"
        );
    }
}
