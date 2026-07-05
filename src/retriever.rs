//! # retriever — the hybrid retrieval pipeline
//!
//! **Why this file exists:** This is the heart of the engine (SPEC §6): it fuses
//! vector similarity and BM25, blends in a confidence score, penalizes test/doc
//! paths, enforces per-file diversity, and optionally expands via the import
//! graph — all with the exact constants and the SPEC §5.3 determinism rules.
//!
//! **What it is / does:** Given a loaded `Index`, an embedder, a query and
//! options, produces the ranked `SearchResult` list that the CLI and the
//! conformance harness emit.
//!
//! **Responsibilities:**
//! - Own intent classification, candidate gathering, RRF, confidence, blending,
//!   penalty, diversity cap, and graph expansion — in that order.
//! - Own the deterministic sort (rounded score desc, chunk_id asc).
//! - It does NOT walk, chunk, embed corpora, or persist.

use crate::chunker::Chunk;
use crate::config::*;
use crate::embedder::{cosine, score_key, Embedder};
use crate::metrics::SearchRecord;
use crate::store::Index;
use crate::tokenizer::{estimate_tokens, tokenize};
use crate::vector_store::rank_by_cosine;
use std::collections::{HashMap, HashSet};

/// One ranked result returned by the pipeline.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub rank: usize,
    pub chunk_id: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_type: String,
    /// Exact tree-sitter node type of the chunk (SPEC-V2 §3), surfaced to callers.
    pub kind: String,
    pub score: f64,
    pub content: String,
}

/// True if the query intent is CODE_LOOKUP (SPEC §6.1).
pub fn is_code_lookup(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    // (1) whole word in {function, class, method, def}
    let words: HashSet<String> = tokenize(&lower).into_iter().collect();
    for kw in ["function", "class", "method", "def"] {
        if words.contains(kw) {
            return true;
        }
    }
    // (2) file-extension token .(py|js|jsx|ts|go|rb|rs|java) with a word boundary
    for ext in ["py", "js", "jsx", "ts", "go", "rb", "rs", "java"] {
        let pat = format!(".{ext}");
        if let Some(pos) = lower.find(&pat) {
            let after = lower[pos + pat.len()..].chars().next();
            let boundary = match after {
                None => true,
                Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
            };
            if boundary {
                return true;
            }
        }
    }
    // (3) phrase heuristics
    if lower.contains("where is ") {
        return true;
    }
    if lower.contains("defined") {
        return true;
    }
    if let Some(fp) = lower.find("find") {
        if lower[fp..].contains("function") {
            return true;
        }
    }
    false
}

/// RRF contribution for one candidate (SPEC §6.4). `vrank`/`frank` are the
/// 0-based ranks in the vector / BM25 candidate lists (None if absent).
pub fn rrf(vrank: Option<usize>, frank: Option<usize>, fts_weight: f64) -> f64 {
    let v = vrank.map(|r| 1.0 / (RRF_K + r as f64)).unwrap_or(0.0);
    let f = frank.map(|r| fts_weight * (1.0 / (RRF_K + r as f64))).unwrap_or(0.0);
    v + f
}

/// Extract crude file-hints from the raw query: whitespace terms containing '.'.
fn file_hints(query: &str) -> Vec<String> {
    query.split_whitespace().filter(|t| t.contains('.')).map(|t| t.to_ascii_lowercase()).collect()
}

/// Assemble a `search` metrics record from a result set (DASHBOARD-SPEC §2.1, §3).
///
/// Shared by the CLI `search` path and the CCE MCP `context_search` tool so both
/// log an identical `cce.metrics/v1` event to `metrics.jsonl` — that identity is
/// what lets `cce dashboard` surface agent usage the same way it surfaces CLI use.
/// `latency_ms` is measured by the caller (the one non-deterministic input).
/// `source` tags who issued the search — `"cli"` (the `cce search` path) or
/// `"mcp"` (the agent's `context_search` tool) — feeding the dashboard's
/// agent-vs-human split (v2.4.1).
pub fn build_search_record(
    index: &Index,
    results: &[SearchResult],
    query: &str,
    top_k: usize,
    graph_enabled: bool,
    latency_ms: f64,
    source: &str,
) -> SearchRecord {
    // SPEC-V2.5 §2 (Layer 1) + §4: both the baseline (whole returned files, deduped)
    // and the served tokens are counted with the ONE savings estimator
    // (`cce.tokens/v1`). `baseline_tokens` sums per-file whole-file estimates the
    // index persisted with that same estimator, so `saved = baseline − served` is
    // coherent and attributed to the `retrieval` bucket.
    let result_count = results.len();
    let baseline_tokens = index.baseline_tokens(results.iter().map(|r| r.file_path.as_str()));
    let served_tokens: u64 = results.iter().map(|r| estimate_tokens(&r.content)).sum();
    let tokens_saved = baseline_tokens.saturating_sub(served_tokens);
    let savings_ratio = if baseline_tokens == 0 {
        0.0
    } else {
        tokens_saved as f64 / baseline_tokens as f64
    };
    let top_score = results.first().map(|r| r.score).unwrap_or(0.0);
    let mean_score = if results.is_empty() {
        0.0
    } else {
        results.iter().map(|r| r.score).sum::<f64>() / results.len() as f64
    };
    let empty = result_count == 0;
    let low_confidence = !empty && top_score < LOW_CONFIDENCE_THRESHOLD;
    SearchRecord {
        query: query.to_string(),
        top_k,
        graph_enabled,
        embedder: index.embedder_name.clone(),
        result_count,
        baseline_tokens,
        served_tokens,
        tokens_saved,
        savings_ratio,
        top_score,
        mean_score,
        empty,
        low_confidence,
        latency_ms,
        source: source.to_string(),
    }
}

/// The main retrieval entry point (SPEC §6).
pub fn search(
    index: &Index,
    embedder: &dyn Embedder,
    query: &str,
    top_k: usize,
    graph_enabled: bool,
) -> Vec<SearchResult> {
    let qvec = embedder.embed(query);
    let mut results = rank_core(index, &qvec, query, top_k);

    // --- Graph expansion (SPEC §6.7) ---
    if graph_enabled {
        expand_graph(index, &qvec, &mut results);
    }

    for (i, r) in results.iter_mut().enumerate() {
        r.rank = i + 1;
    }
    results
}

/// The core §6 ranking (embed → candidates → RRF → confidence → penalty →
/// diversity cap → top-K), WITHOUT graph expansion and WITHOUT assigning final
/// ranks. Exposed so federated search (SPEC-V2.2 §6) can run the identical
/// pipeline over the union of members' chunks and then apply its own expansion.
/// `qvec` is the pre-embedded query vector (so callers embed once).
pub fn rank_core(index: &Index, qvec: &[f64], query: &str, top_k: usize) -> Vec<SearchResult> {
    let chunks = &index.chunks;
    let query_tokens = tokenize(query);
    if query_tokens.is_empty() || chunks.is_empty() {
        return Vec::new();
    }

    let fts_weight = if is_code_lookup(query) {
        FTS_BOOST_CODE_LOOKUP
    } else {
        1.0
    };

    // --- Vector candidates (SPEC §6.2) ---
    let ranked = rank_by_cosine(qvec, chunks); // all chunks, best first
    let mut cosine_by_idx = vec![0.0f64; chunks.len()];
    for (idx, cos) in &ranked {
        cosine_by_idx[*idx] = *cos;
    }
    let vcand_n = (top_k * CANDIDATE_MULTIPLIER).max(1);
    let mut vrank: HashMap<usize, usize> = HashMap::new();
    for (rank, (idx, _)) in ranked.iter().take(vcand_n).enumerate() {
        vrank.insert(*idx, rank);
    }

    // --- BM25 candidates (SPEC §6.3) ---
    let unique_q: Vec<String> = {
        let mut seen = HashSet::new();
        query_tokens.iter().filter(|t| seen.insert((*t).clone())).cloned().collect()
    };
    let bm25_ranked = index.bm25.score(&unique_q);
    let fcand_n = top_k * CANDIDATE_MULTIPLIER;
    let mut frank: HashMap<usize, usize> = HashMap::new();
    for (rank, (idx, _)) in bm25_ranked.iter().take(fcand_n).enumerate() {
        frank.insert(*idx, rank);
    }

    // --- Candidate id set = union (SPEC §6.4) ---
    let mut candidate_idxs: Vec<usize> = Vec::new();
    {
        let mut seen = HashSet::new();
        for idx in vrank.keys().chain(frank.keys()) {
            if seen.insert(*idx) {
                candidate_idxs.push(*idx);
            }
        }
    }

    let hints = file_hints(query);

    // --- RRF (SPEC §6.4) ---
    let rrf_of =
        |idx: usize| -> f64 { rrf(vrank.get(&idx).copied(), frank.get(&idx).copied(), fts_weight) };
    let max_rrf = candidate_idxs.iter().map(|i| rrf_of(*i)).fold(0.0f64, f64::max);

    // --- Confidence + blend + penalty (SPEC §6.5, §6.6) ---
    let mut scored: Vec<(usize, f64)> = Vec::with_capacity(candidate_idxs.len());
    for &idx in &candidate_idxs {
        let chunk = &chunks[idx];
        let cos = if vrank.contains_key(&idx) {
            cosine_by_idx[idx]
        } else {
            // BM25-only: compute cosine now (SPEC §6.5)
            cosine(qvec, &chunk.embedding)
        };
        let vector_distance = 1.0 - cos;
        let normalized_distance = (vector_distance / 2.0).clamp(0.0, 1.0);
        let vector_score = 1.0 - normalized_distance;

        let keyword_distance = keyword_distance(chunk, &unique_q, &hints);
        let keyword_score = (1.0 - keyword_distance / 5.0).max(0.0);
        let recency_score = 0.0;

        let confidence =
            W_VECTOR * vector_score + W_KEYWORD * keyword_score + W_RECENCY * recency_score;

        let norm_rrf = if score_key(max_rrf) != 0 {
            rrf_of(idx) / max_rrf
        } else {
            0.0
        };

        let mut final_score = CONFIDENCE_WEIGHT * confidence + (1.0 - CONFIDENCE_WEIGHT) * norm_rrf;
        if has_penalty_marker(&chunk.file_path) {
            final_score *= PATH_PENALTY;
        }
        scored.push((idx, final_score));
    }

    // --- Sort by score desc, tie-break chunk_id asc (SPEC §6.6) ---
    scored.sort_by(|a, b| {
        score_key(b.1)
            .cmp(&score_key(a.1))
            .then_with(|| chunks[a.0].chunk_id.cmp(&chunks[b.0].chunk_id))
    });

    // --- Diversity cap (SPEC §6.6) ---
    let mut per_file: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<(usize, f64)> = Vec::new();
    for (idx, sc) in &scored {
        let fp = &chunks[*idx].file_path;
        let count = per_file.entry(fp.clone()).or_insert(0);
        if *count < MAX_CHUNKS_PER_FILE {
            *count += 1;
            kept.push((*idx, *sc));
            if kept.len() >= top_k {
                break;
            }
        }
    }

    kept.iter().map(|(idx, sc)| result_from(&chunks[*idx], *sc)).collect()
}

/// keyword_distance per SPEC §6.5: 0 if any query token substring of lowercased
/// content OR any file-hint substring of file_path, else 2.
fn keyword_distance(chunk: &Chunk, unique_q: &[String], hints: &[String]) -> f64 {
    let content_lower = chunk.content.to_ascii_lowercase();
    for q in unique_q {
        if content_lower.contains(q.as_str()) {
            return 0.0;
        }
    }
    let path_lower = chunk.file_path.to_ascii_lowercase();
    for h in hints {
        if path_lower.contains(h.as_str()) {
            return 0.0;
        }
    }
    2.0
}

/// True if the lowercased file path contains any path-penalty marker.
fn has_penalty_marker(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    PATH_PENALTY_MARKERS.iter().any(|m| lower.contains(m))
}

/// Build a `SearchResult` from a chunk + score (rank assigned later). Shared with
/// federated search (SPEC-V2.2 §6).
pub fn result_from(chunk: &Chunk, score: f64) -> SearchResult {
    SearchResult {
        rank: 0,
        chunk_id: chunk.chunk_id.clone(),
        file_path: chunk.file_path.clone(),
        start_line: chunk.start_line,
        end_line: chunk.end_line,
        chunk_type: chunk.chunk_type.clone(),
        kind: chunk.kind.clone(),
        score,
        content: chunk.content.clone(),
    }
}

/// Import-graph expansion (SPEC §6.7). Appends bonus chunks after `results`.
/// Public so federated search reuses the identical intra-store expansion.
pub fn expand_graph(index: &Index, qvec: &[f64], results: &mut Vec<SearchResult>) {
    if results.is_empty() {
        return;
    }
    // (1) file paths of the top 3 ranked results (unique, order-preserving).
    let mut top_files: Vec<String> = Vec::new();
    for r in results.iter().take(3) {
        if !top_files.contains(&r.file_path) {
            top_files.push(r.file_path.clone());
        }
    }
    // Files already represented in the result set.
    let result_files: HashSet<String> = results.iter().map(|r| r.file_path.clone()).collect();

    // (2) neighbor files not already in the result set.
    let mut neighbor_files: Vec<String> = Vec::new();
    for f in &top_files {
        for nb in index.graph.neighbors(f) {
            if !result_files.contains(&nb) && !neighbor_files.contains(&nb) {
                neighbor_files.push(nb);
            }
        }
    }

    // Track duplicates by (file_path, start, end).
    let mut existing: HashSet<(String, usize, usize)> =
        results.iter().map(|r| (r.file_path.clone(), r.start_line, r.end_line)).collect();

    // (3) up to GRAPH_MAX_BONUS_FILES neighbor files, up to 2 chunks each.
    for nb in neighbor_files.into_iter().take(GRAPH_MAX_BONUS_FILES) {
        let mut file_chunks: Vec<(&Chunk, f64)> = index
            .chunks
            .iter()
            .filter(|c| c.file_path == nb)
            .map(|c| (c, cosine(qvec, &c.embedding)))
            .collect();
        file_chunks.sort_by(|a, b| {
            score_key(b.1).cmp(&score_key(a.1)).then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
        });
        for (chunk, cos) in file_chunks.into_iter().take(2) {
            let key = (chunk.file_path.clone(), chunk.start_line, chunk.end_line);
            if existing.contains(&key) {
                continue;
            }
            existing.insert(key);
            let score = cos.max(0.0) * GRAPH_BONUS_CHUNK_SCALE;
            results.push(result_from(chunk, score));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fixture_index() -> Index {
        let e = HashEmbedder;
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"));
        Index::build_from_dir(&dir, &e).0
    }

    #[test]
    fn rrf_anchor() {
        // SPEC §6.4: id at vrank 0 and frank 2, fts_weight 1.0 -> 1/60 + 1/62.
        let v = rrf(Some(0), Some(2), 1.0);
        assert!((v - (1.0 / 60.0 + 1.0 / 62.0)).abs() < 1e-12);
        assert_eq!(format!("{:.6}", v), "0.032796");
        // vector-only and bm25-only contributions.
        assert!((rrf(Some(0), None, 1.0) - 1.0 / 60.0).abs() < 1e-12);
        assert!((rrf(None, Some(0), 1.5) - 1.5 / 60.0).abs() < 1e-12);
        assert_eq!(rrf(None, None, 1.0), 0.0);
    }

    #[test]
    fn intent_classification() {
        assert!(is_code_lookup("where is the hash function"));
        assert!(is_code_lookup("class SessionManager"));
        assert!(is_code_lookup("auth.py login"));
        assert!(is_code_lookup("where is create_session"));
        assert!(is_code_lookup("where hash_password is defined"));
        assert!(!is_code_lookup("hash password"));
        assert!(!is_code_lookup("process payment amount"));
    }

    #[test]
    fn conformance_q1_top1_is_hash_password() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let res = search(&idx, &e, "hash password", 5, false);
        assert!(!res.is_empty());
        assert_eq!(res[0].file_path, "auth.py");
        assert!(res[0].content.contains("def hash_password"));
    }

    #[test]
    fn conformance_q2_top1_is_process_payment() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let res = search(&idx, &e, "process payment amount", 5, false);
        assert_eq!(res[0].file_path, "payments.py");
        assert!(res[0].content.contains("def process_payment"));
    }

    #[test]
    fn conformance_q3_top1_from_auth() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let res = search(&idx, &e, "create session user", 5, false);
        assert_eq!(res[0].file_path, "auth.py");
        assert!(
            res[0].content.contains("create_session") || res[0].content.contains("SessionManager")
        );
    }

    #[test]
    fn empty_query_returns_empty() {
        let idx = fixture_index();
        let e = HashEmbedder;
        assert!(search(&idx, &e, "", 5, false).is_empty());
        assert!(search(&idx, &e, "   ", 5, false).is_empty());
    }

    #[test]
    fn diversity_cap_respected() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let res = search(&idx, &e, "password digest user session payment", 10, false);
        let mut per_file: HashMap<String, usize> = HashMap::new();
        for r in &res {
            *per_file.entry(r.file_path.clone()).or_insert(0) += 1;
        }
        assert!(per_file.values().all(|&c| c <= MAX_CHUNKS_PER_FILE));
    }

    #[test]
    fn graph_expansion_adds_related_file_chunks() {
        let idx = fixture_index();
        let e = HashEmbedder;
        // Query strongly about payments; graph should pull in auth.py (imported).
        let no_graph = search(&idx, &e, "process payment amount", 2, false);
        let with_graph = search(&idx, &e, "process payment amount", 2, true);
        assert!(with_graph.len() >= no_graph.len());
        // With graph enabled, an auth.py chunk should appear as a bonus.
        assert!(with_graph.iter().any(|r| r.file_path == "auth.py"));
    }

    #[test]
    fn scores_are_deterministic_across_runs() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let a = search(&idx, &e, "hash password", 5, false);
        let b = search(&idx, &e, "hash password", 5, false);
        let ax: Vec<(String, i64)> =
            a.iter().map(|r| (r.chunk_id.clone(), score_key(r.score))).collect();
        let bx: Vec<(String, i64)> =
            b.iter().map(|r| (r.chunk_id.clone(), score_key(r.score))).collect();
        assert_eq!(ax, bx);
    }
}
