//! # config — normative constants and runtime configuration
//!
//! **Why this file exists:** SPEC §3 fixes a table of constants that BOTH the
//! Ruby and Rust implementations must agree on exactly. Centralising them here
//! guarantees a single source of truth and makes cross-language equivalence a
//! matter of copying one table, not scattering magic numbers.
//!
//! **What it is / does:** Exposes every SPEC §3 constant plus a small `Config`
//! struct for the choices that vary at runtime (which embedder backend to use).
//!
//! **Responsibilities:**
//! - Own the exact numeric/string constants from SPEC §3.
//! - Own the `EmbedderKind` selection and default values.
//! - It does NOT own any algorithm — only the tunables those algorithms read.

/// Hashing-embedder vector dimension.
pub const EMBED_DIM: usize = 256;
/// Token-count estimate divisor: token_count = floor(bytes / CHARS_PER_TOKEN), min 1.
pub const CHARS_PER_TOKEN: usize = 4;
/// Reciprocal Rank Fusion constant.
pub const RRF_K: f64 = 60.0;
/// Weight of confidence vs normalized RRF in the final blend.
pub const CONFIDENCE_WEIGHT: f64 = 0.5;
/// BM25 weight multiplier applied when the query intent is CODE_LOOKUP.
pub const FTS_BOOST_CODE_LOOKUP: f64 = 1.5;
/// Per-file diversity cap in the returned results.
pub const MAX_CHUNKS_PER_FILE: usize = 3;
/// BM25 term-frequency saturation parameter.
pub const BM25_K1: f64 = 1.2;
/// BM25 length-normalization parameter.
pub const BM25_B: f64 = 0.75;
/// Fetch top_k × this many candidates from each retriever.
pub const CANDIDATE_MULTIPLIER: usize = 3;
/// Confidence blend: vector weight.
pub const W_VECTOR: f64 = 0.5;
/// Confidence blend: keyword weight.
pub const W_KEYWORD: f64 = 0.4;
/// Confidence blend: recency weight (recency == 0 in deterministic mode).
pub const W_RECENCY: f64 = 0.1;
/// Multiplier applied to test/doc-path chunks.
pub const PATH_PENALTY: f64 = 0.8;
/// Substrings that, if present in a lowercased file path, trigger the penalty.
pub const PATH_PENALTY_MARKERS: [&str; 5] = ["tests/", "test_", "docs/", "spec", "plan"];
/// Number of related files pulled in during graph expansion.
pub const GRAPH_MAX_BONUS_FILES: usize = 2;
/// Score scale applied to graph-expansion (bonus) chunks.
pub const GRAPH_BONUS_CHUNK_SCALE: f64 = 0.85;
/// Default number of results returned.
pub const DEFAULT_TOP_K: usize = 10;

/// Maximum file size (bytes) we will index. Files larger than this are skipped.
pub const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;

/// Spec version tag stamped on the persisted index file (internal).
pub const SPEC_VERSION: &str = "1.0";

/// Spec version emitted in `conformance.json` (SPEC-V2 §7). The v2 chunk shape
/// adds `kind`, so both implementations agree on this tag for the equivalence gate.
pub const CONFORMANCE_SPEC_VERSION: &str = "2.0";

// --- Dashboard & observability (DASHBOARD-SPEC v1.1 §1) ---

/// Schema tag stamped on every metrics event and on the aggregate API body.
pub const METRICS_SCHEMA: &str = "cce.metrics/v1";
/// Default metrics log filename, written inside the store directory.
pub const METRICS_FILE: &str = "metrics.jsonl";
/// A non-empty search whose top score is below this is "low confidence".
pub const LOW_CONFIDENCE_THRESHOLD: f64 = 0.30;
/// Current-vs-prior comparison window length, in days.
pub const TREND_WINDOW_DAYS: i64 = 7;
/// Default loopback port for `cce dashboard`.
pub const DEFAULT_DASHBOARD_PORT: u16 = 8787;
/// Default USD price per 1M input tokens, used for the $-saved estimate.
pub const DEFAULT_INPUT_PRICE_PER_MILLION: f64 = 3.00;
/// How many recent searches the aggregate/API returns.
pub const RECENT_SEARCHES_LIMIT: usize = 20;
/// Delta magnitude at or below this is treated as "flat" (no direction).
pub const DIRECTION_EPSILON: f64 = 1e-9;

/// Selects the embedding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    /// Deterministic hashing embedder (SPEC §5.1) — the default.
    Hash,
    /// Optional local Ollama HTTP embedder (SPEC §11).
    Ollama,
}

impl EmbedderKind {
    /// Parse a backend name (`"hash"` / `"ollama"`); unknown names fall back to Hash.
    pub fn parse(s: &str) -> EmbedderKind {
        match s.to_ascii_lowercase().as_str() {
            "ollama" => EmbedderKind::Ollama,
            _ => EmbedderKind::Hash,
        }
    }
}

/// Runtime configuration: the values that are not fixed constants.
#[derive(Debug, Clone)]
pub struct Config {
    pub embedder: EmbedderKind,
}

impl Default for Config {
    fn default() -> Self {
        Config { embedder: EmbedderKind::Hash }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_selects_ollama_case_insensitively() {
        assert_eq!(EmbedderKind::parse("ollama"), EmbedderKind::Ollama);
        assert_eq!(EmbedderKind::parse("Ollama"), EmbedderKind::Ollama);
        assert_eq!(EmbedderKind::parse("OLLAMA"), EmbedderKind::Ollama);
    }

    #[test]
    fn parse_defaults_to_hash_for_unknown() {
        assert_eq!(EmbedderKind::parse("hash"), EmbedderKind::Hash);
        assert_eq!(EmbedderKind::parse("bogus"), EmbedderKind::Hash);
        assert_eq!(EmbedderKind::parse(""), EmbedderKind::Hash);
    }

    #[test]
    fn config_default_uses_hash_embedder() {
        let cfg = Config::default();
        assert_eq!(cfg.embedder, EmbedderKind::Hash);
    }
}
