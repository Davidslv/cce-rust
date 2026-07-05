//! # metrics — the persisted event log (append + robust read)
//!
//! **Why this file exists:** DASHBOARD-SPEC v1.1 gives an LLM user observability
//! into whether CCE is improving or degrading their experience, fed from
//! *persisted* data. Every `search`, `index`, and `feedback` appends one JSON
//! line to `<store>/metrics.jsonl`. This module owns writing and reading that log.
//!
//! **What it is / does:** Defines the injected `Clock` and `IdSource` (this is the
//! one subsystem allowed real wall-clock time and unique IDs — everything else in
//! the engine is deterministic), a best-effort/fail-open append path, a robust
//! reader that skips malformed/blank lines and tolerates unknown future fields,
//! and the small UTC date/time arithmetic the windows and daily series need.
//!
//! **Responsibilities:**
//! - Own the event schema on the write side and the parsed-event types on the
//!   read side.
//! - Own `MetricsWriter` (best-effort, honours `enabled`) and `read_log`.
//! - Own ISO-8601-UTC <-> epoch-seconds conversion (no external time crate).
//! - It deliberately does NOT aggregate — that is `aggregator`'s job — and never
//!   lets a write error break the calling command.

use crate::config::METRICS_SCHEMA;
use crate::savings::SavingsBuckets;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// --- Injected clock + id source (the one place wall-clock time is allowed) ---

/// A source of the current instant, as an ISO-8601 UTC second-precision string.
/// Injected so tests can pin the time and the log stays deterministic.
pub trait Clock {
    fn now_iso(&self) -> String;
}

/// A source of unique 12-hex event IDs. Injected so tests can pin the IDs.
pub trait IdSource {
    fn next_id(&self) -> String;
}

/// Wall-clock implementation of `Clock` (real time, second precision).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_iso(&self) -> String {
        let secs =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        format_iso(secs)
    }
}

/// Unique-ID implementation: 12 lowercase-hex chars derived from the wall-clock
/// nanosecond, a process-wide counter, and the PID, so collisions are absent in
/// practice without pulling in a UUID crate.
#[derive(Debug, Default)]
pub struct HexIdSource {
    counter: AtomicU64,
}

impl IdSource for HexIdSource {
    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let input = format!("{nanos}-{n}-{}", std::process::id());
        let digest = Sha256::digest(input.as_bytes());
        hex_lower(&digest)[..12].to_string()
    }
}

/// Lowercase hex of a byte slice.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// --- Write side ---

/// Payload of a `search` event (schema/event/ts/id are filled by the writer).
#[derive(Debug, Clone)]
pub struct SearchRecord {
    pub query: String,
    pub top_k: usize,
    pub graph_enabled: bool,
    pub embedder: String,
    pub result_count: usize,
    pub baseline_tokens: u64,
    pub served_tokens: u64,
    pub tokens_saved: u64,
    pub savings_ratio: f64,
    pub top_score: f64,
    pub mean_score: f64,
    pub empty: bool,
    pub low_confidence: bool,
    pub latency_ms: f64,
    /// Which path issued the search: `"cli"` (a human at `cce search`) or `"mcp"`
    /// (an agent via the `context_search` tool). Powers the dashboard's
    /// agent-vs-human split (v2.4.1). Additive: older logs without this field read
    /// back as `"cli"` (see `parse_log`).
    pub source: String,
}

/// Payload of an `index` event.
#[derive(Debug, Clone)]
pub struct IndexRecord {
    pub files_indexed: usize,
    pub chunks: usize,
    pub index_bytes: u64,
    pub duration_ms: f64,
    pub embedder: String,
    pub full: bool,
    /// The commit sha the index was built against, when `root` is a git checkout
    /// (best-effort; `None` otherwise). Feeds the dashboard's index-freshness panel
    /// (v2.4.1).
    pub sha: Option<String>,
    /// Where the index came from at write time: `"local"` (built by `cce index`).
    /// A pulled cache's provenance is read live from the sync marker; this records
    /// the log-time value. Additive: absent reads back as `"local"`.
    pub source: String,
    /// Files skipped by the Layer-1 sensitive-file policy (SPEC-V2.1). Feeds the
    /// dashboard's secret-safety panel (v2.4.1).
    pub sensitive_skipped: u64,
}

/// Appends events to a metrics log. Best-effort: honours `enabled`, and never
/// propagates an I/O error to the caller (a warning is printed and the command
/// continues — fail-open, like the base engine's non-fatal paths).
pub struct MetricsWriter<'a> {
    path: PathBuf,
    clock: &'a dyn Clock,
    ids: &'a dyn IdSource,
    enabled: bool,
}

impl<'a> MetricsWriter<'a> {
    pub fn new(path: PathBuf, clock: &'a dyn Clock, ids: &'a dyn IdSource, enabled: bool) -> Self {
        MetricsWriter { path, clock, ids, enabled }
    }

    /// Append a `search` event. Returns the assigned event id (the "query-id"),
    /// or `None` when metrics are disabled or the write failed.
    pub fn log_search(&self, rec: &SearchRecord) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let id = self.ids.next_id();
        // SPEC-V2.5 §3: the seven-bucket ledger, additive. Only `retrieval` is
        // populated in Stage ① (from the Layer 1 accounting); the other six ship
        // present-and-zero, ready for later stages. The legacy top-level
        // `baseline_tokens`/`served_tokens`/`tokens_saved`/`savings_ratio` fields
        // stay for backward compatibility with the v2.4 dashboard aggregator.
        let savings = SavingsBuckets::retrieval_only(rec.tokens_saved, rec.baseline_tokens);
        let obj = serde_json::json!({
            "schema": METRICS_SCHEMA,
            "event": "search",
            "ts": self.clock.now_iso(),
            "id": id,
            "query": rec.query,
            "top_k": rec.top_k,
            "graph_enabled": rec.graph_enabled,
            "embedder": rec.embedder,
            "result_count": rec.result_count,
            "baseline_tokens": rec.baseline_tokens,
            "served_tokens": rec.served_tokens,
            "tokens_saved": rec.tokens_saved,
            "savings_ratio": rec.savings_ratio,
            "savings": savings.to_value(),
            "top_score": rec.top_score,
            "mean_score": rec.mean_score,
            "empty": rec.empty,
            "low_confidence": rec.low_confidence,
            "latency_ms": rec.latency_ms,
            "source": rec.source,
        });
        self.append(&obj).map(|_| id)
    }

    /// Append an `index` event. Best-effort; returns the id on success.
    pub fn log_index(&self, rec: &IndexRecord) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let id = self.ids.next_id();
        let obj = serde_json::json!({
            "schema": METRICS_SCHEMA,
            "event": "index",
            "ts": self.clock.now_iso(),
            "id": id,
            "files_indexed": rec.files_indexed,
            "chunks": rec.chunks,
            "index_bytes": rec.index_bytes,
            "duration_ms": rec.duration_ms,
            "embedder": rec.embedder,
            "full": rec.full,
            "sha": rec.sha,
            "source": rec.source,
            "sensitive_skipped": rec.sensitive_skipped,
        });
        self.append(&obj).map(|_| id)
    }

    /// Append a `feedback` event targeting a prior search id. Returns the id.
    pub fn log_feedback(&self, target_id: &str, helpful: bool, note: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let id = self.ids.next_id();
        let obj = serde_json::json!({
            "schema": METRICS_SCHEMA,
            "event": "feedback",
            "ts": self.clock.now_iso(),
            "id": id,
            "target_id": target_id,
            "helpful": helpful,
            "note": note,
        });
        self.append(&obj).map(|_| id)
    }

    /// Serialize `obj` to one line and append it, creating the store dir if
    /// needed. On any error, warn and return `None` (fail-open).
    fn append(&self, obj: &Value) -> Option<()> {
        match self.try_append(obj) {
            Ok(()) => Some(()),
            Err(e) => {
                eprintln!("warning: could not write metrics to {}: {e}", self.path.display());
                None
            }
        }
    }

    fn try_append(&self, obj: &Value) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let line = serde_json::to_string(obj).map_err(std::io::Error::other)?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

// --- Read side ---

/// A parsed `search` event, carrying only what the aggregator needs plus the raw
/// `ts`/`id`/`query` the recent-searches view echoes back.
#[derive(Debug, Clone)]
pub struct SearchEvent {
    pub secs: i64,
    pub ts: String,
    pub id: String,
    pub query: String,
    pub result_count: u64,
    pub tokens_saved: u64,
    pub savings_ratio: f64,
    pub top_score: f64,
    pub empty: bool,
    pub low_confidence: bool,
    /// `"cli"` or `"mcp"`; an absent field (pre-v2.4.1 logs) normalises to `"cli"`.
    pub source: String,
    /// The seven-bucket savings ledger for this search (SPEC-V2.5 §3). Parsed from
    /// the event's `savings` object; a pre-2.5 event with no such object has its
    /// `retrieval` bucket reconstructed from `tokens_saved`/`baseline_tokens`.
    pub savings: SavingsBuckets,
}

/// A parsed `index` event. Carries its instant plus the v2.4.1 freshness/secret
/// fields the dashboard panels read (all optional so older logs still parse).
#[derive(Debug, Clone)]
pub struct IndexEvent {
    pub secs: i64,
    pub ts: String,
    /// The indexed commit sha, when recorded (`None` on non-git or older logs).
    pub sha: Option<String>,
    /// `"local"`; an absent field (pre-v2.4.1 logs) normalises to `"local"`.
    pub source: String,
    /// Sensitive files skipped during this index run (0 on older logs).
    pub sensitive_skipped: u64,
}

/// A parsed `feedback` event.
#[derive(Debug, Clone)]
pub struct FeedbackEvent {
    pub secs: i64,
    pub target_id: String,
    pub helpful: bool,
}

/// One parsed event. `Unknown` is a valid JSON object we do not aggregate (a
/// future event type or a known type with an unparseable timestamp is dropped as
/// malformed instead — see `parse_log`).
#[derive(Debug, Clone)]
pub enum Event {
    Search(SearchEvent),
    Index(IndexEvent),
    Feedback(FeedbackEvent),
    Unknown,
}

/// The result of parsing a log: the events in file order, plus a count of
/// malformed/blank lines that were skipped (never a crash — DASHBOARD-SPEC §2.4).
#[derive(Debug, Clone, Default)]
pub struct ParsedLog {
    pub events: Vec<Event>,
    pub skipped: usize,
}

impl ParsedLog {
    /// Number of successfully parsed events (the `/api/health` "events" count).
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

/// Read and parse the metrics log at `path`. A missing file is an empty log (the
/// dashboard renders a friendly "no data yet" state). Never panics.
pub fn read_log(path: &Path) -> ParsedLog {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_log(&text),
        Err(_) => ParsedLog::default(),
    }
}

/// Parse newline-delimited JSON into events, skipping malformed/blank lines and
/// tolerating unknown fields (DASHBOARD-SPEC §2.4).
pub fn parse_log(text: &str) -> ParsedLog {
    let mut out = ParsedLog::default();
    for line in text.lines() {
        if line.trim().is_empty() {
            out.skipped += 1;
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                out.skipped += 1;
                continue;
            }
        };
        match v.get("event").and_then(Value::as_str) {
            Some("search") => match parse_ts(&v) {
                Some(secs) => out.events.push(Event::Search(SearchEvent {
                    secs,
                    ts: get_str(&v, "ts"),
                    id: get_str(&v, "id"),
                    query: get_str(&v, "query"),
                    result_count: get_u64(&v, "result_count"),
                    tokens_saved: get_u64(&v, "tokens_saved"),
                    savings_ratio: get_f64(&v, "savings_ratio"),
                    top_score: get_f64(&v, "top_score"),
                    empty: get_bool(&v, "empty"),
                    low_confidence: get_bool(&v, "low_confidence"),
                    source: normalize_source(&get_str(&v, "source"), "cli"),
                    savings: SavingsBuckets::from_event(&v),
                })),
                None => out.skipped += 1,
            },
            Some("index") => match parse_ts(&v) {
                Some(secs) => out.events.push(Event::Index(IndexEvent {
                    secs,
                    ts: get_str(&v, "ts"),
                    sha: v.get("sha").and_then(Value::as_str).map(|s| s.to_string()),
                    source: normalize_source(&get_str(&v, "source"), "local"),
                    sensitive_skipped: get_u64(&v, "sensitive_skipped"),
                })),
                None => out.skipped += 1,
            },
            Some("feedback") => match parse_ts(&v) {
                Some(secs) => out.events.push(Event::Feedback(FeedbackEvent {
                    secs,
                    target_id: get_str(&v, "target_id"),
                    helpful: get_bool(&v, "helpful"),
                })),
                None => out.skipped += 1,
            },
            // Valid JSON but not an event type we aggregate: count it, ignore it.
            _ => out.events.push(Event::Unknown),
        }
    }
    out
}

fn parse_ts(v: &Value) -> Option<i64> {
    v.get("ts").and_then(Value::as_str).and_then(parse_iso)
}

fn get_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn get_u64(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn get_f64(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn get_bool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Normalise a `source` tag: an empty/absent value falls back to `default`. Keeps
/// the additive v2.4.1 schema graceful — pre-v2.4.1 logs with no `source` read as
/// the sensible default (`"cli"` for searches, `"local"` for index events).
fn normalize_source(raw: &str, default: &str) -> String {
    if raw.trim().is_empty() {
        default.to_string()
    } else {
        raw.to_string()
    }
}

// --- UTC date/time arithmetic (no external time crate) ---
//
// Uses Howard Hinnant's civil<->days algorithms, which are exact for the
// proleptic Gregorian calendar and dependency-free.

/// Days from 1970-01-01 to the given civil date (Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil date (year, month, day) from days since 1970-01-01 (Hinnant).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse an ISO-8601 UTC timestamp `YYYY-MM-DDTHH:MM:SS[Z]` to epoch seconds.
/// Returns `None` on any malformation. A trailing timezone is assumed UTC.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    // b[10] is the date/time separator ('T' or space); we do not constrain it.
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: i64 = s.get(5..7)?.parse().ok()?;
    let day: i64 = s.get(8..10)?.parse().ok()?;
    let hour: i64 = s.get(11..13)?.parse().ok()?;
    let min: i64 = s.get(14..16)?.parse().ok()?;
    let sec: i64 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + min * 60 + sec)
}

/// Format epoch seconds as an ISO-8601 UTC second-precision timestamp.
pub fn format_iso(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// The UTC calendar date (`YYYY-MM-DD`) for an instant given in epoch seconds.
pub fn date_str(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A clock returning a fixed instant (deterministic tests).
    struct FixedClock(&'static str);
    impl Clock for FixedClock {
        fn now_iso(&self) -> String {
            self.0.to_string()
        }
    }

    /// An id source yielding a fixed sequence, then repeating the last.
    struct FixedIds {
        seq: Vec<&'static str>,
        i: Cell<usize>,
    }
    impl FixedIds {
        fn new(seq: &[&'static str]) -> Self {
            FixedIds { seq: seq.to_vec(), i: Cell::new(0) }
        }
    }
    impl IdSource for FixedIds {
        fn next_id(&self) -> String {
            let i = self.i.get();
            let id = self.seq[i.min(self.seq.len() - 1)];
            self.i.set(i + 1);
            id.to_string()
        }
    }

    #[test]
    fn epoch_and_known_dates_round_trip() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        let t = parse_iso("2026-07-05T00:00:00Z").unwrap();
        assert_eq!(format_iso(t), "2026-07-05T00:00:00Z");
        assert_eq!(date_str(t), "2026-07-05");
        // one hour past midnight
        let h = parse_iso("2026-07-01T10:00:00Z").unwrap();
        assert_eq!(h - parse_iso("2026-07-01T00:00:00Z").unwrap(), 36000);
        assert_eq!(date_str(h), "2026-07-01");
    }

    #[test]
    fn malformed_timestamps_are_none() {
        assert_eq!(parse_iso(""), None);
        assert_eq!(parse_iso("not-a-date"), None);
        assert_eq!(parse_iso("2026-13-01T00:00:00Z"), None); // bad month
        assert_eq!(parse_iso("2026-07-01T99:00:00Z"), None); // bad hour
    }

    #[test]
    fn hex_id_source_is_12_lowercase_hex_and_unique() {
        let s = HexIdSource::default();
        let a = s.next_id();
        let b = s.next_id();
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    #[test]
    fn append_with_injected_clock_and_id_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".cce").join(crate::config::METRICS_FILE);
        let clock = FixedClock("2026-07-05T13:04:11Z");
        let ids = FixedIds::new(&["aaaaaaaaaaaa", "000000000002"]);
        let w = MetricsWriter::new(path.clone(), &clock, &ids, true);

        let id = w
            .log_search(&SearchRecord {
                query: "login".to_string(),
                top_k: 5,
                graph_enabled: true,
                embedder: "hash".to_string(),
                result_count: 3,
                baseline_tokens: 40000,
                served_tokens: 8000,
                tokens_saved: 32000,
                savings_ratio: 0.8,
                top_score: 0.9,
                mean_score: 0.7,
                empty: false,
                low_confidence: false,
                latency_ms: 5.0,
                source: "cli".to_string(),
            })
            .unwrap();
        assert_eq!(id, "aaaaaaaaaaaa");
        w.log_feedback("aaaaaaaaaaaa", true, "").unwrap();

        let log = read_log(&path);
        assert_eq!(log.skipped, 0);
        assert_eq!(log.event_count(), 2);
        match &log.events[0] {
            Event::Search(s) => {
                assert_eq!(s.id, "aaaaaaaaaaaa");
                assert_eq!(s.ts, "2026-07-05T13:04:11Z");
                assert_eq!(s.secs, parse_iso("2026-07-05T13:04:11Z").unwrap());
                assert_eq!(s.tokens_saved, 32000);
                assert_eq!(s.savings_ratio, 0.8);
                assert_eq!(s.source, "cli");
                assert!(!s.empty);
            }
            _ => panic!("first event should be a search"),
        }
        match &log.events[1] {
            Event::Feedback(f) => {
                assert_eq!(f.target_id, "aaaaaaaaaaaa");
                assert!(f.helpful);
            }
            _ => panic!("second event should be feedback"),
        }
    }

    #[test]
    fn disabled_writer_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("metrics.jsonl");
        let clock = FixedClock("2026-07-05T00:00:00Z");
        let ids = FixedIds::new(&["aaaaaaaaaaaa"]);
        let w = MetricsWriter::new(path.clone(), &clock, &ids, false);
        assert!(w
            .log_index(&IndexRecord {
                files_indexed: 1,
                chunks: 2,
                index_bytes: 3,
                duration_ms: 4.0,
                embedder: "hash".to_string(),
                full: true,
                sha: None,
                source: "local".to_string(),
                sensitive_skipped: 0,
            })
            .is_none());
        assert!(!path.exists(), "no file should be created when disabled");
    }

    #[test]
    fn bad_path_is_fail_open_not_a_panic() {
        // Point the log inside a path whose parent is a regular file, so
        // create_dir_all fails. The write must fail-open (return None), not panic.
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, "i am a file").unwrap();
        let path = blocker.join("sub").join("metrics.jsonl");
        let clock = FixedClock("2026-07-05T00:00:00Z");
        let ids = FixedIds::new(&["aaaaaaaaaaaa"]);
        let w = MetricsWriter::new(path.clone(), &clock, &ids, true);
        let rec = SearchRecord {
            query: "q".to_string(),
            top_k: 5,
            graph_enabled: false,
            embedder: "hash".to_string(),
            result_count: 0,
            baseline_tokens: 0,
            served_tokens: 0,
            tokens_saved: 0,
            savings_ratio: 0.0,
            top_score: 0.0,
            mean_score: 0.0,
            empty: true,
            low_confidence: false,
            latency_ms: 1.0,
            source: "cli".to_string(),
        };
        assert!(w.log_search(&rec).is_none());
    }

    #[test]
    fn parse_log_skips_blank_and_corrupt_lines() {
        let text = concat!(
            "\n",
            "not json at all\n",
            "{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"2026-07-01T10:00:00Z\",\"id\":\"aaaaaaaaaaaa\",\"result_count\":1,\"tokens_saved\":10,\"savings_ratio\":0.5,\"top_score\":0.4,\"empty\":false,\"low_confidence\":false}\n",
            "   \n",
            "{\"event\":\"feedback\",\"ts\":\"2026-07-02T10:00:00Z\",\"id\":\"x\",\"target_id\":\"aaaaaaaaaaaa\",\"helpful\":true}\n",
            "{\"event\":\"search\",\"id\":\"noTs\"}\n",
            "{\"event\":\"futuretype\",\"ts\":\"2026-07-02T10:00:00Z\"}\n"
        );
        let log = parse_log(text);
        // blank + "not json" + blank(spaces) + search-without-ts = 4 skipped.
        assert_eq!(log.skipped, 4);
        // search + feedback + unknown-future = 3 events.
        assert_eq!(log.event_count(), 3);
        assert!(matches!(log.events[0], Event::Search(_)));
        assert!(matches!(log.events[1], Event::Feedback(_)));
        assert!(matches!(log.events[2], Event::Unknown));
    }

    #[test]
    fn v241_source_and_freshness_fields_parse_and_default() {
        // A v2.4.1 mcp search + an index event carrying sha/source/sensitive_skipped,
        // and a pre-v2.4.1 search with no `source` (must default to "cli").
        let text = concat!(
            "{\"event\":\"search\",\"ts\":\"2026-07-01T10:00:00Z\",\"id\":\"a\",\"result_count\":1,\"tokens_saved\":10,\"savings_ratio\":0.5,\"top_score\":0.4,\"empty\":false,\"low_confidence\":false,\"source\":\"mcp\"}\n",
            "{\"event\":\"search\",\"ts\":\"2026-07-01T11:00:00Z\",\"id\":\"b\",\"result_count\":1,\"tokens_saved\":10,\"savings_ratio\":0.5,\"top_score\":0.4,\"empty\":false,\"low_confidence\":false}\n",
            "{\"event\":\"index\",\"ts\":\"2026-07-01T09:00:00Z\",\"id\":\"i\",\"files_indexed\":3,\"chunks\":9,\"index_bytes\":1,\"duration_ms\":1.0,\"embedder\":\"hash\",\"full\":true,\"sha\":\"deadbeef\",\"source\":\"local\",\"sensitive_skipped\":2}\n",
            "{\"event\":\"index\",\"ts\":\"2026-07-01T08:00:00Z\",\"id\":\"j\",\"files_indexed\":1,\"chunks\":1,\"index_bytes\":1,\"duration_ms\":1.0,\"embedder\":\"hash\",\"full\":true}\n"
        );
        let log = parse_log(text);
        assert_eq!(log.skipped, 0);
        assert_eq!(log.event_count(), 4);
        match &log.events[0] {
            Event::Search(s) => assert_eq!(s.source, "mcp"),
            _ => panic!("expected search"),
        }
        match &log.events[1] {
            Event::Search(s) => assert_eq!(s.source, "cli"), // absent → default
            _ => panic!("expected search"),
        }
        match &log.events[2] {
            Event::Index(i) => {
                assert_eq!(i.sha.as_deref(), Some("deadbeef"));
                assert_eq!(i.source, "local");
                assert_eq!(i.sensitive_skipped, 2);
            }
            _ => panic!("expected index"),
        }
        match &log.events[3] {
            Event::Index(i) => {
                assert_eq!(i.sha, None); // absent → None
                assert_eq!(i.source, "local"); // absent → default
                assert_eq!(i.sensitive_skipped, 0);
            }
            _ => panic!("expected index"),
        }
    }

    #[test]
    fn read_missing_log_is_empty() {
        let log = read_log(Path::new("/no/such/metrics.jsonl"));
        assert_eq!(log.event_count(), 0);
        assert_eq!(log.skipped, 0);
    }
}
