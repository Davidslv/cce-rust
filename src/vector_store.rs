//! # vector_store — exact brute-force cosine ranking
//!
//! **Why this file exists:** Vector retrieval needs the cosine similarity of the
//! query against every stored chunk. Corpora are small (SPEC §1.2), so an exact
//! brute-force scan is the correct, simplest choice — no ANN index.
//!
//! **What it is / does:** Ranks chunks by cosine to a query vector, descending,
//! with the SPEC §5.3 determinism rule: compare on the 6-decimal-rounded cosine
//! and break ties by `chunk_id` ascending.
//!
//! **Responsibilities:**
//! - Own `rank_by_cosine`: produce (chunk index, cosine) pairs, best first.
//! - It does NOT decide how many candidates to keep — the retriever slices.

use crate::chunker::Chunk;
use crate::embedder::{cosine, score_key};

/// Rank every chunk by cosine to `query`, descending, deterministic tie-break.
/// Returns `(chunk_index, cosine)` pairs in ranked order.
pub fn rank_by_cosine(query: &[f64], chunks: &[Chunk]) -> Vec<(usize, f64)> {
    let mut scored: Vec<(usize, f64)> =
        chunks.iter().enumerate().map(|(i, c)| (i, cosine(query, &c.embedding))).collect();
    scored.sort_by(|a, b| {
        // cosine descending on the rounded value
        score_key(b.1)
            .cmp(&score_key(a.1))
            .then_with(|| chunks[a.0].chunk_id.cmp(&chunks[b.0].chunk_id))
    });
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::{HashEmbedder, Embedder};

    fn mk(id: &str, emb: Vec<f64>) -> Chunk {
        Chunk {
            chunk_id: id.to_string(),
            file_path: "f".into(),
            start_line: 1,
            end_line: 1,
            chunk_type: "function".into(),
            kind: "function_definition".into(),
            language: "python".into(),
            content: String::new(),
            token_count: 1,
            embedding: emb,
        }
    }

    #[test]
    fn ranks_closest_first() {
        let e = HashEmbedder;
        let q = e.embed("hash password");
        let chunks = vec![
            mk("bbbb", e.embed("process payment amount")),
            mk("aaaa", e.embed("hash password digest")),
        ];
        let ranked = rank_by_cosine(&q, &chunks);
        assert_eq!(ranked[0].0, 1); // the hash password chunk is closest
    }

    #[test]
    fn ties_break_by_chunk_id() {
        // two identical embeddings -> equal cosine -> chunk_id asc wins
        let v = vec![0.0; crate::config::EMBED_DIM];
        let mut a = v.clone();
        a[0] = 1.0;
        let chunks = vec![mk("zzzz", a.clone()), mk("aaaa", a.clone())];
        let mut q = vec![0.0; crate::config::EMBED_DIM];
        q[0] = 1.0;
        let ranked = rank_by_cosine(&q, &chunks);
        assert_eq!(chunks[ranked[0].0].chunk_id, "aaaa");
    }
}
