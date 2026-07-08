//! # usage — the `cce usage` projection (SPEC-USAGE-VISIBILITY §2, v2.8)
//!
//! **Why this file exists:** the metrics log already records who used CCE (the
//! v2.4.1 `source` tag) and what it saved, but until v2.8 that answer surfaced
//! only through the browser dashboard. `cce usage` is the one-shot, CI-friendly
//! terminal counterpart: the agent-vs-human split, the window totals, and the
//! recent queries — as a byte-pinned human block or a versioned `cce.usage/v1`
//! JSON projection.
//!
//! **What it is / does:** PURE PROJECTION — zero new accounting. Both renders are
//! re-shapes of the SAME aggregate the dashboard serves (`aggregator::aggregate`
//! single-repo; `federation::federated_metrics_json_since` for `--workspace`,
//! including the issue-#28 workspace-root log rule), so `cce usage` and
//! `cce dashboard` report identical numbers from the same log. The only pre-step
//! is the `--since` window filter, applied to the parsed events BEFORE
//! aggregation. `now` is injected everywhere (wall clock only at the CLI edge).
//!
//! **Responsibilities:**
//! - Own `--since` parsing (ISO instant/date or a relative `90m|24h|7d|4w`), the
//!   pre-aggregation event filter, and the `--source` display filter.
//! - Own the two byte-pinned renders (human + `cce.usage/v1`), projecting from
//!   the aggregate's serialized JSON value so the shapes stay pinned to
//!   `/api/metrics` where they overlap.
//! - It does NOT aggregate (that is `aggregator`/`federation`), read the clock,
//!   or write anything: deterministic, offline, read-only.

use crate::metrics::{format_iso, parse_iso, Event};
use serde::Serialize;
use serde_json::Value;

/// The schema tag stamped on the `cce usage --json` body.
pub const USAGE_SCHEMA: &str = "cce.usage/v1";

/// How many recent queries the human render shows before the byte-pinned
/// `… (+N more; --json for all)` elision line. The JSON always carries the full
/// aggregate recent list (`RECENT_SEARCHES_LIMIT`).
pub const USAGE_RECENT_HUMAN_LIMIT: usize = 10;

/// The longest query the human recent column prints; a longer query is cut on a
/// char boundary and a single U+2026 ellipsis appended (inside the quotes).
pub const USAGE_QUERY_MAX_CHARS: usize = 44;

/// A resolved `--since` window start: the cutoff instant plus the label the
/// human header prints (`last 24h (since …)` for a relative spec, `since …` for
/// an ISO one). Pure value — derived from the injected `now`, never the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinceCut {
    /// Events with `ts < cutoff_secs` are dropped before aggregation.
    pub cutoff_secs: i64,
    /// The normalized relative spec (`"24h"`) when one was given, else `None`.
    pub relative: Option<String>,
}

impl SinceCut {
    /// The cutoff as an ISO-8601 UTC instant.
    pub fn cutoff_iso(&self) -> String {
        format_iso(self.cutoff_secs)
    }
}

/// Parse a `--since` value against the injected `now` (epoch seconds). Accepts a
/// relative duration (`90m`, `24h`, `7d`, `4w`) or an ISO instant/date
/// (`2026-07-01T09:00:00Z`, `2026-07-01` = midnight UTC). Anything else is a
/// clear error listing the accepted forms — never a silent all-time fallback.
pub fn parse_since(spec: &str, now_secs: i64) -> Result<SinceCut, String> {
    let s = spec.trim();
    let err = || {
        format!(
            "invalid --since value {spec:?} — use a relative duration (90m, 24h, 7d, 4w) or an \
             ISO UTC instant/date (2026-07-01T09:00:00Z, 2026-07-01)"
        )
    };
    if s.is_empty() {
        return Err(err());
    }
    // Relative form: digits + one unit suffix.
    let lower = s.to_ascii_lowercase();
    if let Some(unit) = lower.chars().last().filter(|c| matches!(c, 'm' | 'h' | 'd' | 'w')) {
        let digits = &lower[..lower.len() - 1];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = digits.parse().map_err(|_| err())?;
            if n == 0 {
                return Err(err());
            }
            let secs = match unit {
                'm' => n * 60,
                'h' => n * 3600,
                'd' => n * 86_400,
                _ => n * 7 * 86_400,
            };
            return Ok(SinceCut { cutoff_secs: now_secs - secs, relative: Some(lower) });
        }
    }
    // ISO instant (second precision) or bare date (midnight UTC).
    if let Some(secs) = parse_iso(s) {
        return Ok(SinceCut { cutoff_secs: secs, relative: None });
    }
    if s.len() == 10 {
        if let Some(secs) = parse_iso(&format!("{s}T00:00:00Z")) {
            return Ok(SinceCut { cutoff_secs: secs, relative: None });
        }
    }
    Err(err())
}

/// Drop every timed event with `ts < cutoff` (the `--since` pre-filter, applied
/// BEFORE aggregation). `Unknown` events carry no parsed instant and aggregate
/// to nothing, so they pass through untouched.
pub fn filter_since(events: Vec<Event>, cutoff_secs: i64) -> Vec<Event> {
    events
        .into_iter()
        .filter(|e| match e {
            Event::Search(s) => s.secs >= cutoff_secs,
            Event::Index(i) => i.secs >= cutoff_secs,
            Event::Feedback(f) => f.secs >= cutoff_secs,
            Event::Unknown => true,
        })
        .collect()
}

/// The `--source` display filter: which split leads the human render. It never
/// changes the aggregate — the JSON always carries both splits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Mcp,
    Cli,
}

impl SourceFilter {
    /// Parse the flag value (case-insensitive). Unknown ⇒ a clear error.
    pub fn parse(s: &str) -> Result<SourceFilter, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(SourceFilter::All),
            "mcp" => Ok(SourceFilter::Mcp),
            "cli" => Ok(SourceFilter::Cli),
            other => Err(format!("invalid --source value {other:?} — use mcp, cli, or all")),
        }
    }

    /// The canonical string form (echoed as `source_filter` in the JSON).
    pub const fn as_str(&self) -> &'static str {
        match self {
            SourceFilter::All => "all",
            SourceFilter::Mcp => "mcp",
            SourceFilter::Cli => "cli",
        }
    }

    /// Whether a search with `source` passes the display filter.
    fn shows(&self, source: &str) -> bool {
        match self {
            SourceFilter::All => true,
            SourceFilter::Mcp => source == "mcp",
            SourceFilter::Cli => source != "mcp",
        }
    }
}

/// Format `n` with byte-pinned thousands separators (`38628` → `"38,628"`).
pub fn fmt_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Byte-pinned short token count: `< 1_000` verbatim, `< 100_000` as one-decimal
/// `k` (`8600` → `"8.6k"`), else integer-`k` by floor division (`310_880` →
/// `"310k"`). Shared by the recent column and the footer's session clause.
pub fn fmt_tokens_short(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

// --- projection helpers over the aggregate's serialized JSON value ---

fn get_u64(v: &Value, path: &[&str]) -> u64 {
    path.iter().fold(v, |acc, k| &acc[*k]).as_u64().unwrap_or(0)
}

fn get_f64(v: &Value, path: &[&str]) -> f64 {
    path.iter().fold(v, |acc, k| &acc[*k]).as_f64().unwrap_or(0.0)
}

fn get_str<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    path.iter().fold(v, |acc, k| &acc[*k]).as_str().unwrap_or("")
}

/// One split line's numbers, lifted from the aggregate's `by_source.<tag>`.
struct SplitRow {
    label: &'static str,
    searches: u64,
    tokens_saved: u64,
    pct: i64,
    quality: f64,
    latency_ms: f64,
}

fn split_row(metrics: &Value, tag: &'static str, label: &'static str) -> SplitRow {
    SplitRow {
        label,
        searches: get_u64(metrics, &["by_source", tag, "searches"]),
        tokens_saved: get_u64(metrics, &["by_source", tag, "tokens_saved"]),
        pct: (get_f64(metrics, &["by_source", tag, "mean_savings_ratio"]) * 100.0).round() as i64,
        quality: get_f64(metrics, &["by_source", tag, "mean_top_score"]),
        latency_ms: get_f64(metrics, &["by_source", tag, "mean_latency_ms"]),
    }
}

/// The window line of the human header: `all time`, `last 24h (since …)`, or
/// `since …` — a pure function of the resolved `--since`.
pub fn window_label(since: Option<&SinceCut>) -> String {
    match since {
        None => "all time".to_string(),
        Some(c) => match &c.relative {
            Some(rel) => format!("last {rel} (since {})", c.cutoff_iso()),
            None => format!("since {}", c.cutoff_iso()),
        },
    }
}

/// Truncate a query for the recent column: at most [`USAGE_QUERY_MAX_CHARS`]
/// chars, a longer one cut on a char boundary with a single trailing `…`.
fn short_query(q: &str) -> String {
    if q.chars().count() <= USAGE_QUERY_MAX_CHARS {
        return q.to_string();
    }
    let cut: String = q.chars().take(USAGE_QUERY_MAX_CHARS - 1).collect();
    format!("{cut}…")
}

/// The `HH:MM` of an ISO instant (`2026-07-05T09:58:12Z` → `09:58`); a malformed
/// timestamp renders as `--:--` rather than crashing.
fn hh_mm(ts: &str) -> String {
    ts.get(11..16).map(str::to_string).unwrap_or_else(|| "--:--".to_string())
}

/// Render the byte-pinned human block from the aggregate's serialized value.
/// `metrics` is the dashboard aggregate (single-repo `aggregate`, or the
/// federated roll-up with `by_package`); `workspace` adds the by-package table.
pub fn render_human(
    metrics: &Value,
    since: Option<&SinceCut>,
    source: SourceFilter,
    workspace: bool,
) -> String {
    let mut out = format!("CCE usage — {}\n", window_label(since));
    if get_u64(metrics, &["totals", "searches"]) == 0 {
        out.push_str("  no searches in this window\n");
        return out;
    }

    // The agent/human split (both lines under `all`; one under a narrowed filter).
    let rows: Vec<SplitRow> = [
        (SourceFilter::Mcp, split_row(metrics, "mcp", "agent (mcp)")),
        (SourceFilter::Cli, split_row(metrics, "cli", "human (cli)")),
    ]
    .into_iter()
    .filter(|(tag, _)| source == SourceFilter::All || source == *tag)
    .map(|(_, r)| r)
    .collect();
    let sw = rows.iter().map(|r| r.searches.to_string().len()).max().unwrap_or(1);
    let tw = rows.iter().map(|r| fmt_thousands(r.tokens_saved).len()).max().unwrap_or(1);
    for r in &rows {
        out.push_str(&format!(
            "  {} : {:>sw$} searches · saved ~{:>tw$} tok ({}%) · quality {:.2} · {:.0} ms avg\n",
            r.label,
            r.searches,
            fmt_thousands(r.tokens_saved),
            r.pct,
            r.quality,
            r.latency_ms,
        ));
    }

    // Workspace: the by-package mini-table (members only — federated agent
    // searches from the root log count in the split, not here; the #28 rule).
    if workspace {
        if let Some(pkgs) = metrics["by_package"].as_array() {
            if !pkgs.is_empty() {
                out.push_str("  by package\n");
                let pw =
                    pkgs.iter().map(|p| get_str(p, &["package"]).len()).max().unwrap_or(1);
                let psw = pkgs
                    .iter()
                    .map(|p| get_u64(p, &["searches"]).to_string().len())
                    .max()
                    .unwrap_or(1);
                let ptw = pkgs
                    .iter()
                    .map(|p| fmt_thousands(get_u64(p, &["tokens_saved"])).len())
                    .max()
                    .unwrap_or(1);
                for p in pkgs {
                    let pct =
                        (get_f64(p, &["mean_savings_ratio"]) * 100.0).round() as i64;
                    out.push_str(&format!(
                        "    {:<pw$} : {:>psw$} searches · saved ~{:>ptw$} tok ({}%) · quality {:.2}\n",
                        get_str(p, &["package"]),
                        get_u64(p, &["searches"]),
                        fmt_thousands(get_u64(p, &["tokens_saved"])),
                        pct,
                        get_f64(p, &["mean_top_score"]),
                    ));
                }
            }
        }
    }

    // Recent queries, newest first (the aggregate's order), display-filtered.
    let recent: Vec<&Value> = metrics["recent_searches"]
        .as_array()
        .map(|a| a.iter().filter(|r| source.shows(get_str(r, &["source"]))).collect())
        .unwrap_or_default();
    if !recent.is_empty() {
        out.push_str("  recent (newest first)\n");
        let shown = &recent[..recent.len().min(USAGE_RECENT_HUMAN_LIMIT)];
        let qw = shown
            .iter()
            .map(|r| short_query(get_str(r, &["query"])).chars().count() + 2)
            .max()
            .unwrap_or(2);
        let hw =
            shown.iter().map(|r| get_u64(r, &["result_count"]).to_string().len()).max().unwrap_or(1);
        for r in shown {
            let quoted = format!("\"{}\"", short_query(get_str(r, &["query"])));
            let pad = qw.saturating_sub(quoted.chars().count());
            out.push_str(&format!(
                "    {:<3}  {}  {}{}  {:>hw$} hits  ~{} saved\n",
                get_str(r, &["source"]),
                hh_mm(get_str(r, &["ts"])),
                quoted,
                " ".repeat(pad),
                get_u64(r, &["result_count"]),
                fmt_tokens_short(get_u64(r, &["tokens_saved"])),
            ));
        }
        if recent.len() > shown.len() {
            out.push_str(&format!("    … ({} more; --json for all)\n", recent.len() - shown.len()));
        }
    }
    out
}

// --- the cce.usage/v1 JSON projection ---

#[derive(Debug, Serialize)]
struct UsageJson {
    schema: String,
    generated_ts: String,
    window: WindowJson,
    source_filter: String,
    totals: TotalsJson,
    by_source: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    by_package: Option<Value>,
    recent: Vec<RecentJson>,
}

#[derive(Debug, Serialize)]
struct WindowJson {
    since: Option<String>,
    until: String,
}

#[derive(Debug, Serialize)]
struct TotalsJson {
    searches: u64,
    tokens_saved: u64,
    mean_savings_ratio: f64,
    mean_top_score: f64,
}

#[derive(Debug, Serialize)]
struct RecentJson {
    ts: String,
    source: String,
    query: String,
    result_count: u64,
    tokens_saved: u64,
}

/// Render the versioned `cce.usage/v1` body: a stable re-shape of the aggregate
/// (`totals`/`by_source`/`by_package` are lifted verbatim where they overlap
/// with `/api/metrics`, so a pipeline reading both never sees divergent
/// numbers). `generated_ts`/`window.until` carry the injected `now` and are the
/// only non-log-derived fields (excluded from byte-pins the same way
/// `/api/metrics.generated_ts` is). Ends with a newline.
pub fn render_json(
    metrics: &Value,
    now_secs: i64,
    since: Option<&SinceCut>,
    source: SourceFilter,
    workspace: bool,
) -> String {
    let recent: Vec<RecentJson> = metrics["recent_searches"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| RecentJson {
                    ts: get_str(r, &["ts"]).to_string(),
                    source: get_str(r, &["source"]).to_string(),
                    query: get_str(r, &["query"]).to_string(),
                    result_count: get_u64(r, &["result_count"]),
                    tokens_saved: get_u64(r, &["tokens_saved"]),
                })
                .collect()
        })
        .unwrap_or_default();
    let body = UsageJson {
        schema: USAGE_SCHEMA.to_string(),
        generated_ts: format_iso(now_secs),
        window: WindowJson {
            since: since.map(|c| c.cutoff_iso()),
            until: format_iso(now_secs),
        },
        source_filter: source.as_str().to_string(),
        totals: TotalsJson {
            searches: get_u64(metrics, &["totals", "searches"]),
            tokens_saved: get_u64(metrics, &["totals", "tokens_saved"]),
            mean_savings_ratio: get_f64(metrics, &["totals", "mean_savings_ratio"]),
            mean_top_score: get_f64(metrics, &["totals", "mean_top_score"]),
        },
        by_source: metrics["by_source"].clone(),
        by_package: if workspace { Some(metrics["by_package"].clone()) } else { None },
        recent,
    };
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::aggregate;
    use crate::metrics::parse_log;

    /// A pinned v2.4.1+ log: two agent (mcp) searches, one human (cli) search
    /// (one below any 24h cutoff), and an index event — with latencies.
    fn fixture_log() -> &'static str {
        concat!(
            "{\"schema\":\"cce.metrics/v1\",\"event\":\"index\",\"ts\":\"2026-07-01T08:00:00Z\",\"id\":\"i00000000000\",\"files_indexed\":3,\"chunks\":9,\"index_bytes\":1,\"duration_ms\":1.0,\"embedder\":\"hash\",\"full\":true,\"sha\":\"aaaa1111\",\"source\":\"local\",\"sensitive_skipped\":0}\n",
            "{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"2026-07-01T09:00:00Z\",\"id\":\"c00000000001\",\"query\":\"rrf fusion constant\",\"result_count\":3,\"tokens_saved\":2100,\"savings_ratio\":0.81,\"top_score\":0.74,\"empty\":false,\"low_confidence\":false,\"latency_ms\":12.0,\"source\":\"cli\"}\n",
            "{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"2026-07-05T09:52:00Z\",\"id\":\"a00000000001\",\"query\":\"where is the retry idempotency boundary\",\"result_count\":5,\"tokens_saved\":7900,\"savings_ratio\":0.86,\"top_score\":0.81,\"empty\":false,\"low_confidence\":false,\"latency_ms\":61.0,\"source\":\"mcp\"}\n",
            "{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"2026-07-05T09:58:00Z\",\"id\":\"a00000000002\",\"query\":\"how does the payment flow create a new case\",\"result_count\":5,\"tokens_saved\":8600,\"savings_ratio\":0.9,\"top_score\":0.77,\"empty\":false,\"low_confidence\":false,\"latency_ms\":55.0,\"source\":\"mcp\"}\n",
        )
    }

    fn now() -> i64 {
        parse_iso("2026-07-06T10:00:00Z").unwrap()
    }

    fn metrics_value(since: Option<&SinceCut>) -> Value {
        let mut events = parse_log(fixture_log()).events;
        if let Some(c) = since {
            events = filter_since(events, c.cutoff_secs);
        }
        serde_json::to_value(aggregate(&events, now(), 3.00)).unwrap()
    }

    // --- --since parsing ---

    #[test]
    fn parse_since_accepts_relative_durations() {
        let n = now();
        assert_eq!(parse_since("90m", n).unwrap().cutoff_secs, n - 90 * 60);
        assert_eq!(parse_since("24h", n).unwrap().cutoff_secs, n - 24 * 3600);
        assert_eq!(parse_since("7d", n).unwrap().cutoff_secs, n - 7 * 86_400);
        assert_eq!(parse_since("4w", n).unwrap().cutoff_secs, n - 28 * 86_400);
        // The label keeps the normalized spec for the header.
        assert_eq!(parse_since("24H", n).unwrap().relative.as_deref(), Some("24h"));
    }

    #[test]
    fn parse_since_accepts_iso_instant_and_date() {
        let n = now();
        let c = parse_since("2026-07-01T09:00:00Z", n).unwrap();
        assert_eq!(c.cutoff_secs, parse_iso("2026-07-01T09:00:00Z").unwrap());
        assert_eq!(c.relative, None);
        // A bare date is midnight UTC.
        let d = parse_since("2026-07-01", n).unwrap();
        assert_eq!(d.cutoff_secs, parse_iso("2026-07-01T00:00:00Z").unwrap());
    }

    #[test]
    fn parse_since_rejects_malformed_values_with_guidance() {
        for bad in ["", "yesterday", "24", "h", "0d", "-3d", "2026-13-40", "1.5h"] {
            let err = parse_since(bad, now()).unwrap_err();
            assert!(err.contains("invalid --since"), "{bad}: {err}");
            assert!(err.contains("90m, 24h, 7d, 4w"), "{bad}: {err}");
        }
    }

    #[test]
    fn filter_since_drops_events_before_the_cutoff_only() {
        let events = parse_log(fixture_log()).events;
        let cutoff = parse_iso("2026-07-05T00:00:00Z").unwrap();
        let kept = filter_since(events, cutoff);
        // The index event and the cli search predate the cutoff; 2 mcp searches remain.
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|e| matches!(e, Event::Search(s) if s.source == "mcp")));
    }

    // --- pinned number formatting ---

    #[test]
    fn thousands_and_short_token_formats_are_pinned() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(999), "999");
        assert_eq!(fmt_thousands(38628), "38,628");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
        assert_eq!(fmt_tokens_short(0), "0");
        assert_eq!(fmt_tokens_short(999), "999");
        assert_eq!(fmt_tokens_short(8600), "8.6k");
        assert_eq!(fmt_tokens_short(99_950), "100.0k");
        assert_eq!(fmt_tokens_short(310_880), "310k");
    }

    #[test]
    fn source_filter_parses_and_rejects() {
        assert_eq!(SourceFilter::parse("mcp").unwrap(), SourceFilter::Mcp);
        assert_eq!(SourceFilter::parse("CLI").unwrap(), SourceFilter::Cli);
        assert_eq!(SourceFilter::parse(" all ").unwrap(), SourceFilter::All);
        assert!(SourceFilter::parse("agents").unwrap_err().contains("mcp, cli, or all"));
    }

    // --- the byte-pinned human render ---

    #[test]
    fn human_render_all_time_is_byte_pinned() {
        let got = render_human(&metrics_value(None), None, SourceFilter::All, false);
        let want = "CCE usage — all time\n\
                    \x20 agent (mcp) : 2 searches · saved ~16,500 tok (88%) · quality 0.79 · 58 ms avg\n\
                    \x20 human (cli) : 1 searches · saved ~ 2,100 tok (81%) · quality 0.74 · 12 ms avg\n\
                    \x20 recent (newest first)\n\
                    \x20   mcp  09:58  \"how does the payment flow create a new case\"  5 hits  ~8.6k saved\n\
                    \x20   mcp  09:52  \"where is the retry idempotency boundary\"      5 hits  ~7.9k saved\n\
                    \x20   cli  09:00  \"rrf fusion constant\"                          3 hits  ~2.1k saved\n";
        assert_eq!(got, want);
    }

    #[test]
    fn human_render_since_window_filters_and_labels() {
        let since = parse_since("48h", now()).unwrap();
        let got = render_human(&metrics_value(Some(&since)), Some(&since), SourceFilter::All, false);
        let want = "CCE usage — last 48h (since 2026-07-04T10:00:00Z)\n\
                    \x20 agent (mcp) : 2 searches · saved ~16,500 tok (88%) · quality 0.79 · 58 ms avg\n\
                    \x20 human (cli) : 0 searches · saved ~     0 tok (0%) · quality 0.00 · 0 ms avg\n\
                    \x20 recent (newest first)\n\
                    \x20   mcp  09:58  \"how does the payment flow create a new case\"  5 hits  ~8.6k saved\n\
                    \x20   mcp  09:52  \"where is the retry idempotency boundary\"      5 hits  ~7.9k saved\n";
        assert_eq!(got, want);
    }

    #[test]
    fn human_render_source_filter_narrows_the_display_only() {
        let got = render_human(&metrics_value(None), None, SourceFilter::Cli, false);
        let want = "CCE usage — all time\n\
                    \x20 human (cli) : 1 searches · saved ~2,100 tok (81%) · quality 0.74 · 12 ms avg\n\
                    \x20 recent (newest first)\n\
                    \x20   cli  09:00  \"rrf fusion constant\"  3 hits  ~2.1k saved\n";
        assert_eq!(got, want);
    }

    #[test]
    fn human_render_empty_window_is_the_pinned_friendly_line() {
        let since = parse_since("1h", now()).unwrap();
        let got = render_human(&metrics_value(Some(&since)), Some(&since), SourceFilter::All, false);
        assert_eq!(
            got,
            "CCE usage — last 1h (since 2026-07-06T09:00:00Z)\n  no searches in this window\n"
        );
    }

    #[test]
    fn human_render_elides_beyond_the_recent_limit() {
        // 12 same-day cli searches ⇒ 10 shown + the pinned elision line.
        let mut log = String::new();
        for i in 0..12 {
            log.push_str(&format!(
                "{{\"event\":\"search\",\"ts\":\"2026-07-05T09:{i:02}:00Z\",\"id\":\"q{i:011}\",\"query\":\"q{i}\",\"result_count\":1,\"tokens_saved\":10,\"savings_ratio\":0.5,\"top_score\":0.4,\"empty\":false,\"low_confidence\":false,\"latency_ms\":1.0,\"source\":\"cli\"}}\n"
            ));
        }
        let events = parse_log(&log).events;
        let val = serde_json::to_value(aggregate(&events, now(), 3.00)).unwrap();
        let got = render_human(&val, None, SourceFilter::All, false);
        assert!(got.contains("    … (2 more; --json for all)\n"), "{got}");
        assert_eq!(got.matches(" hits ").count(), 10);
    }

    // --- the cce.usage/v1 JSON projection ---

    #[test]
    fn json_projection_is_byte_pinned() {
        let since = parse_since("7d", now()).unwrap();
        let got =
            render_json(&metrics_value(Some(&since)), now(), Some(&since), SourceFilter::All, false);
        let want = r#"{
  "schema": "cce.usage/v1",
  "generated_ts": "2026-07-06T10:00:00Z",
  "window": {
    "since": "2026-06-29T10:00:00Z",
    "until": "2026-07-06T10:00:00Z"
  },
  "source_filter": "all",
  "totals": {
    "searches": 3,
    "tokens_saved": 18600,
    "mean_savings_ratio": 0.856667,
    "mean_top_score": 0.773333
  },
  "by_source": {
    "cli": {
      "mean_latency_ms": 12.0,
      "mean_savings_ratio": 0.81,
      "mean_top_score": 0.74,
      "searches": 1,
      "tokens_saved": 2100
    },
    "mcp": {
      "mean_latency_ms": 58.0,
      "mean_savings_ratio": 0.88,
      "mean_top_score": 0.79,
      "searches": 2,
      "tokens_saved": 16500
    }
  },
  "recent": [
    {
      "ts": "2026-07-05T09:58:00Z",
      "source": "mcp",
      "query": "how does the payment flow create a new case",
      "result_count": 5,
      "tokens_saved": 8600
    },
    {
      "ts": "2026-07-05T09:52:00Z",
      "source": "mcp",
      "query": "where is the retry idempotency boundary",
      "result_count": 5,
      "tokens_saved": 7900
    },
    {
      "ts": "2026-07-01T09:00:00Z",
      "source": "cli",
      "query": "rrf fusion constant",
      "result_count": 3,
      "tokens_saved": 2100
    }
  ]
}
"#;
        assert_eq!(got, want);
    }

    #[test]
    fn json_carries_both_splits_whatever_the_source_filter() {
        let got = render_json(&metrics_value(None), now(), None, SourceFilter::Mcp, false);
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["source_filter"], "mcp");
        // The display filter never drops data from the projection.
        assert_eq!(v["by_source"]["cli"]["searches"], 1);
        assert_eq!(v["by_source"]["mcp"]["searches"], 2);
        assert_eq!(v["recent"].as_array().unwrap().len(), 3);
        // Single-repo: no by_package key at all.
        assert!(v.get("by_package").is_none());
    }

    // --- dashboard parity: the same numbers as /api/metrics, both paths ---

    #[test]
    fn usage_numbers_equal_the_dashboard_aggregate_over_one_log() {
        // The invariant behind the whole feature: `cce usage` is a projection of
        // the SAME aggregate the dashboard serves — totals, by_source, and the
        // recent list agree field-for-field over one fixture log.
        let dash = metrics_value(None); // what /api/metrics serializes (minus generated_ts)
        let usage: Value =
            serde_json::from_str(&render_json(&dash, now(), None, SourceFilter::All, false))
                .unwrap();
        assert_eq!(usage["totals"]["searches"], dash["totals"]["searches"]);
        assert_eq!(usage["totals"]["tokens_saved"], dash["totals"]["tokens_saved"]);
        assert_eq!(
            usage["totals"]["mean_savings_ratio"],
            dash["totals"]["mean_savings_ratio"]
        );
        assert_eq!(usage["totals"]["mean_top_score"], dash["totals"]["mean_top_score"]);
        // by_source is lifted VERBATIM — byte-identical shape and numbers.
        assert_eq!(usage["by_source"], dash["by_source"]);
        let recent = usage["recent"].as_array().unwrap();
        let dash_recent = dash["recent_searches"].as_array().unwrap();
        assert_eq!(recent.len(), dash_recent.len());
        for (u, d) in recent.iter().zip(dash_recent) {
            for k in ["ts", "source", "query", "result_count", "tokens_saved"] {
                assert_eq!(u[k], d[k], "recent field {k} diverged");
            }
        }
    }
}
