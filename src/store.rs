//! # store — index assembly and on-disk persistence
//!
//! **Why this file exists:** `index` runs in one process; `search`/`stats`/
//! `conformance` run later in a fresh process (SPEC §7). Something must build the
//! full index (walk → chunk → embed → graph) and round-trip it to disk.
//!
//! **What it is / does:** Owns the `Index` aggregate (chunks + import map +
//! recomputed BM25 and graph), a builder that indexes a directory with a chosen
//! embedder, and JSON save/load. Re-indexing is a full rebuild, which is
//! idempotent because chunk IDs are deterministic (SPEC §7).
//!
//! **Responsibilities:**
//! - Own `Index`, `build_from_dir`, `save`, `load`, and store-path helpers.
//! - Recompute BM25 and the graph on load (they are derived, not persisted).
//! - It does NOT run retrieval; it hands its data to `retriever`.

use crate::chunker::{Chunk, Chunker};
use crate::config::SPEC_VERSION;
use crate::embedder::Embedder;
use crate::graph_store::Graph;
use crate::keyword_store::Bm25Index;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// The persisted portion of an index.
#[derive(Debug, Serialize, Deserialize)]
struct IndexData {
    spec_version: String,
    #[serde(default = "default_embedder_name")]
    embedder: String,
    chunks: Vec<Chunk>,
    file_imports: BTreeMap<String, Vec<String>>,
    /// Whole-file token count per indexed file (DASHBOARD-SPEC §3). Optional so
    /// stores written before v1.1 still load (defaults to empty).
    #[serde(default)]
    file_tokens: BTreeMap<String, usize>,
}

fn default_embedder_name() -> String {
    "hash".to_string()
}

/// A fully-loaded, ready-to-query index.
pub struct Index {
    pub chunks: Vec<Chunk>,
    pub file_imports: BTreeMap<String, Vec<String>>,
    /// Whole-file token count per indexed file (DASHBOARD-SPEC §3), used to
    /// compute a search's `baseline_tokens` counterfactual.
    pub file_tokens: BTreeMap<String, usize>,
    pub embedder_name: String,
    pub bm25: Bm25Index,
    pub graph: Graph,
}

/// Summary produced while building an index.
#[derive(Debug, Clone, Copy)]
pub struct BuildStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    /// Files skipped by the Layer-1 sensitive-file policy (SPEC-V2.1 §2).
    pub sensitive_skipped: usize,
    /// Directories the walk could not read (permission-denied / IO). Nonzero means
    /// files beneath them were NOT indexed, so the index is incomplete and may
    /// differ across machines — the caller surfaces this to the operator (#133).
    pub walk_errors: usize,
    pub total_chunks: usize,
}

impl Index {
    /// Assemble the derived structures (BM25 + graph) from chunks and imports.
    fn assemble(
        chunks: Vec<Chunk>,
        file_imports: BTreeMap<String, Vec<String>>,
        file_tokens: BTreeMap<String, usize>,
        embedder_name: String,
    ) -> Index {
        Index::assemble_inner(chunks, file_imports, file_tokens, embedder_name, true)
    }

    /// Assemble an index, choosing whether to build the (expensive) BM25 statistics.
    /// `build_bm25 = false` leaves BM25 empty and builds only the import graph — used
    /// by the federation loader, where each member's own BM25 is never scored (only the
    /// assembled union's is), so building it per member re-tokenizes the whole corpus a
    /// second time for nothing. The graph IS still built: `combined_index` unions each
    /// member's intra-store graph (SPEC-V2.2 §6).
    fn assemble_inner(
        chunks: Vec<Chunk>,
        file_imports: BTreeMap<String, Vec<String>>,
        file_tokens: BTreeMap<String, usize>,
        embedder_name: String,
        build_bm25: bool,
    ) -> Index {
        let files: Vec<String> = {
            let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for c in &chunks {
                set.insert(c.file_path.clone());
            }
            set.into_iter().collect()
        };
        let bm25 = if build_bm25 {
            Bm25Index::build(&chunks)
        } else {
            Bm25Index::empty()
        };
        let graph = Graph::build(&file_imports, &files);
        Index { chunks, file_imports, file_tokens, embedder_name, bm25, graph }
    }

    /// Assemble a ready-to-query `Index` from raw parts. Used by federated search
    /// (SPEC-V2.2 §6) to build the union corpus over several members' chunks; the
    /// derived BM25 index and import graph are recomputed here exactly as for a
    /// freshly built store.
    pub fn from_parts(
        chunks: Vec<Chunk>,
        file_imports: BTreeMap<String, Vec<String>>,
        file_tokens: BTreeMap<String, usize>,
        embedder_name: String,
    ) -> Index {
        Index::assemble(chunks, file_imports, file_tokens, embedder_name)
    }

    /// Sum the whole-file token counts of a set of files (DASHBOARD-SPEC §3).
    /// Callers pass the DISTINCT file paths of a search's returned results; a
    /// file with no recorded count contributes 0.
    pub fn baseline_tokens<'a>(&self, files: impl Iterator<Item = &'a str>) -> u64 {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut total = 0u64;
        for f in files {
            if seen.insert(f) {
                total += self.file_tokens.get(f).copied().unwrap_or(0) as u64;
            }
        }
        total
    }

    /// Build an index by walking `root` and embedding with `embedder`, with the
    /// secure-by-default secret protection of SPEC-V2.1 (Layers 1 & 2) enabled.
    ///
    /// Errors if any chunk fails to embed (fallible backends only — the hash
    /// embedder cannot fail): a store must never contain empty embeddings (#30).
    pub fn build_from_dir(
        root: &Path,
        embedder: &dyn Embedder,
    ) -> Result<(Index, BuildStats), String> {
        Index::build_from_dir_filtered(root, embedder, |_| true)
    }

    /// Build an index over only the files for which `keep(rel_path)` is true, with
    /// secret protection enabled. Used by `cce bench` (SPEC §10.1).
    pub fn build_from_dir_filtered(
        root: &Path,
        embedder: &dyn Embedder,
        keep: impl Fn(&str) -> bool,
    ) -> Result<(Index, BuildStats), String> {
        Index::build_protected(root, embedder, keep, true)
    }

    /// Build an index, choosing whether SPEC-V2.1 secret protection is on.
    ///
    /// When `protect_secrets` is true (the secure default), Layer 1 skips
    /// sensitive files in the walk and Layer 2 redacts high-confidence secrets in
    /// each file's content *before* chunking — so the redacted text is what gets
    /// chunked, embedded, and stored. When false (`--allow-secrets`), both layers
    /// are off and content is indexed verbatim.
    ///
    /// Embedding runs in bounded batches of [`crate::config::EMBED_BATCH_SIZE`]
    /// chunks via [`Embedder::try_embed_batch`] (issue #38) and is fallible
    /// (issue #30): a failed batch — e.g. Ollama dying mid-index — aborts the
    /// whole build with an `Err`, so a store can never silently persist
    /// empty/dead embeddings. Nothing is written by this function; callers only
    /// `save` an `Ok` index.
    pub fn build_protected(
        root: &Path,
        embedder: &dyn Embedder,
        keep: impl Fn(&str) -> bool,
        protect_secrets: bool,
    ) -> Result<(Index, BuildStats), String> {
        let walked = crate::walker::walk(root, protect_secrets);
        let files_skipped = walked.skipped;
        let sensitive_skipped = walked.sensitive_skipped;
        let walk_errors = walked.walk_errors;
        let kept_files: Vec<&(String, String)> =
            walked.files.iter().filter(|(p, _)| keep(p)).collect();
        let files_indexed = kept_files.len();

        let mut chunker = Chunker::new();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut file_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut file_tokens: BTreeMap<String, usize> = BTreeMap::new();

        for (rel_path, raw) in kept_files {
            // Layer 2 (SPEC-V2.1 §2): redact before chunking, so the store never
            // sees the secret and chunk_id/token_count derive from redacted text.
            let content: String = if protect_secrets {
                crate::redactor::redact(raw)
            } else {
                raw.clone()
            };
            // Persist the whole-file token count for the baseline counterfactual
            // (DASHBOARD-SPEC §3 / SPEC-V2.5 §2), independent of how the file chunks.
            // Counted with the ONE savings estimator (`cce.tokens/v1`, SPEC-V2.5 §4)
            // so the retrieval baseline is coherent with `served_tokens`.
            file_tokens
                .insert(rel_path.clone(), crate::tokenizer::estimate_tokens(&content) as usize);
            let fc = chunker.chunk_file(rel_path, &content);
            if !fc.imports.is_empty() {
                file_imports.insert(rel_path.clone(), fc.imports);
            }
            chunks.extend(fc.chunks);
        }

        // Embed in bounded batches through the fallible batch API (#38), so an
        // HTTP backend (Ollama `POST /api/embed`) issues
        // ceil(chunks / EMBED_BATCH_SIZE) requests instead of one per chunk.
        // For the hash backend the default trait impl still maps the pure
        // per-text embed over each batch, so its vectors are byte-identical to
        // the old per-chunk path. Fail loud (#30): a failed batch — including a
        // response with the wrong number of vectors — aborts the whole build
        // with an `Err` naming the batch's file range; nothing is persisted,
        // never an empty vector.
        for batch in chunks.chunks_mut(crate::config::EMBED_BATCH_SIZE) {
            let texts: Vec<String> = batch.iter().map(|c| c.content.clone()).collect();
            let vectors = embedder.try_embed_batch(&texts).map_err(|e| {
                format!(
                    "embedding failed for {} ({e}). Aborting the index — a store must never \
                     contain empty embeddings. Fix the `{}` backend or re-index with the \
                     default hash embedder.",
                    batch_span(batch),
                    embedder.name()
                )
            })?;
            // Per-batch count guard (#30): a misaligned response must never be
            // zipped silently onto the wrong chunks.
            if vectors.len() != batch.len() {
                return Err(format!(
                    "embedding failed for {}: the `{}` backend returned {} vector(s) for {} \
                     chunk(s). Aborting the index — a store must never contain misaligned or \
                     empty embeddings.",
                    batch_span(batch),
                    embedder.name(),
                    vectors.len(),
                    batch.len()
                ));
            }
            for (chunk, vector) in batch.iter_mut().zip(vectors) {
                chunk.embedding = vector;
            }
        }

        let total_chunks = chunks.len();
        let index = Index::assemble(chunks, file_imports, file_tokens, embedder.name().to_string());
        Ok((
            index,
            BuildStats {
                files_indexed,
                files_skipped,
                sensitive_skipped,
                walk_errors,
                total_chunks,
            },
        ))
    }

    /// Persist the index to `path` (JSON). Creates parent directories.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = IndexData {
            spec_version: SPEC_VERSION.to_string(),
            embedder: self.embedder_name.clone(),
            chunks: self.chunks.clone(),
            file_imports: self.file_imports.clone(),
            file_tokens: self.file_tokens.clone(),
        };
        let json = serde_json::to_string(&data).map_err(io::Error::other)?;
        // #101: atomic temp-file + rename, never a bare truncate-then-write — a
        // crash/OOM/disk-full mid-save must not destroy the previous good store,
        // and a concurrent reader must never observe a truncated/0-byte index.json.
        crate::atomic::atomic_write(path, json.as_bytes())
    }

    /// Load an index from `path`, recomputing BM25 and the graph.
    pub fn load(path: &Path) -> io::Result<Index> {
        let json = std::fs::read_to_string(path)?;
        let data: IndexData = serde_json::from_str(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Index::assemble(data.chunks, data.file_imports, data.file_tokens, data.embedder))
    }

    /// Load an index but skip building BM25 (graph only). For the federation loader:
    /// a member store's own BM25 is never scored — only the assembled union's is — so
    /// building it per member re-tokenizes the whole corpus for nothing. The chunks,
    /// imports, file-token counts, and import graph are identical to [`Index::load`];
    /// only `bm25` differs (empty), and it is never read on this path.
    pub fn load_without_bm25(path: &Path) -> io::Result<Index> {
        let json = std::fs::read_to_string(path)?;
        let data: IndexData = serde_json::from_str(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Index::assemble_inner(
            data.chunks,
            data.file_imports,
            data.file_tokens,
            data.embedder,
            false,
        ))
    }

    /// Distinct file paths in the corpus.
    pub fn files(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for c in &self.chunks {
            set.insert(c.file_path.clone());
        }
        set.into_iter().collect()
    }
}

/// Human-readable span of an embedding batch for error context (#38): the
/// first and last chunk's `file:line`, so a batch failure still names where in
/// the walk it happened even though the request covered many chunks.
fn batch_span(batch: &[Chunk]) -> String {
    match (batch.first(), batch.last()) {
        (Some(first), Some(last)) if batch.len() > 1 => format!(
            "a batch of {} chunks ({}:{} .. {}:{})",
            batch.len(),
            first.file_path,
            first.start_line,
            last.file_path,
            last.start_line
        ),
        (Some(only), _) => {
            format!("chunk {}:{} ({})", only.file_path, only.start_line, only.chunk_id)
        }
        _ => "an empty batch".to_string(),
    }
}

/// Default store path for an indexed root: `<root>/.cce/index.json`.
pub fn default_store_path(root: &Path) -> PathBuf {
    root.join(".cce").join("index.json")
}

/// Default metrics-log path for an indexed root: `<root>/.cce/metrics.jsonl`
/// (DASHBOARD-SPEC §2). The metrics log lives beside the index in the store dir.
pub fn default_metrics_path(root: &Path) -> PathBuf {
    root.join(".cce").join(crate::config::METRICS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
    }

    #[test]
    fn builds_seven_chunks_from_fixture() {
        let e = HashEmbedder;
        let (idx, stats) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        assert_eq!(stats.total_chunks, 7);
        assert_eq!(idx.chunks.len(), 7);
        // payments.py -> auth edge present
        assert_eq!(idx.file_imports.get("payments.py"), Some(&vec!["auth".to_string()]));
    }

    #[cfg(unix)]
    #[test]
    fn build_stats_carry_walk_errors_for_an_unreadable_dir() {
        // Issue #133: the traversal-error count must travel from WalkResult all the
        // way to BuildStats (and thence the operator summary + warning), or the
        // silent loss of files under an unreadable directory stays invisible.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("top.py"), "top = 1\n").unwrap();
        let sub = root.join("locked");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.py"), "inner = 2\n").unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        // If we can still read it (e.g. running as root), the scenario can't be
        // exercised — restore and bail rather than assert a false negative.
        let privileged = std::fs::read_dir(&sub).is_ok();

        let e = HashEmbedder;
        let (_idx, stats) = Index::build_from_dir(root, &e).unwrap();

        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        if privileged {
            return;
        }
        assert!(stats.walk_errors >= 1, "an unreadable dir must reach BuildStats.walk_errors");
    }

    #[test]
    fn batched_hash_embeddings_match_per_chunk_embed() {
        // #38: the batched build path must leave the hash backend's output
        // byte-identical to embedding each chunk individually — batching is an
        // execution detail, never a semantic one.
        use crate::embedder::Embedder;
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        for c in &idx.chunks {
            assert_eq!(c.embedding, e.embed(&c.content), "chunk {}", c.chunk_id);
        }
    }

    #[test]
    fn batch_count_mismatch_aborts_the_build() {
        // #38: the store-level per-batch count guard — a backend returning the
        // wrong number of vectors must abort the build, never be zipped
        // silently onto the wrong chunks.
        struct ShortBatchEmbedder;
        impl crate::embedder::Embedder for ShortBatchEmbedder {
            fn embed(&self, _text: &str) -> Vec<f64> {
                vec![1.0]
            }
            fn try_embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
                Ok(vec![vec![1.0]; texts.len().saturating_sub(1)])
            }
            fn name(&self) -> &'static str {
                "short-batch"
            }
        }
        let err = match Index::build_from_dir(&fixture_dir(), &ShortBatchEmbedder) {
            Ok(_) => panic!("a count-mismatched batch must abort the build"),
            Err(e) => e,
        };
        assert!(err.contains("vector(s) for"), "must name the mismatch: {err}");
        assert!(err.contains("Aborting the index"), "must say it aborted: {err}");
    }

    #[test]
    fn persists_whole_file_token_counts_and_baseline_sums() {
        // DASHBOARD-SPEC §3 / SPEC-V2.5 §2+§4: the index persists each file's
        // whole-file token estimate (the "read the whole file" counterfactual) with
        // the ONE savings estimator, so baseline_tokens is accurate and coherent
        // with served_tokens.
        use crate::tokenizer::estimate_tokens;
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();

        // Every source file has a whole-file token count equal to the estimator over
        // the file's full contents.
        for name in ["auth.py", "payments.py", "README.md"] {
            let src = std::fs::read_to_string(fixture_dir().join(name)).unwrap();
            assert_eq!(
                idx.file_tokens.get(name).copied(),
                Some(estimate_tokens(&src) as usize),
                "{name}"
            );
        }

        // The baseline over a set of DISTINCT result files sums their whole-file
        // counts; a missing file contributes 0.
        let baseline = idx.baseline_tokens(["auth.py", "auth.py", "nope.py"].iter().copied());
        assert_eq!(baseline, idx.file_tokens["auth.py"] as u64);

        // Survives a save/load round-trip.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        idx.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.file_tokens, idx.file_tokens);
    }

    #[test]
    fn save_load_roundtrip() {
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        idx.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.chunks.len(), idx.chunks.len());
        // embeddings survive the round-trip
        assert_eq!(loaded.chunks[0].embedding, idx.chunks[0].embedding);
    }

    #[test]
    fn save_persists_exactly_the_serialized_bytes() {
        // #101: switching to an atomic temp-file + rename must change ONLY the
        // write mechanism — the on-disk bytes stay byte-identical to a direct
        // `serde_json::to_string` of the same `IndexData`, so conformance/golden
        // stores and pinned checksums are untouched.
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        idx.save(&path).unwrap();
        let expected = serde_json::to_string(&IndexData {
            spec_version: SPEC_VERSION.to_string(),
            embedder: idx.embedder_name.clone(),
            chunks: idx.chunks.clone(),
            file_imports: idx.file_imports.clone(),
            file_tokens: idx.file_tokens.clone(),
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
    }

    #[test]
    fn save_leaves_no_temp_file_and_writes_a_complete_store() {
        // #101 use case: after a save the store directory holds ONLY index.json —
        // no leftover temp/partial file — and the destination is the complete,
        // loadable store (never a 0-byte torn read).
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".cce");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.json");
        idx.save(&path).unwrap();
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["index.json".to_string()]);
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        Index::load(&path).unwrap();
    }

    #[test]
    fn reindex_is_idempotent() {
        let e = HashEmbedder;
        let (a, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let (b, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let ids_a: Vec<&String> = a.chunks.iter().map(|c| &c.chunk_id).collect();
        let ids_b: Vec<&String> = b.chunks.iter().map(|c| &c.chunk_id).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // Nested path whose parents do not yet exist.
        let path = tmp.path().join("a").join("b").join("index.json");
        idx.save(&path).unwrap();
        assert!(path.exists());
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.chunks.len(), idx.chunks.len());
    }

    #[test]
    fn load_legacy_json_without_embedder_defaults_to_hash() {
        // A store written before the `embedder` field existed must still load,
        // defaulting the backend name to "hash" (serde default).
        let json = r#"{"spec_version":"1.0","chunks":[],"file_imports":{}}"#;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("legacy.json");
        std::fs::write(&path, json).unwrap();
        let idx = Index::load(&path).unwrap();
        assert_eq!(idx.embedder_name, "hash");
        assert!(idx.chunks.is_empty());
    }

    #[test]
    fn load_invalid_json_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("garbage.json");
        std::fs::write(&path, "not valid json at all").unwrap();
        let err = match Index::load(&path) {
            Ok(_) => panic!("invalid JSON must not load"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn default_store_path_appends_cce_index_json() {
        let p = default_store_path(Path::new("/some/root"));
        assert_eq!(p, Path::new("/some/root/.cce/index.json"));
    }
}
