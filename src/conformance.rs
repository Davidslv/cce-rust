//! # conformance — cross-implementation equivalence output (v2)
//!
//! **Why this file exists:** SPEC-V2 §7 keeps `conformance.json` as a hard
//! acceptance gate: two implementations must produce byte-identical chunk arrays
//! on the shared `samples/` corpus. This module owns that exact output format.
//!
//! **What it is / does:** Indexes a fixture directory (hash embedder, graph
//! disabled), lists every chunk sorted by `(file_path, start_line, chunk_id)`,
//! and serializes each as `{file_path, start_line, end_line, chunk_type, kind,
//! chunk_id, token_count}`. The `kind` field is new in v2. The base query section
//! is dropped: the chunk section is the equivalence gate, and the samples are a
//! multi-language corpus for which the old Python-specific queries do not apply.
//!
//! **Responsibilities:**
//! - Own the v2 `conformance.json` schema (struct field order == spec order).
//! - Guarantee determinism: same input -> byte-identical output every run.
//! - It does NOT run retrieval or graph expansion.

use crate::config::CONFORMANCE_SPEC_VERSION;
use crate::embedder::HashEmbedder;
use crate::store::Index;
use serde::Serialize;
use std::path::Path;

// Struct field order below MUST match SPEC-V2 §7 byte-for-byte.

#[derive(Serialize)]
struct ChunkOut {
    file_path: String,
    start_line: usize,
    end_line: usize,
    chunk_type: String,
    kind: String,
    chunk_id: String,
    token_count: usize,
}

#[derive(Serialize)]
struct Conformance {
    spec_version: String,
    impl_language: String,
    chunks: Vec<ChunkOut>,
}

/// Build the v2 conformance JSON string for a fixture directory. Deterministic.
pub fn generate(fixture_dir: &Path) -> String {
    let embedder = HashEmbedder;
    let (index, _) = Index::build_from_dir(fixture_dir, &embedder);

    let mut chunks: Vec<ChunkOut> = index
        .chunks
        .iter()
        .map(|c| ChunkOut {
            file_path: c.file_path.clone(),
            start_line: c.start_line,
            end_line: c.end_line,
            chunk_type: c.chunk_type.clone(),
            kind: c.kind.clone(),
            chunk_id: c.chunk_id.clone(),
            token_count: c.token_count,
        })
        .collect();
    chunks.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });

    let out = Conformance {
        spec_version: CONFORMANCE_SPEC_VERSION.to_string(),
        impl_language: "rust".to_string(),
        chunks,
    };
    // Pretty, deterministic (serde serializes struct fields in declaration order).
    serde_json::to_string_pretty(&out).expect("serialize conformance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn samples() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"))
    }

    #[test]
    fn deterministic_output() {
        let a = generate(&samples());
        let b = generate(&samples());
        assert_eq!(a, b);
    }

    #[test]
    fn emits_v2_shape_with_kind() {
        let json = generate(&samples());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["spec_version"], "2.0");
        assert_eq!(v["impl_language"], "rust");
        // No queries section in v2.
        assert!(v.get("queries").is_none());
        let chunks = v["chunks"].as_array().unwrap();
        // Seven samples across six languages plus one fallback (see §6/§7).
        assert_eq!(chunks.len(), 21);
        // Every chunk carries a non-empty kind; the fallback's kind is "module".
        assert!(chunks.iter().all(|c| !c["kind"].as_str().unwrap().is_empty()));
        let notes = chunks.iter().find(|c| c["file_path"] == "notes.md").unwrap();
        assert_eq!(notes["kind"], "module");
        assert_eq!(notes["chunk_type"], "module");
        assert_eq!(notes["start_line"], 1);
        assert_eq!(notes["end_line"], 3);
    }

    #[test]
    fn chunks_sorted_by_path_line_id() {
        let json = generate(&samples());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let chunks = v["chunks"].as_array().unwrap();
        let keyed: Vec<(String, u64, String)> = chunks
            .iter()
            .map(|c| {
                (
                    c["file_path"].as_str().unwrap().to_string(),
                    c["start_line"].as_u64().unwrap(),
                    c["chunk_id"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let mut sorted = keyed.clone();
        sorted.sort();
        assert_eq!(keyed, sorted);
    }
}
