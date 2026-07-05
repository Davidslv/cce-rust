//! # memory — cross-session decision memory (SPEC-V2.5 §2 Layer 5)
//!
//! **Why this file exists:** Layer 5 lets an agent remember a VALIDATED decision so
//! it need not re-derive it next session. Unlike every other store in the engine,
//! memory is conversational, local-only, and NON-reproducible — it carries a
//! wall-clock `ts` and is never pushed by Sync, never part of `conformance.json`.
//! It therefore lives in its own module, apart from the deterministic code index,
//! with its own append-only store and its own retrieval entry point.
//!
//! **What it is / does:** Owns the `.cce/memory.jsonl` store (append-only,
//! content-addressed, de-duplicated, secret-redacted before write) and `session_recall`
//! over it. Recall REUSES the retrieval engine: memory entries are turned into
//! `Chunk`s, assembled into an `Index`, and ranked by the exact §6 hybrid pipeline
//! (BM25 + vector + RRF), then **precision-filtered** (a score floor AND a shared
//! query-token requirement) so a weak, coincidental match is dropped rather than
//! polluting context (SPEC-V2.5 §2 L5 anti-pollution rule).
//!
//! **Responsibilities:**
//! - Own the byte-pinned id normalization, the `MemoryEntry` shape, and the store
//!   path (`.cce/memory.jsonl`), append, dedupe, and redact-before-write.
//! - Own `recall` (reuse `retriever`/`store`, never reimplement ranking) and its
//!   precision filter.
//! - It does NOT wire the clock (the caller injects it), decide workspace scope, or
//!   render the MCP tool text — that is `mcp::tools`. It never touches the code index.

use crate::chunker::{token_count, Chunk};
use crate::embedder::{Embedder, HashEmbedder};
use crate::metrics::Clock;
use crate::redactor::redact;
use crate::retriever::search;
use crate::store::Index;
use crate::tokenizer::tokenize;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The memory store filename, written inside the `.cce/` store directory.
pub const MEMORY_FILE: &str = "memory.jsonl";

/// Default `top_k` for `session_recall` (SPEC-V2.5 §2 L5: "small default top_k").
/// Deliberately smaller than retrieval's 8 — memory recall is precision-first.
pub const MEMORY_DEFAULT_TOP_K: usize = 5;

/// The precision floor for a recalled memory (SPEC-V2.5 §2 L5 anti-pollution):
/// a hybrid-retrieval score below this is dropped. Aligned with the dashboard's
/// `LOW_CONFIDENCE_THRESHOLD` so "low confidence" means the same thing everywhere.
pub const MEMORY_RECALL_MIN_SCORE: f64 = 0.30;

/// The synthetic `kind`/`chunk_type`/`language` a memory entry carries when it is
/// turned into a `Chunk` for ranking. Kept off the real language namespaces.
const MEMORY_KIND: &str = "decision";
const MEMORY_CHUNK_TYPE: &str = "memory";
const MEMORY_LANGUAGE: &str = "memory";

/// One remembered decision. Field order is fixed, so `serde_json` serializes each
/// JSONL line with deterministic key order (no hash iteration). `area` is omitted
/// entirely when absent (`area?` in the spec), never emitted as `null`.
///
/// - `id`  — first 16 hex of SHA-256 over the **byte-pinned normalized** `text`
///   (see [`normalize`]); same text ⇒ same id ⇒ dedupe.
/// - `text`/`area` — already secret-redacted (SPEC-V2.5 §1/§4) before this struct
///   is ever written.
/// - `ts`  — wall-clock ISO-8601 UTC. This is the ONE store where non-reproducibility
///   is accepted; it is injected via a [`Clock`] so tests never depend on real time,
///   and it is NEVER read by recall ranking (ranking stays deterministic on a fixed
///   store).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    pub ts: String,
}

/// The result of a [`record`] call: the stored (or already-present) entry and
/// whether it was newly appended (`false` ⇒ the id already existed, a dedupe no-op).
#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub entry: MemoryEntry,
    pub is_new: bool,
}

/// One precision-filtered recall result: the entry's id + its stored text/metadata
/// and the hybrid-retrieval `score`. Ranked, id-addressed, agent-chosen (never
/// auto-injected).
#[derive(Debug, Clone)]
pub struct RecallHit {
    pub rank: usize,
    pub id: String,
    pub text: String,
    pub tags: Vec<String>,
    pub area: Option<String>,
    pub score: f64,
}

/// The memory store path for a root: `<root>/.cce/memory.jsonl`. Resolved exactly
/// like the other `.cce/` stores, so per-member and workspace-level memory each
/// live beside that scope's index.
pub fn memory_path(root: &Path) -> PathBuf {
    root.join(".cce").join(MEMORY_FILE)
}

/// The **byte-pinned** id normalization (SPEC-V2.5 §2 Layer 5).
///
/// Exact rule, reproducible in any language from the char sequence alone:
///
/// 1. A *whitespace char* is any `c` for which `c.is_ascii_whitespace()` is true —
///    exactly the five bytes TAB (0x09), LF (0x0A), FF (0x0C), CR (0x0D), and
///    SPACE (0x20). (Vertical tab 0x0B is deliberately NOT whitespace here.)
/// 2. Leading and trailing whitespace chars are removed (trim).
/// 3. Every maximal run of one-or-more internal whitespace chars collapses to a
///    single SPACE (0x20).
/// 4. Every non-whitespace char is preserved verbatim, including non-ASCII (a
///    multi-byte scalar is copied unchanged).
///
/// The id is then `first 16 lowercase-hex chars of SHA-256(normalized UTF-8 bytes)`.
/// The normalized form is used ONLY for id/dedupe; the store keeps the readable
/// (redacted) `text`.
pub fn normalize(text: &str) -> String {
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

/// The content-addressed id of `text`: first 16 lowercase-hex of SHA-256 over its
/// byte-pinned [`normalize`]d form. Same text (up to whitespace) ⇒ same id.
pub fn memory_id(text: &str) -> String {
    let normalized = normalize(text);
    let digest = Sha256::digest(normalized.as_bytes());
    hex_lower(&digest)[..16].to_string()
}

/// Lowercase hex of a byte slice.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Record a VALIDATED decision (SPEC-V2.5 §2 Layer 5).
///
/// Order matters: `text` (and `area`) are secret-redacted FIRST (SPEC-V2.5 §1/§4),
/// so no secret ever reaches disk or the id pre-image. The id is derived from the
/// redacted text; if it already exists in `path`, this is a **no-op** and the
/// existing entry is returned (`is_new = false`). Otherwise a new entry — stamped
/// with `clock.now_iso()` — is appended. `clock` is injected so tests pin the `ts`.
pub fn record(
    path: &Path,
    text: &str,
    tags: &[String],
    area: Option<&str>,
    clock: &dyn Clock,
) -> io::Result<RecordOutcome> {
    // Secret-safe: redact BEFORE anything reaches disk (SPEC-V2.5 §1/§4 invariant).
    let safe_text = redact(text);
    let safe_area = area.map(redact);
    let id = memory_id(&safe_text);

    // Dedupe: re-recording an existing id is a no-op (return the stored entry).
    let existing = load_entries(std::slice::from_ref(&path.to_path_buf()));
    if let Some(found) = existing.into_iter().find(|e| e.id == id) {
        return Ok(RecordOutcome { entry: found, is_new: false });
    }

    let entry = MemoryEntry {
        id,
        text: safe_text,
        tags: tags.to_vec(),
        area: safe_area,
        ts: clock.now_iso(),
    };
    append(path, &entry)?;
    Ok(RecordOutcome { entry, is_new: true })
}

/// Append one entry as a single JSONL line, creating the store dir if needed.
fn append(path: &Path, entry: &MemoryEntry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry).map_err(io::Error::other)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Load the union of memory entries across `paths` (workspace-level + per-member),
/// de-duplicated by id (first occurrence wins), skipping blank/malformed lines and
/// missing files. Preserves first-seen order so recall is deterministic.
pub fn load_entries(paths: &[PathBuf]) -> Vec<MemoryEntry> {
    let mut out: Vec<MemoryEntry> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in paths {
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<MemoryEntry>(line) {
                if seen.insert(e.id.clone()) {
                    out.push(e);
                }
            }
        }
    }
    out
}

/// The searchable text for an entry: its `text` plus its `tags` and `area` (so a
/// query that names an area/tag can match), joined by spaces. The stored `text` a
/// hit returns is unchanged — this is only the ranking corpus.
fn searchable(e: &MemoryEntry) -> String {
    let mut s = e.text.clone();
    for t in &e.tags {
        s.push(' ');
        s.push_str(t);
    }
    if let Some(a) = &e.area {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// Turn a memory entry into a `Chunk` for the retriever. Each entry gets a UNIQUE
/// `file_path` (its own id) so the §6 per-file diversity cap never collapses two
/// distinct memories, and the hex id trips no path-penalty marker.
fn entry_to_chunk(e: &MemoryEntry, embedder: &dyn Embedder) -> Chunk {
    let content = searchable(e);
    let embedding = embedder.embed(&content);
    Chunk {
        chunk_id: e.id.clone(),
        file_path: e.id.clone(),
        start_line: 0,
        end_line: 0,
        chunk_type: MEMORY_CHUNK_TYPE.to_string(),
        kind: MEMORY_KIND.to_string(),
        language: MEMORY_LANGUAGE.to_string(),
        token_count: token_count(&content),
        embedding,
        content,
    }
}

/// True if `query` and `entry` share at least one token (the shared tokenizer):
/// the precision half of the anti-pollution filter that rejects a pure
/// vector-coincidence with no lexical overlap.
fn shares_token(query: &str, entry: &MemoryEntry) -> bool {
    let q: HashSet<String> = tokenize(query).into_iter().collect();
    if q.is_empty() {
        return false;
    }
    tokenize(&searchable(entry)).into_iter().any(|t| q.contains(&t))
}

/// `session_recall` core (SPEC-V2.5 §2 Layer 5): hybrid-rank `entries` for `query`,
/// then **precision-filter** for the anti-pollution rule.
///
/// Recall REUSES the retrieval engine: entries become `Chunk`s, assemble into an
/// `Index`, and are ranked by the exact §6 pipeline (BM25 + vector + RRF) with the
/// deterministic hash embedder — so ranking is fully deterministic on a fixed store
/// (the wall-clock `ts` is never consulted). A hit is kept ONLY if its score ≥
/// `min_score` AND it shares a query token, then the list is truncated to `top_k`.
/// Returns entries the agent CHOOSES to use — never an auto-injected blob.
pub fn recall(
    entries: &[MemoryEntry],
    query: &str,
    top_k: usize,
    min_score: f64,
) -> Vec<RecallHit> {
    if entries.is_empty() {
        return Vec::new();
    }
    let embedder = HashEmbedder;
    let by_id: BTreeMap<&str, &MemoryEntry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let chunks: Vec<Chunk> = entries.iter().map(|e| entry_to_chunk(e, &embedder)).collect();
    let index =
        Index::from_parts(chunks, BTreeMap::new(), BTreeMap::new(), embedder.name().to_string());

    // Rank generously (top_k candidates), then precision-filter down.
    let results = search(&index, &embedder, query, top_k, false);
    let mut hits: Vec<RecallHit> = Vec::new();
    for r in results {
        if r.score < min_score {
            continue;
        }
        let Some(e) = by_id.get(r.chunk_id.as_str()) else { continue };
        if !shares_token(query, e) {
            continue;
        }
        hits.push(RecallHit {
            rank: hits.len() + 1,
            id: e.id.clone(),
            text: e.text.clone(),
            tags: e.tags.clone(),
            area: e.area.clone(),
            score: r.score,
        });
        if hits.len() >= top_k {
            break;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock that always returns a fixed instant, so tests never touch wall-clock.
    struct FixedClock(&'static str);
    impl Clock for FixedClock {
        fn now_iso(&self) -> String {
            self.0.to_string()
        }
    }

    // Secret-shaped inputs are assembled from split fragments via `concat!` so no
    // committed source carries a contiguous secret literal (push protection).
    const AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");

    #[test]
    fn normalize_is_byte_pinned() {
        // Trim + collapse internal ascii-whitespace runs to a single space.
        assert_eq!(normalize("  hash\tthe   password\n"), "hash the password");
        assert_eq!(normalize("RRF_K is 60"), "RRF_K is 60");
        assert_eq!(normalize("Use RRF_K = 60\n\nfor  fusion"), "Use RRF_K = 60 for fusion");
        // Non-ASCII scalars are preserved; only the whitespace run collapses.
        assert_eq!(normalize("你好  世界"), "你好 世界");
        // All-whitespace / empty ⇒ empty.
        assert_eq!(normalize("   \t\n"), "");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn memory_id_golden_and_whitespace_invariant() {
        // Byte-pinned goldens (cce-ruby reconciles to these exact 16-hex ids).
        assert_eq!(memory_id("  hash\tthe   password\n"), "b6c678d513963ddf");
        assert_eq!(memory_id("RRF_K is 60"), "8312d29277088a63");
        assert_eq!(memory_id("Use RRF_K = 60\n\nfor  fusion"), "2b28ac69f565d8e2");
        assert_eq!(memory_id("你好  世界"), "4ec14dba94b11ab6");
        // Id is 16 lowercase-hex.
        let id = memory_id("anything");
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Whitespace-only differences map to the SAME id (drives dedupe).
        assert_eq!(memory_id("hash the password"), memory_id("  hash\tthe   password\n"));
    }

    #[test]
    fn record_appends_then_dedupes_same_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());
        let clock = FixedClock("2026-07-05T00:00:00Z");

        let first = record(&path, "Prefer RRF over naive concat", &[], None, &clock).unwrap();
        assert!(first.is_new);
        assert_eq!(first.entry.ts, "2026-07-05T00:00:00Z");

        // Same text (differing only in whitespace) ⇒ same id ⇒ no-op, ONE line.
        let again = record(&path, "Prefer   RRF\tover naive concat  ", &[], None, &clock).unwrap();
        assert!(!again.is_new);
        assert_eq!(again.entry.id, first.entry.id);

        let entries = load_entries(std::slice::from_ref(&path));
        assert_eq!(entries.len(), 1, "dedupe must keep exactly one entry");
    }

    #[test]
    fn record_redacts_secrets_before_write() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());
        let clock = FixedClock("2026-07-05T00:00:00Z");

        let text = format!("deploy key is AWS = \"{AWS_KEY}\" do not lose it");
        let out = record(&path, &text, &[], Some("secops"), &clock).unwrap();
        // The stored entry carries the REDACTED marker, never the raw secret.
        assert!(out.entry.text.contains("[REDACTED:AWS_ACCESS_KEY]"));
        assert!(!out.entry.text.contains(AWS_KEY));

        // And the raw bytes on disk carry no secret either.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[REDACTED:AWS_ACCESS_KEY]"));
        assert!(!raw.contains(AWS_KEY));
    }

    #[test]
    fn recall_is_deterministic_and_precision_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());
        let clock = FixedClock("2026-07-05T00:00:00Z");
        record(&path, "hash passwords with bcrypt in auth", &["auth".into()], Some("auth"), &clock)
            .unwrap();
        record(&path, "process payments through the ledger service", &[], Some("billing"), &clock)
            .unwrap();
        record(&path, "sessions expire after thirty minutes", &[], None, &clock).unwrap();

        let entries = load_entries(std::slice::from_ref(&path));
        assert_eq!(entries.len(), 3);

        // A matching query returns the right decision, ranked #1, above the floor.
        let hits =
            recall(&entries, "hash password auth", MEMORY_DEFAULT_TOP_K, MEMORY_RECALL_MIN_SCORE);
        assert!(!hits.is_empty());
        assert!(hits[0].text.contains("bcrypt"), "top hit wrong: {:?}", hits[0]);
        assert!(hits.iter().all(|h| h.score >= MEMORY_RECALL_MIN_SCORE));

        // Deterministic across runs on a fixed store.
        let again =
            recall(&entries, "hash password auth", MEMORY_DEFAULT_TOP_K, MEMORY_RECALL_MIN_SCORE);
        let a: Vec<(&str, u64)> = hits.iter().map(|h| (h.id.as_str(), h.score.to_bits())).collect();
        let b: Vec<(&str, u64)> =
            again.iter().map(|h| (h.id.as_str(), h.score.to_bits())).collect();
        assert_eq!(a, b);

        // A query with NO lexical overlap is dropped by the precision filter (the
        // anti-pollution rule): no shared token ⇒ nothing recalled, not noise.
        let none = recall(
            &entries,
            "kubernetes helm chart",
            MEMORY_DEFAULT_TOP_K,
            MEMORY_RECALL_MIN_SCORE,
        );
        assert!(none.is_empty(), "coincidental non-match must be filtered: {none:?}");
    }

    #[test]
    fn recall_unions_workspace_and_member_stores() {
        // Workspace-aware: recall over the union of the workspace-level store and a
        // member store resolves an entry recorded in either scope.
        let tmp = tempfile::tempdir().unwrap();
        let clock = FixedClock("2026-07-05T00:00:00Z");
        let ws = memory_path(tmp.path());
        let member = memory_path(&tmp.path().join("services").join("billing"));
        record(&ws, "workspace convention: trunk-based development", &[], None, &clock).unwrap();
        record(&member, "billing retries payments three times", &[], Some("billing"), &clock)
            .unwrap();

        let entries = load_entries(&[ws.clone(), member.clone()]);
        assert_eq!(entries.len(), 2);
        let hits = recall(
            &entries,
            "billing payments retry",
            MEMORY_DEFAULT_TOP_K,
            MEMORY_RECALL_MIN_SCORE,
        );
        assert!(hits.iter().any(|h| h.text.contains("retries payments")));
    }

    #[test]
    fn load_entries_skips_blank_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = memory_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"id\":\"aaaa000011112222\",\"text\":\"ok\",\"tags\":[],\"ts\":\"t\"}\n\nnot json\n",
        )
        .unwrap();
        let entries = load_entries(std::slice::from_ref(&path));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "ok");
    }

    #[test]
    fn empty_store_recalls_nothing() {
        assert!(recall(&[], "anything", MEMORY_DEFAULT_TOP_K, MEMORY_RECALL_MIN_SCORE).is_empty());
    }

    #[test]
    fn entry_serialization_omits_absent_area_and_is_stable() {
        let e = MemoryEntry {
            id: "0011223344556677".into(),
            text: "no area here".into(),
            tags: vec!["x".into()],
            area: None,
            ts: "2026-07-05T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        // Fixed key order, and `area` is omitted entirely when absent.
        assert_eq!(
            json,
            r#"{"id":"0011223344556677","text":"no area here","tags":["x"],"ts":"2026-07-05T00:00:00Z"}"#
        );
    }
}
