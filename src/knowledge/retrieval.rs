//! # knowledge::retrieval — the M4 knowledge search blend (SPEC-V2.6 §5)
//!
//! **Why this file exists:** Phase A built a heading-chunked, faceted, snapshot-keyed
//! knowledge store. It is worthless until an agent can *retrieve* it. This module makes
//! knowledge chunks searchable through the **exact same hybrid retrieval as code** — the
//! deterministic hash embedder + BM25 + RRF of SPEC §6 — then layers the SPEC-V2.6 §5
//! staleness rules and the L5 precision filter on top, and renders the byte-pinned
//! provenance line. It owns NO bespoke scorer: the base score is `retriever::search`.
//!
//! **What it is / does:** Converts each `KnowledgeChunk` to a code-style `Chunk`
//! (keyed by `record_id`, so a document's sections share a "file" for the diversity cap
//! and same-document neighbouring), assembles an `Index`, ranks with the identical §6
//! pipeline, then: drops `not_planned`/`wontfix` records, precision-filters (score ≥
//! `min_score` AND a shared query token — the L5/memory rule), applies the merged-PR
//! "decided + implemented" boost, and orders by score then recency (`updated_at`
//! newest-first) then `chunk_id`. All deterministic + byte-pinned.
//!
//! **Responsibilities:**
//! - Own `KnowledgeHit`, the provenance grammar, and the staleness rules.
//! - Own same-document neighbour lookup for `expand_chunk`/`related_context`.
//! - It does NOT rank itself (that is `retriever::search`) nor persist.

use crate::config::KNOWLEDGE_MERGED_PR_BOOST;
use crate::embedder::{score_key, Embedder, HashEmbedder};
use crate::knowledge::store::{KnowledgeChunk, KnowledgeStore};
use crate::retriever::search;
use crate::store::Index;
use crate::tokenizer::tokenize;
use std::cell::OnceCell;
use std::collections::{BTreeMap, HashSet};

/// One ranked knowledge result (SPEC-V2.6 §5): the section's identity + content plus
/// the facets the provenance line needs and the final (post-staleness) score.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeHit {
    pub rank: usize,
    pub chunk_id: String,
    pub record_id: String,
    pub title: String,
    pub kind: String,
    pub state: Option<String>,
    pub updated_at: Option<String>,
    pub url: Option<String>,
    /// The final score after the merged-PR boost — comparable to a code hit's score,
    /// so `source:both` blends the two pools by one shared ranking.
    pub score: f64,
    pub content: String,
}

impl KnowledgeHit {
    /// The byte-pinned provenance line (SPEC-V2.6 §5):
    /// `[knowledge] <title> — <state> · <updated_at> · <url>`, omitting missing facets
    /// cleanly (a `None`/empty facet is skipped; if none are present the ` — …` tail is
    /// dropped entirely, leaving just `[knowledge] <title>`).
    pub fn provenance(&self) -> String {
        provenance_line(
            &self.title,
            self.state.as_deref(),
            self.updated_at.as_deref(),
            self.url.as_deref(),
        )
    }
}

/// Build the byte-pinned provenance line from the raw facets (see [`KnowledgeHit::provenance`]).
pub fn provenance_line(
    title: &str,
    state: Option<&str>,
    updated_at: Option<&str>,
    url: Option<&str>,
) -> String {
    let mut facets: Vec<&str> = Vec::new();
    for v in [state, updated_at, url].into_iter().flatten() {
        if !v.is_empty() {
            facets.push(v);
        }
    }
    let mut line = format!("[knowledge] {title}");
    if !facets.is_empty() {
        line.push_str(" — ");
        line.push_str(&facets.join(" · "));
    }
    line
}

/// True if a `state_reason` drops the record from knowledge results (SPEC-V2.6 §5):
/// `not_planned` / `wontfix` are decisions NOT to act, so they never surface.
pub fn is_dropped_reason(state_reason: Option<&str>) -> bool {
    matches!(state_reason, Some("not_planned") | Some("wontfix"))
}

/// True if a link is a pull-request reference — the "decided + implemented" signal
/// (SPEC-V2.6 §5). Deterministic + offline: matched purely on the URL path shape
/// (`/pull/`, `/pulls/`, `/pr/`, `/merge_requests/`), case-insensitive.
pub fn is_merged_pr_link(link: &str) -> bool {
    let l = link.to_ascii_lowercase();
    l.contains("/pull/")
        || l.contains("/pulls/")
        || l.contains("/pr/")
        || l.contains("/merge_requests/")
}

/// True if any of `links` is a merged-PR reference.
fn has_merged_pr(links: &[String]) -> bool {
    links.iter().any(|l| is_merged_pr_link(l))
}

/// The searchable text of a chunk for the shared-query-token guard: the title, the
/// section content, and the labels (so a topical label counts as overlap).
fn shares_query_token(query: &str, chunk: &KnowledgeChunk) -> bool {
    let q: HashSet<String> = tokenize(query).into_iter().collect();
    if q.is_empty() {
        return false;
    }
    let mut hay = String::with_capacity(chunk.title.len() + chunk.content.len());
    hay.push_str(&chunk.title);
    hay.push(' ');
    hay.push_str(&chunk.content);
    for lbl in &chunk.labels {
        hay.push(' ');
        hay.push_str(lbl);
    }
    tokenize(&hay).into_iter().any(|t| q.contains(&t))
}

/// Convert a knowledge chunk to a code-style `Chunk` for ranking. Keyed by `record_id`
/// (the document) so the diversity cap and same-document neighbouring work per record.
/// Reuses the persisted embedding; recomputes deterministically if a legacy snapshot
/// omitted it (so an old Phase-A store still searches identically).
fn to_chunk(kc: &KnowledgeChunk, embedder: &HashEmbedder) -> crate::chunker::Chunk {
    let embedding = if kc.embedding.is_empty() {
        embedder.embed(&kc.content)
    } else {
        kc.embedding.clone()
    };
    crate::chunker::Chunk {
        chunk_id: kc.chunk_id.clone(),
        file_path: kc.record_id.clone(),
        start_line: kc.start_line,
        end_line: kc.end_line,
        chunk_type: "knowledge".to_string(),
        kind: kc.kind.clone(),
        language: "markdown".to_string(),
        content: kc.content.clone(),
        token_count: kc.token_count,
        embedding,
    }
}

/// The live (non-dropped) chunks of a store, in store order — staleness rule 1
/// (SPEC-V2.6 §5): `not_planned`/`wontfix` records are never candidates at all.
fn live_chunks(store: &KnowledgeStore) -> Vec<&KnowledgeChunk> {
    store.chunks.iter().filter(|c| !is_dropped_reason(c.state_reason.as_deref())).collect()
}

/// Build the ranking `Index` over a store's live chunks — the expensive half of a
/// knowledge search (per-chunk conversion, a legacy-snapshot re-embed, and the BM25
/// build all live here). Split out so a long-lived caller (the MCP server, issue #31)
/// can build it once and reuse it across queries via [`search_knowledge_over`];
/// [`search_knowledge`] composes the two for one-shot callers, byte-identically.
pub fn knowledge_ranking_index(store: &KnowledgeStore) -> Index {
    let embedder = HashEmbedder;
    let chunks: Vec<crate::chunker::Chunk> =
        live_chunks(store).iter().map(|c| to_chunk(c, &embedder)).collect();
    Index::from_parts(chunks, BTreeMap::new(), BTreeMap::new(), embedder.name().to_string())
}

/// Search the knowledge store (SPEC-V2.6 §5): rank with the identical §6 hybrid
/// pipeline, drop `not_planned`/`wontfix`, precision-filter (score ≥ `min_score` AND a
/// shared query token), apply the merged-PR boost, then order by score, then recency
/// (`updated_at` newest-first), then `chunk_id`, and truncate to `top_k`.
pub fn search_knowledge(
    store: &KnowledgeStore,
    query: &str,
    top_k: usize,
    min_score: f64,
) -> Vec<KnowledgeHit> {
    if store.chunks.is_empty() || query.trim().is_empty() || top_k == 0 {
        return Vec::new();
    }
    let index = knowledge_ranking_index(store);
    search_knowledge_over(store, &index, query, top_k, min_score)
}

/// [`search_knowledge`] over an already-built ranking index (from
/// [`knowledge_ranking_index`] on the SAME store). Everything after the index build —
/// ranking, staleness rules, the precision filter, ordering — is identical, so the
/// results are byte-for-byte the same whether the index is fresh or reused.
pub fn search_knowledge_over(
    store: &KnowledgeStore,
    index: &Index,
    query: &str,
    top_k: usize,
    min_score: f64,
) -> Vec<KnowledgeHit> {
    if store.chunks.is_empty() || query.trim().is_empty() || top_k == 0 {
        return Vec::new();
    }
    let embedder = HashEmbedder;
    let live = live_chunks(store);
    if live.is_empty() {
        return Vec::new();
    }

    let by_id: BTreeMap<&str, &KnowledgeChunk> =
        live.iter().map(|c| (c.chunk_id.as_str(), *c)).collect();

    // Rank generously (the whole live corpus) so the recency re-order and the precision
    // filter see every candidate before we truncate to `top_k`. No graph (knowledge has
    // no import edges).
    let generous = live.len().max(top_k);
    let ranked = search(index, &embedder, query, generous, false);

    let mut hits: Vec<KnowledgeHit> = Vec::new();
    for r in ranked {
        // Precision filter (SPEC-V2.6 §5, the shared L5/memory rule): base hybrid score
        // ≥ min_score AND a shared query token, so a loose/stale record never surfaces.
        if r.score < min_score {
            continue;
        }
        let Some(kc) = by_id.get(r.chunk_id.as_str()) else { continue };
        if !shares_query_token(query, kc) {
            continue;
        }
        // Staleness rule 3 (SPEC-V2.6 §5): a merged-PR link is "decided + implemented" —
        // scale the base score by the pinned boost.
        let boosted = if has_merged_pr(&kc.links) {
            r.score * KNOWLEDGE_MERGED_PR_BOOST
        } else {
            r.score
        };
        hits.push(KnowledgeHit {
            rank: 0,
            chunk_id: kc.chunk_id.clone(),
            record_id: kc.record_id.clone(),
            title: kc.title.clone(),
            kind: kc.kind.clone(),
            state: kc.state.clone(),
            updated_at: kc.updated_at.clone(),
            url: kc.url.clone(),
            score: boosted,
            content: kc.content.clone(),
        });
    }

    // Final order (SPEC-V2.6 §5): score desc, then recency (`updated_at` newest-first),
    // then `chunk_id` asc. All deterministic; a missing `updated_at` sorts oldest/last.
    hits.sort_by(|a, b| {
        score_key(b.score)
            .cmp(&score_key(a.score))
            .then_with(|| {
                b.updated_at.as_deref().unwrap_or("").cmp(a.updated_at.as_deref().unwrap_or(""))
            })
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    hits.truncate(top_k);
    for (i, h) in hits.iter_mut().enumerate() {
        h.rank = i + 1;
    }
    hits
}

/// A knowledge store loaded once and reused across MCP tool calls (issue #31): the
/// parsed store plus a lazily built ranking index, so a warm knowledge query skips
/// both the JSON parse and the per-query embed + BM25 rebuild. The index is lazy
/// (`OnceCell`) because several consumers (`expand_chunk`, `related_context`, the
/// source resolver) only need the chunks, never the ranking structures. Freshness is
/// the CALLER's job (the server fingerprints the store files); this type is a pure
/// snapshot. Results are byte-identical to the uncached [`search_knowledge`].
pub struct LoadedKnowledge {
    pub store: KnowledgeStore,
    index: OnceCell<Index>,
}

impl LoadedKnowledge {
    pub fn new(store: KnowledgeStore) -> Self {
        LoadedKnowledge { store, index: OnceCell::new() }
    }

    /// The ranking index over the store's live chunks, built on first use.
    fn index(&self) -> &Index {
        self.index.get_or_init(|| knowledge_ranking_index(&self.store))
    }

    /// [`search_knowledge`] against the cached ranking index — byte-identical results.
    pub fn search(&self, query: &str, top_k: usize, min_score: f64) -> Vec<KnowledgeHit> {
        search_knowledge_over(&self.store, self.index(), query, top_k, min_score)
    }
}

/// Find a chunk by id in the store (for `expand_chunk` on a knowledge chunk_id).
pub fn find_chunk<'a>(store: &'a KnowledgeStore, chunk_id: &str) -> Option<&'a KnowledgeChunk> {
    store.chunks.iter().find(|c| c.chunk_id == chunk_id)
}

/// The other sections of the same document (SPEC-V2.6 §5, "related = same-document
/// neighbours"): every chunk sharing `record_id`, excluding `chunk_id`, ordered by
/// `(start_line, chunk_id)`. Powers `expand_chunk scope=file/neighbors` and
/// `related_context` for a knowledge chunk.
pub fn same_document_sections<'a>(
    store: &'a KnowledgeStore,
    record_id: &str,
    exclude_chunk_id: Option<&str>,
) -> Vec<&'a KnowledgeChunk> {
    let mut v: Vec<&KnowledgeChunk> = store
        .chunks
        .iter()
        .filter(|c| c.record_id == record_id && Some(c.chunk_id.as_str()) != exclude_chunk_id)
        .collect();
    v.sort_by(|a, b| a.start_line.cmp(&b.start_line).then_with(|| a.chunk_id.cmp(&b.chunk_id)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::contract::KnowledgeRecord;
    use crate::knowledge::store::ingest_default;

    fn rec(id: &str, title: &str, body: &str) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.into(),
            title: title.into(),
            body: body.into(),
            source: "github-issues".into(),
            url: Some(format!("https://example.test/{id}")),
            state: Some("open".into()),
            state_reason: None,
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            labels: vec![],
            group: None,
            links: vec![],
            extra: None,
        }
    }

    #[test]
    fn provenance_omits_missing_facets_cleanly() {
        assert_eq!(
            provenance_line(
                "Login policy",
                Some("closed"),
                Some("2026-02-01T10:00:00Z"),
                Some("https://x/1")
            ),
            "[knowledge] Login policy — closed · 2026-02-01T10:00:00Z · https://x/1"
        );
        // Only a URL present.
        assert_eq!(
            provenance_line("Epic", None, None, Some("https://x/2")),
            "[knowledge] Epic — https://x/2"
        );
        // Nothing present ⇒ just the title.
        assert_eq!(provenance_line("Bare", None, None, None), "[knowledge] Bare");
        // Empty strings are treated as missing.
        assert_eq!(provenance_line("Bare", Some(""), None, None), "[knowledge] Bare");
    }

    #[test]
    fn merged_pr_link_detection() {
        assert!(is_merged_pr_link("https://example.test/pr/40"));
        assert!(is_merged_pr_link("https://github.test/o/r/pull/9"));
        assert!(is_merged_pr_link("https://gitlab.test/o/r/-/merge_requests/3"));
        assert!(!is_merged_pr_link("https://example.test/issues/7"));
        assert!(!is_merged_pr_link("https://example.test/wiki/page"));
    }

    #[test]
    fn dropped_reasons() {
        assert!(is_dropped_reason(Some("not_planned")));
        assert!(is_dropped_reason(Some("wontfix")));
        assert!(!is_dropped_reason(Some("completed")));
        assert!(!is_dropped_reason(None));
    }

    #[test]
    fn search_is_deterministic_and_precision_filters() {
        let recs = vec![
            rec(
                "a",
                "Login policy",
                "## Rule\n\nLock the account after five failed login attempts.",
            ),
            rec("b", "Payments", "## Refund\n\nRefund a captured charge within thirty days."),
        ];
        let store = ingest_default(&recs, b"feed");
        let a = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        let b = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        let ids_a: Vec<&str> = a.iter().map(|h| h.chunk_id.as_str()).collect();
        let ids_b: Vec<&str> = b.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ids_a, ids_b);
        assert!(!a.is_empty());
        // The login section outranks the payments section for a login query.
        assert!(a[0].title == "Login policy");
        // A no-overlap query recalls nothing (the shared-token guard).
        assert!(search_knowledge(&store, "zzzz totally unrelated xyzzy", 5, 0.30).is_empty());
    }

    #[test]
    fn secret_in_a_title_is_redacted_in_served_provenance() {
        // #111: the provenance header (`[knowledge] <title> — … · <url>`) must
        // never serve a raw secret from a record's title or url facet. The key is
        // split via `concat!` so no contiguous secret literal is committed.
        let aws = concat!("AKIA", "IOSFODNN7EXAMPLE");
        let mut r = rec(
            "leak",
            &format!("Rotate leaked key {aws} in prod"),
            "## Fix\n\nRotate the leaked key and lock the account.",
        );
        r.url = Some(format!("https://example.test/1?token={aws}"));
        let store = ingest_default(&[r], b"feed");
        let hits = search_knowledge(&store, "rotate leaked key prod", 5, 0.30);
        assert!(!hits.is_empty());
        let prov = hits[0].provenance();
        assert!(!prov.contains(aws), "raw secret served in provenance: {prov}");
        assert!(
            prov.contains("[knowledge] Rotate leaked key [REDACTED:AWS_ACCESS_KEY] in prod"),
            "{prov}"
        );
    }

    #[test]
    fn secret_in_state_or_updated_at_is_redacted_in_served_provenance() {
        // #111: `state` and `updated_at` are BOTH rendered into the provenance line
        // (`[knowledge] <title> — <state> · <updated_at> · <url>`) and both are
        // free `Option<String>` in the schema — a secret placed in either must not
        // reach the served header. Keys split via `concat!` (no committed literal).
        let aws = concat!("AKIA", "IOSFODNN7EXAMPLE");
        let ghp = concat!("ghp", "_", "0123456789abcdefghijklmnopqrstuvwx01");
        let mut r = rec(
            "leak",
            "Login policy",
            "## Rule\n\nLock the account after five failed login attempts.",
        );
        r.state = Some(format!("open; leaked {aws}"));
        r.updated_at = Some(format!("2026-06-01 token={ghp}"));
        let store = ingest_default(&[r], b"feed");
        let hits = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        assert!(!hits.is_empty());
        let prov = hits[0].provenance();
        assert!(!prov.contains(aws), "raw state secret served in provenance: {prov}");
        assert!(!prov.contains(ghp), "raw updated_at secret served in provenance: {prov}");
        assert!(prov.contains("open; leaked [REDACTED:AWS_ACCESS_KEY]"), "{prov}");
        assert!(prov.contains("token=[REDACTED:GITHUB_TOKEN]"), "{prov}");
    }

    #[test]
    fn not_planned_records_are_dropped() {
        let mut r =
            rec("a", "Rejected idea", "## Detail\n\nWe considered a new login flow and declined.");
        r.state = Some("closed".into());
        r.state_reason = Some("not_planned".into());
        let store = ingest_default(&[r], b"feed");
        // Even a strong topical match must not surface a not_planned record.
        assert!(search_knowledge(&store, "login flow detail considered", 5, 0.30).is_empty());
    }

    #[test]
    fn recency_orders_newest_first_on_a_tie() {
        // Two records with IDENTICAL content differing only by updated_at ⇒ equal base
        // score ⇒ recency tiebreak ⇒ newest first.
        let body = "## Rule\n\nLock the account after five failed login attempts.";
        let mut older = rec("older", "Login policy", body);
        older.updated_at = Some("2025-01-01T00:00:00Z".into());
        let mut newer = rec("newer", "Login policy", body);
        newer.updated_at = Some("2026-06-01T00:00:00Z".into());
        let store = ingest_default(&[older, newer], b"feed");
        let hits = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        assert!(hits.len() >= 2);
        assert_eq!(hits[0].record_id, "newer");
        assert_eq!(hits[1].record_id, "older");
    }

    #[test]
    fn merged_pr_boost_lifts_a_tied_record() {
        // Identical content; one carries a merged-PR link. The boost must lift it above
        // its otherwise-equal, MORE-recent sibling (proving the boost, not just recency).
        let body = "## Rule\n\nLock the account after five failed login attempts.";
        let mut plain = rec("plain", "Login policy", body);
        plain.updated_at = Some("2026-06-01T00:00:00Z".into()); // newer
        plain.links = vec![];
        let mut implemented = rec("implemented", "Login policy", body);
        implemented.updated_at = Some("2025-01-01T00:00:00Z".into()); // older
        implemented.links = vec!["https://example.test/pull/7".into()];
        let store = ingest_default(&[plain, implemented], b"feed");
        let hits = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        assert!(hits.len() >= 2);
        // The implemented (merged-PR) record wins despite being older.
        assert_eq!(hits[0].record_id, "implemented");
    }

    #[test]
    fn loaded_knowledge_search_is_byte_identical_to_the_one_shot_path() {
        // The cached path (issue #31) must be indistinguishable from the one-shot
        // `search_knowledge`, for every field of every hit — and stable across
        // repeated queries against the same `LoadedKnowledge`.
        let recs = vec![
            rec(
                "a",
                "Login policy",
                "## Rule\n\nLock the account after five failed login attempts.",
            ),
            rec("b", "Payments", "## Refund\n\nRefund a captured charge within thirty days."),
        ];
        let store = ingest_default(&recs, b"feed");
        let one_shot = search_knowledge(&store, "login attempts lock account", 5, 0.30);
        assert!(!one_shot.is_empty());
        let loaded = LoadedKnowledge::new(store);
        assert_eq!(loaded.search("login attempts lock account", 5, 0.30), one_shot);
        assert_eq!(loaded.search("login attempts lock account", 5, 0.30), one_shot);
    }

    #[test]
    fn same_document_sections_excludes_self_and_orders() {
        let r = rec("doc", "Guide", "## First\n\nAlpha.\n\n## Second\n\nBeta.");
        let store = ingest_default(std::slice::from_ref(&r), b"feed");
        let first = &store.chunks[0];
        let others = same_document_sections(&store, "doc", Some(&first.chunk_id));
        assert!(others.iter().all(|c| c.chunk_id != first.chunk_id));
        assert!(others.iter().all(|c| c.record_id == "doc"));
        // Ordered by start_line ascending.
        for w in others.windows(2) {
            assert!(w[0].start_line <= w[1].start_line);
        }
    }
}
