//! # knowledge::contract — the `cce.knowledge/v1` ingest contract (SPEC-V2.6 §3, M2)
//!
//! **Why this file exists:** CCE owns the *engine* — chunking, a neutral ingest
//! contract, retrieval — never the *integrations*. Rather than teach CCE one ticket
//! system's API, any adapter (GitHub, Jira, Linear, Notion…) emits this neutral
//! NDJSON: one knowledge record per line. This module is that boundary — parse it
//! robustly and render each record to a deterministic markdown document.
//!
//! **What it is / does:** Declares `KnowledgeRecord` (the pinned schema
//! `cce.knowledge/v1`), parses an NDJSON stream into records (unknown fields ignored,
//! absent optionals degrade to `None`/empty), and renders a record to the byte-pinned
//! document `# <title>\n\n<body>` that the M1 heading-chunker then splits.
//!
//! **Responsibilities:**
//! - Own the `cce.knowledge/v1` field set, its schema id, and NDJSON parsing.
//! - Own the deterministic record→document rendering.
//! - It does NOT chunk, redact, attach facets, or persist — the store (M3) wires
//!   those in, and it knows nothing about any specific ticket system.

use serde::Deserialize;

/// The pinned schema id for the ingest contract. A bump is a compatibility event.
pub const KNOWLEDGE_SCHEMA_ID: &str = "cce.knowledge/v1";

/// One `cce.knowledge/v1` record (SPEC-V2.6 §3). `id`/`title`/`body`/`source` are
/// required; every other field is optional and degrades gracefully. Unknown fields
/// in the input are ignored (serde drops them), so an adapter may emit extra keys.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct KnowledgeRecord {
    /// Stable unique id, e.g. `"gh:owner/repo#123"`.
    pub id: String,
    /// Human title; becomes the top `# <title>` heading of the rendered document.
    pub title: String,
    /// Markdown body; heading-chunked by M1.
    pub body: String,
    /// Adapter/source tag, e.g. `"github-issues"`.
    pub source: String,
    /// Canonical URL of the record, if any.
    #[serde(default)]
    pub url: Option<String>,
    /// Lifecycle state, e.g. `"open"` | `"closed"`.
    #[serde(default)]
    pub state: Option<String>,
    /// Why it reached its state, e.g. `"completed"` | `"not_planned"` | `"reopened"`.
    #[serde(default)]
    pub state_reason: Option<String>,
    /// ISO-8601 last-updated timestamp; drives recency in M4.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Free-form labels/tags.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Workstream / section / board-column, e.g. `"Checkout"`.
    #[serde(default)]
    pub group: Option<String>,
    /// Related URLs / PRs (a merged PR = intent + impl).
    #[serde(default)]
    pub links: Vec<String>,
    /// Adapter-specific passthrough, ignored by retrieval.
    #[serde(default)]
    pub extra: Option<serde_json::Value>,
}

/// Parse an NDJSON `cce.knowledge/v1` stream into records (SPEC-V2.6 §3).
///
/// One record per non-blank line. Blank/whitespace-only lines are skipped. A line
/// that is not a valid record (bad JSON, or a missing required field) returns an
/// `Err` naming the 1-based line number, so a malformed feed fails loudly and
/// deterministically rather than silently dropping data.
pub fn parse_ndjson(text: &str) -> Result<Vec<KnowledgeRecord>, String> {
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<KnowledgeRecord>(line) {
            Ok(rec) => records.push(rec),
            Err(e) => return Err(format!("line {}: invalid cce.knowledge/v1 record: {e}", i + 1)),
        }
    }
    Ok(records)
}

/// Render a record to its deterministic markdown document (SPEC-V2.6 §4):
/// `# <title>\n\n<body>`. The title is trimmed; the body is emitted verbatim so
/// heading chunking sees exactly the author's markdown.
pub fn render_document(rec: &KnowledgeRecord) -> String {
    format!("# {}\n\n{}", rec.title.trim(), rec.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_and_optional_fields() {
        let line = r#"{"id":"gh:o/r#1","title":"T","body":"b","source":"github-issues","state":"open","labels":["bug","p1"],"url":"https://x/1"}"#;
        let recs = parse_ndjson(line).unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.id, "gh:o/r#1");
        assert_eq!(r.title, "T");
        assert_eq!(r.source, "github-issues");
        assert_eq!(r.state.as_deref(), Some("open"));
        assert_eq!(r.labels, vec!["bug".to_string(), "p1".to_string()]);
        assert_eq!(r.url.as_deref(), Some("https://x/1"));
        // Absent optionals degrade.
        assert_eq!(r.group, None);
        assert!(r.links.is_empty());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let line =
            r#"{"id":"a","title":"t","body":"b","source":"s","issue_type":"epic","signal_len":42}"#;
        let recs = parse_ndjson(line).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].id, "a");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let text = "\n{\"id\":\"a\",\"title\":\"t\",\"body\":\"b\",\"source\":\"s\"}\n   \n";
        let recs = parse_ndjson(text).unwrap();
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn missing_required_field_errors_with_line_number() {
        // `source` is required; its absence is a line-2 error.
        let text = "{\"id\":\"a\",\"title\":\"t\",\"body\":\"b\",\"source\":\"s\"}\n{\"id\":\"b\",\"title\":\"t\",\"body\":\"b\"}\n";
        let err = parse_ndjson(text).unwrap_err();
        assert!(err.starts_with("line 2:"), "{err}");
    }

    #[test]
    fn bad_json_errors_with_line_number() {
        let err = parse_ndjson("not json").unwrap_err();
        assert!(err.starts_with("line 1:"), "{err}");
    }

    #[test]
    fn renders_deterministic_document() {
        let rec = KnowledgeRecord {
            id: "a".into(),
            title: "  Login policy  ".into(),
            body: "## Why\n\nBecause.".into(),
            source: "s".into(),
            url: None,
            state: None,
            state_reason: None,
            updated_at: None,
            labels: vec![],
            group: None,
            links: vec![],
            extra: None,
        };
        assert_eq!(render_document(&rec), "# Login policy\n\n## Why\n\nBecause.");
    }

    #[test]
    fn schema_id_is_pinned() {
        assert_eq!(KNOWLEDGE_SCHEMA_ID, "cce.knowledge/v1");
    }
}
