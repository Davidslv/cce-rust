//! # bench — the benchmark runner behind `cce bench`
//!
//! **Why this file exists:** SPEC §10 requires headline numbers (index speed,
//! query latency, recall, token savings) measured on a pinned real repository
//! using the default hashing embedder, written to `docs/BENCHMARKS.md`.
//!
//! **What it is / does:** Indexes a given repo directory, runs a labeled query
//! set, measures index throughput, per-query p50/p95 latency, Recall@5/@10, and
//! mean token savings, and renders the Markdown report.
//!
//! **Responsibilities:**
//! - Own the default labeled query set (SPEC §10.2) and the metric math.
//! - Own report rendering.
//! - It does NOT clone the repo (the caller passes an already-checked-out dir).

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

/// SPEC §10.2 default query set. The routing query accepts app OR blueprints.
const DEFAULT_QUERIES: &[Labeled] = &[
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

/// Run the benchmark against `repo_dir`. Writes nothing; returns the report.
pub fn run(repo_dir: &Path, commit: &str, corpus_name: &str) -> BenchReport {
    let embedder = HashEmbedder;

    // --- Index (timed) --- Python sources only, per SPEC §10.1.
    let t0 = Instant::now();
    let (index, stats) =
        Index::build_from_dir_filtered(repo_dir, &embedder, |p| p.ends_with(".py"));
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

    for lab in DEFAULT_QUERIES {
        // Latency: repeat >= 5x
        let mut last: Vec<crate::retriever::SearchResult> = Vec::new();
        for _ in 0..repeats {
            let t = Instant::now();
            last = search(&index, &embedder, lab.query, 10, false);
            latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        // Recall@5 / @10
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
        // Token savings
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
    let n_q = DEFAULT_QUERIES.len() as f64;
    let recall_at_5 = hits5 as f64 / n_q;
    let recall_at_10 = hits10 as f64 / n_q;
    let token_savings_pct = if savings_n > 0 {
        (savings_acc / savings_n as f64) * 100.0
    } else {
        0.0
    };

    let markdown = render(
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
| Language | Rust (`{rustc}`) |\n\
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
    fn bench_runs_on_fixture() {
        // The fixture is a tiny "repo"; ensures the runner wires up end-to-end.
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture"));
        let rep = run(&dir, "fixture", "test-fixture");
        // bench indexes only Python sources (SPEC §10.1): 2 .py files -> 6 chunks.
        assert_eq!(rep.total_chunks, 6);
        assert!(rep.markdown.contains("# Benchmarks"));
    }
}
