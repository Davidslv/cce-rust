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
use crate::metrics::{date_str, Event, FeedbackEvent, SearchEvent};
use serde::Serialize;
use std::collections::BTreeMap;

const DAY_SECS: i64 = 86_400;

/// The aggregate, minus `generated_ts` (added by the API at serialization time).
#[derive(Debug, Serialize)]
pub struct Aggregate {
    pub schema: String,
    pub totals: Totals,
    pub north_star: NorthStar,
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
    pub helpful: u64,
    pub not_helpful: u64,
    pub helpful_rate: Option<f64>,
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
    let mut index_count: u64 = 0;
    for (i, e) in events.iter().enumerate() {
        match e {
            Event::Search(s) => searches.push((i, s)),
            Event::Feedback(f) => feedback.push((i, f)),
            Event::Index(_) => index_count += 1,
            Event::Unknown => {}
        }
    }

    let totals = compute_totals(&searches, &feedback, index_count, price);
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
        series,
        recent_searches,
    }
}

fn compute_totals(
    searches: &[(usize, &SearchEvent)],
    feedback: &[(usize, &FeedbackEvent)],
    index_count: u64,
    price: f64,
) -> Totals {
    let tokens_saved: u64 = searches.iter().map(|(_, s)| s.tokens_saved).sum();
    let ratios: Vec<f64> = searches.iter().map(|(_, s)| s.savings_ratio).collect();
    let helpful = feedback.iter().filter(|(_, f)| f.helpful).count() as u64;
    let not_helpful = feedback.iter().filter(|(_, f)| !f.helpful).count() as u64;
    Totals {
        searches: searches.len() as u64,
        indexes: index_count,
        feedback: feedback.len() as u64,
        tokens_saved,
        cost_saved_usd: round2(tokens_saved as f64 / 1_000_000.0 * price),
        mean_savings_ratio: round6(mean(&ratios)),
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
    let tokens_saved: u64 = in_win.iter().map(|s| s.tokens_saved).sum();
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
        acc.tokens_saved += s.tokens_saved;
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
            "/test/fixture/metrics_sample.jsonl"
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
        assert_eq!(agg.totals.helpful_rate, None);
        assert_eq!(agg.north_star.savings.direction, "flat");
        assert_eq!(agg.north_star.quality.direction, "flat");
        assert!(agg.series.daily.is_empty());
        assert!(agg.recent_searches.is_empty());
    }
}
