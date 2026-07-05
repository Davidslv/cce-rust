//! # tests/knowledge_ingest — byte-pinned ingest goldens + additivity proof (SPEC-V2.6 §9)
//!
//! **Why this file exists:** The `cce knowledge index` path is deterministic and
//! byte-pinned so cce-ruby can reconcile to it later, and it must be **fully
//! additive**: adding the markdown chunker + knowledge store must NOT move the code
//! index. This suite freezes a fixed `cce.knowledge/v1` fixture → a byte-identical
//! knowledge store (snapshot id + store checksum + chunk ids pinned), proves facets
//! are attached and a secret in a body is redacted before write, AND proves the
//! committed `conformance.json` regenerates byte-identical (the code path is untouched).
//!
//! **What it is / does:** Ingests `test/fixture/knowledge/curated.jsonl` at the default
//! budget and asserts the store's snapshot/checksum/ids to the byte; regenerates
//! conformance over `test/fixture/samples` and diffs it against the committed file.
//!
//! **Responsibilities:**
//! - Own the ingest determinism goldens + the redaction-before-write proof.
//! - Own the `conformance.json` byte-identical regression (the additivity gate).

use cce::conformance;
use cce::knowledge::{ingest_default, parse_ndjson, KnowledgeStore};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn manifest(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn fixture_store() -> (KnowledgeStore, String) {
    let text = std::fs::read_to_string(manifest("test/fixture/knowledge/curated.jsonl")).unwrap();
    let recs = parse_ndjson(&text).unwrap();
    let store = ingest_default(&recs, text.as_bytes());
    let json = serde_json::to_string_pretty(&store).unwrap() + "\n";
    (store, json)
}

#[test]
fn ingest_is_byte_identical_and_ids_are_pinned() {
    let (store, json) = fixture_store();
    // Snapshot id (hash of the input feed) is pinned.
    assert_eq!(store.snapshot, "598e1b3891572bbb");
    // The persisted store bytes are pinned (cce-ruby must reproduce this checksum). The
    // v2.6 Phase-B store additionally carries the M4 retrieval facets (`title`, `links`)
    // and the deterministic hash embedding per chunk, so this checksum supersedes the
    // Phase-A value; the chunk ids (content-addressed) are unchanged.
    assert_eq!(
        sha256_hex(json.as_bytes()),
        "ab66052f618f84693cd229a1926dfb28d2570697d803fa9006281ee584a41110"
    );
    // The chunk ids are pinned (content-addressed over the redacted rendered doc) —
    // UNCHANGED by the Phase-B store extension.
    let ids: Vec<&str> = store.chunks.iter().map(|c| c.chunk_id.as_str()).collect();
    assert_eq!(ids, vec!["5fb9ad2eca3c1ee6", "64a595c97b7c78af"]);
    assert_eq!(store.schema, "cce.knowledge/v1");
    assert_eq!(store.records, 2);
    // Determinism: a second ingest is byte-identical.
    let (_, json2) = fixture_store();
    assert_eq!(json, json2);
}

#[test]
fn facets_are_attached_from_the_record() {
    let (store, _) = fixture_store();
    let policy = &store.chunks[0];
    assert_eq!(policy.record_id, "gh:acme/app#12");
    assert_eq!(policy.source, "github-issues");
    assert_eq!(policy.state.as_deref(), Some("closed"));
    assert_eq!(policy.state_reason.as_deref(), Some("completed"));
    assert_eq!(policy.updated_at.as_deref(), Some("2026-02-01T10:00:00Z"));
    assert_eq!(policy.group.as_deref(), Some("Identity"));
    assert_eq!(policy.url.as_deref(), Some("https://example.test/12"));
    assert_eq!(policy.labels, vec!["policy".to_string(), "auth".to_string()]);
    // Degraded optionals on the second record.
    let epic = &store.chunks[1];
    assert_eq!(epic.state.as_deref(), Some("open"));
    assert_eq!(epic.state_reason, None);
    assert_eq!(epic.updated_at, None);
    assert_eq!(epic.url, None);
}

#[test]
fn secret_in_a_body_is_redacted_in_the_store() {
    let (store, json) = fixture_store();
    // The raw secret never reaches the store; the marker is present instead.
    assert!(!json.contains("s3cr3tvalue123"), "raw secret leaked into the store");
    assert!(store.chunks[1].content.contains("api_key = [REDACTED:SECRET]"));
}

#[test]
fn save_writes_snapshot_artifact_and_current_pointer() {
    let (store, _) = fixture_store();
    let tmp = tempfile::tempdir().unwrap();
    let path = store.save(tmp.path()).unwrap();
    assert_eq!(path, KnowledgeStore::snapshot_path(tmp.path(), &store.snapshot));
    assert!(path.exists());
    let loaded = KnowledgeStore::load_current(tmp.path()).unwrap();
    assert_eq!(loaded, store);
}

/// The additivity gate (SPEC-V2.6 §1.2): the committed `conformance.json` must
/// regenerate byte-identical. The markdown chunker is NOT registered in the code
/// registry, so the code index's `.md` handling — and thus conformance — is untouched.
#[test]
fn conformance_json_is_byte_identical() {
    let committed = std::fs::read_to_string(manifest("conformance.json")).unwrap();
    // `cce conformance` writes `generate(...) + "\n"`; reproduce that exactly.
    let regenerated = conformance::generate(&manifest("test/fixture/samples")) + "\n";
    assert_eq!(
        regenerated, committed,
        "conformance.json drifted — the code index's .md handling must stay byte-identical"
    );
}
