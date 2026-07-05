//! # conformance — cross-implementation equivalence output
//!
//! **Why this file exists:** SPEC §8 makes `conformance.json` a hard acceptance
//! gate: two implementations must produce identical chunk and query results on a
//! fixed fixture. This module owns that exact output format.
//!
//! **What it is / does:** Indexes a fixture directory (hash embedder), lists all
//! chunks sorted by `(file_path, start_line, chunk_id)`, runs the three SPEC §8.2
//! queries with `top_k = 5` and graph disabled, and serializes the SPEC §8.3
//! JSON with fields in the exact documented order and scores as fixed 6-decimal
//! strings.
//!
//! **Responsibilities:**
//! - Own the `conformance.json` schema (struct field order == spec order).
//! - Guarantee determinism: same input -> byte-identical output every run.
//! - It does NOT include graph expansion (queries run with it disabled).

use crate::config::SPEC_VERSION;
use crate::embedder::{format6, HashEmbedder};
use crate::retriever::search;
use crate::store::Index;
use serde::Serialize;
use std::path::Path;

// Struct field order below MUST match the SPEC §8.3 example byte-for-byte.

#[derive(Serialize)]
struct ChunkOut {
    file_path: String,
    start_line: usize,
    end_line: usize,
    chunk_type: String,
    chunk_id: String,
    token_count: usize,
}

#[derive(Serialize)]
struct ResultOut {
    rank: usize,
    chunk_id: String,
    file_path: String,
    score: String,
}

#[derive(Serialize)]
struct QueryOut {
    query: String,
    top_k: usize,
    graph_enabled: bool,
    results: Vec<ResultOut>,
}

#[derive(Serialize)]
struct Conformance {
    spec_version: String,
    impl_language: String,
    chunks: Vec<ChunkOut>,
    queries: Vec<QueryOut>,
}

/// The three SPEC §8.2 conformance queries.
const QUERIES: [&str; 3] = ["hash password", "process payment amount", "create session user"];

/// Build the conformance JSON string for a fixture directory. Deterministic.
pub fn generate(fixture_dir: &Path) -> String {
    let embedder = HashEmbedder;
    let (index, _) = Index::build_from_dir(fixture_dir, &embedder);

    // chunks sorted by (file_path, start_line, chunk_id)
    let mut chunks: Vec<ChunkOut> = index
        .chunks
        .iter()
        .map(|c| ChunkOut {
            file_path: c.file_path.clone(),
            start_line: c.start_line,
            end_line: c.end_line,
            chunk_type: c.chunk_type.clone(),
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

    let top_k = 5; // SPEC §8.2
    let queries: Vec<QueryOut> = QUERIES
        .iter()
        .map(|q| {
            let results = search(&index, &embedder, q, top_k, false)
                .into_iter()
                .map(|r| ResultOut {
                    rank: r.rank,
                    chunk_id: r.chunk_id,
                    file_path: r.file_path,
                    score: format6(r.score),
                })
                .collect();
            QueryOut { query: (*q).to_string(), top_k, graph_enabled: false, results }
        })
        .collect();

    let out = Conformance {
        spec_version: SPEC_VERSION.to_string(),
        impl_language: "rust".to_string(),
        chunks,
        queries,
    };
    // Pretty, deterministic (serde serializes struct fields in declaration order).
    serde_json::to_string_pretty(&out).expect("serialize conformance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture"))
    }

    #[test]
    fn deterministic_output() {
        let a = generate(&fixture());
        let b = generate(&fixture());
        assert_eq!(a, b);
    }

    #[test]
    fn has_seven_chunks_and_three_queries() {
        let json = generate(&fixture());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["chunks"].as_array().unwrap().len(), 7);
        assert_eq!(v["queries"].as_array().unwrap().len(), 3);
        assert_eq!(v["spec_version"], "1.0");
        assert_eq!(v["impl_language"], "rust");
    }

    #[test]
    fn chunks_sorted_and_scores_are_6dp_strings() {
        let json = generate(&fixture());
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let q1 = &v["queries"][0];
        assert_eq!(q1["query"], "hash password");
        assert_eq!(q1["graph_enabled"], false);
        let score = q1["results"][0]["score"].as_str().unwrap();
        assert_eq!(score.split('.').nth(1).unwrap().len(), 6);
        // Q1 top-1 from auth.py
        assert_eq!(q1["results"][0]["file_path"], "auth.py");
    }
}
