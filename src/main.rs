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

use cce::config::{EmbedderKind, DEFAULT_DASHBOARD_PORT, DEFAULT_INPUT_PRICE_PER_MILLION, METRICS_FILE};
use cce::embedder::{format6, Embedder, HashEmbedder, OllamaEmbedder};
use cce::federation::{
    federated_search, load_member_stores, member_metrics, workspace_stats, FedResult,
};
use cce::metrics::{HexIdSource, IdSource, IndexRecord, MetricsWriter, SystemClock};
use cce::retriever::{build_search_record, search, SearchResult};
use cce::store::{default_store_path, Index};
use cce::sync::commands as sync_cmd;
use cce::workspace::{build_graph, build_manifest, Manifest, WorkspaceGraph};
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
    /// Walk, chunk, embed and persist a directory (or every workspace member).
    Index {
        /// Directory to index (single-repo). Optional only with `--workspace`.
        dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long, default_value = "hash")]
        embedder: String,
        /// Do not append an index event to the metrics log.
        #[arg(long)]
        no_metrics: bool,
        /// Disable secret protection (SPEC-V2.1): index sensitive files and store
        /// content verbatim. Off by default — protection is on unless you pass this.
        #[arg(long)]
        allow_secrets: bool,
        /// Federated index: index every member of the workspace at `[<dir>]` into
        /// its own store, then build the cross-member graph (SPEC-V2.2 §4).
        #[arg(long)]
        workspace: bool,
    },
    /// Search a persisted index (or the workspace federation).
    Search {
        query: String,
        /// Workspace root (positional, with `--workspace`; defaults to `.`).
        #[arg(value_name = "DIR")]
        ws_dir: Option<PathBuf>,
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
        /// Federated search over the workspace at `[<dir>]` (SPEC-V2.2 §6).
        #[arg(long)]
        workspace: bool,
        /// Scope a workspace search to these members (comma-separated names).
        #[arg(long)]
        package: Option<String>,
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
        /// Workspace root (positional, with `--workspace`; defaults to `.`).
        #[arg(value_name = "DIR")]
        ws_dir: Option<PathBuf>,
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
        /// Federate every workspace member's metrics into one dashboard with a
        /// per-package breakdown (SPEC-V2.2 §7).
        #[arg(long)]
        workspace: bool,
    },
    /// Print statistics about a persisted index (or the workspace).
    Stats {
        /// Workspace root (positional, with `--workspace`; defaults to `.`).
        #[arg(value_name = "DIR")]
        ws_dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Per-member workspace stats + totals + cross-member edges (SPEC-V2.2 §7).
        #[arg(long)]
        workspace: bool,
    },
    /// Manage a multi-codebase workspace (SPEC-V2.2 §3/§9).
    Workspace {
        #[command(subcommand)]
        cmd: WorkspaceCmd,
    },
    /// Push/pull the index to/from a content-addressed git cache (SPEC-SYNC §5).
    Sync {
        #[command(subcommand)]
        cmd: SyncCmd,
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
    /// Serve CCE as an MCP server over stdio for an agent (SPEC-MCP).
    Mcp {
        /// Project directory to serve (single-repo store resolution).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Explicit store path (overrides `--dir`/cwd resolution).
        #[arg(long)]
        store: Option<PathBuf>,
        /// Federate over the workspace at `--dir`/cwd (SPEC-V2.2).
        #[arg(long)]
        workspace: bool,
    },
    /// Wire up an editor (Claude Code) to use CCE via MCP, plug-and-play (SPEC-MCP).
    Init {
        /// Project directory to initialise (default: current directory).
        dir: Option<PathBuf>,
        /// Target agent. v1 supports `claude`.
        #[arg(long, default_value = "claude")]
        agent: String,
        /// Pull the CI-built index from this sync remote instead of indexing locally.
        #[arg(long)]
        remote: Option<String>,
        /// Force the index refresh (a `--force` sync pull past a sha mismatch).
        #[arg(long)]
        force: bool,
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
    /// Print the seven-bucket savings ledger + totals + a $ estimate (SPEC-V2.5 §3).
    ///
    /// The figures are "vs full-file baseline — not your real end-to-end agent
    /// cost". For the real delta, run `cce eval` (SPEC-V2.5 §7).
    Savings {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long)]
        store: Option<PathBuf>,
        #[arg(long)]
        metrics: Option<PathBuf>,
        /// Emit the ledger as JSON (the same shape as `/api/metrics.savings_by_layer`).
        #[arg(long)]
        json: bool,
    },
    /// Run the real-world A/B eval harness over recorded runs (SPEC-V2.5 §7).
    ///
    /// Correctness-gated (punts excluded) and cost-primary (cost includes
    /// sub-agents). Does not call a model; it aggregates run outputs recorded by
    /// `eval/run.sh` (see `eval/README.md`).
    Eval {
        /// Recorded run outputs (JSONL). Required.
        runs: PathBuf,
        /// The pinned question set with ground truth (JSONL).
        #[arg(long, default_value = "eval/questions.jsonl")]
        questions: PathBuf,
        /// Emit the full report as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands of `cce sync` (SPEC-SYNC §5). All are workspace-aware and
/// offline-first: a missing/unreachable remote never breaks local commands.
#[derive(Subcommand)]
enum SyncCmd {
    /// Configure the remote + local clone and (optionally) enable git-LFS.
    Init {
        /// The cache git repository URL (SSH or HTTPS or file://).
        #[arg(long)]
        remote: String,
        /// Route `*.cce` blobs through git-LFS (recommended; needs `git-lfs`).
        #[arg(long)]
        lfs: bool,
        /// Disable git-LFS explicitly (overrides `--lfs`).
        #[arg(long)]
        no_lfs: bool,
        /// Override the derived `repo_id` (else the normalized git origin).
        #[arg(long)]
        repo_id: Option<String>,
        /// Project/workspace root (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Ensure a hash-index for HEAD/sha, export the artifact, and put it on the remote.
    Push {
        /// The commit to push (default: HEAD). The working tree must be clean.
        #[arg(long)]
        commit: Option<String>,
        /// Push every workspace member, each keyed by its own repo_id@sha.
        #[arg(long)]
        workspace: bool,
        /// Project/workspace root (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Fetch the cache for a sha and install it into `.cce/`.
    Pull {
        /// The commit to pull (default: HEAD).
        #[arg(long)]
        commit: Option<String>,
        /// Pull the remote's latest pushed sha for the default ref.
        #[arg(long, conflicts_with = "commit")]
        latest: bool,
        /// Overwrite a local cache for a different sha (SPEC-SYNC §9.4).
        #[arg(long)]
        force: bool,
        /// Pull every workspace member from its own cache.
        #[arg(long)]
        workspace: bool,
        /// Project/workspace root (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Show the remote, local cache sha, remote latest, and working-tree match.
    Status {
        /// Project/workspace root (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Re-index locally and confirm the pulled artifact's checksum.
    Verify {
        /// The commit to verify (default: the pulled cache's sha, else HEAD).
        #[arg(long)]
        commit: Option<String>,
        /// Project/workspace root (default: current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

/// Subcommands of `cce workspace` (SPEC-V2.2 §3/§9).
#[derive(Subcommand)]
enum WorkspaceCmd {
    /// Detect members and write `<dir>/.cce/workspace.yml` (SPEC-V2.2 §3).
    Init {
        /// Workspace root (default: current directory).
        dir: Option<PathBuf>,
        /// Overwrite an existing manifest.
        #[arg(long)]
        force: bool,
    },
    /// Print the members and the detected cross-member edges (SPEC-V2.2 §3/§5).
    List {
        /// Workspace root (default: current directory).
        dir: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Index { dir, store, embedder, no_metrics, allow_secrets, workspace } => {
            if workspace {
                cmd_index_workspace(dir, &embedder, !no_metrics, allow_secrets)
            } else {
                match dir {
                    Some(d) => cmd_index(&d, store, &embedder, !no_metrics, allow_secrets),
                    None => Err("index requires a directory (or pass --workspace)".to_string()),
                }
            }
        }
        Command::Search {
            query,
            ws_dir,
            dir,
            store,
            top_k,
            no_graph,
            json,
            no_metrics,
            workspace,
            package,
        } => {
            if workspace {
                cmd_search_workspace(&query, ws_dir.or(dir), top_k, no_graph, json, package)
            } else {
                cmd_search(&query, dir, store, top_k, no_graph, json, !no_metrics)
            }
        }
        Command::Feedback { query_id, helpful, not_helpful, note, dir, store, metrics } => {
            cmd_feedback(&query_id, helpful, not_helpful, &note, dir, store, metrics)
        }
        Command::Dashboard { ws_dir, dir, store, metrics, port, price, no_open, workspace } => {
            if workspace {
                cmd_dashboard_workspace(ws_dir.or(dir), port, price)
            } else {
                cmd_dashboard(dir, store, metrics, port, price, no_open)
            }
        }
        Command::Stats { ws_dir, store, dir, workspace } => {
            if workspace {
                cmd_stats_workspace(ws_dir.or(dir))
            } else {
                cmd_stats(store, dir)
            }
        }
        Command::Workspace { cmd } => match cmd {
            WorkspaceCmd::Init { dir, force } => cmd_workspace_init(dir, force),
            WorkspaceCmd::List { dir } => cmd_workspace_list(dir),
        },
        Command::Sync { cmd } => cmd_sync(cmd),
        Command::Bench { repo_dir, queries, store, commit, name, lang } => {
            cmd_bench(&repo_dir, queries, store, commit, &name, &lang)
        }
        Command::Mcp { dir, store, workspace } => cmd_mcp(dir, store, workspace),
        Command::Init { dir, agent, remote, force } => cmd_init_mcp(dir, agent, remote, force),
        Command::Conformance { fixture_dir, output } => cmd_conformance(&fixture_dir, &output),
        Command::Packs { validate } => cmd_packs(validate),
        Command::Savings { dir, store, metrics, json } => cmd_savings(dir, store, metrics, json),
        Command::Eval { runs, questions, json } => cmd_eval(&runs, &questions, json),
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
    allow_secrets: bool,
) -> Result<(), String> {
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let kind = EmbedderKind::parse(embedder);
    let emb = build_embedder(kind);
    let store_path = store.unwrap_or_else(|| default_store_path(dir));

    // SPEC-V2.1: protection is on unless --allow-secrets is passed. Warn loudly
    // when a run opts out, since sensitive files and raw secrets will be stored.
    let protect_secrets = !allow_secrets;
    if allow_secrets {
        eprintln!(
            "warning: --allow-secrets set — secret protection is DISABLED; sensitive files \
             will be indexed and secrets stored verbatim"
        );
    }

    let start = std::time::Instant::now();
    let (index, stats) = Index::build_protected(dir, emb.as_ref(), |_| true, protect_secrets);
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
        sha: cce::sync::git::head_sha(dir),
        source: "local".to_string(),
        sensitive_skipped: stats.sensitive_skipped as u64,
    });

    println!("Indexed {}", dir.display());
    println!("  files indexed     : {}", stats.files_indexed);
    println!("  files skipped     : {}", stats.files_skipped);
    println!("  sensitive skipped : {}", stats.sensitive_skipped);
    println!("  total chunks      : {}", stats.total_chunks);
    println!("  embedder          : {}", index.embedder_name);
    println!("  store             : {}", store_path.display());
    println!("  elapsed           : {elapsed:.3}s");
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
    // fail-open, so it never affects the result or the exit code. `cce search`
    // serves whole chunk bodies (detail:full), so the L2 chunk_compression bucket
    // is zero here — compression is the agent-facing `context_search` path.
    let record = build_search_record(
        &index,
        &results,
        query,
        top_k,
        graph_enabled,
        latency_ms,
        "cli",
        cce::compress::DetailLevel::Full,
    );
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

/// `cce savings` (SPEC-V2.5 §3): print the seven-bucket ledger, totals, and an
/// offline $ estimate. Purely log-derived, no network. Every surface is labelled
/// with the honesty note.
fn cmd_savings(
    dir: Option<PathBuf>,
    store: Option<PathBuf>,
    metrics: Option<PathBuf>,
    json: bool,
) -> Result<(), String> {
    use cce::metrics::Event;
    let metrics_path = resolve_metrics_path(metrics, store, dir);
    let log = cce::metrics::read_log(&metrics_path);
    let buckets = log.events.iter().filter_map(|e| match e {
        Event::Search(s) => Some(&s.savings),
        _ => None,
    });
    let ledger = cce::savings::sum_by_layer(buckets);
    let table = cce::pricing::PriceTable::builtin();
    let dollars = table.dollars_saved(ledger.total.saved_tokens);

    if json {
        let body = serde_json::json!({
            "savings_by_layer": ledger,
            "pricing_id": table.id,
            "estimated_dollars_saved": format!("{dollars:.2}"),
            "source": metrics_path.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()));
        return Ok(());
    }

    println!("CCE savings ledger  ({})", cce::savings::SAVINGS_NOTE);
    println!("  source : {}", metrics_path.display());
    println!("  pricing: {}  (offline, embedded; edit src/pricing.json to change)", table.id);
    println!();
    println!("  {:<26}{:>14}{:>18}", "layer", "saved_tokens", "baseline_tokens");
    for (name, b) in ledger.ordered() {
        println!("  {:<26}{:>14}{:>18}", name, b.saved_tokens, b.baseline_tokens);
    }
    println!("  {}", "-".repeat(56));
    println!(
        "  {:<26}{:>14}{:>18}",
        "total", ledger.total.saved_tokens, ledger.total.baseline_tokens
    );
    println!();
    println!("  estimated $ saved: ${dollars:.2}  (default-model input rate)");
    println!();
    println!("  This is the internal \"vs full-file\" figure, NOT your real agent cost.");
    println!("  For the real end-to-end delta, run the A/B eval harness: see eval/README.md.");
    Ok(())
}

/// `cce eval` (SPEC-V2.5 §7): aggregate recorded A/B runs into the correctness-
/// gated, cost-primary report. No model call — it reads run outputs from disk.
fn cmd_eval(runs: &Path, questions: &Path, json: bool) -> Result<(), String> {
    let qtext = std::fs::read_to_string(questions)
        .map_err(|e| format!("could not read questions {}: {e}", questions.display()))?;
    let rtext = std::fs::read_to_string(runs)
        .map_err(|e| format!("could not read runs {}: {e}", runs.display()))?;
    let report = cce::eval::evaluate_files(&qtext, &rtext);

    if json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string()));
        return Ok(());
    }

    println!("CCE eval — {}", report.note);
    println!("  questions: {}   skipped runs: {}", report.questions, report.skipped_runs);
    let arm = |name: &str, a: &cce::eval::ArmSummary| {
        println!(
            "  {name:<4}: correct {}/{} runs · punts {} · incorrect {} · correct_cost ${:.2} · mean ${:.2}",
            a.correct, a.runs, a.punts, a.incorrect, a.correct_cost_usd, a.mean_correct_cost_usd
        );
    };
    arm("off", &report.off);
    arm("on", &report.on);
    println!("  paired-correct (both arms): {}", report.paired_correct);
    println!(
        "  paired cost: off ${:.2} · on ${:.2} · saved ${:.2}  ({:.1}%)",
        report.paired_off_cost_usd,
        report.paired_on_cost_usd,
        report.cost_delta_usd,
        report.cost_saved_ratio * 100.0
    );
    Ok(())
}

// --- Workspace mode (SPEC-V2.2 §3/§4/§6/§7/§9) ---

/// The workspace root: the given dir, or the current directory.
fn workspace_root(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| PathBuf::from("."))
}

/// `cce workspace init [<dir>] [--force]` (SPEC-V2.2 §3): detect members and
/// write `<dir>/.cce/workspace.yml`, refusing to clobber an existing one.
fn cmd_workspace_init(dir: Option<PathBuf>, force: bool) -> Result<(), String> {
    let root = workspace_root(dir);
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let path = cce::workspace::manifest_path(&root);
    if path.exists() && !force {
        return Err(format!("{} already exists — pass --force to overwrite", path.display()));
    }
    let manifest = build_manifest(&root);
    if manifest.members.is_empty() {
        return Err("no workspace members detected under this root".to_string());
    }
    manifest.save(&root).map_err(|e| format!("could not write manifest: {e}"))?;
    println!("Wrote {}", path.display());
    println!("workspace: {}", manifest.name);
    println!("members ({}):", manifest.members.len());
    for m in &manifest.members {
        println!(
            "  {:<16} {:<12} {} · package {}",
            m.name,
            m.member_type.as_str(),
            m.path,
            m.package
        );
    }
    Ok(())
}

/// `cce workspace list [<dir>]` (SPEC-V2.2 §3/§5): print members + cross-member edges.
fn cmd_workspace_list(dir: Option<PathBuf>) -> Result<(), String> {
    let root = workspace_root(dir);
    let manifest = Manifest::load(&root)?;
    let graph = build_graph(&root, &manifest);
    println!("workspace: {}", manifest.name);
    println!("members ({}):", manifest.members.len());
    for m in &manifest.members {
        println!(
            "  {:<16} {:<12} {} · package {}",
            m.name,
            m.member_type.as_str(),
            m.path,
            m.package
        );
    }
    println!("edges ({}):", graph.edges.len());
    for e in &graph.edges {
        println!("  {} -> {}  (via {})", e.from, e.to, e.via);
    }
    Ok(())
}

/// `cce index --workspace [<dir>]` (SPEC-V2.2 §4): index each member into its own
/// store (byte-identical to standalone), then build the cross-member graph.
fn cmd_index_workspace(
    dir: Option<PathBuf>,
    embedder: &str,
    metrics_enabled: bool,
    allow_secrets: bool,
) -> Result<(), String> {
    let root = workspace_root(dir);
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let manifest = Manifest::load(&root)?;
    let kind = EmbedderKind::parse(embedder);
    let emb = build_embedder(kind);
    let protect_secrets = !allow_secrets;
    if allow_secrets {
        eprintln!("warning: --allow-secrets set — secret protection is DISABLED for every member");
    }

    let mut total_files = 0usize;
    let mut total_chunks = 0usize;
    println!("Indexing workspace: {}", manifest.name);
    for m in &manifest.members {
        let member_dir = root.join(&m.path);
        let store_path = default_store_path(&member_dir);
        let start = std::time::Instant::now();
        let (index, stats) =
            Index::build_protected(&member_dir, emb.as_ref(), |_| true, protect_secrets);
        index.save(&store_path).map_err(|e| e.to_string())?;
        let elapsed = start.elapsed().as_secs_f64();

        // Per-member index event, beside the member's own store (fail-open).
        let index_bytes = std::fs::metadata(&store_path).map(|md| md.len()).unwrap_or(0);
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
            sha: cce::sync::git::head_sha(&root),
            source: "local".to_string(),
            sensitive_skipped: stats.sensitive_skipped as u64,
        });

        total_files += stats.files_indexed;
        total_chunks += stats.total_chunks;
        println!(
            "  {:<16} files {:>4} · chunks {:>4} · {}",
            m.name,
            stats.files_indexed,
            stats.total_chunks,
            store_path.display()
        );
    }

    let graph = build_graph(&root, &manifest);
    graph.save(&root).map_err(|e| format!("could not write workspace graph: {e}"))?;

    println!("workspace totals: files {total_files} · chunks {total_chunks}");
    println!(
        "cross-member edges ({}) → {}",
        graph.edges.len(),
        cce::workspace::graph_path(&root).display()
    );
    Ok(())
}

/// Parse a `--package a,b` scope into trimmed, non-empty member names.
fn parse_scope(package: Option<String>) -> Option<Vec<String>> {
    package.map(|p| p.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

/// `cce search "q" --workspace [<dir>]` (SPEC-V2.2 §6): federated retrieval.
fn cmd_search_workspace(
    query: &str,
    dir: Option<PathBuf>,
    top_k: Option<usize>,
    no_graph: bool,
    json: bool,
    package: Option<String>,
) -> Result<(), String> {
    let root = workspace_root(dir);
    let manifest = Manifest::load(&root)?;
    let scope = parse_scope(package);
    let members = load_member_stores(&root, &manifest, scope.as_deref())?;
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);
    let top_k = top_k.unwrap_or(cce::config::DEFAULT_TOP_K);

    // Mirror single-repo embedder selection: if members were indexed with ollama,
    // try it (falling back to hash), else hash.
    let uses_ollama = members.iter().any(|m| m.index.embedder_name == "ollama");
    let emb: Box<dyn Embedder> = if uses_ollama {
        let oll = OllamaEmbedder::default();
        if oll.healthy() {
            Box::new(oll)
        } else {
            eprintln!("warning: workspace indexed with ollama but it is unreachable; using hash");
            Box::new(HashEmbedder)
        }
    } else {
        Box::new(HashEmbedder)
    };

    let results = federated_search(&members, &graph, emb.as_ref(), query, top_k, !no_graph);

    if json {
        // A query-id for the json shape (workspace search is read-only over member
        // stores; the id is generated, not logged).
        let query_id = HexIdSource::default().next_id();
        print!("{}", fed_results_json(&results, &query_id));
    } else {
        print_fed_human(&results);
    }
    Ok(())
}

/// Human form (SPEC-V2.2 §6): `<score>  <package> · <file_path>:<a>-<b> (<type>/<kind>)`.
fn print_fed_human(results: &[FedResult]) {
    if results.is_empty() {
        println!("(no results)");
        return;
    }
    for r in results {
        println!(
            "{:>2}. [{}] {} · {}:{}-{} ({}/{})",
            r.rank,
            format6(r.score),
            r.package,
            r.file_path,
            r.start_line,
            r.end_line,
            r.chunk_type,
            r.kind
        );
    }
}

/// JSON form (SPEC-V2.2 §6): array of tagged results + a top-level `query_id`.
fn fed_results_json(results: &[FedResult], query_id: &str) -> String {
    let items: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "rank": r.rank,
                "package": r.package,
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
    let body = serde_json::json!({ "query_id": query_id, "results": items });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// `cce stats --workspace [<dir>]` (SPEC-V2.2 §7): per-member + totals + edges.
fn cmd_stats_workspace(dir: Option<PathBuf>) -> Result<(), String> {
    let root = workspace_root(dir);
    let manifest = Manifest::load(&root)?;
    let members = load_member_stores(&root, &manifest, None)?;
    let stats = workspace_stats(&members);
    let graph = WorkspaceGraph::load_or_empty(&root, &manifest);

    println!("workspace: {}", manifest.name);
    let mut total_files = 0usize;
    let mut total_chunks = 0usize;
    for s in &stats {
        total_files += s.files;
        total_chunks += s.chunks;
        println!("  {} (package {})", s.name, s.package);
        println!("    files : {}", s.files);
        println!("    chunks: {}", s.chunks);
        for (kind, n) in &s.by_kind {
            println!("      {kind:<18}: {n}");
        }
    }
    println!("totals: files {total_files} · chunks {total_chunks}");
    println!("edges ({}):", graph.edges.len());
    for e in &graph.edges {
        println!("  {} -> {}  (via {})", e.from, e.to, e.via);
    }
    Ok(())
}

/// `cce dashboard --workspace [<dir>]` (SPEC-V2.2 §7): federated roll-up dashboard.
fn cmd_dashboard_workspace(
    dir: Option<PathBuf>,
    port: Option<u16>,
    price: Option<f64>,
) -> Result<(), String> {
    let root = workspace_root(dir);
    let manifest = Manifest::load(&root)?;
    let members = member_metrics(&root, &manifest);
    let port = port.unwrap_or(DEFAULT_DASHBOARD_PORT);
    let price = price.unwrap_or(DEFAULT_INPUT_PRICE_PER_MILLION);
    cce::dashboard::run_workspace(members, price, port)
        .map_err(|e| format!("dashboard failed: {e}"))
}

/// `cce sync …` (SPEC-SYNC §5): dispatch the sync subcommands. Each returns a
/// human-readable report on success; the report is printed as-is. Offline-first —
/// a remote failure returns a clear `Err` and never corrupts local state.
fn cmd_sync(cmd: SyncCmd) -> Result<(), String> {
    match cmd {
        SyncCmd::Init { remote, lfs, no_lfs, repo_id, dir } => {
            // git-LFS is on by default (SPEC-SYNC §8); `--no-lfs` opts out. The
            // `--lfs` flag is the documented affirmative (default-on already).
            let _ = lfs;
            let use_lfs = !no_lfs;
            let root = sync_root(dir);
            let report = sync_cmd::cmd_init(&root, &remote, use_lfs, repo_id)?;
            print!("{report}");
            Ok(())
        }
        SyncCmd::Push { commit, workspace, dir } => {
            let root = sync_root(dir);
            let report = sync_cmd::cmd_push(&root, commit, workspace)?;
            print!("{report}");
            Ok(())
        }
        SyncCmd::Pull { commit, latest, force, workspace, dir } => {
            let root = sync_root(dir);
            let target = if latest {
                sync_cmd::PullTarget::Latest
            } else if let Some(sha) = commit {
                sync_cmd::PullTarget::Commit(sha)
            } else {
                sync_cmd::PullTarget::Head
            };
            let report = sync_cmd::cmd_pull(&root, target, force, workspace)?;
            print!("{report}");
            Ok(())
        }
        SyncCmd::Status { dir } => {
            let root = sync_root(dir);
            let report = sync_cmd::cmd_status(&root)?;
            print!("{report}");
            Ok(())
        }
        SyncCmd::Verify { commit, dir } => {
            let root = sync_root(dir);
            let report = sync_cmd::cmd_verify(&root, commit)?;
            print!("{report}");
            Ok(())
        }
    }
}

/// The sync command root: the given `--dir`, or the current directory.
fn sync_root(dir: Option<PathBuf>) -> PathBuf {
    dir.unwrap_or_else(|| PathBuf::from("."))
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

/// `cce mcp` (SPEC-MCP): serve the MCP protocol over stdio for an agent. The
/// server warms the index via CCE Sync (best-effort) then serves until stdin EOF.
fn cmd_mcp(dir: Option<PathBuf>, store: Option<PathBuf>, workspace: bool) -> Result<(), String> {
    let server = cce::mcp::McpServer::new(dir, store, workspace);
    server.serve().map_err(|e| format!("mcp server error: {e}"))
}

/// `cce init` (SPEC-MCP): ensure an index and wire the editor up (`.mcp.json` +
/// `CLAUDE.md`). Idempotent; prints next steps.
fn cmd_init_mcp(
    dir: Option<PathBuf>,
    agent: String,
    remote: Option<String>,
    force: bool,
) -> Result<(), String> {
    let opts = cce::mcp::InitOptions {
        dir: dir.unwrap_or_else(|| PathBuf::from(".")),
        agent,
        remote,
        force,
    };
    let report = cce::mcp::init::run(&opts)?;
    print!("{report}");
    Ok(())
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
