//! # aggregator — the pure metrics aggregate (exact, cross-language-identical)
//!
//! **Why this file exists:** DASHBOARD-SPEC §4 turns the raw event log into the
//! numbers a user actually reads: totals, the two north-stars (token/cost SAVINGS
//! and retrieval QUALITY) with current-vs-prior "improving/degrading" deltas, a
//! daily series, and a recent-searches view. It MUST reproduce the §4.1 anchor
//! exactly, and both language implementations must produce identical numbers.
//!
//! **What it is / does:** A single pure function `aggregate(events, now, price)`.
//! No wall-clock and no randomness inside — the instant is injected — so it is
//! fully testable and language-independent. Output rounding follows §4:
//! ratios/scores to 6 decimals, cost to 2 decimals, round-half-away-from-zero.
//!
//! **Responsibilities:**
//! - Own the aggregate output shape and every formula in §4.
//! - Own the current/prior windowing and the up/down/flat direction rule.
//! - It does NOT read files, serve HTTP, or touch the clock; callers inject
//!   `now` and add the non-conformance `generated_ts` at the edge.

use crate::config::{DIRECTION_EPSILON, RECENT_SEARCHES_LIMIT, TREND_WINDOW_DAYS};
use crate::embedder::round6;
use crate::metrics::{date_str, Event, FeedbackEvent, IndexEvent, SearchEvent};
use crate::savings::{sum_by_layer, SavingsByLayer};
use serde::Serialize;
use std::collections::BTreeMap;

const DAY_SECS: i64 = 86_400;

/// The aggregate, minus `generated_ts` (added by the API at serialization time).
///
/// `by_source`, `secret_safety`, `index_freshness`, and `totals.mean_top_score`
/// were added additively in v2.4.1 to feed the refreshed dashboard panels. They are
/// **pure functions of the log**, so both engines reproduce them identically and the
/// dashboard makes NO network call. Behind-remote is intentionally NOT here — a live
/// remote comparison belongs in `cce sync status` and MCP `index_status`.
#[derive(Debug, Serialize)]
pub struct Aggregate {
    pub schema: String,
    pub totals: Totals,
    pub north_star: NorthStar,
    /// Agent-vs-human split of searches, keyed by `source` (v2.4.1).
    pub by_source: UsageBySource,
    /// Secret-safety reassurance: sensitive files skipped across index runs (v2.4.1).
    pub secret_safety: SecretSafety,
    /// Index freshness from the latest index event (v2.4.1) — purely log-derived,
    /// no network call.
    pub index_freshness: IndexFreshness,
    /// The seven-bucket savings ledger rolled up over the log (SPEC-V2.5 §3).
    /// Purely log-derived, cross-language-identical shape; only `retrieval` is
    /// populated in Stage ①. Carries the mandatory honesty `note`.
    pub savings_by_layer: SavingsByLayer,
    pub series: Series,
    pub recent_searches: Vec<RecentSearch>,
}

#[derive(Debug, Serialize)]
pub struct Totals {
    pub searches: u64,
    pub indexes: u64,
    pub feedback: u64,
    pub tokens_saved: u64,
    pub cost_saved_usd: f64,
    pub mean_savings_ratio: f64,
    /// Lifetime mean of `top_score` over non-empty searches (v2.4.1); `0.0` if none.
    pub mean_top_score: f64,
    pub helpful: u64,
    pub not_helpful: u64,
    pub helpful_rate: Option<f64>,
}

/// The agent-vs-human usage split (v2.4.1): CLI (`cce search`) vs MCP (agent
/// `context_search`) searches. `source` values other than `"mcp"` count as `cli`.
#[derive(Debug, Serialize)]
pub struct UsageBySource {
    pub cli: SourceUsage,
    pub mcp: SourceUsage,
}

/// One source bucket's usage: how many searches and how much they saved/scored.
#[derive(Debug, Serialize)]
pub struct SourceUsage {
    pub searches: u64,
    pub tokens_saved: u64,
    pub mean_savings_ratio: f64,
    pub mean_top_score: f64,
    /// Mean recorded `latency_ms` over the bucket's searches (v2.8, additive):
    /// feeds the `cce usage` split. Pre-v2.4 events with no `latency_ms` count
    /// as `0.0` (the read-side default), keeping the mean a pure log function.
    pub mean_latency_ms: f64,
}

/// Secret-safety reassurance (v2.4.1): the sensitive files the secure-by-default
/// walk skipped, summed over the log's index runs.
#[derive(Debug, Serialize)]
pub struct SecretSafety {
    pub sensitive_skipped: u64,
    pub index_runs: u64,
}

/// Index freshness (v2.4.1): the latest index event's provenance — `source`
/// (`"local"` for `cce index`, `"sync-pull"` for a `cce sync pull` install), `sha`,
/// and `indexed_ts` — all `None` when the log has no index event yet. **Purely
/// log-derived**: the dashboard makes NO network call, so this holds no live
/// remote comparison. Behind-remote lives in `cce sync status` and MCP
/// `index_status`, where a live lookup belongs.
#[derive(Debug, Serialize)]
pub struct IndexFreshness {
    pub indexes: u64,
    pub source: Option<String>,
    pub sha: Option<String>,
    pub indexed_ts: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NorthStar {
    pub savings: Savings,
    pub quality: Quality,
}

#[derive(Debug, Serialize)]
pub struct Savings {
    pub current: SavingsWindow,
    pub prior: SavingsWindow,
    pub delta_ratio: f64,
    pub direction: String,
}

#[derive(Debug, Serialize)]
pub struct SavingsWindow {
    pub searches: u64,
    pub tokens_saved: u64,
    pub mean_savings_ratio: f64,
}

#[derive(Debug, Serialize)]
pub struct Quality {
    pub current: QualityWindow,
    pub prior: QualityWindow,
    pub delta_top_score: f64,
    pub direction: String,
}

#[derive(Debug, Serialize)]
pub struct QualityWindow {
    pub mean_top_score: f64,
    pub empty_rate: f64,
    pub low_conf_rate: f64,
    pub helpful_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub daily: Vec<DailyPoint>,
}

#[derive(Debug, Serialize)]
pub struct DailyPoint {
    pub date: String,
    pub searches: u64,
    pub tokens_saved: u64,
    pub mean_savings_ratio: f64,
    pub mean_top_score: f64,
    pub empty_rate: f64,
    pub low_conf_rate: f64,
    pub helpful: u64,
    pub not_helpful: u64,
}

#[derive(Debug, Serialize)]
pub struct RecentSearch {
    pub ts: String,
    pub id: String,
    pub query: String,
    pub result_count: u64,
    pub tokens_saved: u64,
    pub savings_ratio: f64,
    pub top_score: f64,
    pub empty: bool,
    pub feedback: String,
    /// The event's `source` tag — `"cli"` or `"mcp"` (v2.8, additive): lets the
    /// recent view (dashboard + `cce usage`) label each query agent-vs-human.
    pub source: String,
}

/// Round to 2 decimals, round-half-away-from-zero (for the USD cost estimate).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Direction of a delta where higher-is-better: "up" (improving), "down"
/// (degrading), or "flat" within `DIRECTION_EPSILON`.
pub fn direction(delta: f64) -> &'static str {
    if delta > DIRECTION_EPSILON {
        "up"
    } else if delta < -DIRECTION_EPSILON {
        "down"
    } else {
        "flat"
    }
}

/// Mean of `xs`, or 0.0 when empty.
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

/// `helpful / (helpful + not_helpful)`, or `None` when there is no feedback.
fn helpful_rate(helpful: u64, not_helpful: u64) -> Option<f64> {
    let total = helpful + not_helpful;
    if total == 0 {
        None
    } else {
        Some(round6(helpful as f64 / total as f64))
    }
}

/// Aggregate the parsed events as of the instant `now_secs` (epoch seconds),
/// pricing token savings at `price` USD per 1M input tokens. Pure (DASHBOARD-SPEC §4).
pub fn aggregate(events: &[Event], now_secs: i64, price: f64) -> Aggregate {
    // Partition, preserving file order and each event's index (used for the
    // "latest wins" feedback resolution and the recent-searches tie-break).
    let mut searches: Vec<(usize, &SearchEvent)> = Vec::new();
    let mut feedback: Vec<(usize, &FeedbackEvent)> = Vec::new();
    let mut indexes: Vec<(usize, &IndexEvent)> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::Search(s) => searches.push((i, s)),
            Event::Feedback(f) => feedback.push((i, f)),
            Event::Index(x) => indexes.push((i, x)),
            Event::Unknown => {}
        }
    }

    let totals = compute_totals(&searches, &feedback, indexes.len() as u64, price);
    let north_star = NorthStar {
        savings: compute_savings(&searches, now_secs),
        quality: compute_quality(&searches, &feedback, now_secs),
    };
    let series = Series { daily: compute_daily(&searches, &feedback) };
    let recent_searches = compute_recent(&searches, &feedback);

    Aggregate {
        schema: crate::config::METRICS_SCHEMA.to_string(),
        totals,
        north_star,
        by_source: compute_usage_by_source(&searches),
        secret_safety: compute_secret_safety(&indexes),
        index_freshness: compute_index_freshness(&indexes),
        savings_by_layer: sum_by_layer(searches.iter().map(|(_, s)| &s.savings)),
        series,
        recent_searches,
    }
}

/// `is_mcp` for a search's `source` tag: only the exact `"mcp"` value counts as an
/// agent search; every other value (`"cli"`, empty, unknown) is a human search.
fn is_mcp(source: &str) -> bool {
    source == "mcp"
}

/// The lifetime mean of `top_score` over non-empty searches in `searches` (0.0 if
/// none). Shared by totals and the per-source split.
fn mean_top_score_of(searches: &[&SearchEvent]) -> f64 {
    let scores: Vec<f64> =
        searches.iter().filter(|s| s.result_count > 0).map(|s| s.top_score).collect();
    round6(mean(&scores))
}

/// Split searches into the CLI (human) and MCP (agent) buckets (v2.4.1).
fn compute_usage_by_source(searches: &[(usize, &SearchEvent)]) -> UsageBySource {
    let mcp: Vec<&SearchEvent> =
        searches.iter().filter(|(_, s)| is_mcp(&s.source)).map(|(_, s)| *s).collect();
    let cli: Vec<&SearchEvent> =
        searches.iter().filter(|(_, s)| !is_mcp(&s.source)).map(|(_, s)| *s).collect();
    UsageBySource { cli: source_usage(&cli), mcp: source_usage(&mcp) }
}

/// The usage figures for one source bucket.
fn source_usage(searches: &[&SearchEvent]) -> SourceUsage {
    // Saturating so a corrupt/forged tokens_saved (e.g. u64::MAX) clamps instead of
    // overflow-panicking (debug) or wrapping to garbage (release) — see #127.
    let tokens_saved: u64 = searches.iter().map(|s| s.tokens_saved).fold(0u64, u64::saturating_add);
    let ratios: Vec<f64> = searches.iter().map(|s| s.savings_ratio).collect();
    let latencies: Vec<f64> = searches.iter().map(|s| s.latency_ms).collect();
    SourceUsage {
        searches: searches.len() as u64,
        tokens_saved,
        mean_savings_ratio: round6(mean(&ratios)),
        mean_top_score: mean_top_score_of(searches),
        mean_latency_ms: round6(mean(&latencies)),
    }
}

/// Sum the sensitive-files-skipped over the log's index runs (v2.4.1).
fn compute_secret_safety(indexes: &[(usize, &IndexEvent)]) -> SecretSafety {
    SecretSafety {
        sensitive_skipped: indexes
            .iter()
            .map(|(_, x)| x.sensitive_skipped)
            .fold(0u64, u64::saturating_add),
        index_runs: indexes.len() as u64,
    }
}

/// Freshness from the latest index event (by ts, then file order). Log-derived and
/// pure; the API edge layers the live remote comparison on top (v2.4.1).
fn compute_index_freshness(indexes: &[(usize, &IndexEvent)]) -> IndexFreshness {
    // Latest = greatest (secs, file-index); ties resolve to later in the file.
    let latest =
        indexes.iter().max_by(|a, b| (a.1.secs, a.0).cmp(&(b.1.secs, b.0))).map(|(_, x)| *x);
    IndexFreshness {
        indexes: indexes.len() as u64,
        source: latest.map(|x| x.source.clone()),
        sha: latest.and_then(|x| x.sha.clone()),
        indexed_ts: latest.map(|x| x.ts.clone()),
    }
}

fn compute_totals(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
    index_count: u64,
    price: f64,
) -> Totals {
    // Saturating roll-up: a forged/corrupt tokens_saved cannot panic (debug) or
    // wrap to garbage (release), matching savings.rs's saturating policy — see #127.
    let tokens_saved: u64 =
        searches.iter().map(|(_, s)| s.tokens_saved).fold(0u64, u64::saturating_add);
    let ratios: Vec<f64> = searches.iter().map(|(_, s)| s.savings_ratio).collect();
    let all: Vec<&SearchEvent> = searches.iter().map(|(_, s)| *s).collect();
    let helpful = feedback.iter().filter(|(_, f)| f.helpful).count() as u64;
    let not_helpful = feedback.iter().filter(|(_, f)| !f.helpful).count() as u64;
    Totals {
        searches: searches.len() as u64,
        indexes: index_count,
        feedback: feedback.len() as u64,
        tokens_saved,
        cost_saved_usd: round2(tokens_saved as f64 / 1_000_000.0 * price),
        mean_savings_ratio: round6(mean(&ratios)),
        mean_top_score: mean_top_score_of(&all),
        helpful,
        not_helpful,
        helpful_rate: helpful_rate(helpful, not_helpful),
    }
}

/// A half-open window `[start, end)` in epoch seconds.
struct Window {
    start: i64,
    end: i64,
}

impl Window {
    fn contains(&self, secs: i64) -> bool {
        secs >= self.start && secs < self.end
    }
}

/// Current window `[now-7d, now)` and prior window `[now-14d, now-7d)`.
fn windows(now_secs: i64) -> (Window, Window) {
    let w = TREND_WINDOW_DAYS * DAY_SECS;
    (
        Window { start: now_secs - w, end: now_secs },
        Window { start: now_secs - 2 * w, end: now_secs - w },
    )
}

fn compute_savings(searches: &[(usize, &SearchEvent)], now_secs: i64) -> Savings {
    let (cur, prior) = windows(now_secs);
    let current = savings_window(searches, &cur);
    let prior = savings_window(searches, &prior);
    // Delta of the (rounded) mean savings ratios; both languages round identically.
    let delta = round6(current.mean_savings_ratio - prior.mean_savings_ratio);
    Savings { current, prior, delta_ratio: delta, direction: direction(delta).to_string() }
}

fn savings_window(searches: &[(usize, &SearchEvent)], win: &Window) -> SavingsWindow {
    let in_win: Vec<&SearchEvent> =
        searches.iter().filter(|(_, s)| win.contains(s.secs)).map(|(_, s)| *s).collect();
    let tokens_saved: u64 = in_win.iter().map(|s| s.tokens_saved).fold(0u64, u64::saturating_add);
    let ratios: Vec<f64> = in_win.iter().map(|s| s.savings_ratio).collect();
    SavingsWindow {
        searches: in_win.len() as u64,
        tokens_saved,
        mean_savings_ratio: round6(mean(&ratios)),
    }
}

fn compute_quality(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
    now_secs: i64,
) -> Quality {
    let (cur, prior) = windows(now_secs);
    let current = quality_window(searches, feedback, &cur);
    let prior = quality_window(searches, feedback, &prior);
    let delta = round6(current.mean_top_score - prior.mean_top_score);
    Quality { current, prior, delta_top_score: delta, direction: direction(delta).to_string() }
}

fn quality_window(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
    win: &Window,
) -> QualityWindow {
    let in_win: Vec<&SearchEvent> =
        searches.iter().filter(|(_, s)| win.contains(s.secs)).map(|(_, s)| *s).collect();
    let total = in_win.len() as f64;
    // mean_top_score over NON-EMPTY searches (result_count > 0).
    let top_scores: Vec<f64> =
        in_win.iter().filter(|s| s.result_count > 0).map(|s| s.top_score).collect();
    let empty = in_win.iter().filter(|s| s.empty).count() as f64;
    let low_conf = in_win.iter().filter(|s| s.low_confidence).count() as f64;
    let (empty_rate, low_conf_rate) = if total > 0.0 {
        (round6(empty / total), round6(low_conf / total))
    } else {
        (0.0, 0.0)
    };
    let helpful = feedback.iter().filter(|(_, f)| win.contains(f.secs) && f.helpful).count() as u64;
    let not_helpful =
        feedback.iter().filter(|(_, f)| win.contains(f.secs) && !f.helpful).count() as u64;
    QualityWindow {
        mean_top_score: round6(mean(&top_scores)),
        empty_rate,
        low_conf_rate,
        helpful_rate: helpful_rate(helpful, not_helpful),
    }
}

/// One day's running accumulator over its searches and feedback.
#[derive(Default)]
struct DayAcc {
    searches: u64,
    tokens_saved: u64,
    ratios: Vec<f64>,
    top_scores: Vec<f64>, // non-empty only
    empty: u64,
    low_conf: u64,
    helpful: u64,
    not_helpful: u64,
}

fn compute_daily(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
) -> Vec<DailyPoint> {
    // BTreeMap keyed by date gives ascending order. A date appears iff it has at
    // least one search or feedback event (index-only days do not appear).
    let mut by_day: BTreeMap<String, DayAcc> = BTreeMap::new();
    for (_, s) in searches {
        let acc = by_day.entry(date_str(s.secs)).or_default();
        acc.searches += 1;
        acc.tokens_saved = acc.tokens_saved.saturating_add(s.tokens_saved);
        acc.ratios.push(s.savings_ratio);
        if s.result_count > 0 {
            acc.top_scores.push(s.top_score);
        }
        if s.empty {
            acc.empty += 1;
        }
        if s.low_confidence {
            acc.low_conf += 1;
        }
    }
    for (_, f) in feedback {
        let acc = by_day.entry(date_str(f.secs)).or_default();
        if f.helpful {
            acc.helpful += 1;
        } else {
            acc.not_helpful += 1;
        }
    }
    by_day
        .into_iter()
        .map(|(date, a)| {
            let total = a.searches as f64;
            let (empty_rate, low_conf_rate) = if total > 0.0 {
                (round6(a.empty as f64 / total), round6(a.low_conf as f64 / total))
            } else {
                (0.0, 0.0)
            };
            DailyPoint {
                date,
                searches: a.searches,
                tokens_saved: a.tokens_saved,
                mean_savings_ratio: round6(mean(&a.ratios)),
                mean_top_score: round6(mean(&a.top_scores)),
                empty_rate,
                low_conf_rate,
                helpful: a.helpful,
                not_helpful: a.not_helpful,
            }
        })
        .collect()
}

fn compute_recent(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
) -> Vec<RecentSearch> {
    // Resolve each search id to its latest feedback (latest ts, then latest in
    // file order wins).
    let mut latest_fb: BTreeMap<&str, (i64, usize, bool)> = BTreeMap::new();
    for (i, f) in feedback {
        let key = f.target_id.as_str();
        let candidate = (f.secs, *i, f.helpful);
        match latest_fb.get(key) {
            Some(&(s, li, _)) if (s, li) >= (candidate.0, candidate.1) => {}
            _ => {
                latest_fb.insert(key, candidate);
            }
        }
    }

    // Newest first: by ts descending, then later-in-file first for ties.
    let mut ordered: Vec<(usize, &SearchEvent)> = searches.to_vec();
    ordered.sort_by(|a, b| b.1.secs.cmp(&a.1.secs).then(b.0.cmp(&a.0)));

    ordered
        .into_iter()
        .take(RECENT_SEARCHES_LIMIT)
        .map(|(_, s)| {
            let feedback = match latest_fb.get(s.id.as_str()) {
                Some((_, _, true)) => "helpful",
                Some((_, _, false)) => "not_helpful",
                None => "none",
            };
            RecentSearch {
                ts: s.ts.clone(),
                id: s.id.clone(),
                query: s.query.clone(),
                result_count: s.result_count,
                tokens_saved: s.tokens_saved,
                savings_ratio: round6(s.savings_ratio),
                top_score: round6(s.top_score),
                empty: s.empty,
                feedback: feedback.to_string(),
                source: s.source.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{parse_iso, parse_log};
    use std::path::PathBuf;

    fn sample_log() -> Vec<Event> {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/fixture/base/metrics_sample.jsonl"
        ));
        let text = std::fs::read_to_string(path).unwrap();
        parse_log(&text).events
    }

    fn now() -> i64 {
        parse_iso("2026-07-05T00:00:00Z").unwrap()
    }

    #[test]
    fn direction_rule() {
        assert_eq!(direction(0.01), "up");
        assert_eq!(direction(-0.01), "down");
        assert_eq!(direction(0.0), "flat");
        assert_eq!(direction(1e-12), "flat"); // within epsilon
    }

    #[test]
    fn adversarial_tokens_saved_saturates_instead_of_overflowing() {
        // #127: two search events whose tokens_saved sum past u64::MAX must clamp to
        // u64::MAX, not overflow-panic (debug) or wrap to garbage (release). The
        // assertion pins the graceful SATURATED result, so it holds in BOTH profiles.
        let line = |id: &str, ts: &str| {
            format!(
                "{{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"{ts}\",\
                 \"id\":\"{id}\",\"query\":\"q\",\"result_count\":1,\
                 \"tokens_saved\":18446744073709551615,\"savings_ratio\":0.5,\
                 \"top_score\":0.9,\"empty\":false,\"low_confidence\":false}}"
            )
        };
        let text = format!(
            "{}\n{}\n",
            line("aaaaaaaaaaaa", "2026-07-04T10:00:00Z"),
            line("bbbbbbbbbbbb", "2026-07-04T11:00:00Z")
        );
        let events = parse_log(&text).events;
        let agg = aggregate(&events, now(), 3.00);
        // Totals, per-source, per-window, and daily roll-ups all clamp at u64::MAX.
        assert_eq!(agg.totals.tokens_saved, u64::MAX);
        assert_eq!(agg.by_source.cli.tokens_saved, u64::MAX);
        assert_eq!(agg.north_star.savings.current.tokens_saved, u64::MAX);
        let day = agg.series.daily.iter().find(|d| d.date == "2026-07-04").unwrap();
        assert_eq!(day.tokens_saved, u64::MAX);
    }

    #[test]
    fn anchor_totals() {
        let agg = aggregate(&sample_log(), now(), 3.00);
        let t = &agg.totals;
        assert_eq!(t.searches, 4);
        assert_eq!(t.indexes, 1);
        assert_eq!(t.feedback, 2);
        assert_eq!(t.tokens_saved, 53000);
        assert_eq!(t.cost_saved_usd, 0.16);
        assert_eq!(t.mean_savings_ratio, 0.525000);
        assert_eq!(t.helpful, 1);
        assert_eq!(t.not_helpful, 1);
        assert_eq!(t.helpful_rate, Some(0.500000));
    }

    #[test]
    fn anchor_savings_north_star() {
        let agg = aggregate(&sample_log(), now(), 3.00);
        let s = &agg.north_star.savings;
        assert_eq!(s.current.searches, 3);
        assert_eq!(s.current.tokens_saved, 48000);
        assert_eq!(s.current.mean_savings_ratio, 0.533333);
        assert_eq!(s.prior.searches, 1);
        assert_eq!(s.prior.tokens_saved, 5000);
        assert_eq!(s.prior.mean_savings_ratio, 0.500000);
        assert_eq!(s.delta_ratio, 0.033333);
        assert_eq!(s.direction, "up");
    }

    #[test]
    fn anchor_quality_north_star() {
        let agg = aggregate(&sample_log(), now(), 3.00);
        let q = &agg.north_star.quality;
        assert_eq!(q.current.mean_top_score, 0.750000);
        assert_eq!(q.current.empty_rate, 0.333333);
        assert_eq!(q.current.low_conf_rate, 0.000000);
        assert_eq!(q.current.helpful_rate, Some(0.500000));
        assert_eq!(q.prior.mean_top_score, 0.400000);
        assert_eq!(q.prior.empty_rate, 0.000000);
        assert_eq!(q.prior.low_conf_rate, 0.000000);
        assert_eq!(q.prior.helpful_rate, None);
        assert_eq!(q.delta_top_score, 0.350000);
        assert_eq!(q.direction, "up");
    }

    #[test]
    fn anchor_daily_series() {
        let agg = aggregate(&sample_log(), now(), 3.00);
        let dates: Vec<&str> = agg.series.daily.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, vec!["2026-06-25", "2026-07-01", "2026-07-02", "2026-07-03"]);

        let d0702 = agg.series.daily.iter().find(|d| d.date == "2026-07-02").unwrap();
        assert_eq!(d0702.searches, 2);
        assert_eq!(d0702.empty_rate, 0.500000);
        assert_eq!(d0702.helpful, 1);

        let d0703 = agg.series.daily.iter().find(|d| d.date == "2026-07-03").unwrap();
        assert_eq!(d0703.searches, 0);
        assert_eq!(d0703.not_helpful, 1);
    }

    #[test]
    fn recent_searches_newest_first_with_feedback_resolved() {
        let agg = aggregate(&sample_log(), now(), 3.00);
        let r = &agg.recent_searches;
        assert_eq!(r.len(), 4);
        // Newest first: 2026-07-02T11:00 (cccc), then 07-02T10:00 (bbbb), ...
        assert_eq!(r[0].id, "cccccccccccc");
        assert_eq!(r[0].feedback, "none");
        assert!(r[0].empty);
        // bbbb had a not_helpful feedback.
        let bbbb = r.iter().find(|x| x.id == "bbbbbbbbbbbb").unwrap();
        assert_eq!(bbbb.feedback, "not_helpful");
        // aaaa had a helpful feedback.
        let aaaa = r.iter().find(|x| x.id == "aaaaaaaaaaaa").unwrap();
        assert_eq!(aaaa.feedback, "helpful");
        // Oldest (dddd) is last.
        assert_eq!(r[3].id, "dddddddddddd");
    }

    #[test]
    fn empty_log_is_a_valid_no_data_aggregate() {
        let agg = aggregate(&[], now(), 3.00);
        assert_eq!(agg.totals.searches, 0);
        assert_eq!(agg.totals.tokens_saved, 0);
        assert_eq!(agg.totals.cost_saved_usd, 0.0);
        assert_eq!(agg.totals.mean_savings_ratio, 0.0);
        assert_eq!(agg.totals.mean_top_score, 0.0);
        assert_eq!(agg.totals.helpful_rate, None);
        assert_eq!(agg.north_star.savings.direction, "flat");
        assert_eq!(agg.north_star.quality.direction, "flat");
        // v2.4.1 panels degrade gracefully to a clean zero/none state.
        assert_eq!(agg.by_source.cli.searches, 0);
        assert_eq!(agg.by_source.mcp.searches, 0);
        assert_eq!(agg.secret_safety.sensitive_skipped, 0);
        assert_eq!(agg.secret_safety.index_runs, 0);
        assert_eq!(agg.index_freshness.indexes, 0);
        assert_eq!(agg.index_freshness.source, None);
        assert_eq!(agg.index_freshness.sha, None);
        assert!(agg.series.daily.is_empty());
        assert!(agg.recent_searches.is_empty());
    }

    #[test]
    fn anchor_mean_top_score_and_source_split() {
        // The §4.1 anchor has no `source` on any search, so every search is CLI.
        let agg = aggregate(&sample_log(), now(), 3.00);
        // Lifetime mean top score over non-empty searches (0.9, 0.6, 0.4).
        assert_eq!(agg.totals.mean_top_score, 0.633333);
        // Agent-vs-human split: all four are CLI; none are MCP.
        assert_eq!(agg.by_source.cli.searches, 4);
        assert_eq!(agg.by_source.cli.tokens_saved, 53000);
        assert_eq!(agg.by_source.cli.mean_savings_ratio, 0.525000);
        assert_eq!(agg.by_source.cli.mean_top_score, 0.633333);
        assert_eq!(agg.by_source.mcp.searches, 0);
        assert_eq!(agg.by_source.mcp.tokens_saved, 0);
        assert_eq!(agg.by_source.mcp.mean_savings_ratio, 0.0);
        assert_eq!(agg.by_source.mcp.mean_top_score, 0.0);
        // The anchor's single index event has no sensitive_skipped (→ 0), source
        // defaults to "local", and no sha.
        assert_eq!(agg.secret_safety.sensitive_skipped, 0);
        assert_eq!(agg.secret_safety.index_runs, 1);
        assert_eq!(agg.index_freshness.indexes, 1);
        assert_eq!(agg.index_freshness.source.as_deref(), Some("local"));
        assert_eq!(agg.index_freshness.sha, None);
        assert_eq!(agg.index_freshness.indexed_ts.as_deref(), Some("2026-07-01T09:00:00Z"));
    }

    #[test]
    fn anchor_savings_by_layer_from_pre_2_5_log() {
        // The §4.1 anchor log predates v2.5 (no `savings` object), so every search's
        // retrieval bucket is reconstructed from its top-level tokens_saved/baseline.
        let agg = aggregate(&sample_log(), now(), 3.00);
        let s = &agg.savings_by_layer;
        assert_eq!(s.retrieval.saved_tokens, 53000);
        assert_eq!(s.retrieval.baseline_tokens, 70000);
        // The six unbuilt layers ship present-and-zero.
        assert_eq!(s.chunk_compression, crate::savings::Bucket::default());
        assert_eq!(s.progressive_disclosure, crate::savings::Bucket::default());
        // Total equals retrieval (the only populated bucket) and the honesty note is set.
        assert_eq!(s.total.saved_tokens, 53000);
        assert_eq!(s.total.baseline_tokens, 70000);
        assert_eq!(s.note, crate::savings::SAVINGS_NOTE);
    }

    #[test]
    fn savings_by_layer_reads_a_v2_5_savings_object() {
        // A v2.5 search event carrying an explicit `savings` object.
        let text = "{\"event\":\"search\",\"ts\":\"2026-07-02T10:00:00Z\",\"id\":\"s0\",\"result_count\":2,\"tokens_saved\":1,\"baseline_tokens\":2,\"savings_ratio\":0.5,\"top_score\":0.8,\"empty\":false,\"low_confidence\":false,\"source\":\"mcp\",\"savings\":{\"retrieval\":{\"saved_tokens\":900,\"baseline_tokens\":1000}}}\n";
        let events = parse_log(text).events;
        let agg = aggregate(&events, now(), 3.00);
        // The object's retrieval bucket wins over the legacy top-level fields.
        assert_eq!(agg.savings_by_layer.retrieval.saved_tokens, 900);
        assert_eq!(agg.savings_by_layer.retrieval.baseline_tokens, 1000);
    }

    #[test]
    fn mcp_searches_and_index_freshness_split_out() {
        // A synthetic log: one CLI search, one MCP search, and two index events
        // whose latest carries a sha + a sensitive_skipped count.
        let text = concat!(
            "{\"event\":\"index\",\"ts\":\"2026-07-01T08:00:00Z\",\"id\":\"i0\",\"files_indexed\":5,\"chunks\":9,\"index_bytes\":1,\"duration_ms\":1.0,\"embedder\":\"hash\",\"full\":true,\"sha\":\"aaaa1111\",\"source\":\"local\",\"sensitive_skipped\":1}\n",
            "{\"event\":\"index\",\"ts\":\"2026-07-02T08:00:00Z\",\"id\":\"i1\",\"files_indexed\":5,\"chunks\":9,\"index_bytes\":1,\"duration_ms\":1.0,\"embedder\":\"hash\",\"full\":true,\"sha\":\"bbbb2222\",\"source\":\"local\",\"sensitive_skipped\":3}\n",
            "{\"event\":\"search\",\"ts\":\"2026-07-02T10:00:00Z\",\"id\":\"s0\",\"result_count\":2,\"tokens_saved\":100,\"savings_ratio\":0.5,\"top_score\":0.8,\"empty\":false,\"low_confidence\":false,\"source\":\"cli\"}\n",
            "{\"event\":\"search\",\"ts\":\"2026-07-02T11:00:00Z\",\"id\":\"s1\",\"result_count\":3,\"tokens_saved\":300,\"savings_ratio\":0.75,\"top_score\":0.9,\"empty\":false,\"low_confidence\":false,\"source\":\"mcp\"}\n"
        );
        let events = parse_log(text).events;
        let agg = aggregate(&events, now(), 3.00);
        assert_eq!(agg.by_source.cli.searches, 1);
        assert_eq!(agg.by_source.cli.tokens_saved, 100);
        assert_eq!(agg.by_source.mcp.searches, 1);
        assert_eq!(agg.by_source.mcp.tokens_saved, 300);
        assert_eq!(agg.by_source.mcp.mean_savings_ratio, 0.750000);
        assert_eq!(agg.by_source.mcp.mean_top_score, 0.900000);
        // Secret-safety sums both runs (1 + 3).
        assert_eq!(agg.secret_safety.sensitive_skipped, 4);
        assert_eq!(agg.secret_safety.index_runs, 2);
        // Freshness reflects the LATEST index event.
        assert_eq!(agg.index_freshness.sha.as_deref(), Some("bbbb2222"));
        assert_eq!(agg.index_freshness.indexed_ts.as_deref(), Some("2026-07-02T08:00:00Z"));
    }
}
