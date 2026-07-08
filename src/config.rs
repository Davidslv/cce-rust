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

/// Chunks embedded per fallible batch call on the index path (issue #38).
/// Bounds the request size for HTTP backends (Ollama `POST /api/embed`), so
/// indexing issues ceil(chunks / EMBED_BATCH_SIZE) requests instead of one
/// per chunk. Purely an execution detail: vectors, chunk ids, and conformance
/// output are identical for any positive value (the hash backend's default
/// batch impl still maps its pure per-text embed over the inputs).
pub const EMBED_BATCH_SIZE: usize = 64;

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

// --- Workspace mode (SPEC-V2.2 §1) ---

/// The workspace manifest filename, written under the workspace root `.cce/`.
pub const WORKSPACE_FILE: &str = "workspace.yml";
/// The cross-member dependency graph filename, under the root `.cce/`.
pub const WORKSPACE_GRAPH_FILE: &str = "workspace-graph.json";
/// Max number of distinct target members a single federated search expands into
/// via cross-member dependency edges (SPEC-V2.2 §6).
pub const GRAPH_MAX_BONUS_MEMBERS: usize = 2;
/// Max chunks pulled from each cross-member target during graph expansion.
pub const GRAPH_BONUS_MEMBER_CHUNKS: usize = 2;

// --- Retrieval / L2 chunk compression (SPEC-V2.5 §2, §5) ---

/// The default L2 detail level `context_search` serves at when neither the tool
/// call nor `.cce/config` overrides it (SPEC-V2.5 §5, `retrieval.detail`). Chosen
/// to save by default: compact = signature + doc + first body line + elision.
pub const DEFAULT_RETRIEVAL_DETAIL: crate::compress::DetailLevel =
    crate::compress::DetailLevel::Compact;

/// The `retrieval.*` runtime configuration (SPEC-V2.5 §5). Only `detail` is wired
/// in Stage ② (L2); the other keys (`top_k`, `confidence_threshold`, `max_tokens`)
/// remain tool inputs / later stages. All keys optional; absent ⇒ the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalConfig {
    pub detail: crate::compress::DetailLevel,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        RetrievalConfig { detail: DEFAULT_RETRIEVAL_DETAIL }
    }
}

impl RetrievalConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `retrieval:` block, an
    /// absent `detail`, or an unrecognised value all fall back to the default.
    pub fn from_yaml(text: &str) -> RetrievalConfig {
        #[derive(serde::Deserialize)]
        struct RawRetrieval {
            detail: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            retrieval: Option<RawRetrieval>,
        }
        let mut cfg = RetrievalConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(d) = raw.retrieval.and_then(|r| r.detail) {
                if let Some(level) = crate::compress::DetailLevel::parse(&d) {
                    cfg.detail = level;
                }
            }
        }
        cfg
    }

    /// Load the retrieval config for `root`: the per-project `.cce/config` if it
    /// exists and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> RetrievalConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| RetrievalConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

// --- Output compression / L4 (SPEC-V2.5 §2 Layer 4, §5) ---

/// The L4 output-compression level `cce init` writes into CLAUDE.md and the
/// `set_output_compression` MCP tool switches at runtime (SPEC-V2.5 §2 Layer 4).
/// Each level maps to a static, byte-pinned instruction block — the transform is a
/// pure function of the level, so both engines emit identical bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputLevel {
    /// No output rules — the agent's default verbosity.
    Off,
    /// Be concise; drop filler/preamble/postamble.
    Lite,
    /// Fewest correct words; code as minimal diffs, never whole files; no pre/postamble.
    /// The config default (SPEC-V2.5 §5, `output.level`).
    Standard,
    /// Standard + telegraphic prose; code as minimal diffs only.
    Max,
}

impl OutputLevel {
    /// Parse the config/tool string form (case-insensitive). Unknown ⇒ `None`, so
    /// callers can surface an actionable error rather than silently defaulting.
    pub fn parse(s: &str) -> Option<OutputLevel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(OutputLevel::Off),
            "lite" => Some(OutputLevel::Lite),
            "standard" => Some(OutputLevel::Standard),
            "max" => Some(OutputLevel::Max),
            _ => None,
        }
    }

    /// The canonical string form.
    pub const fn as_str(&self) -> &'static str {
        match self {
            OutputLevel::Off => "off",
            OutputLevel::Lite => "lite",
            OutputLevel::Standard => "standard",
            OutputLevel::Max => "max",
        }
    }
}

/// The default L4 output level when neither `.cce/config` nor a tool call overrides
/// it (SPEC-V2.5 §5, `output.level`). Chosen to save by default: `standard`.
pub const DEFAULT_OUTPUT_LEVEL: OutputLevel = OutputLevel::Standard;

/// The `output.*` runtime configuration (SPEC-V2.5 §5). Only `level` is defined.
/// Absent block / absent key / unrecognised value all fall back to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    pub level: OutputLevel,
}

impl Default for OutputConfig {
    fn default() -> Self {
        OutputConfig { level: DEFAULT_OUTPUT_LEVEL }
    }
}

impl OutputConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `output:` block, an
    /// absent `level`, or an unrecognised value all fall back to the default.
    pub fn from_yaml(text: &str) -> OutputConfig {
        #[derive(serde::Deserialize)]
        struct RawOutput {
            level: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            output: Option<RawOutput>,
        }
        let mut cfg = OutputConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(l) = raw.output.and_then(|o| o.level) {
                if let Some(level) = OutputLevel::parse(&l) {
                    cfg.level = level;
                }
            }
        }
        cfg
    }

    /// Load the output config for `root`: the per-project `.cce/config` if it
    /// exists and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> OutputConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| OutputConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

// --- Memory recall / L5 (SPEC-V2.5 §2 Layer 5, §5) ---

/// The default for `memory.enabled` (SPEC-V2.5 §5): memory recall is ON by default.
/// Absent config / absent block / absent key all resolve to this.
pub const DEFAULT_MEMORY_ENABLED: bool = true;

/// The `memory.*` runtime configuration (SPEC-V2.5 §5). Only `enabled` is defined:
/// when false, the `record_decision`/`session_recall` tools become explicit no-ops.
/// Absent block / absent key / bad YAML all fall back to the default (enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryConfig {
    pub enabled: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig { enabled: DEFAULT_MEMORY_ENABLED }
    }
}

impl MemoryConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `memory:` block, an
    /// absent `enabled`, or unparseable YAML all fall back to the default.
    pub fn from_yaml(text: &str) -> MemoryConfig {
        #[derive(serde::Deserialize)]
        struct RawMemory {
            enabled: Option<bool>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            memory: Option<RawMemory>,
        }
        let mut cfg = MemoryConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(e) = raw.memory.and_then(|m| m.enabled) {
                cfg.enabled = e;
            }
        }
        cfg
    }

    /// Load the memory config for `root`: the per-project `.cce/config` if it exists
    /// and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> MemoryConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| MemoryConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

// --- Turn summarization / L6 (SPEC-V2.5 §2 Layer 6, §5) ---

/// The default for `summarization.auto_tokens` (SPEC-V2.5 §5): `null` ⇒ MANUAL ONLY.
/// Turn summarization has no auto-trigger unless the project sets a threshold; the
/// server never calls a model and never auto-injects — it only exposes a deterministic
/// "due" signal derived from the offline `cce.tokens/v1` served-token counter.
pub const DEFAULT_SUMMARIZATION_AUTO_TOKENS: Option<u64> = None;

/// The `summarization.*` runtime configuration (SPEC-V2.5 §5). Only `auto_tokens` is
/// defined: the returned-token total (counted with `cce.tokens/v1`) above which the
/// session MAY be summarized. `None` (config `null`, the default) means manual only.
/// Absent block / absent key / bad YAML all fall back to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizationConfig {
    pub auto_tokens: Option<u64>,
}

impl Default for SummarizationConfig {
    fn default() -> Self {
        SummarizationConfig { auto_tokens: DEFAULT_SUMMARIZATION_AUTO_TOKENS }
    }
}

impl SummarizationConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `summarization:` block, an
    /// absent/`null` `auto_tokens`, or unparseable YAML all fall back to the default
    /// (`None` ⇒ manual only). A non-integer/negative value fails the parse ⇒ default.
    pub fn from_yaml(text: &str) -> SummarizationConfig {
        #[derive(serde::Deserialize)]
        struct RawSummarization {
            auto_tokens: Option<u64>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            summarization: Option<RawSummarization>,
        }
        let mut cfg = SummarizationConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(s) = raw.summarization {
                cfg.auto_tokens = s.auto_tokens;
            }
        }
        cfg
    }

    /// Load the summarization config for `root`: the per-project `.cce/config` if it
    /// exists and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> SummarizationConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| SummarizationConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

// --- Markdown heading-chunker / Knowledge sources (SPEC-V2.6 §2/§8) ---

/// The default `markdown.max_section_tokens` (SPEC-V2.6 §8): the byte-pinned split
/// budget. A heading section whose estimated token count exceeds this splits at its
/// deeper headings; below it, deeper headings roll into their parent.
pub const DEFAULT_MARKDOWN_MAX_SECTION_TOKENS: usize = 400;

/// The `markdown.*` runtime configuration (SPEC-V2.6 §8). Only `max_section_tokens`
/// is defined. Absent block / absent key / bad YAML all fall back to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownConfig {
    pub max_section_tokens: usize,
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        MarkdownConfig { max_section_tokens: DEFAULT_MARKDOWN_MAX_SECTION_TOKENS }
    }
}

impl MarkdownConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `markdown:` block, an
    /// absent/zero `max_section_tokens`, or bad YAML all fall back to the default.
    pub fn from_yaml(text: &str) -> MarkdownConfig {
        #[derive(serde::Deserialize)]
        struct RawMarkdown {
            max_section_tokens: Option<usize>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            markdown: Option<RawMarkdown>,
        }
        let mut cfg = MarkdownConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(t) = raw.markdown.and_then(|m| m.max_section_tokens) {
                if t > 0 {
                    cfg.max_section_tokens = t;
                }
            }
        }
        cfg
    }

    /// Load the markdown config for `root`: the per-project `.cce/config` if it
    /// exists and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> MarkdownConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| MarkdownConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

/// The default for `knowledge.enabled` (SPEC-V2.6 §8): knowledge ingest/retrieval
/// is ON by default. Absent config / absent block / absent key all resolve to this.
pub const DEFAULT_KNOWLEDGE_ENABLED: bool = true;

/// The default `knowledge.min_score` (SPEC-V2.6 §8): the recall precision floor,
/// shared with L5 memory recall (`MEMORY_RECALL_MIN_SCORE`). A knowledge hit below
/// this hybrid score is dropped so a loosely-related/stale record never surfaces.
pub const DEFAULT_KNOWLEDGE_MIN_SCORE: f64 = 0.30;

/// The default `knowledge.default_source` (SPEC-V2.6 §5/§8): when the caller omits
/// `source` AND a knowledge store exists, retrieval blends `both`. (When no store
/// exists, retrieval always resolves to `code`, preserving today's behaviour.)
pub const DEFAULT_KNOWLEDGE_SOURCE: &str = "both";

/// The deterministic "decided + implemented" boost (SPEC-V2.6 §5): a knowledge
/// record whose `links` include a merged-PR reference is intent AND implementation,
/// so its base hybrid score is scaled by this pinned factor before the final sort.
pub const KNOWLEDGE_MERGED_PR_BOOST: f64 = 1.10;

/// The `knowledge.*` runtime configuration (SPEC-V2.6 §8): `enabled` (Phase A) plus
/// the M4 retrieval keys `min_score` (the recall precision floor) and `default_source`
/// (`code` | `knowledge` | `both`, used only when the caller omits `source`). Absent
/// block / absent key / bad YAML all fall back to the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeConfig {
    pub enabled: bool,
    pub min_score: f64,
    pub default_source: String,
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        KnowledgeConfig {
            enabled: DEFAULT_KNOWLEDGE_ENABLED,
            min_score: DEFAULT_KNOWLEDGE_MIN_SCORE,
            default_source: DEFAULT_KNOWLEDGE_SOURCE.to_string(),
        }
    }
}

impl KnowledgeConfig {
    /// Parse from the `.cce/config` YAML text. Tolerant: no `knowledge:` block, an
    /// absent key, or bad YAML all fall back to the defaults. A `default_source` that
    /// is not one of `code`/`knowledge`/`both` is ignored (falls back to the default).
    pub fn from_yaml(text: &str) -> KnowledgeConfig {
        #[derive(serde::Deserialize)]
        struct RawKnowledge {
            enabled: Option<bool>,
            min_score: Option<f64>,
            default_source: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct RawRoot {
            knowledge: Option<RawKnowledge>,
        }
        let mut cfg = KnowledgeConfig::default();
        if let Ok(raw) = serde_yaml::from_str::<RawRoot>(text) {
            if let Some(k) = raw.knowledge {
                if let Some(e) = k.enabled {
                    cfg.enabled = e;
                }
                if let Some(s) = k.min_score {
                    if s.is_finite() {
                        cfg.min_score = s;
                    }
                }
                if let Some(src) = k.default_source {
                    if matches!(src.as_str(), "code" | "knowledge" | "both") {
                        cfg.default_source = src;
                    }
                }
            }
        }
        cfg
    }

    /// Load the knowledge config for `root`: the per-project `.cce/config` if it
    /// exists and parses, else the default. Offline, read-only.
    pub fn load(root: &std::path::Path) -> KnowledgeConfig {
        std::fs::read_to_string(root.join(".cce").join("config"))
            .ok()
            .map(|t| KnowledgeConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

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

    #[test]
    fn retrieval_default_detail_is_compact() {
        use crate::compress::DetailLevel;
        assert_eq!(RetrievalConfig::default().detail, DetailLevel::Compact);
        assert_eq!(DEFAULT_RETRIEVAL_DETAIL, DetailLevel::Compact);
    }

    #[test]
    fn retrieval_config_reads_detail_and_tolerates_junk() {
        use crate::compress::DetailLevel;
        assert_eq!(
            RetrievalConfig::from_yaml("retrieval:\n  detail: signature\n").detail,
            DetailLevel::Signature
        );
        assert_eq!(
            RetrievalConfig::from_yaml("retrieval:\n  detail: full\n").detail,
            DetailLevel::Full
        );
        // Absent block, absent key, and an unknown value all fall back to compact.
        assert_eq!(RetrievalConfig::from_yaml("sync:\n  lfs: true\n").detail, DetailLevel::Compact);
        assert_eq!(RetrievalConfig::from_yaml("retrieval: {}\n").detail, DetailLevel::Compact);
        assert_eq!(
            RetrievalConfig::from_yaml("retrieval:\n  detail: bogus\n").detail,
            DetailLevel::Compact
        );
        assert_eq!(RetrievalConfig::from_yaml("not: yaml: [").detail, DetailLevel::Compact);
    }

    #[test]
    fn retrieval_config_load_from_disk_and_absent() {
        use crate::compress::DetailLevel;
        let tmp = tempfile::tempdir().unwrap();
        // Absent ⇒ default.
        assert_eq!(RetrievalConfig::load(tmp.path()).detail, DetailLevel::Compact);
        // Present ⇒ honoured.
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "retrieval:\n  detail: signature\n").unwrap();
        assert_eq!(RetrievalConfig::load(tmp.path()).detail, DetailLevel::Signature);
    }

    #[test]
    fn output_level_parse_and_as_str_round_trip() {
        for (s, lvl) in [
            ("off", OutputLevel::Off),
            ("lite", OutputLevel::Lite),
            ("standard", OutputLevel::Standard),
            ("max", OutputLevel::Max),
        ] {
            assert_eq!(OutputLevel::parse(s), Some(lvl));
            assert_eq!(OutputLevel::parse(&s.to_uppercase()), Some(lvl));
            assert_eq!(lvl.as_str(), s);
        }
        // Unknown / blank ⇒ None (so callers can error, not silently default).
        assert_eq!(OutputLevel::parse("bogus"), None);
        assert_eq!(OutputLevel::parse(""), None);
    }

    #[test]
    fn output_default_level_is_standard() {
        assert_eq!(OutputConfig::default().level, OutputLevel::Standard);
        assert_eq!(DEFAULT_OUTPUT_LEVEL, OutputLevel::Standard);
    }

    #[test]
    fn output_config_reads_level_and_tolerates_junk() {
        assert_eq!(OutputConfig::from_yaml("output:\n  level: off\n").level, OutputLevel::Off);
        assert_eq!(OutputConfig::from_yaml("output:\n  level: lite\n").level, OutputLevel::Lite);
        assert_eq!(OutputConfig::from_yaml("output:\n  level: max\n").level, OutputLevel::Max);
        // Absent block, absent key, unknown value, and bad YAML all fall back.
        assert_eq!(OutputConfig::from_yaml("sync:\n  lfs: true\n").level, OutputLevel::Standard);
        assert_eq!(OutputConfig::from_yaml("output: {}\n").level, OutputLevel::Standard);
        assert_eq!(
            OutputConfig::from_yaml("output:\n  level: bogus\n").level,
            OutputLevel::Standard
        );
        assert_eq!(OutputConfig::from_yaml("not: yaml: [").level, OutputLevel::Standard);
    }

    #[test]
    fn output_config_load_from_disk_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // Absent ⇒ default.
        assert_eq!(OutputConfig::load(tmp.path()).level, OutputLevel::Standard);
        // Present ⇒ honoured.
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "output:\n  level: max\n").unwrap();
        assert_eq!(OutputConfig::load(tmp.path()).level, OutputLevel::Max);
    }

    #[test]
    fn memory_default_is_enabled() {
        assert!(MemoryConfig::default().enabled);
        assert_eq!(MemoryConfig::default().enabled, DEFAULT_MEMORY_ENABLED);
    }

    #[test]
    fn memory_config_reads_enabled_and_tolerates_junk() {
        assert!(!MemoryConfig::from_yaml("memory:\n  enabled: false\n").enabled);
        assert!(MemoryConfig::from_yaml("memory:\n  enabled: true\n").enabled);
        // Absent block, absent key, and bad YAML all fall back to the default (true).
        assert!(MemoryConfig::from_yaml("sync:\n  lfs: true\n").enabled);
        assert!(MemoryConfig::from_yaml("memory: {}\n").enabled);
        assert!(MemoryConfig::from_yaml("not: yaml: [").enabled);
    }

    #[test]
    fn memory_config_load_from_disk_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // Absent ⇒ default (enabled).
        assert!(MemoryConfig::load(tmp.path()).enabled);
        // Present ⇒ honoured.
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "memory:\n  enabled: false\n").unwrap();
        assert!(!MemoryConfig::load(tmp.path()).enabled);
    }

    #[test]
    fn summarization_default_is_manual_only() {
        assert_eq!(SummarizationConfig::default().auto_tokens, None);
        assert_eq!(DEFAULT_SUMMARIZATION_AUTO_TOKENS, None);
    }

    #[test]
    fn summarization_config_reads_auto_tokens_and_tolerates_junk() {
        assert_eq!(
            SummarizationConfig::from_yaml("summarization:\n  auto_tokens: 5000\n").auto_tokens,
            Some(5000)
        );
        // Explicit null ⇒ manual only.
        assert_eq!(
            SummarizationConfig::from_yaml("summarization:\n  auto_tokens: null\n").auto_tokens,
            None
        );
        // Absent block, absent key, a negative (invalid u64), and bad YAML all fall back.
        assert_eq!(SummarizationConfig::from_yaml("sync:\n  lfs: true\n").auto_tokens, None);
        assert_eq!(SummarizationConfig::from_yaml("summarization: {}\n").auto_tokens, None);
        assert_eq!(
            SummarizationConfig::from_yaml("summarization:\n  auto_tokens: -3\n").auto_tokens,
            None
        );
        assert_eq!(SummarizationConfig::from_yaml("not: yaml: [").auto_tokens, None);
    }

    #[test]
    fn summarization_config_load_from_disk_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        // Absent ⇒ default (manual only).
        assert_eq!(SummarizationConfig::load(tmp.path()).auto_tokens, None);
        // Present ⇒ honoured.
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "summarization:\n  auto_tokens: 12000\n").unwrap();
        assert_eq!(SummarizationConfig::load(tmp.path()).auto_tokens, Some(12000));
    }

    #[test]
    fn markdown_default_is_400_tokens() {
        assert_eq!(MarkdownConfig::default().max_section_tokens, 400);
        assert_eq!(DEFAULT_MARKDOWN_MAX_SECTION_TOKENS, 400);
    }

    #[test]
    fn markdown_config_reads_budget_and_tolerates_junk() {
        assert_eq!(
            MarkdownConfig::from_yaml("markdown:\n  max_section_tokens: 250\n").max_section_tokens,
            250
        );
        // Absent block, absent key, zero, and bad YAML all fall back to 400.
        assert_eq!(MarkdownConfig::from_yaml("sync:\n  lfs: true\n").max_section_tokens, 400);
        assert_eq!(MarkdownConfig::from_yaml("markdown: {}\n").max_section_tokens, 400);
        assert_eq!(
            MarkdownConfig::from_yaml("markdown:\n  max_section_tokens: 0\n").max_section_tokens,
            400
        );
        assert_eq!(MarkdownConfig::from_yaml("not: yaml: [").max_section_tokens, 400);
    }

    #[test]
    fn markdown_config_load_from_disk_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(MarkdownConfig::load(tmp.path()).max_section_tokens, 400);
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "markdown:\n  max_section_tokens: 120\n").unwrap();
        assert_eq!(MarkdownConfig::load(tmp.path()).max_section_tokens, 120);
    }

    #[test]
    fn knowledge_default_is_enabled() {
        assert!(KnowledgeConfig::default().enabled);
        assert_eq!(KnowledgeConfig::default().enabled, DEFAULT_KNOWLEDGE_ENABLED);
        // M4 retrieval defaults (SPEC-V2.6 §8): the recall floor and the blend source.
        assert_eq!(KnowledgeConfig::default().min_score, DEFAULT_KNOWLEDGE_MIN_SCORE);
        assert_eq!(KnowledgeConfig::default().min_score, 0.30);
        assert_eq!(KnowledgeConfig::default().default_source, "both");
    }

    #[test]
    fn knowledge_config_reads_enabled_and_tolerates_junk() {
        assert!(!KnowledgeConfig::from_yaml("knowledge:\n  enabled: false\n").enabled);
        assert!(KnowledgeConfig::from_yaml("knowledge:\n  enabled: true\n").enabled);
        // Absent block, absent key, and bad YAML all fall back to the default (true).
        assert!(KnowledgeConfig::from_yaml("sync:\n  lfs: true\n").enabled);
        assert!(KnowledgeConfig::from_yaml("knowledge: {}\n").enabled);
        assert!(KnowledgeConfig::from_yaml("not: yaml: [").enabled);
    }

    #[test]
    fn knowledge_config_reads_min_score_and_default_source() {
        let cfg = KnowledgeConfig::from_yaml(
            "knowledge:\n  min_score: 0.5\n  default_source: knowledge\n",
        );
        assert_eq!(cfg.min_score, 0.5);
        assert_eq!(cfg.default_source, "knowledge");
        // `code` and `both` are the other accepted sources.
        assert_eq!(
            KnowledgeConfig::from_yaml("knowledge:\n  default_source: code\n").default_source,
            "code"
        );
        // An unknown source is ignored (falls back to the default `both`).
        assert_eq!(
            KnowledgeConfig::from_yaml("knowledge:\n  default_source: nonsense\n").default_source,
            "both"
        );
        // Absent keys fall back to the defaults.
        assert_eq!(KnowledgeConfig::from_yaml("knowledge: {}\n").min_score, 0.30);
        assert_eq!(KnowledgeConfig::from_yaml("knowledge: {}\n").default_source, "both");
    }

    #[test]
    fn knowledge_config_load_from_disk_and_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(KnowledgeConfig::load(tmp.path()).enabled);
        let cce = tmp.path().join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), "knowledge:\n  enabled: false\n").unwrap();
        assert!(!KnowledgeConfig::load(tmp.path()).enabled);
    }
}
