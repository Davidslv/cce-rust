//! # grammar — Layer 3 grammar compression: the compact, byte-pinned result grammar
//!
//! **Why this file exists:** SPEC-V2.5 §2 Layer 3 is the "close the loop" layer: the
//! MCP read tools must serialize a result set in ONE canonical, minimal-token,
//! deterministic grammar — no prose scaffolding, fixed field order, terse separators —
//! and that density must be *measurable*. This module owns both halves: the single
//! definition of a result row's **compact grammar** line (which `mcp::tools` renders
//! from, so the served bytes and the measured bytes are the same), and a pinned
//! **verbose baseline** rendering of the same result set used only to quantify the
//! saving. Grammar is the one savings layer that is self-measurable — our format vs a
//! pinned verbose alternative — so this is where the `grammar` ledger bucket is filled.
//!
//! **What it is / does:** Defines [`GrammarRow`] (the fields a result header carries),
//! [`compact_line`] (the byte-pinned one-line dense form — rank, score, optional
//! `package · `, `file:start-end`, `(type/kind)`, `#chunk_id`), [`render_compact`] /
//! [`render_verbose`] (the whole result set in each grammar), and [`grammar_savings`]
//! (`saved = tokens(verbose) − tokens(compact)`, `baseline = tokens(verbose)`, counted
//! with the ONE `cce.tokens/v1` estimator). Both renderings deliberately cover only the
//! per-result framing — NOT chunk bodies — so this measurement is independent of the L2
//! `chunk_compression` bucket and never double-counts it.
//!
//! **Responsibilities:**
//! - Own the byte-pinned compact result-grammar line and the pinned verbose baseline.
//! - Own the `grammar` bucket math for a `context_search` result set.
//! - Stay deterministic and offline: pure functions of the row fields, fixed order,
//!   no wall-clock / random / hash-iteration. It counts framing only; bodies are the
//!   caller's concern (served at `detail`, measured by the `chunk_compression` bucket).

use crate::embedder::format6;
use crate::retriever::SearchResult;
use crate::savings::Bucket;
use crate::tokenizer::estimate_tokens;

/// The fields of one rendered `context_search` result header — everything the compact
/// grammar and the verbose baseline serialize. `package` is `Some` only in workspace
/// (federated) results; single-repo results leave it `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarRow {
    pub rank: usize,
    /// The relevance score's 6-dp fixed form (already formatted by [`format6`]), kept
    /// as a string so the grammar is a pure function of bytes, not of a float.
    pub score6: String,
    pub package: Option<String>,
    pub file_path: String,
    pub start: usize,
    pub end: usize,
    pub chunk_type: String,
    pub kind: String,
    pub chunk_id: String,
}

impl GrammarRow {
    /// Build a row view from a single-repo/federated [`SearchResult`]. The score is
    /// pinned through [`format6`] so it matches what the tool renders. `package` is
    /// `None` because a `SearchResult`'s `file_path` already carries the member prefix
    /// in workspace mode — the grammar bucket measures framing density either way.
    pub fn from_result(r: &SearchResult) -> GrammarRow {
        GrammarRow {
            rank: r.rank,
            score6: format6(r.score),
            package: None,
            file_path: r.file_path.clone(),
            start: r.start_line,
            end: r.end_line,
            chunk_type: r.chunk_type.clone(),
            kind: r.kind.clone(),
            chunk_id: r.chunk_id.clone(),
        }
    }
}

/// The byte-pinned compact grammar for ONE result: a single dense line
/// `«rank». [«score»] «pkg»«file»:«start»-«end» («type»/«kind») #«chunk_id»` — no
/// trailing newline (the caller adds it). This is THE canonical result-header grammar;
/// `mcp::tools::format_rows` renders from it, so the served bytes and the measured
/// compact bytes are one and the same. `package`, when present, is prefixed as
/// `«package» · ` (U+00B7 middle dot), matching the federated result header.
pub fn compact_line(row: &GrammarRow) -> String {
    let pkg = match &row.package {
        Some(p) => format!("{p} · "),
        None => String::new(),
    };
    format!(
        "{:>2}. [{}] {}{}:{}-{} ({}/{}) #{}",
        row.rank,
        row.score6,
        pkg,
        row.file_path,
        row.start,
        row.end,
        row.chunk_type,
        row.kind,
        row.chunk_id
    )
}

/// Render a whole result set in the compact grammar: one [`compact_line`] per row, each
/// terminated by `\n`. This is the framing the tool emits (chunk bodies excluded).
pub fn render_compact(rows: &[GrammarRow]) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(&compact_line(row));
        out.push('\n');
    }
    out
}

/// Render a whole result set in the **pinned verbose baseline** grammar: a labelled,
/// multi-line-per-field prose block per result, blocks separated by a blank line. This
/// is the deliberately-verbose alternative the compact grammar is measured against
/// (SPEC-V2.5 §2 Layer 3, §3) — it is NOT what the tool emits; it exists only as the
/// self-measurement counterfactual. Byte-pinned and checked in as a golden fixture.
pub fn render_verbose(rows: &[GrammarRow]) -> String {
    let blocks: Vec<String> = rows
        .iter()
        .map(|row| {
            let mut b = format!("Result {}:\n  relevance score: {}\n", row.rank, row.score6);
            if let Some(pkg) = &row.package {
                b.push_str(&format!("  package: {pkg}\n"));
            }
            b.push_str(&format!(
                "  file path: {}\n  start line: {}\n  end line: {}\n  chunk type: {}\n  \
                 chunk kind: {}\n  chunk id: {}\n",
                row.file_path, row.start, row.end, row.chunk_type, row.kind, row.chunk_id
            ));
            b
        })
        .collect();
    blocks.join("\n")
}

/// The `grammar` savings bucket for a result set (SPEC-V2.5 §2 Layer 3, §3):
/// `baseline = tokens(verbose_baseline)`, `saved = baseline − tokens(compact_grammar)`,
/// counted with the ONE `cce.tokens/v1` estimator. An empty result set saves nothing
/// (grammar compression scales per result), so it yields a clean zero bucket rather
/// than the estimator's non-zero floor on the empty string.
pub fn grammar_savings(rows: &[GrammarRow]) -> Bucket {
    if rows.is_empty() {
        return Bucket::default();
    }
    let compact = estimate_tokens(&render_compact(rows));
    let baseline = estimate_tokens(&render_verbose(rows));
    Bucket { saved_tokens: baseline.saturating_sub(compact), baseline_tokens: baseline }
}

/// The `grammar` bucket for a slice of [`SearchResult`]s — the entry point
/// `retriever::build_search_record` uses when logging a `search` event.
pub fn grammar_savings_for_results(results: &[SearchResult]) -> Bucket {
    let rows: Vec<GrammarRow> = results.iter().map(GrammarRow::from_result).collect();
    grammar_savings(&rows)
}

// --- the opt-in MCP result footer (SPEC-USAGE-VISIBILITY §3, v2.8) ---

/// The numbers the usage footer prints — every one already computed for the
/// recorded `search` event (plus the union chunk count the renderer was given).
/// The footer is a PURE PROJECTION of these values: rendering it never changes a
/// recorded metric (Invariant 1).
#[derive(Debug, Clone, PartialEq)]
pub struct FooterFacts {
    /// `result_count` off the search event.
    pub result_count: u64,
    /// The searched corpus size — the same chunk count the result renderer shows.
    pub total_chunks: u64,
    /// `served_tokens` off the search event.
    pub served_tokens: u64,
    /// `baseline_tokens` off the search event.
    pub baseline_tokens: u64,
    /// `tokens_saved` off the search event.
    pub tokens_saved: u64,
    /// `savings_ratio` off the search event (0..=1).
    pub savings_ratio: f64,
}

/// The byte-pinned one-line usage footer (`mcp.result_footer: on`):
/// `cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880 baseline · saved ~8,676 (88%)`
/// — no trailing newline (the caller adds it). `session` totals (searches +
/// tokens saved THIS session, including this call) append the trailing clause
/// `· session: 42 searches, ~310k saved`. Thousands separators and the short
/// token form are the pinned `cce usage` formats (`crate::usage`).
pub fn usage_footer_line(facts: &FooterFacts, session: Option<(u64, u64)>) -> String {
    let pct = (facts.savings_ratio * 100.0).round() as i64;
    let mut line = format!(
        "cce: {} results from {} chunks · served ~{} tok vs ~{} baseline · saved ~{} ({pct}%)",
        facts.result_count,
        crate::usage::fmt_thousands(facts.total_chunks),
        crate::usage::fmt_thousands(facts.served_tokens),
        crate::usage::fmt_thousands(facts.baseline_tokens),
        crate::usage::fmt_thousands(facts.tokens_saved),
    );
    if let Some((searches, saved)) = session {
        line.push_str(&format!(
            " · session: {searches} searches, ~{} saved",
            crate::usage::fmt_tokens_short(saved)
        ));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned fixture result set used by the goldens below and by the checked-in
    /// `test/fixture/savings/grammar_*.txt` files. Mixes a single-repo row (no package)
    /// with a federated row (package present) so both branches are pinned.
    fn fixture_rows() -> Vec<GrammarRow> {
        vec![
            GrammarRow {
                rank: 1,
                score6: format6(0.912345),
                package: None,
                file_path: "src/auth.py".into(),
                start: 10,
                end: 24,
                chunk_type: "function".into(),
                kind: "function_definition".into(),
                chunk_id: "039f8bd5b80a698e".into(),
            },
            GrammarRow {
                rank: 2,
                score6: format6(0.5),
                package: Some("billing".into()),
                file_path: "lib/charge.rb".into(),
                start: 3,
                end: 7,
                chunk_type: "method".into(),
                kind: "method".into(),
                chunk_id: "61707be0deb092a1".into(),
            },
            GrammarRow {
                rank: 3,
                score6: format6(0.048),
                package: None,
                file_path: "app/models/user.rb".into(),
                start: 1,
                end: 2,
                chunk_type: "class".into(),
                kind: "class".into(),
                chunk_id: "aaaabbbbccccdddd".into(),
            },
        ]
    }

    #[test]
    fn compact_line_is_byte_pinned() {
        let rows = fixture_rows();
        assert_eq!(
            compact_line(&rows[0]),
            " 1. [0.912345] src/auth.py:10-24 (function/function_definition) #039f8bd5b80a698e"
        );
        // The federated row prefixes `package · `.
        assert_eq!(
            compact_line(&rows[1]),
            " 2. [0.500000] billing · lib/charge.rb:3-7 (method/method) #61707be0deb092a1"
        );
    }

    #[test]
    fn render_compact_matches_the_checked_in_fixture() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/savings/grammar_compact.txt");
        let golden = std::fs::read_to_string(path).unwrap();
        assert_eq!(render_compact(&fixture_rows()), golden);
    }

    #[test]
    fn render_verbose_matches_the_pinned_baseline_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/fixture/savings/grammar_verbose_baseline.txt"
        );
        let golden = std::fs::read_to_string(path).unwrap();
        assert_eq!(render_verbose(&fixture_rows()), golden);
    }

    #[test]
    fn grammar_savings_math_is_pinned_on_the_fixture() {
        let rows = fixture_rows();
        let compact_t = estimate_tokens(&render_compact(&rows));
        let verbose_t = estimate_tokens(&render_verbose(&rows));
        let bucket = grammar_savings(&rows);
        // The bucket is exactly verbose − compact, baselined on verbose.
        assert_eq!(bucket.baseline_tokens, verbose_t);
        assert_eq!(bucket.saved_tokens, verbose_t - compact_t);
        // Byte-pinned absolute numbers (cce.tokens/v1 over the golden fixtures).
        assert_eq!(bucket, Bucket { saved_tokens: 77, baseline_tokens: 134 });
        // The compact grammar really is smaller than the verbose baseline.
        assert!(compact_t < verbose_t);
    }

    #[test]
    fn empty_result_set_is_a_clean_zero_bucket() {
        assert_eq!(grammar_savings(&[]), Bucket::default());
        assert_eq!(render_compact(&[]), "");
        assert_eq!(render_verbose(&[]), "");
    }

    #[test]
    fn usage_footer_line_is_byte_pinned() {
        // The spec's exact example (SPEC-USAGE-VISIBILITY §3.3).
        let facts = FooterFacts {
            result_count: 5,
            total_chunks: 38_628,
            served_tokens: 1_204,
            baseline_tokens: 9_880,
            tokens_saved: 8_676,
            savings_ratio: 0.88,
        };
        assert_eq!(
            usage_footer_line(&facts, None),
            "cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880 baseline · saved \
             ~8,676 (88%)"
        );
        // `session` appends the pinned trailing clause.
        assert_eq!(
            usage_footer_line(&facts, Some((42, 310_880))),
            "cce: 5 results from 38,628 chunks · served ~1,204 tok vs ~9,880 baseline · saved \
             ~8,676 (88%) · session: 42 searches, ~310k saved"
        );
    }

    #[test]
    fn usage_footer_line_zero_result_set_is_clean() {
        let facts = FooterFacts {
            result_count: 0,
            total_chunks: 12,
            served_tokens: 0,
            baseline_tokens: 0,
            tokens_saved: 0,
            savings_ratio: 0.0,
        };
        assert_eq!(
            usage_footer_line(&facts, None),
            "cce: 0 results from 12 chunks · served ~0 tok vs ~0 baseline · saved ~0 (0%)"
        );
    }

    #[test]
    fn from_result_pins_the_score_and_drops_package() {
        let r = SearchResult {
            rank: 4,
            chunk_id: "cafef00dcafef00d".into(),
            file_path: "member/x.rs".into(),
            start_line: 5,
            end_line: 9,
            chunk_type: "function".into(),
            kind: "function_item".into(),
            score: 0.25,
            content: "fn x() {}".into(),
        };
        let row = GrammarRow::from_result(&r);
        assert_eq!(row.score6, "0.250000");
        assert_eq!(row.package, None);
        assert_eq!(
            compact_line(&row),
            " 4. [0.250000] member/x.rs:5-9 (function/function_item) #cafef00dcafef00d"
        );
    }
}
