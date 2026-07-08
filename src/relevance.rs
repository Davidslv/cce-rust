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
//! query plus macro-averaged per backend. Cases with line-ranged anchors are
//! additionally scored at token resolution — token-level recall / precision /
//! IoU weighted with the ONE `cce.tokens/v1` estimator (issue #85). A
//! comparison mode diffs two backends per query and reports the paired
//! significance of each mean delta — t, two-sided p, 95% CI, n (issue #84).
//! Deterministic for deterministic backends: with the hash embedder the JSON
//! report (`cce.relevance.report/v2`) is byte-pinnable, conformance-style.
//!
//! **Responsibilities:**
//! - Own the `cce.relevance/v1` fixture contract and its parsing.
//! - Own the IR metric math and the deterministic report rendering.
//! - It does NOT rank: every backend is an existing pipeline entry point
//!   (`retriever::search`, `retriever::bm25_only_search`,
//!   `vector_store::rank_by_cosine`). This harness only measures — it never
//!   changes ranking behavior.

use crate::config::DEFAULT_TOP_K;
use crate::chunker::Chunk;
use crate::embedder::{format6, Embedder};
use crate::retriever::{bm25_only_search, result_from, search, SearchResult};
use crate::store::Index;
use crate::tokenizer::estimate_tokens;
use crate::vector_store::rank_by_cosine;
use std::collections::{BTreeMap, BTreeSet};

/// The pinned schema id for the fixture contract. A bump is a compatibility event.
pub const RELEVANCE_SCHEMA_ID: &str = "cce.relevance/v1";

/// The pinned schema id of the `--json` report shape. v2 (issues #84/#85)
/// added the `compare` paired-significance block and the token-level fields;
/// every v1 field is carried unchanged.
pub const RELEVANCE_REPORT_SCHEMA_ID: &str = "cce.relevance.report/v2";

// --- Fixture contract (cce.relevance/v1) ---

/// One expected-result anchor. A retrieved result matches an anchor when every
/// present facet matches: `file_path` equality, chunk `kind` equality, and/or
/// line-range overlap.
///
/// String forms (the documented contract):
/// - `"auth.py"` — any chunk of that file
/// - `"auth.py#function_definition"` — a chunk of that file with that kind
/// - `"#interface_declaration"` — any chunk of that kind, in any file
/// - `"auth.py@10-42"` / `"auth.py#function_definition@10-42"` — additionally
///   require the result's line span to OVERLAP the 1-based inclusive range
///   (issue #85; a range always requires a file path)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub file_path: Option<String>,
    pub kind: Option<String>,
    /// Optional 1-based inclusive line range (`@a-b`), issue #85. Ranged
    /// anchors additionally feed the token-level metrics.
    pub range: Option<(usize, usize)>,
}

/// `Anchor::split_range`'s result: the remaining anchor body plus the parsed
/// `@a-b` range, if one was present.
type SplitRange<'a> = (&'a str, Option<(usize, usize)>);

impl Anchor {
    /// Parse the `path`, `path#kind`, `#kind`, `path@a-b`, or `path#kind@a-b`
    /// string form.
    ///
    /// The range facet is ADDITIVE (issue #85): text after the last `@` is a
    /// range only when it consists solely of digits and `-` — then it must be
    /// a valid `a-b` with `1 ≤ a ≤ b`, else the anchor is rejected. Any other
    /// `@` stays literal path/kind content, so every pre-range fixture parses
    /// unchanged.
    pub fn parse(s: &str) -> Result<Anchor, String> {
        let (body, range) = Self::split_range(s)?;
        let (path, kind) = match body.split_once('#') {
            Some((p, k)) => (p.trim(), Some(k.trim())),
            None => (body.trim(), None),
        };
        let file_path = if path.is_empty() { None } else { Some(path.to_string()) };
        let kind = match kind {
            Some(k) if !k.is_empty() => Some(k.to_string()),
            Some(_) => return Err(format!("anchor {s:?} has an empty kind after '#'")),
            None => None,
        };
        if file_path.is_none() && kind.is_none() {
            return Err("anchor is empty — expected `path`, `path#kind`, `#kind`, or `path@a-b`"
                .to_string());
        }
        if range.is_some() && file_path.is_none() {
            return Err(format!(
                "anchor {s:?} has a line range but no file path — a range needs `path@a-b` or \
                 `path#kind@a-b`"
            ));
        }
        Ok(Anchor { file_path, kind, range })
    }

    /// Split an optional trailing `@a-b` range facet off an anchor string.
    fn split_range(s: &str) -> Result<SplitRange<'_>, String> {
        let Some((body, tail)) = s.rsplit_once('@') else {
            return Ok((s, None));
        };
        let tail = tail.trim();
        // Only an all-[0-9-] tail is an attempted range facet; anything else
        // (e.g. `user@host.py`) is literal anchor text, as before issue #85.
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Ok((s, None));
        }
        let bad = || {
            format!(
                "anchor {s:?} has an invalid line range after '@' — expected `a-b` with 1 ≤ a ≤ b"
            )
        };
        let (a, b) = tail.split_once('-').ok_or_else(bad)?;
        let a: usize = a.parse().map_err(|_| bad())?;
        let b: usize = b.parse().map_err(|_| bad())?;
        if a == 0 || b < a {
            return Err(bad());
        }
        Ok((body, Some((a, b))))
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
        if let Some((a, b)) = self.range {
            // Inclusive span overlap: the result must touch [a, b].
            if result.end_line < a || result.start_line > b {
                return false;
            }
        }
        true
    }

    /// The canonical string form (for reporting).
    pub fn display(&self) -> String {
        let mut out = match (&self.file_path, &self.kind) {
            (Some(p), Some(k)) => format!("{p}#{k}"),
            (Some(p), None) => p.clone(),
            (None, Some(k)) => format!("#{k}"),
            (None, None) => String::new(),
        };
        if let Some((a, b)) = self.range {
            out.push_str(&format!("@{a}-{b}"));
        }
        out
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

// --- Token-level span metrics (issue #85) ---

/// Per-line `cce.tokens/v1` weights for every file the index knows, built from
/// the indexed chunk texts (so it works identically for `--dir` and `--store`,
/// with zero extra file I/O).
///
/// Rules, all deterministic:
/// - A line's weight is `estimate_tokens(line_text)` — the ONE `cce.tokens/v1`
///   estimator, applied per line (newline excluded).
/// - Where chunks overlap (a method nested in its class), the FIRST chunk in
///   index order wins; the outer chunk carries the truer (indented) line text.
/// - A line no chunk covers — a gap between definitions, or a range past the
///   end of the file — weighs `estimate_tokens("") = 1`, the estimator floor.
#[derive(Debug, Clone, Default)]
pub struct LineWeights {
    files: BTreeMap<String, BTreeMap<usize, u64>>,
}

impl LineWeights {
    /// Build the per-line weight table from the index's chunks.
    pub fn from_chunks(chunks: &[Chunk]) -> LineWeights {
        let mut files: BTreeMap<String, BTreeMap<usize, u64>> = BTreeMap::new();
        for c in chunks {
            let file = files.entry(c.file_path.clone()).or_default();
            for (i, line) in c.content.lines().enumerate() {
                file.entry(c.start_line + i).or_insert_with(|| estimate_tokens(line));
            }
        }
        LineWeights { files }
    }

    /// The `cce.tokens/v1` weight of one line (see the type docs for the
    /// uncovered-line rule).
    pub fn weight(&self, file: &str, line: usize) -> u64 {
        self.files.get(file).and_then(|m| m.get(&line)).copied().unwrap_or(1)
    }
}

/// Token-level boundary metrics for one query (issue #85): how much of the
/// EXACT expected span was retrieved, and how much of the retrieved text was
/// inside it — the resolution chunk-level hit/miss cannot see.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenScore {
    /// Overlap tokens over retrieved tokens.
    pub precision: f64,
    /// Overlap tokens over expected-span tokens.
    pub recall: f64,
    /// Overlap tokens over the union of both (Jaccard / IoU).
    pub iou: f64,
}

/// Collect a `file → line set` union of spans.
type LineSets = BTreeMap<String, BTreeSet<usize>>;

/// Total `cce.tokens/v1` mass of a line-set union.
fn token_mass(sets: &LineSets, weights: &LineWeights) -> u64 {
    sets.iter().map(|(f, lines)| lines.iter().map(|l| weights.weight(f, *l)).sum::<u64>()).sum()
}

/// Score the token-level overlap between a query's RANGED anchors and its
/// top-k results' line spans, weighted with `cce.tokens/v1` line weights.
/// Returns `None` when the case has no ranged anchor (chunk-level metrics
/// only — the exact v1 behavior). Unranged anchors of a mixed case do not
/// contribute: only spans the fixture pinned to lines can be token-scored.
pub fn score_tokens(
    results: &[SearchResult],
    expected: &[Anchor],
    k: usize,
    weights: &LineWeights,
) -> Option<TokenScore> {
    let mut relevant: LineSets = BTreeMap::new();
    for a in expected {
        if let (Some((lo, hi)), Some(f)) = (a.range, a.file_path.as_ref()) {
            relevant.entry(f.clone()).or_default().extend(lo..=hi);
        }
    }
    if relevant.is_empty() {
        return None;
    }
    let mut retrieved: LineSets = BTreeMap::new();
    for r in &results[..results.len().min(k)] {
        retrieved.entry(r.file_path.clone()).or_default().extend(r.start_line..=r.end_line);
    }
    let mut overlap: LineSets = BTreeMap::new();
    for (f, lines) in &relevant {
        if let Some(got) = retrieved.get(f) {
            let inter: BTreeSet<usize> = lines.intersection(got).copied().collect();
            if !inter.is_empty() {
                overlap.insert(f.clone(), inter);
            }
        }
    }
    let relevant_mass = token_mass(&relevant, weights);
    let retrieved_mass = token_mass(&retrieved, weights);
    let overlap_mass = token_mass(&overlap, weights);
    let union_mass = relevant_mass + retrieved_mass - overlap_mass;
    let ratio = |num: u64, den: u64| if den == 0 { 0.0 } else { num as f64 / den as f64 };
    Some(TokenScore {
        precision: ratio(overlap_mass, retrieved_mass),
        recall: ratio(overlap_mass, relevant_mass),
        iou: ratio(overlap_mass, union_mass),
    })
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
    /// Token-level span metrics, when the case has ranged anchors (issue #85).
    pub tokens: Option<TokenScore>,
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
    weights: &LineWeights,
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
    let tokens = score_tokens(results, expected, k, weights);
    QueryScore { id: id.to_string(), k, precision, recall, mrr, f1, first_relevant_rank, tokens }
}

/// Macro-averaged token-level aggregates over the RANGED cases of one backend
/// (issue #85). Absent from a report when no case carries a line range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenAggregate {
    /// How many cases carried ranged anchors (the macro-average denominator).
    pub queries: usize,
    pub precision: f64,
    pub recall: f64,
    pub iou: f64,
}

/// One backend's report: macro-averaged aggregates plus the per-query scores.
#[derive(Debug, Clone)]
pub struct BackendReport {
    pub backend: Backend,
    pub precision: f64,
    pub recall: f64,
    pub mrr: f64,
    pub f1: f64,
    /// Token-level macro averages, when any case has ranged anchors.
    pub tokens: Option<TokenAggregate>,
    pub queries: Vec<QueryScore>,
}

/// Evaluate every case at one backend (macro average over queries).
pub fn evaluate_backend(
    index: &Index,
    embedder: &dyn Embedder,
    backend: Backend,
    cases: &[Case],
) -> BackendReport {
    let weights = LineWeights::from_chunks(&index.chunks);
    let queries: Vec<QueryScore> = cases
        .iter()
        .map(|c| {
            let results = run_backend(index, embedder, backend, &c.query, c.k);
            score_query(&c.id, &results, &c.expected, c.k, &weights)
        })
        .collect();
    let n = queries.len().max(1) as f64;
    let mean = |f: fn(&QueryScore) -> f64| queries.iter().map(f).sum::<f64>() / n;
    let ranged: Vec<TokenScore> = queries.iter().filter_map(|q| q.tokens).collect();
    let tokens = if ranged.is_empty() {
        None
    } else {
        let tn = ranged.len() as f64;
        Some(TokenAggregate {
            queries: ranged.len(),
            precision: ranged.iter().map(|t| t.precision).sum::<f64>() / tn,
            recall: ranged.iter().map(|t| t.recall).sum::<f64>() / tn,
            iou: ranged.iter().map(|t| t.iou).sum::<f64>() / tn,
        })
    };
    BackendReport {
        backend,
        precision: mean(|q| q.precision),
        recall: mean(|q| q.recall),
        mrr: mean(|q| q.mrr),
        f1: mean(|q| q.f1),
        tokens,
        queries,
    }
}

// --- Paired significance testing for --compare (issue #84) ---

/// One metric's paired t-test over the per-query deltas of a comparison.
#[derive(Debug, Clone)]
pub struct MetricStats {
    /// Human table label (`P@k`, `recall`, `MRR`, `F1`).
    pub metric: &'static str,
    /// JSON report key (`precision_at_k`, `recall`, `mrr`, `f1`).
    pub key: &'static str,
    pub stats: crate::stats::PairedStats,
}

/// The paired-significance block of a two-backend comparison: per metric, the
/// t-statistic, two-sided p-value, 95% CI on the mean delta, and n — so a
/// compare table states how much evidence there is that a delta is real, not
/// just how big it looks (issue #84).
#[derive(Debug, Clone)]
pub struct CompareStats {
    pub a: Backend,
    pub b: Backend,
    /// Fixed metric order: P@k, recall, MRR, F1 — the summary-table order.
    pub metrics: Vec<MetricStats>,
}

/// Run the paired t-test per metric over the per-query deltas (`b − a`,
/// paired by case order — both reports score the same fixture cases).
pub fn compare_stats(a: &BackendReport, b: &BackendReport) -> CompareStats {
    let deltas = |f: fn(&QueryScore) -> f64| -> Vec<f64> {
        a.queries.iter().zip(b.queries.iter()).map(|(qa, qb)| f(qb) - f(qa)).collect()
    };
    let metrics = vec![
        MetricStats {
            metric: "P@k",
            key: "precision_at_k",
            stats: crate::stats::paired_t(&deltas(|q| q.precision)),
        },
        MetricStats {
            metric: "recall",
            key: "recall",
            stats: crate::stats::paired_t(&deltas(|q| q.recall)),
        },
        MetricStats {
            metric: "MRR",
            key: "mrr",
            stats: crate::stats::paired_t(&deltas(|q| q.mrr)),
        },
        MetricStats { metric: "F1", key: "f1", stats: crate::stats::paired_t(&deltas(|q| q.f1)) },
    ];
    CompareStats { a: a.backend, b: b.backend, metrics }
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
    // Token-level table, only when the fixture set carries ranged anchors
    // (issue #85) — an unranged set renders exactly the v1 block above.
    if let Some(agg) = reports.iter().find_map(|r| r.tokens) {
        out.push_str(&format!(
            "\n  token-level span metrics ({} of {n} queries carry ranged anchors)\n",
            agg.queries
        ));
        out.push_str(&format!(
            "  {:<10}{:>12}{:>12}{:>12}\n",
            "backend", "tok-P", "tok-recall", "tok-IoU"
        ));
        for r in reports {
            if let Some(t) = r.tokens {
                out.push_str(&format!(
                    "  {:<10}{:>12}{:>12}{:>12}\n",
                    r.backend.name(),
                    format6(t.precision),
                    format6(t.recall),
                    format6(t.iou)
                ));
            }
        }
    }
    out
}

/// The per-query comparison table between exactly two backends (`--compare`),
/// followed by the paired-significance block (issue #84). A positive delta
/// means `b` beats `a` on that query.
pub fn render_compare_human(a: &BackendReport, b: &BackendReport, stats: &CompareStats) -> String {
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
    // The significance block (issue #84): is the mean delta evidence, or noise?
    let n = stats.metrics.first().map(|m| m.stats.n).unwrap_or(0);
    out.push_str(&format!(
        "\n  paired t-test on the per-query deltas (two-sided, n={n}, df={})\n",
        n.saturating_sub(1)
    ));
    out.push_str(&format!(
        "  {:<10}{:>14}{:>12}{:>12}{:>26}\n",
        "metric", "mean-delta", "t", "p", "95% CI"
    ));
    for m in &stats.metrics {
        let s = &m.stats;
        let t = s.t.map(format6_signed).unwrap_or_else(|| "n/a".to_string());
        let p = s.p.map(format6).unwrap_or_else(|| "n/a".to_string());
        let ci = s
            .ci95
            .map(|(lo, hi)| format!("[{}, {}]", format6_signed(lo), format6_signed(hi)))
            .unwrap_or_else(|| "n/a".to_string());
        out.push_str(&format!(
            "  {:<10}{:>14}{:>12}{:>12}{:>26}\n",
            m.metric,
            format6_signed(s.mean_delta),
            t,
            p,
            ci
        ));
    }
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

/// One `PairedStats` as report JSON: 6-decimal strings (signed for deltas and
/// bounds), `null` where a statistic is undefined at this input.
fn stats_json(s: &crate::stats::PairedStats) -> serde_json::Value {
    serde_json::json!({
        "n": s.n,
        "mean_delta": format6_signed(s.mean_delta),
        "t": s.t.map(format6_signed),
        "p": s.p.map(format6),
        "ci95_low": s.ci95.map(|(lo, _)| format6_signed(lo)),
        "ci95_high": s.ci95.map(|(_, hi)| format6_signed(hi)),
    })
}

/// The `--json` report (byte-pinnable for deterministic backends): pretty-printed,
/// serde_json's alphabetical key order, scores as fixed 6-decimal strings, and a
/// single trailing newline — the same grammar discipline as `cce search --json`.
///
/// Schema `cce.relevance.report/v2`: every v1 field unchanged, plus
/// - per-query `tokens` and per-backend `tokens` aggregates, present only for
///   cases/sets with ranged anchors (issue #85);
/// - a top-level `compare` block with the per-metric paired t-test, present
///   only in `--compare` mode (issue #84).
pub fn render_json(
    corpus: &str,
    embedder_name: &str,
    reports: &[BackendReport],
    compare: Option<&CompareStats>,
) -> String {
    let backends: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            let per_query: Vec<serde_json::Value> = r
                .queries
                .iter()
                .map(|q| {
                    let mut obj = serde_json::json!({
                        "id": q.id,
                        "k": q.k,
                        "precision_at_k": format6(q.precision),
                        "recall": format6(q.recall),
                        "mrr": format6(q.mrr),
                        "f1": format6(q.f1),
                        "first_relevant_rank": q.first_relevant_rank,
                    });
                    if let Some(t) = q.tokens {
                        obj["tokens"] = serde_json::json!({
                            "precision": format6(t.precision),
                            "recall": format6(t.recall),
                            "iou": format6(t.iou),
                        });
                    }
                    obj
                })
                .collect();
            let mut obj = serde_json::json!({
                "backend": r.backend.name(),
                "precision_at_k": format6(r.precision),
                "recall": format6(r.recall),
                "mrr": format6(r.mrr),
                "f1": format6(r.f1),
                "per_query": per_query,
            });
            if let Some(t) = r.tokens {
                obj["tokens"] = serde_json::json!({
                    "queries": t.queries,
                    "precision": format6(t.precision),
                    "recall": format6(t.recall),
                    "iou": format6(t.iou),
                });
            }
            obj
        })
        .collect();
    let n = reports.first().map(|r| r.queries.len()).unwrap_or(0);
    let mut body = serde_json::json!({
        "schema": RELEVANCE_REPORT_SCHEMA_ID,
        "corpus": corpus,
        "embedder": embedder_name,
        "queries": n,
        "backends": backends,
    });
    if let Some(cs) = compare {
        let mut metrics = serde_json::Map::new();
        for m in &cs.metrics {
            metrics.insert(m.key.to_string(), stats_json(&m.stats));
        }
        body["compare"] = serde_json::json!({
            "a": cs.a.name(),
            "b": cs.b.name(),
            "metrics": metrics,
        });
    }
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
        assert_eq!(p, Anchor { file_path: Some("auth.py".into()), kind: None, range: None });
        let pk = Anchor::parse("auth.py#function_definition").unwrap();
        assert_eq!(
            pk,
            Anchor {
                file_path: Some("auth.py".into()),
                kind: Some("function_definition".into()),
                range: None
            }
        );
        let k = Anchor::parse("#interface_declaration").unwrap();
        assert_eq!(
            k,
            Anchor { file_path: None, kind: Some("interface_declaration".into()), range: None }
        );
        assert_eq!(p.display(), "auth.py");
        assert_eq!(pk.display(), "auth.py#function_definition");
        assert_eq!(k.display(), "#interface_declaration");
    }

    #[test]
    fn anchor_parses_line_range_forms() {
        // path@a-b and path#kind@a-b (issue #85).
        let r = Anchor::parse("src/auth.py@10-42").unwrap();
        assert_eq!(
            r,
            Anchor { file_path: Some("src/auth.py".into()), kind: None, range: Some((10, 42)) }
        );
        assert_eq!(r.display(), "src/auth.py@10-42");
        let rk = Anchor::parse("auth.py#function_definition@3-4").unwrap();
        assert_eq!(
            rk,
            Anchor {
                file_path: Some("auth.py".into()),
                kind: Some("function_definition".into()),
                range: Some((3, 4))
            }
        );
        assert_eq!(rk.display(), "auth.py#function_definition@3-4");
        // A single-line span is a-a.
        assert_eq!(Anchor::parse("f.py@7-7").unwrap().range, Some((7, 7)));
    }

    #[test]
    fn anchor_range_grammar_is_additive_for_literal_at_signs() {
        // An `@` whose tail is not all digits-and-dashes stays literal path
        // text — the exact pre-#85 parse.
        let a = Anchor::parse("user@host.py").unwrap();
        assert_eq!(a.file_path.as_deref(), Some("user@host.py"));
        assert_eq!(a.range, None);
        let b = Anchor::parse("v2@latest.md#section").unwrap();
        assert_eq!(b.file_path.as_deref(), Some("v2@latest.md"));
        assert_eq!(b.kind.as_deref(), Some("section"));
        assert_eq!(b.range, None);
    }

    #[test]
    fn anchor_rejects_malformed_and_pathless_ranges() {
        // An attempted range facet (all digits/dashes) must be a valid a-b.
        assert!(Anchor::parse("f.py@10-").is_err());
        assert!(Anchor::parse("f.py@-42").is_err());
        assert!(Anchor::parse("f.py@42").is_err());
        assert!(Anchor::parse("f.py@42-10").is_err());
        assert!(Anchor::parse("f.py@0-4").is_err());
        assert!(Anchor::parse("f.py@1-2-3").is_err());
        // A range needs a file path to span.
        assert!(Anchor::parse("#function_definition@1-5").is_err());
        assert!(Anchor::parse("@1-5").is_err());
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

    #[test]
    fn ranged_anchor_matching_requires_span_overlap() {
        // The helper's result spans lines 1-2.
        let r = result("auth.py", "function_definition", 1);
        assert!(Anchor::parse("auth.py@1-2").unwrap().matches(&r));
        assert!(Anchor::parse("auth.py@2-10").unwrap().matches(&r)); // partial overlap
        assert!(Anchor::parse("auth.py@1-1").unwrap().matches(&r)); // touches first line
        assert!(!Anchor::parse("auth.py@3-10").unwrap().matches(&r)); // disjoint below
        assert!(!Anchor::parse("payments.py@1-2").unwrap().matches(&r)); // wrong file
        assert!(!Anchor::parse("auth.py#class_definition@1-2").unwrap().matches(&r));
        // wrong kind
    }

    // --- Token-level span metrics (issue #85; hand-computed) ---

    /// A result with an explicit line span.
    fn spanned(file_path: &str, rank: usize, start_line: usize, end_line: usize) -> SearchResult {
        SearchResult { start_line, end_line, ..result(file_path, "function_definition", rank) }
    }

    /// Uniform weights: every line weighs 1 token (the empty `LineWeights`
    /// fallback), so masses reduce to line counts.
    fn uniform() -> LineWeights {
        LineWeights::default()
    }

    #[test]
    fn score_tokens_returns_none_without_ranged_anchors() {
        let expected = vec![Anchor::parse("auth.py").unwrap()];
        let results = vec![spanned("auth.py", 1, 1, 10)];
        assert_eq!(score_tokens(&results, &expected, 5, &uniform()), None);
    }

    #[test]
    fn score_tokens_partial_overlap_hand_computed() {
        // Anchor spans lines 10-19 (10 lines); the one retrieved chunk spans
        // 15-24 (10 lines). Overlap = 15-19 (5 lines). Uniform weights:
        //   recall = 5/10, precision = 5/10, IoU = 5/(10+10-5) = 1/3.
        let expected = vec![Anchor::parse("auth.py@10-19").unwrap()];
        let results = vec![spanned("auth.py", 1, 15, 24)];
        let t = score_tokens(&results, &expected, 5, &uniform()).unwrap();
        assert!((t.recall - 0.5).abs() < 1e-12);
        assert!((t.precision - 0.5).abs() < 1e-12);
        assert!((t.iou - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn score_tokens_multi_anchor_union_dedupes_overlap() {
        // Two ranged anchors on one file overlap each other: 1-6 and 4-10.
        // Their union is lines 1-10 (10 lines), NOT 6+7=13 — the union is a
        // set. Retrieved chunk covers 1-10 exactly → all metrics 1.0.
        let expected =
            vec![Anchor::parse("f.py@1-6").unwrap(), Anchor::parse("f.py@4-10").unwrap()];
        let results = vec![spanned("f.py", 1, 1, 10)];
        let t = score_tokens(&results, &expected, 5, &uniform()).unwrap();
        assert_eq!(t.recall, 1.0);
        assert_eq!(t.precision, 1.0);
        assert_eq!(t.iou, 1.0);
    }

    #[test]
    fn score_tokens_multi_file_union_and_wrong_file_precision_cost() {
        // Anchors: a.py lines 1-4 and b.py lines 1-4 (8 relevant lines).
        // Retrieved: a.py 1-4 (perfect) and c.py 1-8 (all waste).
        //   overlap = 4, relevant = 8, retrieved = 4+8 = 12
        //   recall = 4/8 = 0.5, precision = 4/12 = 1/3, IoU = 4/(8+12-4) = 0.25
        let expected = vec![Anchor::parse("a.py@1-4").unwrap(), Anchor::parse("b.py@1-4").unwrap()];
        let results = vec![spanned("a.py", 1, 1, 4), spanned("c.py", 2, 1, 8)];
        let t = score_tokens(&results, &expected, 5, &uniform()).unwrap();
        assert!((t.recall - 0.5).abs() < 1e-12);
        assert!((t.precision - 1.0 / 3.0).abs() < 1e-12);
        assert!((t.iou - 0.25).abs() < 1e-12);
    }

    #[test]
    fn score_tokens_zero_overlap_is_all_zero() {
        let expected = vec![Anchor::parse("f.py@1-5").unwrap()];
        let results = vec![spanned("f.py", 1, 6, 10), spanned("g.py", 2, 1, 5)];
        let t = score_tokens(&results, &expected, 5, &uniform()).unwrap();
        assert_eq!(t.recall, 0.0);
        assert_eq!(t.precision, 0.0);
        assert_eq!(t.iou, 0.0);
    }

    #[test]
    fn score_tokens_no_results_is_zero_precision_not_nan() {
        let expected = vec![Anchor::parse("f.py@1-5").unwrap()];
        let t = score_tokens(&[], &expected, 5, &uniform()).unwrap();
        assert_eq!(t.recall, 0.0);
        assert_eq!(t.precision, 0.0);
        assert_eq!(t.iou, 0.0);
    }

    #[test]
    fn score_tokens_respects_the_k_cutoff() {
        // The only overlapping result sits past k → scores are zero.
        let expected = vec![Anchor::parse("f.py@1-5").unwrap()];
        let results = vec![spanned("g.py", 1, 1, 5), spanned("f.py", 2, 1, 5)];
        let t = score_tokens(&results, &expected, 1, &uniform()).unwrap();
        assert_eq!(t.recall, 0.0);
    }

    #[test]
    fn score_tokens_mixed_case_only_ranged_anchors_feed_token_metrics() {
        // One ranged + one unranged anchor: token metrics see ONLY the range.
        // Retrieved covers the ranged span exactly → token metrics all 1.0,
        // even though the unranged anchor's file was never retrieved.
        let expected = vec![Anchor::parse("f.py@1-5").unwrap(), Anchor::parse("other.py").unwrap()];
        let results = vec![spanned("f.py", 1, 1, 5)];
        let t = score_tokens(&results, &expected, 5, &uniform()).unwrap();
        assert_eq!(t.recall, 1.0);
        assert_eq!(t.precision, 1.0);
        assert_eq!(t.iou, 1.0);
    }

    #[test]
    fn line_weights_use_the_tokens_v1_estimator_per_line() {
        // One chunk: lines 3-4 of python.py, contents pinned below.
        //   line 3: "def read_config(path):"  = 22 bytes → floor(22/4) = 5
        //   line 4: "    return os.path.join(path, \"config.yml\")" = 43 bytes → 10
        let chunk = Chunk {
            chunk_id: "x".into(),
            file_path: "python.py".into(),
            start_line: 3,
            end_line: 4,
            chunk_type: "function".into(),
            kind: "function_definition".into(),
            language: "python".into(),
            content: "def read_config(path):\n    return os.path.join(path, \"config.yml\")".into(),
            token_count: 16,
            embedding: Vec::new(),
        };
        let w = LineWeights::from_chunks(&[chunk]);
        assert_eq!(w.weight("python.py", 3), 5);
        assert_eq!(w.weight("python.py", 4), 10);
        // Uncovered lines — gaps and ranges beyond the file — weigh the
        // estimator floor of 1, same as estimate_tokens("").
        assert_eq!(w.weight("python.py", 1), 1);
        assert_eq!(w.weight("python.py", 999), 1);
        assert_eq!(w.weight("missing.py", 3), 1);
        assert_eq!(estimate_tokens(""), 1);
    }

    #[test]
    fn score_tokens_weighted_partial_overlap_hand_computed() {
        // Weighted version of the partial-overlap case, one file:
        //   chunk A (retrieved): lines 1-2, texts of 8 and 4 bytes → 2 + 1 tokens
        //   anchor range: lines 2-3; line 3 uncovered → weight 1
        //   overlap = line 2 → 1 token; relevant = 1+1 = 2; retrieved = 2+1 = 3
        //   recall = 1/2, precision = 1/3, IoU = 1/(2+3-1) = 0.25
        let chunk = Chunk {
            chunk_id: "x".into(),
            file_path: "f.py".into(),
            start_line: 1,
            end_line: 2,
            chunk_type: "function".into(),
            kind: "function_definition".into(),
            language: "python".into(),
            content: "abcdefgh\nabcd".into(),
            token_count: 3,
            embedding: Vec::new(),
        };
        let w = LineWeights::from_chunks(&[chunk]);
        let expected = vec![Anchor::parse("f.py@2-3").unwrap()];
        let results = vec![spanned("f.py", 1, 1, 2)];
        let t = score_tokens(&results, &expected, 5, &w).unwrap();
        assert!((t.recall - 0.5).abs() < 1e-12);
        assert!((t.precision - 1.0 / 3.0).abs() < 1e-12);
        assert!((t.iou - 0.25).abs() < 1e-12);
    }

    #[test]
    fn line_weights_overlapping_chunks_first_wins() {
        // A class chunk covers lines 1-2 with true (indented) text; a nested
        // method chunk re-covers line 2 without the indent. First wins.
        let class_chunk = Chunk {
            chunk_id: "c".into(),
            file_path: "f.py".into(),
            start_line: 1,
            end_line: 2,
            chunk_type: "class".into(),
            kind: "class_definition".into(),
            language: "python".into(),
            content: "class A:\n    def m(self): pass".into(),
            token_count: 7,
            embedding: Vec::new(),
        };
        let method_chunk = Chunk {
            chunk_id: "m".into(),
            file_path: "f.py".into(),
            start_line: 2,
            end_line: 2,
            chunk_type: "function".into(),
            kind: "function_definition".into(),
            language: "python".into(),
            content: "def m(self): pass".into(),
            token_count: 4,
            embedding: Vec::new(),
        };
        let w = LineWeights::from_chunks(&[class_chunk, method_chunk]);
        // "    def m(self): pass" = 21 bytes → 5, not the unindented 17 → 4.
        assert_eq!(w.weight("f.py", 2), 5);
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
        let s = score_query("q", &results, &expected, 5, &LineWeights::default());
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
        let s = score_query("q", &results, &expected, 1, &LineWeights::default());
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
        let s = score_query("q", &results, &expected, 5, &LineWeights::default());
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
        let s = score_query("q", &results, &expected, 2, &LineWeights::default());
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
        let s = score_query("q", &results, &expected, 5, &LineWeights::default());
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
        let s = score_query("q", &results, &expected, 3, &LineWeights::default());
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
        let a = render_json("corpus", "hash", &reports, None);
        let b = render_json("corpus", "hash", &reports, None);
        assert_eq!(a, b);
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["schema"], RELEVANCE_REPORT_SCHEMA_ID);
        assert_eq!(v["schema"], "cce.relevance.report/v2");
        assert_eq!(v["queries"], 1);
        let backends = v["backends"].as_array().unwrap();
        assert_eq!(backends.len(), 3);
        assert_eq!(backends[0]["backend"], "bm25");
        // Scores are fixed 6-decimal strings, like `cce search --json`.
        assert!(backends[0]["precision_at_k"].as_str().unwrap().contains('.'));
        // No ranged case, no compare → the v1 shape carried over: no token or
        // compare keys anywhere.
        assert!(backends[0].get("tokens").is_none());
        assert!(backends[0]["per_query"][0].get("tokens").is_none());
        assert!(v.get("compare").is_none());
        assert!(a.ends_with("}\n"));
    }

    #[test]
    fn compare_stats_hand_computed_and_rendered() {
        // Two synthetic reports over the same 6 cases, engineered so the MRR
        // deltas are exactly [0.2, 0.1, 0.0, 0.3, −0.1, 0.1] — the stats.rs
        // hand-worked example (t = √3, CI = [−0.048413, 0.248413]) — while
        // precision deltas are all zero (p = 1, t undefined).
        let mk = |backend, mrrs: &[f64]| BackendReport {
            backend,
            precision: 0.5,
            recall: 1.0,
            mrr: mrrs.iter().sum::<f64>() / mrrs.len() as f64,
            f1: 0.6,
            tokens: None,
            queries: mrrs
                .iter()
                .enumerate()
                .map(|(i, m)| QueryScore {
                    id: format!("q{i}"),
                    k: 5,
                    precision: 0.5,
                    recall: 1.0,
                    mrr: *m,
                    f1: 0.6,
                    first_relevant_rank: Some(1),
                    tokens: None,
                })
                .collect(),
        };
        let a = mk(Backend::Bm25, &[0.5, 0.5, 0.5, 0.5, 0.5, 0.5]);
        let b = mk(Backend::Hybrid, &[0.7, 0.6, 0.5, 0.8, 0.4, 0.6]);
        let cs = compare_stats(&a, &b);
        assert_eq!(cs.metrics.len(), 4);
        let mrr = &cs.metrics[2];
        assert_eq!(mrr.key, "mrr");
        assert_eq!(mrr.stats.n, 6);
        assert!((mrr.stats.t.unwrap() - 3.0_f64.sqrt()).abs() < 1e-9);
        let p = &cs.metrics[0];
        assert_eq!(p.key, "precision_at_k");
        assert_eq!(p.stats.t, None);
        assert_eq!(p.stats.p, Some(1.0));

        // Human block: table header, the undefined-t marker, and the CI pair.
        let human = render_compare_human(&a, &b, &cs);
        assert!(human.contains("paired t-test on the per-query deltas (two-sided, n=6, df=5)"));
        assert!(human.contains("mean-delta"));
        assert!(human.contains("95% CI"));
        assert!(human.contains("n/a"), "{human}");
        assert!(human.contains("[-0.048413, +0.248413]"), "{human}");
        assert!(human.contains("+1.732051"), "{human}");

        // JSON block: alphabetical keys, strings for defined stats, null for
        // the undefined t.
        let json = render_json("corpus", "hash", &[a, b], Some(&cs));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["compare"]["a"], "bm25");
        assert_eq!(v["compare"]["b"], "hybrid");
        let m = &v["compare"]["metrics"];
        assert_eq!(m["mrr"]["n"], 6);
        assert_eq!(m["mrr"]["t"], "+1.732051");
        assert_eq!(m["mrr"]["mean_delta"], "+0.100000");
        assert_eq!(m["mrr"]["ci95_low"], "-0.048413");
        assert_eq!(m["mrr"]["ci95_high"], "+0.248413");
        assert_eq!(m["precision_at_k"]["t"], serde_json::Value::Null);
        assert_eq!(m["precision_at_k"]["p"], "1.000000");
        assert_eq!(m["recall"]["n"], 6);
        assert_eq!(m["f1"]["n"], 6);
    }

    #[test]
    fn render_json_carries_token_fields_for_ranged_cases() {
        // A synthetic single-backend report with one ranged case: the tokens
        // objects must appear at both per-query and backend level, as fixed
        // 6-decimal strings.
        let report = BackendReport {
            backend: Backend::Bm25,
            precision: 0.2,
            recall: 1.0,
            mrr: 1.0,
            f1: 1.0 / 3.0,
            tokens: Some(TokenAggregate { queries: 1, precision: 0.5, recall: 1.0, iou: 0.5 }),
            queries: vec![QueryScore {
                id: "ranged".into(),
                k: 5,
                precision: 0.2,
                recall: 1.0,
                mrr: 1.0,
                f1: 1.0 / 3.0,
                first_relevant_rank: Some(1),
                tokens: Some(TokenScore { precision: 0.5, recall: 1.0, iou: 0.5 }),
            }],
        };
        let json = render_json("corpus", "hash", std::slice::from_ref(&report), None);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["backends"][0]["tokens"]["queries"], 1);
        assert_eq!(v["backends"][0]["tokens"]["precision"], "0.500000");
        assert_eq!(v["backends"][0]["per_query"][0]["tokens"]["recall"], "1.000000");
        assert_eq!(v["backends"][0]["per_query"][0]["tokens"]["iou"], "0.500000");

        // And the human table grows the token-level section.
        let human = render_human("corpus", "hash", &[report]);
        assert!(human.contains("token-level span metrics (1 of 1 queries carry ranged anchors)"));
        assert!(human.contains("tok-P"));
        assert!(human.contains("tok-IoU"));
    }
}
