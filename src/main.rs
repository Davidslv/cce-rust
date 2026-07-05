//! # main / cli — command-line entry points
//!
//! **Why this file exists:** SPEC §9 requires a single `cce` executable exposing
//! `index`, `search`, `stats`, `bench`, and `conformance`. This binary parses
//! arguments and drives the library, keeping all algorithm logic in `lib`.
//!
//! **What it is / does:** Defines the clap command tree, resolves store paths,
//! selects the embedder backend (with Ollama health-check + fallback), and
//! prints human or JSON output. Errors exit non-zero with a clear message;
//! invalid/empty inputs return empty results rather than crashing.
//!
//! **Responsibilities:**
//! - Own argument parsing, store-path resolution, and output formatting.
//! - It does NOT implement chunking, embedding, or retrieval — it calls `lib`.

use cce::chunker::token_count;
use cce::config::{
    EmbedderKind, DEFAULT_DASHBOARD_PORT, DEFAULT_INPUT_PRICE_PER_MILLION,
    LOW_CONFIDENCE_THRESHOLD, METRICS_FILE,
};
use cce::embedder::{format6, Embedder, HashEmbedder, OllamaEmbedder};
use cce::metrics::{HexIdSource, IndexRecord, MetricsWriter, SearchRecord, SystemClock};
use cce::retriever::{search, SearchResult};
use cce::store::{default_store_path, Index};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "cce", version, about = "Code Context Engine (clean-room Rust, SPEC v1.0)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk, chunk, embed and persist a directory.
    Index {
        dir: PathBuf,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long, default_value = "hash")]
        embedder: String,
        /// Do not append an index event to the metrics log.
        #[arg(long)]
        no_metrics: bool,
    },
    /// Search a persisted index.
    Search {
        query: String,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        top_k: Option<usize>,
        #[arg(long)]
        no_graph: bool,
        #[arg(long)]
        json: bool,
        /// Do not append a search event to the metrics log.
        #[arg(long)]
        no_metrics: bool,
    },
    /// Rate a past search result helpful or not (DASHBOARD-SPEC §5).
    Feedback {
        /// The query-id printed by `cce search` (the target search event id).
        query_id: String,
        #[arg(long)]
        helpful: bool,
        #[arg(long)]
        not_helpful: bool,
        #[arg(long, default_value = "")]
        note: String,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        metrics: Option<PathBuf>,
    },
    /// Serve the local, read-only metrics dashboard (DASHBOARD-SPEC §6).
    Dashboard {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        metrics: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        /// USD price per 1M input tokens for the $-saved estimate.
        #[arg(long)]
        price: Option<f64>,
        /// Suppress any browser-open behavior (this build only prints the URL).
        #[arg(long)]
        no_open: bool,
    },
    /// Print statistics about a persisted index.
    Stats {
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Benchmark the pipeline on a real repository for one language (SPEC-V2 §8).
    Bench {
        repo_dir: PathBuf,
        #[arg(long)]
        queries: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        /// Corpus commit to record in the report (default: git HEAD).
        #[arg(long)]
        commit: Option<String>,
        /// Human-readable corpus name for the report.
        #[arg(long, default_value = "pallets/flask@3.0.3")]
        name: String,
        /// Language to benchmark: ruby, rust, typescript, c (or python default).
        #[arg(long, default_value = "python")]
        lang: String,
    },
    /// Emit conformance.json for a fixture directory (SPEC-V2 §7).
    Conformance {
        fixture_dir: PathBuf,
        #[arg(short = 'o', long, default_value = "conformance.json")]
        output: PathBuf,
    },
    /// List the registered language packs, or validate them (SPEC-V2 §5).
    Packs {
        /// Run the three validator layers over every pack; exit non-zero on failure.
        #[arg(long)]
        validate: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Index { dir, store, embedder, no_metrics } => {
            cmd_index(&dir, store, &embedder, !no_metrics)
        }
        Command::Search { query, dir, store, top_k, no_graph, json, no_metrics } => {
            cmd_search(&query, dir, store, top_k, no_graph, json, !no_metrics)
        }
        Command::Feedback { query_id, helpful, not_helpful, note, dir, store, metrics } => {
            cmd_feedback(&query_id, helpful, not_helpful, &note, dir, store, metrics)
        }
        Command::Dashboard { dir, store, metrics, port, price, no_open } => {
            cmd_dashboard(dir, store, metrics, port, price, no_open)
        }
        Command::Stats { store, dir } => cmd_stats(store, dir),
        Command::Bench { repo_dir, queries, store, commit, name, lang } => {
            cmd_bench(&repo_dir, queries, store, commit, &name, &lang)
        }
        Command::Conformance { fixture_dir, output } => cmd_conformance(&fixture_dir, &output),
        Command::Packs { validate } => cmd_packs(validate),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the store path for read commands: --store, else --dir/.cce, else ./.cce.
fn resolve_read_store(store: Option<PathBuf>, dir: Option<PathBuf>) -> PathBuf {
    if let Some(s) = store {
        s
    } else if let Some(d) = dir {
        default_store_path(&d)
    } else {
        default_store_path(Path::new("."))
    }
}

/// The metrics log lives beside the index in the store dir: `<store-dir>/metrics.jsonl`.
fn metrics_beside_store(store_path: &Path) -> PathBuf {
    match store_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(METRICS_FILE),
        _ => PathBuf::from(METRICS_FILE),
    }
}

/// Resolve the metrics-log path for feedback/dashboard: explicit --metrics wins,
/// else it sits beside the resolved store (from --store/--dir or the default).
fn resolve_metrics_path(
    metrics: Option<PathBuf>,
    store: Option<PathBuf>,
    dir: Option<PathBuf>,
) -> PathBuf {
    if let Some(m) = metrics {
        m
    } else {
        metrics_beside_store(&resolve_read_store(store, dir))
    }
}

/// Build an embedder for indexing, health-checking Ollama and falling back.
fn build_embedder(kind: EmbedderKind) -> Box<dyn Embedder> {
    match kind {
        EmbedderKind::Hash => Box::new(HashEmbedder),
        EmbedderKind::Ollama => {
            let oll = OllamaEmbedder::default();
            if oll.healthy() {
                eprintln!("using ollama embedder ({} @ {})", oll.model, oll.base_url);
                Box::new(oll)
            } else {
                eprintln!(
                    "warning: Ollama unreachable at {}; falling back to the hash embedder",
                    oll.base_url
                );
                Box::new(HashEmbedder)
            }
        }
    }
}

fn cmd_index(
    dir: &Path,
    store: Option<PathBuf>,
    embedder: &str,
    metrics_enabled: bool,
) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let kind = EmbedderKind::parse(embedder);
    let emb = build_embedder(kind);
    let store_path = store.unwrap_or_else(|| default_store_path(dir));

    let start = std::time::Instant::now();
    let (index, stats) = Index::build_from_dir(dir, emb.as_ref());
    index.save(&store_path).map_err(|e| e.to_string())?;
    let elapsed = start.elapsed().as_secs_f64();

    // Best-effort metrics: an index event (DASHBOARD-SPEC §2.2). Never fatal.
    let index_bytes = std::fs::metadata(&store_path).map(|m| m.len()).unwrap_or(0);
    let clock = SystemClock;
    let ids = HexIdSource::default();
    let writer =
        MetricsWriter::new(metrics_beside_store(&store_path), &clock, &ids, metrics_enabled);
    writer.log_index(&IndexRecord {
        files_indexed: stats.files_indexed,
        chunks: stats.total_chunks,
        index_bytes,
        duration_ms: elapsed * 1000.0,
        embedder: index.embedder_name.clone(),
        full: true,
    });

    println!("Indexed {}", dir.display());
    println!("  files indexed : {}", stats.files_indexed);
    println!("  files skipped : {}", stats.files_skipped);
    println!("  total chunks  : {}", stats.total_chunks);
    println!("  embedder      : {}", index.embedder_name);
    println!("  store         : {}", store_path.display());
    println!("  elapsed       : {elapsed:.3}s");
    Ok(())
}

fn cmd_search(
    query: &str,
    dir: Option<PathBuf>,
    store: Option<PathBuf>,
    top_k: Option<usize>,
    no_graph: bool,
    json: bool,
    metrics_enabled: bool,
) -> Result<(), String> {
    let store_path = resolve_read_store(store, dir);
    let index = Index::load(&store_path)
        .map_err(|e| format!("could not load store {}: {e}", store_path.display()))?;
    let top_k = top_k.unwrap_or(cce::config::DEFAULT_TOP_K);
    let graph_enabled = !no_graph;

    // Use the backend recorded at index time; fall back to hash for search.
    let emb: Box<dyn Embedder> = if index.embedder_name == "ollama" {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            Box::new(oll)
        } else {
            eprintln!("warning: index used ollama but it is unreachable; query embedded with hash");
            Box::new(HashEmbedder)
        }
    } else {
        Box::new(HashEmbedder)
    };

    let start = std::time::Instant::now();
    let results = search(&index, emb.as_ref(), query, top_k, graph_enabled);
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    // Best-effort metrics: a search event (DASHBOARD-SPEC §2.1). The write is
    // fail-open, so it never affects the result or the exit code.
    let record = build_search_record(&index, &results, query, top_k, graph_enabled, latency_ms);
    let clock = SystemClock;
    let ids = HexIdSource::default();
    let writer =
        MetricsWriter::new(metrics_beside_store(&store_path), &clock, &ids, metrics_enabled);
    let query_id = writer.log_search(&record);

    if json {
        print!("{}", results_json(&results, query_id.as_deref()));
    } else {
        print_human(&results);
        if let Some(id) = &query_id {
            println!("query-id: {id}  ·  rate with: cce feedback {id} --helpful|--not-helpful");
        }
    }
    Ok(())
}

/// Assemble a search metrics record from the results (DASHBOARD-SPEC §2.1, §3).
fn build_search_record(
    index: &Index,
    results: &[SearchResult],
    query: &str,
    top_k: usize,
    graph_enabled: bool,
    latency_ms: f64,
) -> SearchRecord {
    let result_count = results.len();
    let baseline_tokens = index.baseline_tokens(results.iter().map(|r| r.file_path.as_str()));
    let served_tokens: u64 = results.iter().map(|r| token_count(&r.content) as u64).sum();
    let tokens_saved = baseline_tokens.saturating_sub(served_tokens);
    let savings_ratio = if baseline_tokens == 0 {
        0.0
    } else {
        tokens_saved as f64 / baseline_tokens as f64
    };
    let top_score = results.first().map(|r| r.score).unwrap_or(0.0);
    let mean_score = if results.is_empty() {
        0.0
    } else {
        results.iter().map(|r| r.score).sum::<f64>() / results.len() as f64
    };
    let empty = result_count == 0;
    let low_confidence = !empty && top_score < LOW_CONFIDENCE_THRESHOLD;
    SearchRecord {
        query: query.to_string(),
        top_k,
        graph_enabled,
        embedder: index.embedder_name.clone(),
        result_count,
        baseline_tokens,
        served_tokens,
        tokens_saved,
        savings_ratio,
        top_score,
        mean_score,
        empty,
        low_confidence,
        latency_ms,
    }
}

fn results_json(results: &[SearchResult], query_id: Option<&str>) -> String {
    let items: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "rank": r.rank,
                "chunk_id": r.chunk_id,
                "file_path": r.file_path,
                "start_line": r.start_line,
                "end_line": r.end_line,
                "chunk_type": r.chunk_type,
                "kind": r.kind,
                "score": format6(r.score),
            })
        })
        .collect();
    // DASHBOARD-SPEC §5: --json gains a top-level `query_id` field (the object
    // now wraps the results array).
    let body = serde_json::json!({ "query_id": query_id, "results": items });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

fn print_human(results: &[SearchResult]) {
    if results.is_empty() {
        println!("(no results)");
        return;
    }
    for r in results {
        let snippet: String = r.content.lines().next().unwrap_or("").chars().take(80).collect();
        println!(
            "{:>2}. [{}] {}:{}-{} ({}/{})\n    {}",
            r.rank,
            format6(r.score),
            r.file_path,
            r.start_line,
            r.end_line,
            r.chunk_type,
            r.kind,
            snippet.trim()
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_feedback(
    query_id: &str,
    helpful: bool,
    not_helpful: bool,
    note: &str,
    dir: Option<PathBuf>,
    store: Option<PathBuf>,
    metrics: Option<PathBuf>,
) -> Result<(), String> {
    // Exactly one of --helpful / --not-helpful is required (DASHBOARD-SPEC §5).
    if helpful == not_helpful {
        return Err("provide exactly one of --helpful or --not-helpful".to_string());
    }
    let metrics_path = resolve_metrics_path(metrics, store, dir);

    // If no search event with this id exists, warn but still record it (our
    // documented choice: feedback is cheap and a later re-index may reveal it).
    let log = cce::metrics::read_log(&metrics_path);
    let known = log.events.iter().any(|e| match e {
        cce::metrics::Event::Search(s) => s.id == query_id,
        _ => false,
    });
    if !known {
        eprintln!(
            "warning: no search event with id {query_id} in {}; recording anyway",
            metrics_path.display()
        );
    }

    let clock = SystemClock;
    let ids = HexIdSource::default();
    let writer = MetricsWriter::new(metrics_path, &clock, &ids, true);
    match writer.log_feedback(query_id, helpful, note) {
        Some(id) => {
            let verdict = if helpful { "helpful" } else { "not helpful" };
            println!("recorded feedback ({verdict}) for {query_id}  [event {id}]");
            Ok(())
        }
        None => Err("could not write feedback to the metrics log".to_string()),
    }
}

fn cmd_dashboard(
    dir: Option<PathBuf>,
    store: Option<PathBuf>,
    metrics: Option<PathBuf>,
    port: Option<u16>,
    price: Option<f64>,
    _no_open: bool,
) -> Result<(), String> {
    let metrics_path = resolve_metrics_path(metrics, store, dir);
    let port = port.unwrap_or(DEFAULT_DASHBOARD_PORT);
    let price = price.unwrap_or(DEFAULT_INPUT_PRICE_PER_MILLION);
    cce::dashboard::run(metrics_path, price, port).map_err(|e| format!("dashboard failed: {e}"))
}

fn cmd_stats(store: Option<PathBuf>, dir: Option<PathBuf>) -> Result<(), String> {
    let store_path = resolve_read_store(store, dir);
    let index = Index::load(&store_path)
        .map_err(|e| format!("could not load store {}: {e}", store_path.display()))?;

    let chunk_count = index.chunks.len();
    let file_count = index.files().len();
    let mut per_lang: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut per_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut total_tokens = 0usize;
    for c in &index.chunks {
        *per_lang.entry(c.language.clone()).or_insert(0) += 1;
        *per_kind.entry(c.kind.clone()).or_insert(0) += 1;
        total_tokens += c.token_count;
    }
    let avg_tokens = if chunk_count > 0 {
        total_tokens as f64 / chunk_count as f64
    } else {
        0.0
    };
    let size = std::fs::metadata(&store_path).map(|m| m.len()).unwrap_or(0);

    println!("Store: {}", store_path.display());
    println!("  chunks         : {chunk_count}");
    println!("  files          : {file_count}");
    println!("  avg token/chunk: {avg_tokens:.1}");
    println!("  store size     : {} bytes", size);
    println!("  by language:");
    for (lang, n) in &per_lang {
        println!("    {lang:<12}: {n}");
    }
    println!("  by kind:");
    for (kind, n) in &per_kind {
        println!("    {kind:<20}: {n}");
    }
    Ok(())
}

/// `cce packs` / `cce packs --validate` (SPEC-V2 §5): list registered packs, or
/// run the three validator layers over every pack and exit non-zero on failure.
fn cmd_packs(validate: bool) -> Result<(), String> {
    let registry = cce::packs::default_registry();
    if validate {
        let reports = cce::packs::validators::validate_all(&registry);
        let mut failed = 0usize;
        for report in &reports {
            if report.ok() {
                println!("[pack:{}] ok", report.name);
            } else {
                failed += 1;
                for d in &report.diagnostics {
                    println!("{d}");
                }
            }
        }
        if failed > 0 {
            return Err(format!("{failed} pack(s) failed validation"));
        }
        println!("all {} packs passed validation", reports.len());
        Ok(())
    } else {
        println!("Registered language packs ({}):", registry.all().len());
        for pack in registry.all() {
            println!(
                "  {:<12} {:<24} {} fn / {} class types · grammar: {} node kinds",
                pack.name(),
                pack.extensions().join(","),
                pack.function_types().len(),
                pack.class_types().len(),
                pack.grammar().node_kind_count(),
            );
        }
        Ok(())
    }
}

fn cmd_bench(
    repo_dir: &Path,
    _queries: Option<PathBuf>,
    _store: Option<PathBuf>,
    commit: Option<String>,
    name: &str,
    lang: &str,
) -> Result<(), String> {
    if !repo_dir.is_dir() {
        return Err(format!("not a directory: {}", repo_dir.display()));
    }
    let commit = commit.unwrap_or_else(|| detect_commit(repo_dir));
    let report = cce::bench::run(repo_dir, &commit, name, lang);

    let out_path = Path::new("docs/BENCHMARKS.md");
    if let Some(p) = out_path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::write(out_path, &report.markdown).map_err(|e| e.to_string())?;

    println!(
        "Benchmark complete ({}, {}, commit {}):",
        report.corpus_name, report.language, report.commit
    );
    println!("  files/chunks : {}/{}", report.total_files, report.total_chunks);
    println!(
        "  index        : {:.3}s ({:.1} chunks/s)",
        report.index_seconds, report.chunks_per_sec
    );
    println!("  latency      : p50 {:.3}ms  p95 {:.3}ms", report.p50_ms, report.p95_ms);
    println!(
        "  recall@5/@10 : {:.1}% / {:.1}%",
        report.recall_at_5 * 100.0,
        report.recall_at_10 * 100.0
    );
    println!("  token savings: {:.1}%", report.token_savings_pct);
    println!("  wrote        : {}", out_path.display());
    Ok(())
}

/// Detect the git commit of a repo dir; "unknown" if not a git checkout.
fn detect_commit(repo_dir: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn cmd_conformance(fixture_dir: &Path, output: &Path) -> Result<(), String> {
    if !fixture_dir.is_dir() {
        return Err(format!("not a directory: {}", fixture_dir.display()));
    }
    let json = cce::conformance::generate(fixture_dir);
    std::fs::write(output, format!("{json}\n")).map_err(|e| e.to_string())?;
    println!("wrote {}", output.display());
    Ok(())
}
