//! # bench — the benchmark runner behind `cce bench`
//!
//! **Why this file exists:** SPEC-V2 §8 requires headline numbers (index speed,
//! query latency, recall, token savings) measured per language on a pinned real
//! repository using the default hashing embedder, written to `docs/BENCHMARKS.md`.
//!
//! **What it is / does:** Indexes the WHOLE repository exactly as `cce index`
//! does (SPEC-V2 §8) — no extension pre-filter, so the file set is identical to
//! the sibling implementation's — runs the language's labeled query set, and
//! measures index throughput, per-query p50/p95 latency, Recall@5/@10, and mean
//! token savings, then renders the Markdown report. The `language` argument
//! selects only the query set and the report label. The four benchmarked
//! languages are Ruby, Rust, TypeScript, and C (SPEC-V2 §8); Python/JavaScript
//! stay validated packs but ship no labeled corpus (Python's flask set is kept as
//! the back-compatible default).
//!
//! **Responsibilities:**
//! - Own the per-language labeled query sets (SPEC-V2 §8) and the metric math.
//! - Own report rendering.
//! - It does NOT clone the repo (the caller passes an already-checked-out dir) and
//!   it does NOT pre-filter files — whole-repo indexing is the point.

use crate::chunker::token_count;
use crate::embedder::HashEmbedder;
use crate::retriever::search;
use crate::store::Index;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

/// A labeled query: text plus the path substrings that count as a hit (any-of).
struct Labeled {
    query: &'static str,
    expected: &'static [&'static str],
}

/// Python (flask) set — the back-compatible default (base SPEC §10.2).
const PYTHON_QUERIES: &[Labeled] = &[
    Labeled { query: "where are blueprints registered", expected: &["blueprints"] },
    Labeled { query: "application factory and app configuration", expected: &["app"] },
    Labeled { query: "load configuration from environment or file", expected: &["config"] },
    Labeled { query: "session cookie serialization", expected: &["sessions"] },
    Labeled { query: "url routing and rule mapping", expected: &["app", "blueprints"] },
    Labeled { query: "render a template with context", expected: &["templating"] },
    Labeled { query: "command line interface entry point", expected: &["cli"] },
    Labeled { query: "json encoder and decoder for responses", expected: &["json"] },
    Labeled { query: "request and response context management", expected: &["ctx"] },
    Labeled { query: "send a file as a response", expected: &["helpers"] },
];

/// Ruby (sinatra/sinatra) labeled set (SPEC-V2 §8).
const RUBY_QUERIES: &[Labeled] = &[
    Labeled { query: "route matching and dispatch", expected: &["base"] },
    Labeled { query: "render erb/haml template", expected: &["base"] },
    Labeled { query: "session and cookies", expected: &["base"] },
    Labeled { query: "mime type helpers", expected: &["base"] },
    Labeled { query: "middleware stack", expected: &["base"] },
    Labeled { query: "delegator methods", expected: &["base"] },
    Labeled { query: "handle errors and show exceptions", expected: &["show_exceptions"] },
    Labeled { query: "streaming responses", expected: &["base"] },
    Labeled { query: "rack response building", expected: &["base"] },
    Labeled { query: "url helpers", expected: &["base"] },
];

/// Rust (sharkdp/hyperfine) labeled set (SPEC-V2 §8).
const RUST_QUERIES: &[Labeled] = &[
    Labeled { query: "run a benchmark and measure timing", expected: &["benchmark"] },
    Labeled { query: "parse command line options", expected: &["options"] },
    Labeled { query: "export results as json", expected: &["export"] },
    Labeled { query: "export as markdown", expected: &["export"] },
    Labeled { query: "warmup runs", expected: &["benchmark"] },
    Labeled { query: "shell spawning and command execution", expected: &["command"] },
    Labeled { query: "outlier detection statistics", expected: &["outlier"] },
    Labeled { query: "progress bar output", expected: &["benchmark"] },
    Labeled { query: "parameter ranges", expected: &["parameter"] },
    Labeled { query: "timing measurement", expected: &["timer"] },
];

/// TypeScript (pmndrs/zustand) labeled set (SPEC-V2 §8).
const TYPESCRIPT_QUERIES: &[Labeled] = &[
    Labeled { query: "create a store", expected: &["vanilla"] },
    Labeled { query: "react hook to use the store", expected: &["react"] },
    Labeled { query: "persist middleware", expected: &["middleware"] },
    Labeled { query: "subscribe with selector", expected: &["middleware"] },
    Labeled { query: "shallow equality", expected: &["shallow"] },
    Labeled { query: "combine slices", expected: &["middleware"] },
    Labeled { query: "devtools integration", expected: &["middleware"] },
    Labeled { query: "set and get state", expected: &["vanilla"] },
    Labeled { query: "immer middleware", expected: &["middleware"] },
    Labeled { query: "context provider", expected: &["context"] },
];

/// C (jqlang/jq) labeled set (SPEC-V2 §8).
const C_QUERIES: &[Labeled] = &[
    Labeled { query: "parse a json value", expected: &["jv"] },
    Labeled { query: "builtin functions", expected: &["builtin"] },
    Labeled { query: "execute bytecode", expected: &["execute"] },
    Labeled { query: "print/format json output", expected: &["jv_print"] },
    Labeled { query: "lexer/tokenizer", expected: &["lexer"] },
    Labeled { query: "compile the program", expected: &["compile"] },
    Labeled { query: "object and array construction", expected: &["jv"] },
    Labeled { query: "decode number", expected: &["jv"] },
    Labeled { query: "main entry point", expected: &["main"] },
    Labeled { query: "unicode handling", expected: &["jv_unicode"] },
];

/// The labeled query set for a language, or `None` for an unknown language.
fn queries_for(lang: &str) -> Option<&'static [Labeled]> {
    match lang {
        "python" => Some(PYTHON_QUERIES),
        "ruby" => Some(RUBY_QUERIES),
        "rust" => Some(RUST_QUERIES),
        "typescript" => Some(TYPESCRIPT_QUERIES),
        "c" => Some(C_QUERIES),
        _ => None,
    }
}

/// Best-effort `rustc --version`; falls back to a generic label.
fn detect_rustc_version() -> String {
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "rustc (stable)".to_string())
}

/// Percentile (nearest-rank) of a sorted millisecond list.
fn percentile(sorted_ms: &[f64], pct: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let rank = ((pct / 100.0) * sorted_ms.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted_ms.len() - 1);
    sorted_ms[idx]
}

/// Result of a benchmark run, ready to render.
pub struct BenchReport {
    pub language: String,
    pub total_files: usize,
    pub total_chunks: usize,
    pub index_seconds: f64,
    pub chunks_per_sec: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub token_savings_pct: f64,
    pub commit: String,
    pub corpus_name: String,
    pub markdown: String,
}

/// Run the benchmark against `repo_dir` for `language`. Writes nothing; returns
/// the report. The `language` selects only the labeled query set and the report
/// label; an unknown language falls back to the Python (flask) query set.
pub fn run(repo_dir: &Path, commit: &str, corpus_name: &str, language: &str) -> BenchReport {
    let embedder = HashEmbedder;
    let queries = queries_for(language).unwrap_or(PYTHON_QUERIES);

    // --- Index (timed) --- the WHOLE repository, exactly as `cce index` does
    // (SPEC-V2 §8): the normal walker (SPEC §7.1 ignore rules, >2 MB and non-UTF-8
    // skipped), then pack-matched files are AST-chunked and every other text file
    // becomes a fallback `module` chunk. No extension pre-filter, so both language
    // implementations index the identical file set.
    let t0 = Instant::now();
    let (index, stats) =
        Index::build_from_dir(repo_dir, &embedder).expect("hash embedder cannot fail");
    let index_seconds = t0.elapsed().as_secs_f64();
    let total_chunks = stats.total_chunks;
    let total_files = stats.files_indexed;
    let chunks_per_sec = if index_seconds > 0.0 {
        total_chunks as f64 / index_seconds
    } else {
        0.0
    };

    // --- Query latency, recall, token savings ---
    let mut latencies: Vec<f64> = Vec::new();
    let mut hits5 = 0usize;
    let mut hits10 = 0usize;
    let mut savings_acc = 0.0f64;
    let mut savings_n = 0usize;
    let repeats = 5;

    for lab in queries {
        let mut last: Vec<crate::retriever::SearchResult> = Vec::new();
        for _ in 0..repeats {
            let t = Instant::now();
            last = search(&index, &embedder, lab.query, 10, false);
            latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let hit_in = |k: usize| -> bool {
            last.iter().take(k).any(|r| {
                lab.expected.iter().any(|sub| !sub.is_empty() && r.file_path.contains(sub))
            })
        };
        if hit_in(5) {
            hits5 += 1;
        }
        if hit_in(10) {
            hits10 += 1;
        }
        let served: usize = last.iter().map(|r| chunk_token_count(&index, r)).sum();
        let distinct_files: BTreeSet<String> = last.iter().map(|r| r.file_path.clone()).collect();
        let baseline: usize =
            distinct_files.iter().map(|f| whole_file_token_count(repo_dir, f)).sum();
        if baseline > 0 {
            savings_acc += 1.0 - (served as f64 / baseline as f64);
            savings_n += 1;
        }
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50_ms = percentile(&latencies, 50.0);
    let p95_ms = percentile(&latencies, 95.0);
    let n_q = queries.len() as f64;
    let recall_at_5 = hits5 as f64 / n_q;
    let recall_at_10 = hits10 as f64 / n_q;
    let token_savings_pct = if savings_n > 0 {
        (savings_acc / savings_n as f64) * 100.0
    } else {
        0.0
    };

    let markdown = render(
        language,
        corpus_name,
        commit,
        total_files,
        total_chunks,
        index_seconds,
        chunks_per_sec,
        p50_ms,
        p95_ms,
        recall_at_5,
        recall_at_10,
        token_savings_pct,
    );

    BenchReport {
        language: language.to_string(),
        total_files,
        total_chunks,
        index_seconds,
        chunks_per_sec,
        p50_ms,
        p95_ms,
        recall_at_5,
        recall_at_10,
        token_savings_pct,
        commit: commit.to_string(),
        corpus_name: corpus_name.to_string(),
        markdown,
    }
}

/// token_count of the chunk backing a result (looked up by identity).
fn chunk_token_count(index: &Index, r: &crate::retriever::SearchResult) -> usize {
    index.chunks.iter().find(|c| c.chunk_id == r.chunk_id).map(|c| c.token_count).unwrap_or(0)
}

/// token_count of an entire file (the baseline: whole file as one blob).
fn whole_file_token_count(repo_dir: &Path, rel: &str) -> usize {
    match std::fs::read_to_string(repo_dir.join(rel)) {
        Ok(s) => token_count(&s),
        Err(_) => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn render(
    language: &str,
    corpus: &str,
    commit: &str,
    total_files: usize,
    total_chunks: usize,
    index_seconds: f64,
    chunks_per_sec: f64,
    p50_ms: f64,
    p95_ms: f64,
    recall_at_5: f64,
    recall_at_10: f64,
    token_savings_pct: f64,
) -> String {
    let rustc = detect_rustc_version();
    format!(
        "# Benchmarks\n\n\
Generated by `cce bench` using the default deterministic **hash** embedder.\n\n\
## Environment\n\n\
| Field | Value |\n|---|---|\n\
| Query set (language) | {language} |\n\
| Engine | Rust (`{rustc}`) |\n\
| OS / Arch | {os} / {arch} |\n\
| Embedder | hash (SPEC §5.1) |\n\
| Corpus | {corpus} |\n\
| Commit | `{commit}` |\n\n\
## Index\n\n\
| Metric | Value |\n|---|---|\n\
| Total files indexed | {total_files} |\n\
| Total chunks | {total_chunks} |\n\
| Wall-clock seconds | {index_seconds:.3} |\n\
| Chunks / second | {chunks_per_sec:.1} |\n\n\
## Query latency (labeled set, >=5 repeats each)\n\n\
| Metric | Value |\n|---|---|\n\
| p50 | {p50_ms:.3} ms |\n\
| p95 | {p95_ms:.3} ms |\n\n\
## Retrieval quality\n\n\
| Metric | Value |\n|---|---|\n\
| Recall@5 | {r5:.1}% |\n\
| Recall@10 | {r10:.1}% |\n\
| Mean token savings | {savings:.1}% |\n\n\
## Interpretation\n\n\
With the hashing embedder, retrieval is essentially lexical (SPEC §10 note), so \
recall reflects keyword overlap between the query and code identifiers rather \
than semantic similarity. Token savings are large because a query returns a \
handful of function-sized chunks instead of the whole files they live in, which \
is exactly the point of the engine: feed a model precise snippets, not entire \
files. Recall and token-savings numbers are deterministic and should match the \
Ruby implementation on the same corpus; latency is language-specific.\n",
        language = language,
        rustc = rustc,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        corpus = corpus,
        commit = commit,
        total_files = total_files,
        total_chunks = total_chunks,
        index_seconds = index_seconds,
        chunks_per_sec = chunks_per_sec,
        p50_ms = p50_ms,
        p95_ms = p95_ms,
        r5 = recall_at_5 * 100.0,
        r10 = recall_at_10 * 100.0,
        savings = token_savings_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_nearest_rank() {
        let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 50.0), 3.0);
        assert_eq!(percentile(&v, 95.0), 5.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn query_sets_resolve_per_language() {
        assert_eq!(queries_for("ruby").unwrap().len(), 10);
        assert_eq!(queries_for("rust").unwrap().len(), 10);
        assert_eq!(queries_for("typescript").unwrap().len(), 10);
        assert_eq!(queries_for("c").unwrap().len(), 10);
        assert!(queries_for("cobol").is_none());
    }

    #[test]
    fn bench_indexes_the_whole_repo_like_cce_index() {
        // The base fixture is a tiny "repo". Whole-repo indexing (SPEC-V2 §8):
        // auth.py (4) + payments.py (2) + README.md fallback (1) = 7 chunks over
        // 3 files (metrics_sample.jsonl is skipped by the walker's .jsonl rule).
        let dir =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"));
        let rep = run(&dir, "fixture", "test-fixture", "python");
        assert_eq!(rep.total_files, 3);
        assert_eq!(rep.total_chunks, 7);
        assert_eq!(rep.language, "python");
        assert!(rep.markdown.contains("# Benchmarks"));
    }

    #[test]
    fn bench_does_not_pre_filter_by_language() {
        // With lang=ruby over the samples corpus, whole-repo indexing still covers
        // ALL seven sample files (six languages + the notes.md fallback), not just
        // ruby.rb — proving the extension pre-filter is gone (SPEC-V2 §8).
        let dir =
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"));
        let rep = run(&dir, "fixture", "samples", "ruby");
        assert_eq!(rep.language, "ruby");
        assert_eq!(rep.total_files, 7);
        assert_eq!(rep.total_chunks, 21);
        assert!(rep.markdown.contains("| Query set (language) | ruby |"));
    }
}
