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

use cce::config::EmbedderKind;
use cce::embedder::{format6, Embedder, HashEmbedder, OllamaEmbedder};
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
    },
    /// Print statistics about a persisted index.
    Stats {
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Benchmark the pipeline on a real repository (SPEC §10).
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
    },
    /// Emit conformance.json for a fixture directory (SPEC §8).
    Conformance {
        fixture_dir: PathBuf,
        #[arg(short = 'o', long, default_value = "conformance.json")]
        output: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Index { dir, store, embedder } => cmd_index(&dir, store, &embedder),
        Command::Search { query, dir, store, top_k, no_graph, json } => {
            cmd_search(&query, dir, store, top_k, no_graph, json)
        }
        Command::Stats { store, dir } => cmd_stats(store, dir),
        Command::Bench { repo_dir, queries, store, commit, name } => {
            cmd_bench(&repo_dir, queries, store, commit, &name)
        }
        Command::Conformance { fixture_dir, output } => cmd_conformance(&fixture_dir, &output),
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

fn cmd_index(dir: &Path, store: Option<PathBuf>, embedder: &str) -> Result<(), String> {
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

    let results = search(&index, emb.as_ref(), query, top_k, graph_enabled);
    if json {
        print!("{}", results_json(&results));
    } else {
        print_human(&results);
    }
    Ok(())
}

fn results_json(results: &[SearchResult]) -> String {
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
                "score": format6(r.score),
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string()) + "\n"
}

fn print_human(results: &[SearchResult]) {
    if results.is_empty() {
        println!("(no results)");
        return;
    }
    for r in results {
        let snippet: String = r.content.lines().next().unwrap_or("").chars().take(80).collect();
        println!(
            "{:>2}. [{}] {}:{}-{} ({})\n    {}",
            r.rank,
            format6(r.score),
            r.file_path,
            r.start_line,
            r.end_line,
            r.chunk_type,
            snippet.trim()
        );
    }
}

fn cmd_stats(store: Option<PathBuf>, dir: Option<PathBuf>) -> Result<(), String> {
    let store_path = resolve_read_store(store, dir);
    let index = Index::load(&store_path)
        .map_err(|e| format!("could not load store {}: {e}", store_path.display()))?;

    let chunk_count = index.chunks.len();
    let file_count = index.files().len();
    let mut per_lang: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut total_tokens = 0usize;
    for c in &index.chunks {
        *per_lang.entry(c.language.clone()).or_insert(0) += 1;
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
    Ok(())
}

fn cmd_bench(
    repo_dir: &Path,
    _queries: Option<PathBuf>,
    _store: Option<PathBuf>,
    commit: Option<String>,
    name: &str,
) -> Result<(), String> {
    if !repo_dir.is_dir() {
        return Err(format!("not a directory: {}", repo_dir.display()));
    }
    let commit = commit.unwrap_or_else(|| detect_commit(repo_dir));
    let report = cce::bench::run(repo_dir, &commit, name);

    let out_path = Path::new("docs/BENCHMARKS.md");
    if let Some(p) = out_path.parent() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::fs::write(out_path, &report.markdown).map_err(|e| e.to_string())?;

    println!("Benchmark complete ({}, commit {}):", report.corpus_name, report.commit);
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
