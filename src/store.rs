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
}

fn default_embedder_name() -> String {
    "hash".to_string()
}

/// A fully-loaded, ready-to-query index.
pub struct Index {
    pub chunks: Vec<Chunk>,
    pub file_imports: BTreeMap<String, Vec<String>>,
    pub embedder_name: String,
    pub bm25: Bm25Index,
    pub graph: Graph,
}

/// Summary produced while building an index.
#[derive(Debug, Clone, Copy)]
pub struct BuildStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub total_chunks: usize,
}

impl Index {
    /// Assemble the derived structures (BM25 + graph) from chunks and imports.
    fn assemble(
        chunks: Vec<Chunk>,
        file_imports: BTreeMap<String, Vec<String>>,
        embedder_name: String,
    ) -> Index {
        let files: Vec<String> = {
            let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for c in &chunks {
                set.insert(c.file_path.clone());
            }
            set.into_iter().collect()
        };
        let bm25 = Bm25Index::build(&chunks);
        let graph = Graph::build(&file_imports, &files);
        Index { chunks, file_imports, embedder_name, bm25, graph }
    }

    /// Build an index by walking `root` and embedding with `embedder`.
    pub fn build_from_dir(root: &Path, embedder: &dyn Embedder) -> (Index, BuildStats) {
        Index::build_from_dir_filtered(root, embedder, |_| true)
    }

    /// Build an index over only the files for which `keep(rel_path)` is true.
    /// Used by `cce bench` to index a repo's Python sources (SPEC §10.1).
    pub fn build_from_dir_filtered(
        root: &Path,
        embedder: &dyn Embedder,
        keep: impl Fn(&str) -> bool,
    ) -> (Index, BuildStats) {
        let walked = crate::walker::walk(root);
        let files_skipped = walked.skipped;
        let kept_files: Vec<&(String, String)> =
            walked.files.iter().filter(|(p, _)| keep(p)).collect();
        let files_indexed = kept_files.len();

        let mut chunker = Chunker::new();
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut file_imports: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for (rel_path, content) in kept_files {
            let fc = chunker.chunk_file(rel_path, content);
            if !fc.imports.is_empty() {
                file_imports.insert(rel_path.clone(), fc.imports);
            }
            for mut chunk in fc.chunks {
                chunk.embedding = embedder.embed(&chunk.content);
                chunks.push(chunk);
            }
        }

        let total_chunks = chunks.len();
        let index = Index::assemble(chunks, file_imports, embedder.name().to_string());
        (index, BuildStats { files_indexed, files_skipped, total_chunks })
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
        };
        let json = serde_json::to_string(&data).map_err(io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Load an index from `path`, recomputing BM25 and the graph.
    pub fn load(path: &Path) -> io::Result<Index> {
        let json = std::fs::read_to_string(path)?;
        let data: IndexData = serde_json::from_str(&json)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(Index::assemble(data.chunks, data.file_imports, data.embedder))
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

/// Default store path for an indexed root: `<root>/.cce/index.json`.
pub fn default_store_path(root: &Path) -> PathBuf {
    root.join(".cce").join("index.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture"))
    }

    #[test]
    fn builds_seven_chunks_from_fixture() {
        let e = HashEmbedder;
        let (idx, stats) = Index::build_from_dir(&fixture_dir(), &e);
        assert_eq!(stats.total_chunks, 7);
        assert_eq!(idx.chunks.len(), 7);
        // payments.py -> auth edge present
        assert_eq!(idx.file_imports.get("payments.py"), Some(&vec!["auth".to_string()]));
    }

    #[test]
    fn save_load_roundtrip() {
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        idx.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.chunks.len(), idx.chunks.len());
        // embeddings survive the round-trip
        assert_eq!(loaded.chunks[0].embedding, idx.chunks[0].embedding);
    }

    #[test]
    fn reindex_is_idempotent() {
        let e = HashEmbedder;
        let (a, _) = Index::build_from_dir(&fixture_dir(), &e);
        let (b, _) = Index::build_from_dir(&fixture_dir(), &e);
        let ids_a: Vec<&String> = a.chunks.iter().map(|c| &c.chunk_id).collect();
        let ids_b: Vec<&String> = b.chunks.iter().map(|c| &c.chunk_id).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let e = HashEmbedder;
        let (idx, _) = Index::build_from_dir(&fixture_dir(), &e);
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
