//! # federation — federated indexing, search, stats, and dashboard roll-up
//!
//! **Why this file exists:** SPEC-V2.2 defines a workspace search as, exactly, the
//! standard §6 retrieval run over the *union* of the in-scope members' stored
//! chunks — with each result tagged by member, a `(member, file_path)` diversity
//! key, and graph expansion that adds cross-member dependency edges on top of the
//! per-member import graphs. That union-equals-single-index equivalence is the
//! correctness anchor, so it must be realised in one place, deterministically.
//!
//! **What it is / does:** Loads each member's own store (byte-identical to a
//! standalone index — isolation preserved), builds a combined corpus with
//! member-namespaced paths (so BM25 stats span the union and the diversity key is
//! `(member, file_path)`), runs the shared `retriever::rank_core`, then applies
//! intra-store expansion (the union of members' import graphs) plus cross-member
//! expansion (pull chunks from a dependency target member). Also owns the
//! per-member stats roll-up and the dashboard `by_package` federation.
//!
//! **Responsibilities:**
//! - Own combined-corpus assembly, `--package` scoping, and result tagging.
//! - Own federated stats and the federated metrics aggregate (`by_package`).
//! - It does NOT own the ranking math (that is `retriever`) nor detection/edges
//!   (that is `workspace`).

use crate::aggregator::aggregate;
use crate::config::{GRAPH_BONUS_CHUNK_SCALE, GRAPH_BONUS_MEMBER_CHUNKS, GRAPH_MAX_BONUS_MEMBERS};
use crate::embedder::{cosine, round6, score_key, Embedder};
use crate::graph_store::Graph;
use crate::metrics::read_log;
use crate::retriever::{expand_graph, rank_core, result_from, SearchResult};
use crate::store::{default_metrics_path, default_store_path, Index};
use crate::workspace::{Manifest, WorkspaceGraph};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// One loaded member: its manifest identity plus its own store.
pub struct MemberStore {
    pub name: String,
    pub package: String,
    /// Path of the member relative to the workspace root.
    pub rel_path: String,
    pub index: Index,
}

/// One federated search result, tagged with its member/package. `file_path` is
/// member-relative (the member namespace is stripped for output).
#[derive(Debug, Clone, Serialize)]
pub struct FedResult {
    pub rank: usize,
    pub package: String,
    #[serde(skip_serializing)]
    pub member: String,
    pub chunk_id: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_type: String,
    pub kind: String,
    #[serde(skip_serializing)]
    pub score: f64,
    #[serde(skip_serializing)]
    pub content: String,
}

/// Prefix a member-relative path with the member namespace (`<member>/<rel>`).
fn namespaced(member: &str, rel: &str) -> String {
    format!("{member}/{rel}")
}

/// Split a namespaced path back into `(member, member_relative_path)`.
fn denamespace(path: &str) -> (&str, &str) {
    match path.split_once('/') {
        Some((m, rest)) => (m, rest),
        None => (path, ""),
    }
}

/// Load the in-scope member stores. `scope` (from `--package`) restricts to the
/// named members; an unknown name is an error. A member whose store is missing is
/// an error telling the user to index the workspace.
pub fn load_member_stores(
    root: &Path,
    manifest: &Manifest,
    scope: Option<&[String]>,
) -> Result<Vec<MemberStore>, String> {
    let selected: Vec<&crate::workspace::Member> = match scope {
        Some(names) => {
            let mut out = Vec::new();
            for n in names {
                let m = manifest.member(n).ok_or_else(|| format!("unknown member/package: {n}"))?;
                out.push(m);
            }
            out
        }
        None => manifest.members.iter().collect(),
    };

    let mut stores = Vec::with_capacity(selected.len());
    for m in selected {
        let store_path = default_store_path(&root.join(&m.path));
        let index = Index::load(&store_path).map_err(|_| {
            format!(
                "member '{}' is not indexed ({} missing) — run `cce index --workspace` first",
                m.name,
                store_path.display()
            )
        })?;
        stores.push(MemberStore {
            name: m.name.clone(),
            package: m.package.clone(),
            rel_path: m.path.clone(),
            index,
        });
    }
    Ok(stores)
}

/// Build the combined union corpus over `members`, with member-namespaced paths.
/// Returns the assembled `Index` (BM25 over the union), whose graph is replaced by
/// the union of each member's intra-store import graph (namespaced) so no spurious
/// cross-member edges are introduced by module-name resolution.
pub fn combined_index(members: &[MemberStore]) -> Index {
    let mut chunks = Vec::new();
    let mut file_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut file_tokens: BTreeMap<String, usize> = BTreeMap::new();
    let mut union_pairs: Vec<(String, String)> = Vec::new();
    let embedder_name = members
        .first()
        .map(|m| m.index.embedder_name.clone())
        .unwrap_or_else(|| "hash".to_string());

    for m in members {
        for c in &m.index.chunks {
            let mut nc = c.clone();
            nc.file_path = namespaced(&m.name, &c.file_path);
            chunks.push(nc);
        }
        for (f, imports) in &m.index.file_imports {
            file_imports.insert(namespaced(&m.name, f), imports.clone());
        }
        for (f, toks) in &m.index.file_tokens {
            file_tokens.insert(namespaced(&m.name, f), *toks);
        }
        // Union the member's own intra-store graph, namespaced.
        for (from, to) in m.index.graph.out_pairs() {
            union_pairs.push((namespaced(&m.name, &from), namespaced(&m.name, &to)));
        }
    }

    let mut index = Index::from_parts(chunks, file_imports, file_tokens, embedder_name);
    index.graph = Graph::from_pairs(&union_pairs);
    index
}

/// Run a federated search over `members` with the cross-member `graph`
/// (SPEC-V2.2 §6). `graph_enabled` toggles both intra-store and cross-member
/// expansion. Returns member-tagged results in final rank order.
pub fn federated_search(
    members: &[MemberStore],
    graph: &WorkspaceGraph,
    embedder: &dyn Embedder,
    query: &str,
    top_k: usize,
    graph_enabled: bool,
) -> Vec<FedResult> {
    let combined = combined_index(members);
    let package_of: BTreeMap<String, String> =
        members.iter().map(|m| (m.name.clone(), m.package.clone())).collect();

    let qvec = embedder.embed(query);
    let mut results = rank_core(&combined, &qvec, query, top_k);
    if results.is_empty() {
        return Vec::new();
    }

    if graph_enabled {
        // Members represented by the core top results, in rank order (used to
        // follow cross-member edges). Captured before any expansion.
        let core_members: Vec<String> = top_result_members(&results);
        // Intra-store expansion over the union import graph (SPEC §6.7).
        expand_graph(&combined, &qvec, &mut results);
        // Cross-member expansion (SPEC-V2.2 §6): pull chunks from dependency
        // target members.
        cross_member_expand(&combined, graph, &qvec, &core_members, &mut results);
    }

    results.into_iter().enumerate().map(|(i, r)| fed_result(i + 1, r, &package_of)).collect()
}

/// The distinct members of the top (≤3) results, order-preserving.
fn top_result_members(results: &[SearchResult]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in results.iter().take(3) {
        let (member, _) = denamespace(&r.file_path);
        if !out.iter().any(|m| m == member) {
            out.push(member.to_string());
        }
    }
    out
}

/// Cross-member graph expansion (SPEC-V2.2 §6). For each source member among the
/// top results, follow its dependency edges `A -> B` and pull up to
/// `GRAPH_BONUS_MEMBER_CHUNKS` best-scoring chunks from each target member `B`,
/// across at most `GRAPH_MAX_BONUS_MEMBERS` distinct targets.
fn cross_member_expand(
    combined: &Index,
    graph: &WorkspaceGraph,
    qvec: &[f64],
    source_members: &[String],
    results: &mut Vec<SearchResult>,
) {
    // Members already represented in the result set are not re-pulled.
    let mut represented: BTreeSet<String> =
        results.iter().map(|r| denamespace(&r.file_path).0.to_string()).collect();
    let mut existing: BTreeSet<(String, usize, usize)> =
        results.iter().map(|r| (r.file_path.clone(), r.start_line, r.end_line)).collect();

    // Ordered, deduplicated list of target members to expand into.
    let mut targets: Vec<String> = Vec::new();
    for src in source_members {
        for tgt in graph.targets_from(src) {
            if represented.contains(&tgt) || targets.contains(&tgt) {
                continue;
            }
            targets.push(tgt);
            if targets.len() >= GRAPH_MAX_BONUS_MEMBERS {
                break;
            }
        }
        if targets.len() >= GRAPH_MAX_BONUS_MEMBERS {
            break;
        }
    }

    for tgt in targets {
        // Best chunks in the target member by cosine to the query.
        let mut member_chunks: Vec<(&crate::chunker::Chunk, f64)> = combined
            .chunks
            .iter()
            .filter(|c| denamespace(&c.file_path).0 == tgt)
            .map(|c| (c, cosine(qvec, &c.embedding)))
            .collect();
        member_chunks.sort_by(|a, b| {
            score_key(b.1).cmp(&score_key(a.1)).then_with(|| a.0.chunk_id.cmp(&b.0.chunk_id))
        });
        for (chunk, cos) in member_chunks.into_iter().take(GRAPH_BONUS_MEMBER_CHUNKS) {
            let key = (chunk.file_path.clone(), chunk.start_line, chunk.end_line);
            if existing.contains(&key) {
                continue;
            }
            existing.insert(key);
            let score = cos.max(0.0) * GRAPH_BONUS_CHUNK_SCALE;
            results.push(result_from(chunk, score));
        }
        represented.insert(tgt);
    }
}

/// Convert a namespaced `SearchResult` into a member-tagged `FedResult`.
fn fed_result(rank: usize, r: SearchResult, package_of: &BTreeMap<String, String>) -> FedResult {
    let (member, rel) = denamespace(&r.file_path);
    let package = package_of.get(member).cloned().unwrap_or_else(|| member.to_string());
    FedResult {
        rank,
        package,
        member: member.to_string(),
        chunk_id: r.chunk_id,
        file_path: rel.to_string(),
        start_line: r.start_line,
        end_line: r.end_line,
        chunk_type: r.chunk_type,
        kind: r.kind,
        score: r.score,
        content: r.content,
    }
}

// --- Federated stats (SPEC-V2.2 §7) ---

/// Per-member statistics for `cce stats --workspace`.
#[derive(Debug, Clone)]
pub struct MemberStats {
    pub name: String,
    pub package: String,
    pub files: usize,
    pub chunks: usize,
    pub by_kind: BTreeMap<String, usize>,
}

/// Compute per-member stats over the loaded member stores.
pub fn workspace_stats(members: &[MemberStore]) -> Vec<MemberStats> {
    members
        .iter()
        .map(|m| {
            let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
            for c in &m.index.chunks {
                *by_kind.entry(c.kind.clone()).or_insert(0) += 1;
            }
            MemberStats {
                name: m.name.clone(),
                package: m.package.clone(),
                files: m.index.files().len(),
                chunks: m.index.chunks.len(),
                by_kind,
            }
        })
        .collect()
}

// --- Federated dashboard (SPEC-V2.2 §7) ---

/// A member reference for dashboard federation: its identity and metrics log.
pub struct MemberMetrics {
    pub name: String,
    pub package: String,
    pub metrics_path: PathBuf,
}

/// The per-member metrics log paths for a workspace (`<member>/.cce/metrics.jsonl`).
pub fn member_metrics(root: &Path, manifest: &Manifest) -> Vec<MemberMetrics> {
    manifest
        .members
        .iter()
        .map(|m| MemberMetrics {
            name: m.name.clone(),
            package: m.package.clone(),
            metrics_path: default_metrics_path(&root.join(&m.path)),
        })
        .collect()
}

/// One `by_package` breakdown row (SPEC-V2.2 §7). `mean_top_score` (retrieval
/// quality per member) was added in v2.4.1 so the per-package panel shows savings,
/// searches, AND quality.
#[derive(Debug, Clone, Serialize)]
pub struct PackageRollup {
    pub package: String,
    pub searches: u64,
    pub tokens_saved: u64,
    pub mean_savings_ratio: f64,
    pub mean_top_score: f64,
}

/// Build the federated metrics aggregate as a JSON value: the normal §4 roll-up
/// over the concatenation of members' events, plus a `by_package` section
/// (searches, tokens saved, mean savings per member). `now_secs`/`price` are
/// injected for determinism.
pub fn federated_metrics_json(
    members: &[MemberMetrics],
    now_secs: i64,
    price: f64,
) -> serde_json::Value {
    // Roll-up: concatenate every member's events (member tag not needed for the
    // roll-up itself; per-package numbers come from per-member aggregates).
    let mut all_events = Vec::new();
    for m in members {
        let log = read_log(&m.metrics_path);
        all_events.extend(log.events);
    }
    let rollup = aggregate(&all_events, now_secs, price);
    let mut val = serde_json::to_value(&rollup).unwrap_or(serde_json::Value::Null);

    let by_package: Vec<PackageRollup> = members
        .iter()
        .map(|m| {
            let log = read_log(&m.metrics_path);
            let agg = aggregate(&log.events, now_secs, price);
            PackageRollup {
                package: m.package.clone(),
                searches: agg.totals.searches,
                tokens_saved: agg.totals.tokens_saved,
                mean_savings_ratio: round6(agg.totals.mean_savings_ratio),
                mean_top_score: round6(agg.totals.mean_top_score),
            }
        })
        .collect();

    if let Some(obj) = val.as_object_mut() {
        obj.insert("by_package".to_string(), serde_json::to_value(&by_package).unwrap_or_default());
    }
    val
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use crate::workspace::{build_manifest, build_graph};
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/workspace"))
    }

    /// Index every member of a temp copy of the fixture and load the stores.
    fn indexed_members(root: &Path) -> (Manifest, WorkspaceGraph, Vec<MemberStore>) {
        let manifest = build_manifest(root);
        let graph = build_graph(root, &manifest);
        let e = HashEmbedder;
        for m in &manifest.members {
            let member_dir = root.join(&m.path);
            let (idx, _) = Index::build_from_dir(&member_dir, &e);
            idx.save(&default_store_path(&member_dir)).unwrap();
        }
        let members = load_member_stores(root, &manifest, None).unwrap();
        (manifest, graph, members)
    }

    fn copy_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path();
        for entry in walkdir::WalkDir::new(fixture()).into_iter().flatten() {
            let rel = entry.path().strip_prefix(fixture()).unwrap();
            let target = dst.join(rel);
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
        tmp
    }

    #[test]
    fn member_store_is_byte_identical_to_standalone() {
        let tmp = copy_fixture();
        let (manifest, _, _) = indexed_members(tmp.path());
        let e = HashEmbedder;
        for m in &manifest.members {
            let member_dir = tmp.path().join(&m.path);
            let store = default_store_path(&member_dir);
            let federated_bytes = std::fs::read(&store).unwrap();
            // Standalone index of the same dir, written elsewhere.
            let (idx, _) = Index::build_from_dir(&member_dir, &e);
            let alt = tmp.path().join(format!("{}-standalone.json", m.name));
            idx.save(&alt).unwrap();
            let standalone_bytes = std::fs::read(&alt).unwrap();
            assert_eq!(federated_bytes, standalone_bytes, "member {} not byte-identical", m.name);
        }
    }

    #[test]
    fn scoped_search_returns_labelled_chunks_from_both_members() {
        let tmp = copy_fixture();
        let (manifest, graph, _) = indexed_members(tmp.path());
        let scope = vec!["app".to_string(), "billing".to_string()];
        let members = load_member_stores(tmp.path(), &manifest, Some(&scope)).unwrap();
        let e = HashEmbedder;
        let res = federated_search(&members, &graph, &e, "billing charge amount", 10, false);
        assert!(!res.is_empty());
        let packages: BTreeSet<&str> = res.iter().map(|r| r.package.as_str()).collect();
        assert!(packages.contains("app"), "expected app chunks, got {packages:?}");
        assert!(packages.contains("billing"), "expected billing chunks, got {packages:?}");
        // file_path is member-relative: the member namespace is stripped, so a
        // billing result reads `lib/billing.rb`, not `billing/lib/billing.rb`.
        for r in res.iter().filter(|r| r.package == "billing") {
            assert!(!r.file_path.starts_with("billing/"), "namespace leaked: {}", r.file_path);
        }
    }

    #[test]
    fn federation_equals_union_index() {
        // The correctness anchor: a federated search (no graph) equals the standard
        // §6 ranking over the union of the two members' chunks, in the same order.
        let tmp = copy_fixture();
        let (manifest, graph, _) = indexed_members(tmp.path());
        let scope = vec!["app".to_string(), "billing".to_string()];
        let members = load_member_stores(tmp.path(), &manifest, Some(&scope)).unwrap();
        let e = HashEmbedder;
        let query = "billing charge amount";

        let fed = federated_search(&members, &graph, &e, query, 10, false);

        // Independently build the union index and rank over it.
        let union = combined_index(&members);
        let qvec = e.embed(query);
        let union_ranked = rank_core(&union, &qvec, query, 10);

        let fed_ids: Vec<&str> = fed.iter().map(|r| r.chunk_id.as_str()).collect();
        let union_ids: Vec<&str> = union_ranked.iter().map(|r| r.chunk_id.as_str()).collect();
        assert_eq!(fed_ids, union_ids);
    }

    #[test]
    fn unknown_package_scope_is_an_error() {
        let tmp = copy_fixture();
        let (manifest, _, _) = indexed_members(tmp.path());
        let scope = vec!["nope".to_string()];
        let err = match load_member_stores(tmp.path(), &manifest, Some(&scope)) {
            Ok(_) => panic!("unknown package must error"),
            Err(e) => e,
        };
        assert!(err.contains("unknown member/package"), "got: {err}");
    }

    #[test]
    fn graph_hop_expands_app_result_into_billing() {
        let tmp = copy_fixture();
        let (manifest, graph, _) = indexed_members(tmp.path());
        let members = load_member_stores(tmp.path(), &manifest, None).unwrap();
        let e = HashEmbedder;
        // A query whose top hit is the app (its application module) and which
        // shares no tokens with billing; the app->billing edge must pull a billing
        // chunk in only when graph expansion is on.
        let no_graph = federated_search(&members, &graph, &e, "application boot", 3, false);
        assert_eq!(no_graph[0].package, "app", "expected top result in app");
        assert!(
            !no_graph.iter().any(|r| r.package == "billing"),
            "billing should only appear via the graph hop"
        );
        let with_graph = federated_search(&members, &graph, &e, "application boot", 3, true);
        assert!(
            with_graph.iter().any(|r| r.package == "billing"),
            "graph hop must expand into billing"
        );
    }

    #[test]
    fn workspace_stats_counts_per_member() {
        let tmp = copy_fixture();
        let (_, _, members) = indexed_members(tmp.path());
        let stats = workspace_stats(&members);
        assert_eq!(stats.len(), 3);
        assert!(stats.iter().all(|s| s.chunks >= 1));
        let total: usize = stats.iter().map(|s| s.chunks).sum();
        assert!(total >= 3);
    }

    #[test]
    fn federated_dashboard_rolls_up_and_breaks_down_by_package() {
        // Two members, each with one search event; the roll-up totals both while
        // by_package attributes each member's numbers to its package.
        let tmp = tempfile::tempdir().unwrap();
        let mk = |name: &str, ts: &str, tokens: u64, ratio: f64| {
            let dir = tmp.path().join(name).join(".cce");
            std::fs::create_dir_all(&dir).unwrap();
            let line = format!(
                "{{\"schema\":\"cce.metrics/v1\",\"event\":\"search\",\"ts\":\"{ts}\",\"id\":\"{name}00000000\",\"query\":\"q\",\"result_count\":1,\"tokens_saved\":{tokens},\"savings_ratio\":{ratio},\"top_score\":0.9,\"empty\":false,\"low_confidence\":false}}\n"
            );
            std::fs::write(dir.join("metrics.jsonl"), line).unwrap();
            MemberMetrics {
                name: name.to_string(),
                package: name.to_string(),
                metrics_path: dir.join("metrics.jsonl"),
            }
        };
        let members = vec![
            mk("app", "2026-07-04T10:00:00Z", 1000, 0.5),
            mk("billing", "2026-07-04T11:00:00Z", 3000, 0.75),
        ];
        let now = crate::metrics::parse_iso("2026-07-05T00:00:00Z").unwrap();
        let json = federated_metrics_json(&members, now, 3.00);
        // Roll-up totals span both members.
        assert_eq!(json["totals"]["searches"], 2);
        assert_eq!(json["totals"]["tokens_saved"], 4000);
        // by_package attributes per member.
        let by = json["by_package"].as_array().unwrap();
        assert_eq!(by.len(), 2);
        let app = by.iter().find(|p| p["package"] == "app").unwrap();
        assert_eq!(app["searches"], 1);
        assert_eq!(app["tokens_saved"], 1000);
        assert_eq!(app["mean_top_score"], 0.9); // v2.4.1: quality per member
        let billing = by.iter().find(|p| p["package"] == "billing").unwrap();
        assert_eq!(billing["tokens_saved"], 3000);
        // The roll-up carries the v2.4.1 agent-vs-human split (all CLI here).
        assert_eq!(json["usage_by_source"]["cli"]["searches"], 2);
        assert_eq!(json["usage_by_source"]["mcp"]["searches"], 0);
    }
}
