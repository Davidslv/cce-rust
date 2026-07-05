//! # session — the per-session MCP ledger + turn-summary digest (SPEC-V2.5 §2 Layer 6)
//!
//! **Why this file exists:** `cce mcp` is one long-lived process per session. Layer 6
//! (turn summarization) keeps a long session's running context small by letting the
//! agent compress its own history into a tiny digest instead of re-sending the raw
//! transcript. Something must accumulate — in memory, for THIS session only — a
//! deterministic, order-preserving record of what the agent did (searches, expansions,
//! related-context widenings, recorded decisions), and turn that record into a
//! **bounded, structured, byte-deterministic** digest. That is this module.
//!
//! **What it is / does:** Owns [`SessionLedger`] (an ordered, wall-clock-FREE list of
//! [`LedgerEvent`]s the server appends as tools run) and [`SessionLedger::digest`],
//! which renders a compact digest. The digest is a **pure function of the recorded
//! call sequence** — same events ⇒ identical bytes: files/chunks are deduped and
//! sorted lexicographically (no hash-iteration order), queries and decisions keep
//! first-seen order with exact-duplicate dedupe, and every list is bounded by
//! [`SUMMARY_MAX_ITEMS`] with the byte-pinned `… (+N more)` elision marker (the same
//! style as L2 compression). It is NOT an LLM-written prose summary — that would break
//! determinism and offline operation.
//!
//! **Responsibilities:**
//! - Own [`LedgerEvent`], [`SessionLedger`] (append + digest), [`SummaryScope`], and
//!   the byte-pinned digest grammar (header, section format, elision marker, label
//!   truncation).
//! - Stay deterministic and offline: no wall-clock, no `HashMap` iteration order, no
//!   model calls. The digest only references already-redacted content the tools pass
//!   in (file paths, queries, chunk ids, redacted decision labels) — it never re-emits
//!   a raw chunk body.
//! - It does NOT decide WHEN to summarize, resolve stores, or format MCP envelopes —
//!   that is `mcp::server`/`mcp::tools`.

use std::collections::BTreeSet;

/// The maximum number of items rendered per digest section before the byte-pinned
/// `… (+N more)` elision marker replaces the tail (SPEC-V2.5 §2 Layer 6: "cap the
/// number of items with a deterministic elision marker"). Keeps the digest bounded
/// no matter how long the session ran.
pub const SUMMARY_MAX_ITEMS: usize = 20;

/// The maximum number of characters kept in a decision's short label; a longer label
/// is truncated on a char boundary and a single U+2026 ellipsis appended. The label
/// is derived from the ALREADY-REDACTED decision text (see [`short_label`]).
pub const SUMMARY_LABEL_MAX_CHARS: usize = 60;

/// The byte-pinned digest elision-marker prefix. The full marker for `n` omitted
/// items is `… (+N more)` — a leading U+2026 HORIZONTAL ELLIPSIS, then `" (+"`, the
/// count, and `" more)"`. Deliberately mirrors L2 compression's `… (+N lines)` style
/// (SPEC-V2.5 §2 Layer 6). Both engines emit these exact bytes. See [`summary_elision`].
pub const SUMMARY_ELISION_PREFIX: &str = "… (+";

/// The digest's top header line (byte-pinned). Every digest starts with exactly this.
pub const DIGEST_HEADER: &str = "CCE session digest";

/// Shown (in place of any sections) when the session ledger is entirely empty.
pub const DIGEST_EMPTY_BODY: &str = "(nothing recorded this session yet)";

/// Build the byte-pinned elision marker for `n` omitted items: the exact string
/// `… (+N more)`. The word is always `more` (no singular special case), matching the
/// always-plural rule of L2's `… (+N lines)`.
pub fn summary_elision(n: usize) -> String {
    format!("{SUMMARY_ELISION_PREFIX}{n} more)")
}

/// One order-preserving, wall-clock-free record of a tool call THIS session made.
/// The server appends one of these as each context-touching tool runs; the digest is
/// computed purely from the accumulated sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerEvent {
    /// A `context_search`: its `query` plus the `chunk_id`s and `file_path`s it
    /// returned (as displayed to the agent).
    Search { query: String, chunk_ids: Vec<String>, file_paths: Vec<String> },
    /// An `expand_chunk`: the `chunk_id` expanded and the `scope`.
    Expand { chunk_id: String, scope: String },
    /// A `related_context`: the `chunk_id` whose neighbours were requested.
    Related { chunk_id: String },
    /// A `record_decision`: the decision's content-addressed `id` and a short,
    /// already-redacted `label` derived from its text.
    Decision { id: String, label: String },
}

/// Which slice of the session digest to render (SPEC-V2.5 §2 Layer 6). `All` shows
/// every section; the others narrow to one slice so the agent can ask for exactly
/// what it needs. `Files` covers the "files/chunks touched" pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryScope {
    /// Everything: files, chunks, queries, decisions (the default).
    All,
    /// Just the files AND chunks touched this session.
    Files,
    /// Just the queries issued this session.
    Queries,
    /// Just the decisions recorded this session.
    Decisions,
}

impl SummaryScope {
    /// Parse the tool string form (case-insensitive). Unknown ⇒ `None`, so the tool
    /// can surface an actionable error rather than silently defaulting.
    pub fn parse(s: &str) -> Option<SummaryScope> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Some(SummaryScope::All),
            "files" => Some(SummaryScope::Files),
            "queries" => Some(SummaryScope::Queries),
            "decisions" => Some(SummaryScope::Decisions),
            _ => None,
        }
    }

    /// The canonical string form.
    pub const fn as_str(&self) -> &'static str {
        match self {
            SummaryScope::All => "all",
            SummaryScope::Files => "files",
            SummaryScope::Queries => "queries",
            SummaryScope::Decisions => "decisions",
        }
    }
}

/// The in-memory, per-session ledger: an ordered list of [`LedgerEvent`]s. Lives on
/// the long-lived `McpServer` for the life of ONE stdio session and is never
/// persisted — a fresh server starts with an empty ledger, so it never leaks across
/// sessions. Append-only and order-preserving; contains no wall-clock.
#[derive(Debug, Default, Clone)]
pub struct SessionLedger {
    events: Vec<LedgerEvent>,
}

impl SessionLedger {
    /// A fresh, empty ledger.
    pub fn new() -> Self {
        SessionLedger::default()
    }

    /// The number of tool calls recorded so far (order-preserving length).
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Record a `context_search` (query + the ids/paths it returned).
    pub fn record_search(&mut self, query: &str, chunk_ids: &[String], file_paths: &[String]) {
        self.events.push(LedgerEvent::Search {
            query: query.to_string(),
            chunk_ids: chunk_ids.to_vec(),
            file_paths: file_paths.to_vec(),
        });
    }

    /// Record an `expand_chunk` (chunk_id + scope).
    pub fn record_expand(&mut self, chunk_id: &str, scope: &str) {
        self.events
            .push(LedgerEvent::Expand { chunk_id: chunk_id.to_string(), scope: scope.to_string() });
    }

    /// Record a `related_context` (chunk_id).
    pub fn record_related(&mut self, chunk_id: &str) {
        self.events.push(LedgerEvent::Related { chunk_id: chunk_id.to_string() });
    }

    /// Record a `record_decision` (id + a short, already-redacted label).
    pub fn record_decision(&mut self, id: &str, label: &str) {
        self.events.push(LedgerEvent::Decision { id: id.to_string(), label: label.to_string() });
    }

    /// The distinct file paths touched, deduped and sorted lexicographically (via a
    /// `BTreeSet`, so ordering is deterministic — never hash-iteration order).
    fn files(&self) -> Vec<String> {
        let mut set: BTreeSet<&str> = BTreeSet::new();
        for e in &self.events {
            if let LedgerEvent::Search { file_paths, .. } = e {
                for f in file_paths {
                    set.insert(f.as_str());
                }
            }
        }
        set.into_iter().map(str::to_string).collect()
    }

    /// The distinct chunk ids touched (search results + expansions + widenings),
    /// deduped and sorted lexicographically.
    fn chunks(&self) -> Vec<String> {
        let mut set: BTreeSet<&str> = BTreeSet::new();
        for e in &self.events {
            match e {
                LedgerEvent::Search { chunk_ids, .. } => {
                    for c in chunk_ids {
                        set.insert(c.as_str());
                    }
                }
                LedgerEvent::Expand { chunk_id, .. } | LedgerEvent::Related { chunk_id } => {
                    set.insert(chunk_id.as_str());
                }
                LedgerEvent::Decision { .. } => {}
            }
        }
        set.into_iter().map(str::to_string).collect()
    }

    /// The queries issued, in first-seen order, with exact duplicates dropped.
    fn queries(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.events {
            if let LedgerEvent::Search { query, .. } = e {
                if !out.iter().any(|q| q == query) {
                    out.push(query.clone());
                }
            }
        }
        out
    }

    /// The decisions recorded, in first-seen order, deduped by id, rendered as
    /// `#<id> <label>` lines.
    fn decisions(&self) -> Vec<String> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut out: Vec<String> = Vec::new();
        for e in &self.events {
            if let LedgerEvent::Decision { id, label } = e {
                if seen.insert(id.as_str()) {
                    out.push(format!("#{id} {label}"));
                }
            }
        }
        out
    }

    /// Render the byte-pinned session digest for `scope` (SPEC-V2.5 §2 Layer 6).
    ///
    /// Deterministic and bounded: a pure function of the recorded call sequence, with
    /// each section capped at [`SUMMARY_MAX_ITEMS`] and elided by `… (+N more)`. An
    /// entirely empty ledger yields the header + [`DIGEST_EMPTY_BODY`], for every
    /// scope. Section order under `All`: files, chunks, queries, decisions.
    pub fn digest(&self, scope: SummaryScope) -> String {
        let mut out = String::from(DIGEST_HEADER);
        if self.events.is_empty() {
            out.push('\n');
            out.push_str(DIGEST_EMPTY_BODY);
            return out;
        }
        match scope {
            SummaryScope::All => {
                push_section(&mut out, "files", &self.files());
                push_section(&mut out, "chunks", &self.chunks());
                push_section(&mut out, "queries", &self.queries());
                push_section(&mut out, "decisions", &self.decisions());
            }
            SummaryScope::Files => {
                push_section(&mut out, "files", &self.files());
                push_section(&mut out, "chunks", &self.chunks());
            }
            SummaryScope::Queries => push_section(&mut out, "queries", &self.queries()),
            SummaryScope::Decisions => push_section(&mut out, "decisions", &self.decisions()),
        }
        out
    }
}

/// Append one digest section: a `\n<name> (<count>):` header (the count is the TRUE
/// distinct total, before capping), then up to [`SUMMARY_MAX_ITEMS`] `- <item>` lines,
/// with a trailing `… (+N more)` marker when the list is longer.
fn push_section(out: &mut String, name: &str, items: &[String]) {
    out.push_str(&format!("\n{name} ({}):", items.len()));
    for (i, item) in items.iter().enumerate() {
        if i >= SUMMARY_MAX_ITEMS {
            out.push('\n');
            out.push_str(&summary_elision(items.len() - SUMMARY_MAX_ITEMS));
            break;
        }
        out.push_str(&format!("\n- {item}"));
    }
}

/// Derive a short, single-line label from an ALREADY-REDACTED decision text: collapse
/// every internal ASCII-whitespace run to a single space, trim the ends, then truncate
/// to [`SUMMARY_LABEL_MAX_CHARS`] characters (on a char boundary) with a single U+2026
/// ellipsis appended when cut. Secret-safe: the caller passes the redacted text, so no
/// raw secret ever reaches the label.
pub fn short_label(text: &str) -> String {
    let collapsed = collapse_whitespace(text);
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= SUMMARY_LABEL_MAX_CHARS {
        collapsed
    } else {
        let mut s: String = chars[..SUMMARY_LABEL_MAX_CHARS].iter().collect();
        s.push('…');
        s
    }
}

/// Trim and collapse: drop leading/trailing ASCII whitespace and collapse every
/// internal ASCII-whitespace run to a single space. Non-ASCII scalars are preserved
/// verbatim. Byte-pinned so the label is reproducible from the char sequence alone.
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut started = false;
    for c in text.chars() {
        if c.is_ascii_whitespace() {
            if started {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
            started = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_scope_parse_and_as_str_round_trip() {
        for (s, sc) in [
            ("all", SummaryScope::All),
            ("files", SummaryScope::Files),
            ("queries", SummaryScope::Queries),
            ("decisions", SummaryScope::Decisions),
        ] {
            assert_eq!(SummaryScope::parse(s), Some(sc));
            assert_eq!(SummaryScope::parse(&s.to_uppercase()), Some(sc));
            assert_eq!(sc.as_str(), s);
        }
        assert_eq!(SummaryScope::parse("chunks"), None);
        assert_eq!(SummaryScope::parse(""), None);
    }

    #[test]
    fn summary_elision_is_byte_pinned() {
        assert_eq!(summary_elision(3), "… (+3 more)");
        assert_eq!(summary_elision(1), "… (+1 more)"); // always plural, by design
        assert_eq!(summary_elision(0), "… (+0 more)");
    }

    #[test]
    fn short_label_collapses_and_truncates() {
        assert_eq!(short_label("  use   bcrypt\tfor hashing\n"), "use bcrypt for hashing");
        assert_eq!(short_label("你好  世界"), "你好 世界");
        // Truncation on a char boundary with a single trailing ellipsis.
        let long = "a".repeat(80);
        let label = short_label(&long);
        assert_eq!(label.chars().count(), SUMMARY_LABEL_MAX_CHARS + 1);
        assert!(label.ends_with('…'));
        // Exactly at the limit is NOT truncated.
        let exact = "b".repeat(SUMMARY_LABEL_MAX_CHARS);
        assert_eq!(short_label(&exact), exact);
    }

    #[test]
    fn empty_ledger_digests_to_the_pinned_empty_body_for_every_scope() {
        let led = SessionLedger::new();
        assert!(led.is_empty());
        for scope in
            [SummaryScope::All, SummaryScope::Files, SummaryScope::Queries, SummaryScope::Decisions]
        {
            assert_eq!(
                led.digest(scope),
                "CCE session digest\n(nothing recorded this session yet)"
            );
        }
    }

    #[test]
    fn digest_is_byte_pinned_for_a_fixed_call_sequence() {
        let mut led = SessionLedger::new();
        led.record_search(
            "hash password",
            &["bbbb000000000000".into(), "aaaa000000000000".into()],
            &["auth.py".into()],
        );
        led.record_search(
            "process payment",
            &["cccc000000000000".into()],
            &["payments.py".into(), "auth.py".into()],
        );
        // A duplicate query is dropped; a repeated file/chunk is deduped.
        led.record_search("hash password", &["aaaa000000000000".into()], &["auth.py".into()]);
        led.record_expand("bbbb000000000000", "body");
        led.record_related("cccc000000000000");
        led.record_decision("1111222233334444", "use bcrypt for password hashing");

        // Files + chunks are sorted; queries + decisions keep first-seen order.
        let golden = "CCE session digest\n\
             files (2):\n- auth.py\n- payments.py\n\
             chunks (3):\n- aaaa000000000000\n- bbbb000000000000\n- cccc000000000000\n\
             queries (2):\n- hash password\n- process payment\n\
             decisions (1):\n- #1111222233334444 use bcrypt for password hashing";
        assert_eq!(led.digest(SummaryScope::All), golden);

        // Deterministic: same sequence ⇒ identical bytes.
        let mut led2 = SessionLedger::new();
        led2.record_search(
            "hash password",
            &["bbbb000000000000".into(), "aaaa000000000000".into()],
            &["auth.py".into()],
        );
        led2.record_search(
            "process payment",
            &["cccc000000000000".into()],
            &["payments.py".into(), "auth.py".into()],
        );
        led2.record_search("hash password", &["aaaa000000000000".into()], &["auth.py".into()]);
        led2.record_expand("bbbb000000000000", "body");
        led2.record_related("cccc000000000000");
        led2.record_decision("1111222233334444", "use bcrypt for password hashing");
        assert_eq!(led2.digest(SummaryScope::All), golden);
    }

    #[test]
    fn scope_slices_return_only_their_section() {
        let mut led = SessionLedger::new();
        led.record_search("q1", &["aaaa000000000000".into()], &["a.py".into()]);
        led.record_decision("deadbeefdeadbeef", "chose approach X");

        assert_eq!(led.digest(SummaryScope::Queries), "CCE session digest\nqueries (1):\n- q1");
        assert_eq!(
            led.digest(SummaryScope::Files),
            "CCE session digest\nfiles (1):\n- a.py\nchunks (1):\n- aaaa000000000000"
        );
        assert_eq!(
            led.digest(SummaryScope::Decisions),
            "CCE session digest\ndecisions (1):\n- #deadbeefdeadbeef chose approach X"
        );
    }

    #[test]
    fn a_slice_with_no_items_shows_a_zero_count_header() {
        // The ledger is non-empty, but the requested slice has nothing.
        let mut led = SessionLedger::new();
        led.record_search("only a query", &[], &[]);
        assert_eq!(led.digest(SummaryScope::Decisions), "CCE session digest\ndecisions (0):");
    }

    #[test]
    fn lists_are_bounded_with_the_elision_marker() {
        let mut led = SessionLedger::new();
        // 25 distinct queries → 20 shown + "… (+5 more)".
        for i in 0..25 {
            led.record_search(&format!("q{i:02}"), &[], &[]);
        }
        let out = led.digest(SummaryScope::Queries);
        assert!(out.starts_with("CCE session digest\nqueries (25):"));
        assert!(out.contains("\n- q00"));
        assert!(out.contains("\n- q19"));
        assert!(!out.contains("\n- q20"), "the 21st item must be elided");
        assert!(out.ends_with("… (+5 more)"));
        // Exactly SUMMARY_MAX_ITEMS lines are shown before the marker.
        assert_eq!(out.matches("\n- q").count(), SUMMARY_MAX_ITEMS);
    }

    #[test]
    fn ledger_len_tracks_recorded_calls_in_order() {
        let mut led = SessionLedger::new();
        assert_eq!(led.len(), 0);
        led.record_search("q", &[], &[]);
        led.record_expand("id", "body");
        assert_eq!(led.len(), 2);
    }
}
