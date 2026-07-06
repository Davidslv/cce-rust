//! # keyword_store — BM25 keyword index and scoring
//!
//! **Why this file exists:** Hybrid retrieval (SPEC §6) fuses vector similarity
//! with lexical BM25 so exact identifier/keyword matches are not lost by the
//! hashing embedder. This module owns the BM25 half.
//!
//! **What it is / does:** Builds a corpus index (per-doc token frequencies and
//! lengths, document frequencies, average length) from the chunk contents using
//! the shared tokenizer, and scores documents for a query with the exact
//! Lucene-form BM25 of SPEC §6.3 (non-negative idf).
//!
//! **Responsibilities:**
//! - Own `Bm25Index` construction and `score` (ranked, deterministic tie-break).
//! - Exclude documents that score 0 (no query term present).
//! - It does NOT persist itself; the store recomputes it on load.

use crate::chunker::Chunk;
use crate::config::{BM25_B, BM25_K1};
use crate::embedder::score_key;
use crate::tokenizer::tokenize;
use std::collections::HashMap;

/// Precomputed BM25 statistics for a corpus of chunks.
pub struct Bm25Index {
    /// Per-document term frequencies.
    tf: Vec<HashMap<String, usize>>,
    /// Per-document length |D| (token count from tokenize(content)).
    doc_len: Vec<usize>,
    /// Document frequency n_q per term.
    df: HashMap<String, usize>,
    /// Average document length.
    avgdl: f64,
    /// Number of documents.
    n: usize,
    /// chunk_id per document, for deterministic tie-breaking.
    chunk_ids: Vec<String>,
}

impl Bm25Index {
    /// An empty index (no documents). Used for the per-member stores loaded on the
    /// federation path, whose BM25 is never queried — only the assembled *union*'s
    /// BM25 is scored — so building each member's BM25 on load is pure wasted work
    /// (it re-tokenizes the whole corpus a second time). Scoring an empty index
    /// returns no candidates, which is exactly what a never-queried member wants.
    pub fn empty() -> Bm25Index {
        Bm25Index {
            tf: Vec::new(),
            doc_len: Vec::new(),
            df: HashMap::new(),
            avgdl: 0.0,
            n: 0,
            chunk_ids: Vec::new(),
        }
    }

    /// Build the index from chunks, tokenizing each chunk's content.
    pub fn build(chunks: &[Chunk]) -> Bm25Index {
        let mut tf = Vec::with_capacity(chunks.len());
        let mut doc_len = Vec::with_capacity(chunks.len());
        let mut df: HashMap<String, usize> = HashMap::new();
        let mut chunk_ids = Vec::with_capacity(chunks.len());
        let mut total_len: usize = 0;

        for c in chunks {
            let toks = tokenize(&c.content);
            let mut freqs: HashMap<String, usize> = HashMap::new();
            for t in &toks {
                *freqs.entry(t.clone()).or_insert(0) += 1;
            }
            for term in freqs.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            total_len += toks.len();
            doc_len.push(toks.len());
            tf.push(freqs);
            chunk_ids.push(c.chunk_id.clone());
        }

        let n = chunks.len();
        let avgdl = if n > 0 { total_len as f64 / n as f64 } else { 0.0 };
        Bm25Index { tf, doc_len, df, avgdl, n, chunk_ids }
    }

    /// idf(q) = ln(1 + (N - n_q + 0.5)/(n_q + 0.5)) (Lucene, non-negative).
    fn idf(&self, term: &str) -> f64 {
        let n_q = *self.df.get(term).unwrap_or(&0) as f64;
        let n = self.n as f64;
        (1.0 + (n - n_q + 0.5) / (n_q + 0.5)).ln()
    }

    /// BM25 score of a single document for a set of query terms.
    fn score_doc(&self, doc: usize, query_terms: &[String]) -> f64 {
        let dl = self.doc_len[doc] as f64;
        let mut s = 0.0;
        for q in query_terms {
            let n_q = *self.df.get(q).unwrap_or(&0);
            if n_q == 0 {
                continue;
            }
            let f = *self.tf[doc].get(q).unwrap_or(&0) as f64;
            if f == 0.0 {
                continue;
            }
            let idf = self.idf(q);
            let denom = f + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / self.avgdl);
            s += idf * (f * (BM25_K1 + 1.0)) / denom;
        }
        s
    }

    /// Score all documents for the unique query tokens; return ranked
    /// `(doc_index, score)`, score descending, tie-break chunk_id ascending.
    /// Documents scoring 0 are excluded.
    pub fn score(&self, unique_query_tokens: &[String]) -> Vec<(usize, f64)> {
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for doc in 0..self.n {
            let s = self.score_doc(doc, unique_query_tokens);
            if score_key(s) > 0 {
                scored.push((doc, s));
            }
        }
        scored.sort_by(|a, b| {
            score_key(b.1)
                .cmp(&score_key(a.1))
                .then_with(|| self.chunk_ids[a.0].cmp(&self.chunk_ids[b.0]))
        });
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, content: &str) -> Chunk {
        Chunk {
            chunk_id: id.to_string(),
            file_path: "f".into(),
            start_line: 1,
            end_line: 1,
            chunk_type: "function".into(),
            kind: "function_definition".into(),
            language: "python".into(),
            content: content.to_string(),
            token_count: 1,
            embedding: vec![],
        }
    }

    #[test]
    fn worked_anchor_example() {
        // D1 = user login user, D2 = payment process; query = "user"
        let chunks = vec![mk("d1", "user login user"), mk("d2", "payment process")];
        let idx = Bm25Index::build(&chunks);
        assert_eq!(idx.doc_len, vec![3, 2]);
        assert!((idx.avgdl - 2.5).abs() < 1e-12);
        let q = ["user".to_string()];
        // idf = ln(2)
        assert!((idx.idf("user") - 2.0_f64.ln()).abs() < 1e-4);
        let scored = idx.score(&q);
        // Only D1 scores; D2 excluded.
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].0, 0);
        assert!((scored[0].1 - 0.902273).abs() < 1e-4);
    }

    #[test]
    fn zero_score_docs_excluded() {
        let chunks = vec![mk("d1", "alpha beta"), mk("d2", "gamma delta")];
        let idx = Bm25Index::build(&chunks);
        let scored = idx.score(&["nonexistent".to_string()]);
        assert!(scored.is_empty());
    }
}
