//! # knowledge::ask — the Knowledge-Ask golden suite (Epic Davidslv/signal-engine#8 · U5.4)
//!
//! **Why this file exists:** `cce relevance` measures *code* ranking quality; this is
//! its sibling for the **curated knowledge corpus**. A knowledge host is only worth
//! standing up if the questions an operator actually asks are answered *from the
//! corpus* — and stay answered after every rebuild. This module is that standing
//! regression check: a committed suite of real questions, each pinned to the curated
//! record a good answer must surface, run through the **exact** retrieval MCP
//! `context_search` serves for `source: knowledge` ([`search_knowledge`]). No
//! reimplementation — it measures the real thing.
//!
//! **What it is / does:** Parses a `cce.knowledge.ask/v1` suite (NDJSON: an optional
//! header naming the corpus feed, then one labeled query per line), runs each query
//! against a [`KnowledgeStore`], scores whether the expected record(s) surfaced in the
//! top-k with the same IR metrics family as `cce relevance` (precision@k / recall /
//! MRR / F1), and renders a human table or the byte-pinnable
//! `cce.knowledge.ask.report/v1` JSON. A query is **proven** when every expected
//! record surfaces in its top-k (recall == 1.0).
//!
//! **Responsibilities:**
//! - Own the `cce.knowledge.ask/v1` suite contract and its parsing.
//! - Score each case against `record_id` anchors; never re-rank (that is
//!   [`search_knowledge`]'s job).
//! - Render deterministic, byte-pinnable reports (fixed 6-decimal scores).

use crate::embedder::format6;
use crate::knowledge::retrieval::{search_knowledge, KnowledgeHit};
use crate::knowledge::store::KnowledgeStore;

/// The pinned schema id for the suite contract. A bump is a compatibility event.
pub const ASK_SCHEMA_ID: &str = "cce.knowledge.ask/v1";

/// The pinned schema id of the `--json` report shape.
pub const ASK_REPORT_SCHEMA_ID: &str = "cce.knowledge.ask.report/v1";

/// The default cut-off when a case omits `k` (the knowledge search default).
pub const DEFAULT_ASK_K: usize = 10;

// --- Suite contract (cce.knowledge.ask/v1) ---

/// One labeled query: run `query` verbatim, expect the `expect` record ids in the top-`k`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskCase {
    pub id: String,
    pub query: String,
    /// Non-empty set of expected `record_id` anchors a good top-k must surface.
    pub expect: Vec<String>,
    pub k: usize,
}

/// A parsed suite: the optional corpus-feed hint from the header plus the cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskSuite {
    /// A `cce.knowledge/v1` feed path, resolved relative to the suite file. `--dir`
    /// (an installed store) overrides it.
    pub corpus: Option<String>,
    pub cases: Vec<AskCase>,
}

/// Parse a `cce.knowledge.ask/v1` NDJSON suite.
///
/// The first non-blank line MAY be a header `{"schema":"cce.knowledge.ask/v1",
/// "corpus":"…"}` (pins the schema, names the default corpus feed). Every other
/// non-blank line is a case `{"id":"…","query":"…","expect":["…"],"k":N}`. `id`
/// defaults to `q<line-number>`, `k` to [`DEFAULT_ASK_K`]; `query` and a non-empty
/// `expect` are required. A malformed line fails loudly with its 1-based number.
pub fn parse_suite(text: &str) -> Result<AskSuite, String> {
    let mut corpus: Option<String> = None;
    let mut cases: Vec<AskCase> = Vec::new();
    let mut seen_header = false;
    let mut seen_case = false;

    for (i, line) in text.lines().enumerate() {
        let line_no = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let obj: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("line {line_no}: invalid JSON: {e}"))?;

        // A header line carries `schema` and no `query`.
        if obj.get("schema").is_some() && obj.get("query").is_none() {
            if seen_header || seen_case {
                return Err(format!("line {line_no}: header must be the first line"));
            }
            let schema = obj.get("schema").and_then(|s| s.as_str()).unwrap_or_default();
            if schema != ASK_SCHEMA_ID {
                return Err(format!(
                    "line {line_no}: unknown schema {schema:?} (expected {ASK_SCHEMA_ID:?})"
                ));
            }
            seen_header = true;
            corpus = obj
                .get("corpus")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            continue;
        }

        // A case line.
        let query = obj
            .get("query")
            .and_then(|q| q.as_str())
            .ok_or_else(|| format!("line {line_no}: case missing string `query`"))?
            .to_string();
        let expect: Vec<String> = obj
            .get("expect")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if expect.is_empty() {
            return Err(format!("line {line_no}: case needs a non-empty `expect` array"));
        }
        let k = match obj.get("k") {
            None => DEFAULT_ASK_K,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| format!("line {line_no}: `k` must be a positive integer"))?;
                if n == 0 {
                    return Err(format!("line {line_no}: `k` must be >= 1"));
                }
                n as usize
            }
        };
        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("q{line_no}"));
        cases.push(AskCase { id, query, expect, k });
        seen_case = true;
    }

    if cases.is_empty() {
        return Err("suite has no cases".to_string());
    }
    Ok(AskSuite { corpus, cases })
}

// --- Scoring ---

/// One case's score against its expected `record_id` anchors. `proven` is the
/// headline: every expected record surfaced in the top-k.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseScore {
    pub id: String,
    pub k: usize,
    /// Number of expected anchors (the recall denominator).
    pub expected: usize,
    pub precision: f64,
    pub recall: f64,
    pub mrr: f64,
    pub f1: f64,
    /// 1-based rank of the first hit whose record matches an expected anchor.
    pub first_hit_rank: Option<usize>,
    /// recall == 1.0: every expected record surfaced in the top-k.
    pub proven: bool,
}

/// Score one case's ranked knowledge hits against its expected record ids. Only the
/// top-`k` hits are considered (the knowledge search already truncated to the case's
/// `k`, but scoring re-applies the cut defensively so a suite `k` smaller than the
/// query's is honored). A hit is *relevant* when its `record_id` is in `expect`; an
/// anchor is *matched* when any considered hit carries it.
pub fn score_case(case: &AskCase, hits: &[KnowledgeHit]) -> CaseScore {
    let considered = &hits[..hits.len().min(case.k)];
    let mut relevant_retrieved = 0usize;
    let mut first_hit_rank: Option<usize> = None;
    for (i, h) in considered.iter().enumerate() {
        if case.expect.iter().any(|e| e == &h.record_id) {
            relevant_retrieved += 1;
            if first_hit_rank.is_none() {
                first_hit_rank = Some(i + 1);
            }
        }
    }
    let anchors_matched =
        case.expect.iter().filter(|e| considered.iter().any(|h| &h.record_id == *e)).count();

    let precision = relevant_retrieved as f64 / case.k as f64;
    let recall = anchors_matched as f64 / case.expect.len() as f64;
    let mrr = first_hit_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0);
    let f1 = if precision + recall > 0.0 {
        2.0 * precision * recall / (precision + recall)
    } else {
        0.0
    };
    CaseScore {
        id: case.id.clone(),
        k: case.k,
        expected: case.expect.len(),
        precision,
        recall,
        mrr,
        f1,
        first_hit_rank,
        proven: (recall - 1.0).abs() < f64::EPSILON,
    }
}

/// The whole suite scored against a corpus: macro-averaged metrics plus the
/// proven-query headline (`proven` of `queries`).
#[derive(Debug, Clone, PartialEq)]
pub struct SuiteReport {
    pub corpus: String,
    pub min_score: f64,
    pub queries: usize,
    pub proven: usize,
    pub precision: f64,
    pub recall: f64,
    pub mrr: f64,
    pub f1: f64,
    pub cases: Vec<CaseScore>,
}

impl SuiteReport {
    /// True iff every case in the suite proved — the CI gate condition.
    pub fn all_proven(&self) -> bool {
        self.queries > 0 && self.proven == self.queries
    }
}

/// Run every case in `suite` against `store` through [`search_knowledge`] at
/// `min_score`, then aggregate. Deterministic: [`search_knowledge`] uses the hash
/// embedder and no wall-clock, so the report is byte-pinnable.
pub fn evaluate(
    store: &KnowledgeStore,
    suite: &AskSuite,
    min_score: f64,
    corpus: &str,
) -> SuiteReport {
    let cases: Vec<CaseScore> = suite
        .cases
        .iter()
        .map(|c| {
            let hits = search_knowledge(store, &c.query, c.k, min_score);
            score_case(c, &hits)
        })
        .collect();

    let n = cases.len();
    let mean = |f: &dyn Fn(&CaseScore) -> f64| -> f64 {
        if n == 0 {
            0.0
        } else {
            cases.iter().map(f).sum::<f64>() / n as f64
        }
    };
    let proven = cases.iter().filter(|c| c.proven).count();
    SuiteReport {
        corpus: corpus.to_string(),
        min_score,
        queries: n,
        proven,
        precision: mean(&|c| c.precision),
        recall: mean(&|c| c.recall),
        mrr: mean(&|c| c.mrr),
        f1: mean(&|c| c.f1),
        cases,
    }
}

// --- Rendering (deterministic; format6 fixed 6-decimal strings) ---

/// The human summary: the proven headline, the corpus, then one row per case and the
/// macro-averaged footer.
pub fn render_human(report: &SuiteReport) -> String {
    let mut out = String::new();
    out.push_str("CCE knowledge-ask — answers vs the curated corpus (cce.knowledge.ask/v1)\n");
    out.push_str(&format!("  corpus   : {}\n", report.corpus));
    out.push_str(&format!("  min_score: {}\n", format6(report.min_score)));
    out.push_str(&format!("  proven   : {}/{}\n\n", report.proven, report.queries));
    out.push_str(&format!(
        "  {:<28}{:>7}{:>10}{:>10}{:>8}  {}\n",
        "query", "P@k", "recall", "MRR", "rank", "proven"
    ));
    for c in &report.cases {
        let rank = c.first_hit_rank.map(|r| r.to_string()).unwrap_or_else(|| "-".to_string());
        out.push_str(&format!(
            "  {:<28}{:>7}{:>10}{:>10}{:>8}  {}\n",
            truncate(&c.id, 28),
            format6(c.precision),
            format6(c.recall),
            format6(c.mrr),
            rank,
            if c.proven { "yes" } else { "NO" },
        ));
    }
    out.push_str(&format!(
        "  {:<28}{:>7}{:>10}{:>10}\n",
        "mean",
        format6(report.precision),
        format6(report.recall),
        format6(report.mrr),
    ));
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// The `--json` report (byte-pinnable): pretty-printed, serde_json's alphabetical key
/// order, scores as fixed 6-decimal strings, one trailing newline — the same grammar
/// discipline as `cce relevance --json`. Schema `cce.knowledge.ask.report/v1`.
pub fn render_json(report: &SuiteReport) -> String {
    let cases: Vec<serde_json::Value> = report
        .cases
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "k": c.k,
                "expected": c.expected,
                "precision_at_k": format6(c.precision),
                "recall": format6(c.recall),
                "mrr": format6(c.mrr),
                "f1": format6(c.f1),
                "first_hit_rank": c.first_hit_rank,
                "proven": c.proven,
            })
        })
        .collect();
    let body = serde_json::json!({
        "schema": ASK_REPORT_SCHEMA_ID,
        "corpus": report.corpus,
        "min_score": format6(report.min_score),
        "queries": report.queries,
        "proven": report.proven,
        "precision_at_k": format6(report.precision),
        "recall": format6(report.recall),
        "mrr": format6(report.mrr),
        "f1": format6(report.f1),
        "cases": cases,
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::contract::parse_ndjson;
    use crate::knowledge::store::ingest_default;

    fn hit(record_id: &str, rank: usize, score: f64) -> KnowledgeHit {
        KnowledgeHit {
            rank,
            chunk_id: format!("{record_id}#0"),
            record_id: record_id.to_string(),
            title: record_id.to_string(),
            kind: "section".to_string(),
            state: None,
            updated_at: None,
            url: None,
            score,
            content: String::new(),
        }
    }

    #[test]
    fn parses_header_and_cases() {
        let text = "{\"schema\":\"cce.knowledge.ask/v1\",\"corpus\":\"c.jsonl\"}\n\
                    {\"id\":\"a\",\"query\":\"q one\",\"expect\":[\"r1\"],\"k\":3}\n\
                    {\"query\":\"q two\",\"expect\":[\"r2\",\"r3\"]}\n";
        let suite = parse_suite(text).unwrap();
        assert_eq!(suite.corpus.as_deref(), Some("c.jsonl"));
        assert_eq!(suite.cases.len(), 2);
        assert_eq!(
            suite.cases[0],
            AskCase {
                id: "a".to_string(),
                query: "q one".to_string(),
                expect: vec!["r1".to_string()],
                k: 3,
            }
        );
        // Defaulted id (line 3) and k.
        assert_eq!(suite.cases[1].id, "q3");
        assert_eq!(suite.cases[1].k, DEFAULT_ASK_K);
        assert_eq!(suite.cases[1].expect, vec!["r2".to_string(), "r3".to_string()]);
    }

    #[test]
    fn rejects_bad_schema_and_empty_expect_and_zero_k() {
        assert!(parse_suite("{\"schema\":\"cce.knowledge.ask/v9\"}\n").is_err());
        assert!(parse_suite("{\"query\":\"q\",\"expect\":[]}\n").is_err());
        assert!(parse_suite("{\"query\":\"q\",\"expect\":[\"r\"],\"k\":0}\n").is_err());
        assert!(parse_suite("{\"expect\":[\"r\"]}\n").is_err()); // missing query
        assert!(parse_suite("\n  \n").is_err()); // no cases
    }

    #[test]
    fn header_must_lead() {
        let text = "{\"query\":\"q\",\"expect\":[\"r\"]}\n\
                    {\"schema\":\"cce.knowledge.ask/v1\"}\n";
        assert!(parse_suite(text).is_err());
    }

    #[test]
    fn score_full_recall_is_proven() {
        let case = AskCase {
            id: "c".to_string(),
            query: "q".to_string(),
            expect: vec!["r1".to_string(), "r2".to_string()],
            k: 5,
        };
        let hits = vec![hit("r1", 1, 0.9), hit("x", 2, 0.5), hit("r2", 3, 0.4)];
        let s = score_case(&case, &hits);
        assert!(s.proven);
        assert_eq!(s.recall, 1.0);
        assert_eq!(s.first_hit_rank, Some(1));
        assert_eq!(s.precision, 2.0 / 5.0); // 2 relevant of k=5
        assert_eq!(s.expected, 2);
    }

    #[test]
    fn score_missing_anchor_is_not_proven() {
        let case = AskCase {
            id: "c".to_string(),
            query: "q".to_string(),
            expect: vec!["r1".to_string(), "gone".to_string()],
            k: 5,
        };
        let hits = vec![hit("r1", 1, 0.9)];
        let s = score_case(&case, &hits);
        assert!(!s.proven);
        assert_eq!(s.recall, 0.5);
    }

    #[test]
    fn k_cut_excludes_late_hit() {
        // The expected record is at rank 3 but k=2 → not considered → not proven.
        let case = AskCase {
            id: "c".to_string(),
            query: "q".to_string(),
            expect: vec!["r2".to_string()],
            k: 2,
        };
        let hits = vec![hit("a", 1, 0.9), hit("b", 2, 0.8), hit("r2", 3, 0.7)];
        let s = score_case(&case, &hits);
        assert!(!s.proven);
        assert_eq!(s.first_hit_rank, None);
    }

    #[test]
    fn evaluate_over_a_tiny_corpus_is_deterministic_and_proves() {
        let feed = "{\"id\":\"k:lockout\",\"title\":\"Lock accounts after five failed logins\",\"body\":\"After five consecutive failed login attempts the account is locked for fifteen minutes.\",\"source\":\"github-issues\"}\n\
                    {\"id\":\"k:retry\",\"title\":\"Webhook retry uses exponential backoff with jitter\",\"body\":\"Failed webhook deliveries retry on an exponential backoff schedule with random jitter.\",\"source\":\"github-issues\"}\n";
        let records = parse_ndjson(feed).unwrap();
        let store = ingest_default(&records, feed.as_bytes());
        let suite = AskSuite {
            corpus: Some("feed.jsonl".to_string()),
            cases: vec![
                AskCase {
                    id: "lockout".to_string(),
                    query: "how many failed login attempts before the account is locked"
                        .to_string(),
                    expect: vec!["k:lockout".to_string()],
                    k: 5,
                },
                AskCase {
                    id: "retry".to_string(),
                    query: "webhook retry backoff jitter schedule".to_string(),
                    expect: vec!["k:retry".to_string()],
                    k: 5,
                },
            ],
        };
        let report = evaluate(&store, &suite, 0.30, "feed.jsonl");
        assert_eq!(report.queries, 2);
        assert!(
            report.all_proven(),
            "both queries should surface their record:\n{}",
            render_human(&report)
        );

        // Rendering is deterministic and byte-stable.
        let a = render_json(&report);
        let b = render_json(&evaluate(&store, &suite, 0.30, "feed.jsonl"));
        assert_eq!(a, b);
        assert!(a.contains("\"schema\": \"cce.knowledge.ask.report/v1\""));
        assert!(a.ends_with("\n"));
    }
}
