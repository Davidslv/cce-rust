//! # relevance — the retrieval-relevance evaluation harness (issue #63)
//!
//! **Why this file exists:** CCE's measurement story had two legs: `cce
//! conformance` proves output *stability* (byte-identity) and `cce bench` /
//! `cce eval` measure *latency and token savings*. Nothing measured **ranking
//! quality** — so a proposed ranking change could not show which queries it
//! helps or hurts. This module is that third leg: labeled query→expected-result
//! fixtures scored with standard IR metrics, runnable per retrieval backend.
//!
//! **What it is / does:** Parses `cce.relevance/v1` fixture sets (NDJSON, one
//! labeled query per line), runs each query through the REAL retrieval
//! pipeline at a named backend configuration (`bm25` = the issue-#30 degraded
//! mode, `vector` = pure cosine ranking, `hybrid` = the full SPEC §6 pipeline
//! that `cce search` serves), and scores precision@k, recall, MRR, and F1 per
//! query plus macro-averaged per backend. A comparison mode diffs two backends
//! per query. Deterministic for deterministic backends: with the hash embedder
//! the JSON report is byte-pinnable, conformance-style.
//!
//! **Responsibilities:**
//! - Own the `cce.relevance/v1` fixture contract and its parsing.
//! - Own the IR metric math and the deterministic report rendering.
//! - It does NOT rank: every backend is an existing pipeline entry point
//!   (`retriever::search`, `retriever::bm25_only_search`,
//!   `vector_store::rank_by_cosine`). This harness only measures — it never
//!   changes ranking behavior.

use crate::config::DEFAULT_TOP_K;
use crate::embedder::{format6, Embedder};
use crate::retriever::{bm25_only_search, result_from, search, SearchResult};
use crate::store::Index;
use crate::vector_store::rank_by_cosine;

/// The pinned schema id for the fixture contract. A bump is a compatibility event.
pub const RELEVANCE_SCHEMA_ID: &str = "cce.relevance/v1";

/// The pinned schema id of the `--json` report shape.
pub const RELEVANCE_REPORT_SCHEMA_ID: &str = "cce.relevance.report/v1";

// --- Fixture contract (cce.relevance/v1) ---

/// One expected-result anchor. A retrieved result matches an anchor when every
/// present facet matches: `file_path` equality and/or chunk `kind` equality.
///
/// String forms (the documented contract):
/// - `"auth.py"` — any chunk of that file
/// - `"auth.py#function_definition"` — a chunk of that file with that kind
/// - `"#interface_declaration"` — any chunk of that kind, in any file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub file_path: Option<String>,
    pub kind: Option<String>,
}

impl Anchor {
    /// Parse the `path`, `path#kind`, or `#kind` string form.
    pub fn parse(s: &str) -> Result<Anchor, String> {
        let (path, kind) = match s.split_once('#') {
            Some((p, k)) => (p.trim(), Some(k.trim())),
            None => (s.trim(), None),
        };
        let file_path = if path.is_empty() { None } else { Some(path.to_string()) };
        let kind = match kind {
            Some(k) if !k.is_empty() => Some(k.to_string()),
            Some(_) => return Err(format!("anchor {s:?} has an empty kind after '#'")),
            None => None,
        };
        if file_path.is_none() && kind.is_none() {
            return Err("anchor is empty — expected `path`, `path#kind`, or `#kind`".to_string());
        }
        Ok(Anchor { file_path, kind })
    }

    /// True when `result` satisfies every present facet of this anchor.
    pub fn matches(&self, result: &SearchResult) -> bool {
        if let Some(fp) = &self.file_path {
            if &result.file_path != fp {
                return false;
            }
        }
        if let Some(k) = &self.kind {
            if &result.kind != k {
                return false;
            }
        }
        true
    }

    /// The canonical string form (for reporting).
    pub fn display(&self) -> String {
        match (&self.file_path, &self.kind) {
            (Some(p), Some(k)) => format!("{p}#{k}"),
            (Some(p), None) => p.clone(),
            (None, Some(k)) => format!("#{k}"),
            (None, None) => String::new(),
        }
    }
}

/// One labeled query case.
#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    pub query: String,
    pub expected: Vec<Anchor>,
    pub k: usize,
}

/// A parsed fixture set: the optional corpus hint from the header line plus the cases.
#[derive(Debug, Clone)]
pub struct FixtureSet {
    /// Corpus directory declared by the header line, relative to the fixture file.
    pub corpus: Option<String>,
    pub cases: Vec<Case>,
}

/// Parse a `cce.relevance/v1` NDJSON fixture set.
///
/// Line grammar:
/// - Blank lines are skipped.
/// - An optional single **header** line (an object with a `schema` key and no
///   `query`) pins the schema id and may declare a default `corpus` directory,
///   resolved relative to the fixture file by the CLI.
/// - Every other line is a **case**: `{"id", "query", "expected": [anchors], "k"}`.
///   `query` and a non-empty `expected` are required; `id` defaults to `q<line#>`
///   and `k` defaults to 10 (`DEFAULT_TOP_K`).
pub fn parse_fixtures(text: &str) -> Result<FixtureSet, String> {
    let mut corpus: Option<String> = None;
    let mut cases: Vec<Case> = Vec::new();
    let mut seen_case = false;
    for (lineno, line) in text.lines().enumerate() {
        let lineno = lineno + 1;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("fixture line {lineno}: invalid JSON: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| format!("fixture line {lineno}: expected a JSON object"))?;

        // Header line: `schema` present, `query` absent.
        if obj.contains_key("schema") && !obj.contains_key("query") {
            if seen_case || corpus.is_some() || !cases.is_empty() {
                return Err(format!(
                    "fixture line {lineno}: header must be the first non-blank line"
                ));
            }
            let schema = obj.get("schema").and_then(|s| s.as_str()).unwrap_or("");
            if schema != RELEVANCE_SCHEMA_ID {
                return Err(format!(
                    "fixture line {lineno}: unsupported schema {schema:?} (expected {RELEVANCE_SCHEMA_ID:?})"
                ));
            }
            corpus = obj.get("corpus").and_then(|c| c.as_str()).map(|c| c.to_string());
            // Distinguish "no header yet" from "header seen" for the check above.
            if corpus.is_none() {
                corpus = Some(String::new());
            }
            continue;
        }

        let query = obj
            .get("query")
            .and_then(|q| q.as_str())
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| format!("fixture line {lineno}: missing or empty \"query\""))?;
        let expected_raw = obj
            .get("expected")
            .and_then(|e| e.as_array())
            .filter(|a| !a.is_empty())
            .ok_or_else(|| {
                format!("fixture line {lineno}: \"expected\" must be a non-empty array of anchors")
            })?;
        let mut expected = Vec::with_capacity(expected_raw.len());
        for a in expected_raw {
            let s = a
                .as_str()
                .ok_or_else(|| format!("fixture line {lineno}: anchors must be strings"))?;
            expected.push(Anchor::parse(s).map_err(|e| format!("fixture line {lineno}: {e}"))?);
        }
        let id = obj
            .get("id")
            .and_then(|i| i.as_str())
            .map(|i| i.to_string())
            .unwrap_or_else(|| format!("q{lineno}"));
        let k =
            match obj.get("k") {
                None => DEFAULT_TOP_K,
                Some(kv) => kv.as_u64().filter(|k| *k > 0).ok_or_else(|| {
                    format!("fixture line {lineno}: \"k\" must be a positive integer")
                })? as usize,
            };
        seen_case = true;
        cases.push(Case { id, query: query.to_string(), expected, k });
    }
    // Normalize the empty-string corpus sentinel back to None.
    let corpus = corpus.filter(|c| !c.is_empty());
    if cases.is_empty() {
        return Err("fixture set contains no query cases".to_string());
    }
    Ok(FixtureSet { corpus, cases })
}

// --- Backends ---

/// A named retrieval configuration. Every variant calls an EXISTING pipeline
/// entry point — the harness never reimplements ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Keyword-only ranking: `retriever::bm25_only_search` (the explicit issue-#30
    /// degraded mode). No embeddings touched.
    Bm25,
    /// Pure vector ranking: `vector_store::rank_by_cosine` over every chunk (the
    /// SPEC §6.2 candidate order, before fusion).
    Vector,
    /// The full SPEC §6 hybrid pipeline `cce search` serves: RRF fusion +
    /// confidence blend + path penalty + diversity cap + graph expansion.
    Hybrid,
}

impl Backend {
    pub fn parse(s: &str) -> Result<Backend, String> {
        match s.trim() {
            "bm25" => Ok(Backend::Bm25),
            "vector" => Ok(Backend::Vector),
            "hybrid" => Ok(Backend::Hybrid),
            other => Err(format!("unknown backend {other:?} (expected bm25, vector, or hybrid)")),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Bm25 => "bm25",
            Backend::Vector => "vector",
            Backend::Hybrid => "hybrid",
        }
    }

    /// All backends, in the fixed report order.
    pub fn all() -> [Backend; 3] {
        [Backend::Bm25, Backend::Vector, Backend::Hybrid]
    }
}

/// Parse a comma-separated backend list (e.g. `"bm25,hybrid"`).
pub fn parse_backends(s: &str) -> Result<Vec<Backend>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let b = Backend::parse(part)?;
        if !out.contains(&b) {
            out.push(b);
        }
    }
    if out.is_empty() {
        return Err("--backend requires at least one of bm25, vector, hybrid".to_string());
    }
    Ok(out)
}

/// Run one query through the named backend and return the top-`k` results.
/// Every arm is a real pipeline entry point.
pub fn run_backend(
    index: &Index,
    embedder: &dyn Embedder,
    backend: Backend,
    query: &str,
    k: usize,
) -> Vec<SearchResult> {
    if query.trim().is_empty() || index.chunks.is_empty() {
        return Vec::new();
    }
    match backend {
        Backend::Bm25 => bm25_only_search(index, query, k),
        Backend::Vector => {
            let qvec = embedder.embed(query);
            let ranked = rank_by_cosine(&qvec, &index.chunks);
            let mut results: Vec<SearchResult> = ranked
                .iter()
                .take(k)
                .map(|(idx, cos)| result_from(&index.chunks[*idx], *cos))
                .collect();
            for (i, r) in results.iter_mut().enumerate() {
                r.rank = i + 1;
            }
            results
        }
        Backend::Hybrid => {
            // The exact `cce search` default (graph expansion on); the graph may
            // append bonus chunks past top-k, so truncate to the k-slice the
            // metrics are defined over.
            let mut results = search(index, embedder, query, k, true);
            results.truncate(k);
            results
        }
    }
}

// --- Metrics ---

/// The scored metrics for one query at one backend.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryScore {
    pub id: String,
    pub k: usize,
    /// Relevant results in the top-k, over k (the standard precision@k).
    pub precision: f64,
    /// Anchors matched by any top-k result, over the number of anchors.
    pub recall: f64,
    /// 1/rank of the first relevant result; 0.0 when none is retrieved.
    pub mrr: f64,
    /// Harmonic mean of precision@k and recall; 0.0 when both are 0.
    pub f1: f64,
    /// 1-based rank of the first relevant result, if any.
    pub first_relevant_rank: Option<usize>,
}

/// Score one query's ranked results against its expected anchors (SPEC IR
/// definitions; only the first `k` results are considered).
///
/// A result is *relevant* if it matches ANY anchor; an anchor is *matched* if
/// ANY considered result matches it. Precision divides by `k` even when fewer
/// than `k` results were returned — returning less than asked is the backend's
/// own ranking outcome, not the harness's.
pub fn score_query(
    id: &str,
    results: &[SearchResult],
    expected: &[Anchor],
    k: usize,
) -> QueryScore {
    let considered = &results[..results.len().min(k)];
    let mut relevant_retrieved = 0usize;
    let mut first_relevant_rank: Option<usize> = None;
    for (i, r) in considered.iter().enumerate() {
        if expected.iter().any(|a| a.matches(r)) {
            relevant_retrieved += 1;
            if first_relevant_rank.is_none() {
                first_relevant_rank = Some(i + 1);
            }
        }
    }
    let anchors_matched =
        expected.iter().filter(|a| considered.iter().any(|r| a.matches(r))).count();

    let precision = relevant_retrieved as f64 / k as f64;
    let recall = if expected.is_empty() {
        0.0
    } else {
        anchors_matched as f64 / expected.len() as f64
    };
    let mrr = first_relevant_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0);
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    QueryScore { id: id.to_string(), k, precision, recall, mrr, f1, first_relevant_rank }
}

/// One backend's report: macro-averaged aggregates plus the per-query scores.
#[derive(Debug, Clone)]
pub struct BackendReport {
    pub backend: Backend,
    pub precision: f64,
    pub recall: f64,
    pub mrr: f64,
    pub f1: f64,
    pub queries: Vec<QueryScore>,
}

/// Evaluate every case at one backend (macro average over queries).
pub fn evaluate_backend(
    index: &Index,
    embedder: &dyn Embedder,
    backend: Backend,
    cases: &[Case],
) -> BackendReport {
    let queries: Vec<QueryScore> = cases
        .iter()
        .map(|c| {
            let results = run_backend(index, embedder, backend, &c.query, c.k);
            score_query(&c.id, &results, &c.expected, c.k)
        })
        .collect();
    let n = queries.len().max(1) as f64;
    let mean = |f: fn(&QueryScore) -> f64| queries.iter().map(f).sum::<f64>() / n;
    BackendReport {
        backend,
        precision: mean(|q| q.precision),
        recall: mean(|q| q.recall),
        mrr: mean(|q| q.mrr),
        f1: mean(|q| q.f1),
        queries,
    }
}

// --- Rendering (deterministic; format6 fixed 6-decimal strings) ---

/// The human summary table: one row per backend.
pub fn render_human(corpus: &str, embedder_name: &str, reports: &[BackendReport]) -> String {
    let mut out = String::new();
    out.push_str("CCE relevance — ranking quality vs labeled fixtures (cce.relevance/v1)\n");
    out.push_str(&format!("  corpus  : {corpus}\n"));
    out.push_str(&format!("  embedder: {embedder_name}\n"));
    let n = reports.first().map(|r| r.queries.len()).unwrap_or(0);
    out.push_str(&format!("  queries : {n}\n\n"));
    out.push_str(&format!(
        "  {:<10}{:>12}{:>12}{:>12}{:>12}\n",
        "backend", "P@k", "recall", "MRR", "F1"
    ));
    for r in reports {
        out.push_str(&format!(
            "  {:<10}{:>12}{:>12}{:>12}{:>12}\n",
            r.backend.name(),
            format6(r.precision),
            format6(r.recall),
            format6(r.mrr),
            format6(r.f1)
        ));
    }
    out
}

/// The per-query comparison table between exactly two backends (`--compare`).
/// A positive delta means `b` beats `a` on that query.
pub fn render_compare_human(a: &BackendReport, b: &BackendReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n  per-query deltas ({} → {}; positive = {} wins)\n",
        a.backend.name(),
        b.backend.name(),
        b.backend.name()
    ));
    out.push_str(&format!(
        "  {:<24}{:>12}{:>12}{:>12}{:>12}\n",
        "query", "dP@k", "drecall", "dMRR", "dF1"
    ));
    for (qa, qb) in a.queries.iter().zip(b.queries.iter()) {
        out.push_str(&format!(
            "  {:<24}{:>12}{:>12}{:>12}{:>12}\n",
            qa.id,
            format6_signed(qb.precision - qa.precision),
            format6_signed(qb.recall - qa.recall),
            format6_signed(qb.mrr - qa.mrr),
            format6_signed(qb.f1 - qa.f1)
        ));
    }
    out.push_str(&format!(
        "  {:<24}{:>12}{:>12}{:>12}{:>12}\n",
        "mean",
        format6_signed(b.precision - a.precision),
        format6_signed(b.recall - a.recall),
        format6_signed(b.mrr - a.mrr),
        format6_signed(b.f1 - a.f1)
    ));
    out
}

/// `format6` with an explicit sign, for delta columns (`+0.200000`, `-0.033333`).
/// Negative zero is normalized to `+0.000000`.
pub fn format6_signed(v: f64) -> String {
    let s = format6(v);
    if let Some(stripped) = s.strip_prefix('-') {
        if stripped.chars().all(|c| c == '0' || c == '.') {
            return format!("+{stripped}");
        }
        return s;
    }
    format!("+{s}")
}

/// The `--json` report (byte-pinnable for deterministic backends): pretty-printed,
/// serde_json's alphabetical key order, scores as fixed 6-decimal strings, and a
/// single trailing newline — the same grammar discipline as `cce search --json`.
pub fn render_json(corpus: &str, embedder_name: &str, reports: &[BackendReport]) -> String {
    let backends: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            let per_query: Vec<serde_json::Value> = r
                .queries
                .iter()
                .map(|q| {
                    serde_json::json!({
                        "id": q.id,
                        "k": q.k,
                        "precision_at_k": format6(q.precision),
                        "recall": format6(q.recall),
                        "mrr": format6(q.mrr),
                        "f1": format6(q.f1),
                        "first_relevant_rank": q.first_relevant_rank,
                    })
                })
                .collect();
            serde_json::json!({
                "backend": r.backend.name(),
                "precision_at_k": format6(r.precision),
                "recall": format6(r.recall),
                "mrr": format6(r.mrr),
                "f1": format6(r.f1),
                "per_query": per_query,
            })
        })
        .collect();
    let n = reports.first().map(|r| r.queries.len()).unwrap_or(0);
    let body = serde_json::json!({
        "schema": RELEVANCE_REPORT_SCHEMA_ID,
        "corpus": corpus,
        "embedder": embedder_name,
        "queries": n,
        "backends": backends,
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fixture_index() -> Index {
        let e = HashEmbedder;
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"));
        Index::build_from_dir(&dir, &e).unwrap().0
    }

    fn result(file_path: &str, kind: &str, rank: usize) -> SearchResult {
        SearchResult {
            rank,
            chunk_id: format!("id{rank}"),
            file_path: file_path.to_string(),
            start_line: 1,
            end_line: 2,
            chunk_type: "function".to_string(),
            kind: kind.to_string(),
            score: 1.0 / rank as f64,
            content: String::new(),
        }
    }

    // --- Anchor parsing & matching ---

    #[test]
    fn anchor_parses_path_kind_and_combined_forms() {
        let p = Anchor::parse("auth.py").unwrap();
        assert_eq!(p, Anchor { file_path: Some("auth.py".into()), kind: None });
        let pk = Anchor::parse("auth.py#function_definition").unwrap();
        assert_eq!(
            pk,
            Anchor { file_path: Some("auth.py".into()), kind: Some("function_definition".into()) }
        );
        let k = Anchor::parse("#interface_declaration").unwrap();
        assert_eq!(k, Anchor { file_path: None, kind: Some("interface_declaration".into()) });
        assert_eq!(p.display(), "auth.py");
        assert_eq!(pk.display(), "auth.py#function_definition");
        assert_eq!(k.display(), "#interface_declaration");
    }

    #[test]
    fn anchor_rejects_empty_and_dangling_hash() {
        assert!(Anchor::parse("").is_err());
        assert!(Anchor::parse("   ").is_err());
        assert!(Anchor::parse("auth.py#").is_err());
        assert!(Anchor::parse("#").is_err());
    }

    #[test]
    fn anchor_matching_respects_each_facet() {
        let r = result("auth.py", "function_definition", 1);
        assert!(Anchor::parse("auth.py").unwrap().matches(&r));
        assert!(Anchor::parse("auth.py#function_definition").unwrap().matches(&r));
        assert!(Anchor::parse("#function_definition").unwrap().matches(&r));
        assert!(!Anchor::parse("payments.py").unwrap().matches(&r));
        assert!(!Anchor::parse("auth.py#class_definition").unwrap().matches(&r));
        assert!(!Anchor::parse("#class_definition").unwrap().matches(&r));
    }

    // --- Metric math (hand-computed expected values) ---

    #[test]
    fn score_query_hand_computed_mixed_case() {
        // k=5, anchors {A=auth.py, B=payments.py}; ranked results:
        //   1 other.py, 2 auth.py, 3 other.py, 4 auth.py, 5 payments.py
        // relevant at ranks 2, 4, 5 → P@5 = 3/5 = 0.6
        // both anchors matched → recall = 2/2 = 1.0
        // first relevant at rank 2 → MRR = 0.5
        // F1 = 2·0.6·1.0/(0.6+1.0) = 0.75
        let expected =
            vec![Anchor::parse("auth.py").unwrap(), Anchor::parse("payments.py").unwrap()];
        let results = vec![
            result("other.py", "function_definition", 1),
            result("auth.py", "function_definition", 2),
            result("other.py", "function_definition", 3),
            result("auth.py", "class_definition", 4),
            result("payments.py", "function_definition", 5),
        ];
        let s = score_query("q", &results, &expected, 5);
        assert!((s.precision - 0.6).abs() < 1e-12);
        assert!((s.recall - 1.0).abs() < 1e-12);
        assert!((s.mrr - 0.5).abs() < 1e-12);
        assert!((s.f1 - 0.75).abs() < 1e-12);
        assert_eq!(s.first_relevant_rank, Some(2));
    }

    #[test]
    fn score_query_perfect_top1() {
        // k=1, one anchor, hit at rank 1 → everything 1.0.
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![result("auth.py", "function_definition", 1)];
        let s = score_query("q", &results, &expected, 1);
        assert_eq!(s.precision, 1.0);
        assert_eq!(s.recall, 1.0);
        assert_eq!(s.mrr, 1.0);
        assert_eq!(s.f1, 1.0);
        assert_eq!(s.first_relevant_rank, Some(1));
    }

    #[test]
    fn score_query_no_relevant_results_is_all_zero() {
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![result("other.py", "function_definition", 1)];
        let s = score_query("q", &results, &expected, 5);
        assert_eq!(s.precision, 0.0);
        assert_eq!(s.recall, 0.0);
        assert_eq!(s.mrr, 0.0);
        assert_eq!(s.f1, 0.0);
        assert_eq!(s.first_relevant_rank, None);
    }

    #[test]
    fn score_query_only_first_k_results_count() {
        // The hit sits at rank 3 but k=2 → nothing relevant in the k-slice.
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![
            result("other.py", "function_definition", 1),
            result("other.py", "class_definition", 2),
            result("auth.py", "function_definition", 3),
        ];
        let s = score_query("q", &results, &expected, 2);
        assert_eq!(s.precision, 0.0);
        assert_eq!(s.recall, 0.0);
        assert_eq!(s.mrr, 0.0);
    }

    #[test]
    fn score_query_precision_divides_by_k_with_fewer_results() {
        // Two results returned at k=5, one relevant → P@5 = 1/5, recall = 1/1.
        // F1 = 2·0.2·1.0/1.2 = 1/3.
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![
            result("auth.py", "function_definition", 1),
            result("other.py", "function_definition", 2),
        ];
        let s = score_query("q", &results, &expected, 5);
        assert!((s.precision - 0.2).abs() < 1e-12);
        assert_eq!(s.recall, 1.0);
        assert_eq!(s.mrr, 1.0);
        assert!((s.f1 - (1.0 / 3.0)).abs() < 1e-12);
    }

    #[test]
    fn score_query_duplicate_file_hits_count_once_for_recall() {
        // Three chunks of the same expected file → precision counts each hit,
        // recall counts the anchor once.
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![
            result("auth.py", "function_definition", 1),
            result("auth.py", "class_definition", 2),
            result("auth.py", "function_definition", 3),
        ];
        let s = score_query("q", &results, &expected, 3);
        assert_eq!(s.precision, 1.0);
        assert_eq!(s.recall, 1.0);
    }

    // --- Fixture parsing ---

    #[test]
    fn parse_fixtures_header_and_defaults() {
        let text = concat!(
            "{\"schema\":\"cce.relevance/v1\",\"corpus\":\"../corpus\"}\n",
            "\n",
            "{\"query\":\"hash password\",\"expected\":[\"auth.py\"]}\n",
            "{\"id\":\"named\",\"query\":\"refund\",\"expected\":[\"payments.py\"],\"k\":3}\n",
        );
        let set = parse_fixtures(text).unwrap();
        assert_eq!(set.corpus.as_deref(), Some("../corpus"));
        assert_eq!(set.cases.len(), 2);
        assert_eq!(set.cases[0].id, "q3"); // line-number default
        assert_eq!(set.cases[0].k, DEFAULT_TOP_K);
        assert_eq!(set.cases[1].id, "named");
        assert_eq!(set.cases[1].k, 3);
    }

    #[test]
    fn parse_fixtures_without_header_has_no_corpus() {
        let set = parse_fixtures("{\"query\":\"a\",\"expected\":[\"f.py\"]}\n").unwrap();
        assert_eq!(set.corpus, None);
        assert_eq!(set.cases.len(), 1);
    }

    #[test]
    fn parse_fixtures_rejects_bad_input_with_line_numbers() {
        let err = parse_fixtures("not json\n").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
        let err = parse_fixtures("{\"query\":\"a\",\"expected\":[]}\n").unwrap_err();
        assert!(err.contains("non-empty array"), "{err}");
        let err = parse_fixtures("{\"query\":\"\",\"expected\":[\"f\"]}\n").unwrap_err();
        assert!(err.contains("query"), "{err}");
        let err = parse_fixtures("{\"query\":\"a\",\"expected\":[\"f\"],\"k\":0}\n").unwrap_err();
        assert!(err.contains("positive integer"), "{err}");
        let err = parse_fixtures("").unwrap_err();
        assert!(err.contains("no query cases"), "{err}");
        // A wrong schema id is refused.
        let err = parse_fixtures("{\"schema\":\"cce.relevance/v9\"}\n").unwrap_err();
        assert!(err.contains("unsupported schema"), "{err}");
        // A header after a case is refused.
        let err = parse_fixtures(concat!(
            "{\"query\":\"a\",\"expected\":[\"f\"]}\n",
            "{\"schema\":\"cce.relevance/v1\"}\n",
        ))
        .unwrap_err();
        assert!(err.contains("first non-blank line"), "{err}");
    }

    // --- Backend parsing ---

    #[test]
    fn parse_backends_list_dedupes_and_validates() {
        assert_eq!(parse_backends("bm25,hybrid").unwrap(), vec![Backend::Bm25, Backend::Hybrid]);
        assert_eq!(parse_backends("bm25, bm25 ,").unwrap(), vec![Backend::Bm25]);
        assert!(parse_backends("bm42").is_err());
        assert!(parse_backends("").is_err());
    }

    // --- Backends run the real pipeline ---

    #[test]
    fn backends_agree_on_the_conformance_anchor_query() {
        // "hash password" → auth.py top-1 on every backend (same anchors the
        // retriever's own conformance tests pin).
        let idx = fixture_index();
        let e = HashEmbedder;
        for b in Backend::all() {
            let res = run_backend(&idx, &e, b, "hash password", 5);
            assert!(!res.is_empty(), "{} returned nothing", b.name());
            assert_eq!(res[0].file_path, "auth.py", "backend {}", b.name());
            assert!(res.len() <= 5);
            assert_eq!(res[0].rank, 1);
        }
    }

    #[test]
    fn run_backend_empty_query_returns_empty() {
        let idx = fixture_index();
        let e = HashEmbedder;
        for b in Backend::all() {
            assert!(run_backend(&idx, &e, b, "   ", 5).is_empty());
        }
    }

    #[test]
    fn evaluate_backend_is_deterministic() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let cases = vec![Case {
            id: "q1".into(),
            query: "hash password".into(),
            expected: vec![Anchor::parse("auth.py").unwrap()],
            k: 5,
        }];
        let a = evaluate_backend(&idx, &e, Backend::Hybrid, &cases);
        let b = evaluate_backend(&idx, &e, Backend::Hybrid, &cases);
        assert_eq!(format6(a.f1), format6(b.f1));
        assert_eq!(a.queries[0].first_relevant_rank, b.queries[0].first_relevant_rank);
        assert_eq!(a.queries[0].first_relevant_rank, Some(1));
        assert_eq!(a.mrr, 1.0);
    }

    // --- Rendering ---

    #[test]
    fn format6_signed_pins_signs() {
        assert_eq!(format6_signed(0.2), "+0.200000");
        assert_eq!(format6_signed(-0.0333333), "-0.033333");
        assert_eq!(format6_signed(0.0), "+0.000000");
        // A tiny negative that rounds to zero is normalized to the + form.
        assert_eq!(format6_signed(-0.0000001), "+0.000000");
    }

    #[test]
    fn render_json_shape_is_stable() {
        let idx = fixture_index();
        let e = HashEmbedder;
        let cases = vec![Case {
            id: "q1".into(),
            query: "hash password".into(),
            expected: vec![Anchor::parse("auth.py").unwrap()],
            k: 5,
        }];
        let reports: Vec<BackendReport> =
            Backend::all().iter().map(|b| evaluate_backend(&idx, &e, *b, &cases)).collect();
        let a = render_json("corpus", "hash", &reports);
        let b = render_json("corpus", "hash", &reports);
        assert_eq!(a, b);
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["schema"], RELEVANCE_REPORT_SCHEMA_ID);
        assert_eq!(v["queries"], 1);
        let backends = v["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 3);
        assert_eq!(backends[0]["backend"], "bm25");
        // Scores are fixed 6-decimal strings, like `cce search --json`.
        assert!(backends[0]["precision_at_k"].as_str().unwrap().contains('.'));
        assert!(a.ends_with("}\n"));
    }
}
