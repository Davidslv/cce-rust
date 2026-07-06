//! # sync::artifact — the portable, byte-exact interchange format (SPEC-SYNC §2,
//! reconciled to the single canonical format in SPEC-SYNC-RECONCILE.md)
//!
//! **Why this file exists:** Ruby stores in SQLite, Rust in JSON, so the cache
//! cannot be either native store. SPEC-SYNC §2 defines a *canonical, deterministic*
//! interchange artifact both engines export and import. It must be **byte-identical
//! across people and across both engines** for the same `repo@sha` — that identity
//! is what makes the cache content-addressable and `--verify` meaningful. This file
//! owns that format down to the last byte.
//!
//! **What it is / does:** A UTF-8 stream with an LF after **every** line (including
//! the last):
//!   line 1        = the manifest JSON,
//!   lines 2..N+1  = one JSON object per chunk, sorted by `(file_path, start_line,
//!                   id)` (N = `chunk_count`),
//!   line N+2      = the graph JSON `{"edges":[…],"nodes":[…]}`.
//! Every object uses **sorted keys and compact separators** (serde_json's default
//! `Map` is a `BTreeMap`, so `to_string` yields sorted, whitespace-free JSON).
//! Embeddings are encoded as **standard base64 (with padding) of 256 little-endian
//! IEEE-754 `f64` bytes** (NOT decimals), so the bytes match across languages
//! regardless of float→string formatting. **Provenance is removed entirely** (no
//! `built_at`/`built_by`) so the artifact is reproducible. `file_tokens` lives in
//! the manifest. `checksum` = lowercase-hex SHA-256 over the ENTIRE stream built
//! with the manifest's `checksum` value set to `""`.
//!
//! **Responsibilities:**
//! - Own the `Manifest`/`Artifact` types, canonical (de)serialization, the checksum.
//! - Own the base64 f64 embedding codec and the lossless `Index` <-> `Artifact` map.
//! - It does NOT know about git, remotes, or the CLI.

use crate::chunker::Chunk;
use crate::store::Index;
use crate::sync::hex_lower;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The edge `type` recorded for every import relationship in the graph.
pub const EDGE_TYPE: &str = "import";

/// The artifact manifest (SPEC-SYNC §2, line 1). Every field is deterministic for a
/// given `repo@sha` and pack set, so the whole artifact is reproducible. Keys in the
/// serialized form are sorted: `cce_version, checksum, chunk_count, embedder,
/// file_tokens, pack_set_id, repo_id, sha`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub repo_id: String,
    pub sha: String,
    /// `cce_version` at `major.minor` (the format-compat window).
    pub cce_version: String,
    /// Always `"hash"` for a shareable artifact.
    pub embedder: String,
    pub pack_set_id: String,
    pub chunk_count: usize,
    /// Lowercase-hex SHA-256 checksum (filled in by `Artifact::from_index`).
    pub checksum: String,
    /// Whole-file token counts (DASH §3 baseline), keys sorted. Lives in the
    /// manifest per the canonical format; keeps the round-trip lossless.
    pub file_tokens: BTreeMap<String, usize>,
}

/// The metadata needed to stamp a manifest, supplied by the caller. Provenance is
/// gone — only the content identity remains.
#[derive(Debug, Clone)]
pub struct ManifestMeta {
    pub repo_id: String,
    pub sha: String,
}

/// A fully-materialized artifact: its manifest plus the content it carries.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub manifest: Manifest,
    /// Chunks in canonical `(file_path, start_line, id)` order.
    pub chunks: Vec<Chunk>,
    /// The raw import structure (file -> imported module names), which the graph
    /// line serializes as edges and which round-trips losslessly.
    pub file_imports: BTreeMap<String, Vec<String>>,
}

/// Encode a 256-d embedding as standard base64 (with padding, no newlines) of its
/// little-endian IEEE-754 `f64` bytes (SPEC-SYNC §2). An empty vector → `""`.
pub fn encode_embedding(embedding: &[f64]) -> String {
    let mut bytes = Vec::with_capacity(embedding.len() * 8);
    for &x in embedding {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    B64.encode(bytes)
}

/// Decode a base64 embedding back into `f64`s. Errors on invalid base64 or a byte
/// length that is not a multiple of 8.
pub fn decode_embedding(s: &str) -> Result<Vec<f64>, String> {
    let bytes = B64.decode(s.as_bytes()).map_err(|e| format!("bad embedding base64: {e}"))?;
    if bytes.len() % 8 != 0 {
        return Err(format!("embedding byte length {} is not a multiple of 8", bytes.len()));
    }
    // The %8 check above guarantees an empty remainder.
    let (chunks, _remainder) = bytes.as_chunks::<8>();
    Ok(chunks.iter().map(|arr| f64::from_le_bytes(*arr)).collect())
}

/// Canonical JSON `Value` for one chunk (keys sorted by serde_json's `BTreeMap`).
fn chunk_value(c: &Chunk) -> Value {
    json!({
        "chunk_type": c.chunk_type,
        "content": c.content,
        "embedding": encode_embedding(&c.embedding),
        "end_line": c.end_line,
        "file_path": c.file_path,
        "id": c.chunk_id,
        "kind": c.kind,
        "language": c.language,
        "start_line": c.start_line,
        "token_count": c.token_count,
    })
}

/// Reconstruct a `Chunk` from its canonical JSON `Value` (lossless inverse of
/// `chunk_value`).
fn chunk_from_value(v: &Value) -> Result<Chunk, String> {
    let get_str = |k: &str| -> Result<String, String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("chunk missing string field `{k}`"))
    };
    let get_usize = |k: &str| -> Result<usize, String> {
        v.get(k)
            .and_then(|x| x.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| format!("chunk missing integer field `{k}`"))
    };
    let embedding = decode_embedding(&get_str("embedding")?)?;
    Ok(Chunk {
        chunk_id: get_str("id")?,
        file_path: get_str("file_path")?,
        start_line: get_usize("start_line")?,
        end_line: get_usize("end_line")?,
        chunk_type: get_str("chunk_type")?,
        kind: get_str("kind")?,
        language: get_str("language")?,
        content: get_str("content")?,
        token_count: get_usize("token_count")?,
        embedding,
    })
}

/// Canonical JSON `Value` for the graph line: `{"edges":[…],"nodes":[…]}`.
///
/// - **nodes** = every indexed file (the keys of `file_tokens`), each `{"id": path}`,
///   sorted by `id`.
/// - **edges** = the **resolved** `file → file` import edges (base SPEC §6.7): an
///   edge `A → B` exists only when a module imported by `A` resolves — by the same
///   stem-matching the retriever's graph expansion uses — to a corpus file `B`.
///   External / unresolved imports (e.g. `os`, `fs`, `std`) produce NO edge. Each
///   edge is `{"source": A, "target": B, "type": "import"}`, sorted by
///   `(source, target, type)`.
fn graph_value(
    file_imports: &BTreeMap<String, Vec<String>>,
    file_tokens: &BTreeMap<String, usize>,
) -> Value {
    let files: Vec<String> = file_tokens.keys().cloned().collect();
    let nodes: Vec<Value> = files.iter().map(|id| json!({ "id": id })).collect();

    // Resolve module names to corpus files exactly as the query-time graph does;
    // out_pairs yields resolved (source, target) edges in sorted order.
    let graph = crate::graph_store::Graph::build(file_imports, &files);
    let mut pairs = graph.out_pairs();
    pairs.sort();
    let edges: Vec<Value> = pairs
        .into_iter()
        .map(|(source, target)| json!({ "source": source, "target": target, "type": EDGE_TYPE }))
        .collect();

    json!({ "edges": edges, "nodes": nodes })
}

/// The filename stem of a path (basename without its last extension), used to turn
/// a resolved `target` file back into a module name the resolver re-resolves to it.
fn file_stem(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => base.to_string(),
    }
}

/// Reconstruct `file_imports` from the graph value's resolved `edges`, grouping by
/// `source` with each `target` reduced to a module name (its file stem). Because the
/// serialized edges are already resolved, re-`build`ing the graph over these stems
/// yields the identical file→file edges — so search-expansion behaviour is preserved
/// (external imports that produced no edge are simply absent, which changes nothing).
fn imports_from_graph(graph: &Value) -> Result<BTreeMap<String, Vec<String>>, String> {
    let edges = graph
        .get("edges")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "graph missing array `edges`".to_string())?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in edges {
        let source = e
            .get("source")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "edge missing string `source`".to_string())?;
        let target = e
            .get("target")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "edge missing string `target`".to_string())?;
        out.entry(source.to_string()).or_default().push(file_stem(target));
    }
    Ok(out)
}

/// The manifest JSON `Value`. Keys are sorted by serde_json's `BTreeMap` backing.
fn manifest_value(m: &Manifest) -> Value {
    json!({
        "cce_version": m.cce_version,
        "checksum": m.checksum,
        "chunk_count": m.chunk_count,
        "embedder": m.embedder,
        "file_tokens": m.file_tokens,
        "pack_set_id": m.pack_set_id,
        "repo_id": m.repo_id,
        "sha": m.sha,
    })
}

/// Parse the manifest line into a `Manifest`.
fn parse_manifest(line: &str) -> Result<Manifest, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("bad manifest JSON: {e}"))?;
    let s = |k: &str| -> Result<String, String> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|x| x.to_string())
            .ok_or_else(|| format!("manifest missing string field `{k}`"))
    };
    let chunk_count = v
        .get("chunk_count")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize)
        .ok_or_else(|| "manifest missing integer field `chunk_count`".to_string())?;
    let ft_obj = v
        .get("file_tokens")
        .and_then(|x| x.as_object())
        .ok_or_else(|| "manifest missing object `file_tokens`".to_string())?;
    let mut file_tokens: BTreeMap<String, usize> = BTreeMap::new();
    for (k, val) in ft_obj {
        let n = val.as_u64().ok_or_else(|| format!("file_tokens[{k}] is not an integer"))?;
        file_tokens.insert(k.clone(), n as usize);
    }
    Ok(Manifest {
        repo_id: s("repo_id")?,
        sha: s("sha")?,
        cce_version: s("cce_version")?,
        embedder: s("embedder")?,
        pack_set_id: s("pack_set_id")?,
        chunk_count,
        checksum: s("checksum")?,
        file_tokens,
    })
}

/// Sort chunks into the canonical `(file_path, start_line, id)` order.
fn sort_chunks(chunks: &mut [Chunk]) {
    chunks.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
}

impl Artifact {
    /// The canonical stream, computing the graph from `file_imports` + the
    /// manifest's `file_tokens`.
    fn stream(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(&manifest_value(&self.manifest).to_string());
        out.push('\n');
        for c in &self.chunks {
            out.push_str(&chunk_value(c).to_string());
            out.push('\n');
        }
        out.push_str(&graph_value(&self.file_imports, &self.manifest.file_tokens).to_string());
        out.push('\n');
        out.into_bytes()
    }

    /// Export a local `Index` to a canonical artifact (SPEC-SYNC §2). The index MUST
    /// have been built with the hash embedder — the caller enforces that; here we
    /// stamp `embedder = "hash"` and compute everything deterministically.
    pub fn from_index(index: &Index, meta: ManifestMeta) -> Artifact {
        let mut chunks = index.chunks.clone();
        sort_chunks(&mut chunks);
        let manifest = Manifest {
            repo_id: meta.repo_id,
            sha: meta.sha,
            cce_version: crate::sync::SYNC_FORMAT_VERSION.to_string(),
            embedder: crate::sync::HASH_EMBEDDER.to_string(),
            pack_set_id: crate::sync::pack_set_id(),
            chunk_count: chunks.len(),
            checksum: String::new(),
            file_tokens: index.file_tokens.clone(),
        };
        let mut artifact = Artifact { manifest, chunks, file_imports: index.file_imports.clone() };
        // Checksum = SHA-256 over the whole stream with checksum == "".
        artifact.manifest.checksum = artifact.computed_checksum();
        artifact
    }

    /// The canonical artifact bytes (line 1 carries the real checksum).
    pub fn to_bytes(&self) -> Vec<u8> {
        self.stream()
    }

    /// Recompute the checksum from the current content: SHA-256 over the whole
    /// canonical stream serialized with the manifest's `checksum` set to `""`.
    pub fn computed_checksum(&self) -> String {
        let mut probe = self.clone();
        probe.manifest.checksum = String::new();
        hex_lower(&Sha256::digest(probe.stream()))
    }

    /// Parse a canonical artifact from bytes, validating the structure and the
    /// stored checksum.
    pub fn from_bytes(bytes: &[u8]) -> Result<Artifact, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| format!("artifact is not UTF-8: {e}"))?;
        // Every line is LF-terminated; the trailing newline yields a final empty
        // element which we drop.
        let mut lines: Vec<&str> = text.split('\n').collect();
        if lines.last() == Some(&"") {
            lines.pop();
        }
        if lines.len() < 2 {
            return Err("artifact too short: need at least a manifest and a graph line".to_string());
        }
        let manifest = parse_manifest(lines[0])?;
        let n = manifest.chunk_count;
        if lines.len() != n + 2 {
            return Err(format!(
                "artifact line count {} does not match chunk_count {} (expected {})",
                lines.len(),
                n,
                n + 2
            ));
        }
        let mut chunks = Vec::with_capacity(n);
        for line in &lines[1..=n] {
            let v: Value =
                serde_json::from_str(line).map_err(|e| format!("bad chunk JSON: {e}"))?;
            chunks.push(chunk_from_value(&v)?);
        }
        let graph: Value =
            serde_json::from_str(lines[n + 1]).map_err(|e| format!("bad graph JSON: {e}"))?;
        let file_imports = imports_from_graph(&graph)?;

        let artifact = Artifact { manifest, chunks, file_imports };
        let recomputed = artifact.computed_checksum();
        if recomputed != artifact.manifest.checksum {
            return Err(format!(
                "checksum mismatch: manifest says {}, recomputed {}",
                artifact.manifest.checksum, recomputed
            ));
        }
        Ok(artifact)
    }

    /// Materialize a ready-to-persist `Index` from the artifact (the lossless
    /// inverse of `from_index`). The embedder name is `"hash"`.
    pub fn into_index(self) -> Index {
        Index::from_parts(
            self.chunks,
            self.file_imports,
            self.manifest.file_tokens,
            crate::sync::HASH_EMBEDDER.to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
    }

    fn samples() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"))
    }

    fn meta() -> ManifestMeta {
        ManifestMeta {
            repo_id: "example.com__acme__demo".to_string(),
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
        }
    }

    fn built() -> Artifact {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder);
        Artifact::from_index(&idx, meta())
    }

    #[test]
    fn embedding_codec_round_trips_exactly() {
        let v: Vec<f64> = (0..256).map(|i| (i as f64) * 0.001 - 0.123).collect();
        let enc = encode_embedding(&v);
        let dec = decode_embedding(&enc).unwrap();
        assert_eq!(v, dec);
        // 256 f64 = 2048 bytes -> base64 length 2732 (with padding).
        assert_eq!(enc.len(), 2732);
    }

    #[test]
    fn empty_embedding_encodes_to_empty_string() {
        assert_eq!(encode_embedding(&[]), "");
        assert_eq!(decode_embedding("").unwrap(), Vec::<f64>::new());
    }

    #[test]
    fn decode_rejects_bad_length() {
        let enc = B64.encode([1u8, 2, 3]);
        assert!(decode_embedding(&enc).is_err());
    }

    #[test]
    fn decode_rejects_invalid_base64() {
        assert!(decode_embedding("!!!!").is_err());
    }

    #[test]
    fn manifest_line_has_exactly_the_canonical_keys_sorted() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let first = text.lines().next().unwrap();
        // Keys, in order, with no provenance.
        assert!(first.starts_with("{\"cce_version\":\"2.3\",\"checksum\":\""));
        assert!(!first.contains("built_at"));
        assert!(!first.contains("built_by"));
        // Canonical key order: cce_version < checksum < chunk_count < embedder <
        // file_tokens < pack_set_id < repo_id < sha.
        let keys: Vec<&str> = [
            "\"cce_version\"",
            "\"checksum\"",
            "\"chunk_count\"",
            "\"embedder\"",
            "\"file_tokens\"",
            "\"pack_set_id\"",
            "\"repo_id\"",
            "\"sha\"",
        ]
        .to_vec();
        let mut last = 0usize;
        for k in keys {
            let idx = first.find(k).unwrap_or_else(|| panic!("missing key {k}"));
            assert!(idx >= last, "key {k} out of order");
            last = idx;
        }
        // Compact separators.
        assert!(!first.contains(", "));
        assert!(!first.contains(": "));
    }

    #[test]
    fn stream_shape_is_manifest_chunks_graph_with_trailing_lf() {
        let a = built();
        let bytes = a.to_bytes();
        assert_eq!(bytes.last(), Some(&b'\n'), "the last line is LF-terminated");
        let text = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = text.split('\n').collect();
        // manifest + N chunks + graph + trailing empty (from the final LF).
        assert_eq!(lines.len(), a.manifest.chunk_count + 2 + 1);
        assert!(lines[0].contains("\"chunk_count\":"));
        // Graph is the last non-empty line.
        assert!(lines[a.manifest.chunk_count + 1].starts_with("{\"edges\":"));
    }

    #[test]
    fn graph_line_edges_are_resolved_file_to_file() {
        // base fixture: payments.py imports `auth` (resolves to auth.py) and auth.py
        // imports `hashlib` (external → NO edge).
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let graph_line = text.lines().nth(a.manifest.chunk_count + 1).unwrap();
        let g: Value = serde_json::from_str(graph_line).unwrap();
        // Nodes are every indexed file, sorted by id.
        let node_ids: Vec<&str> =
            g["nodes"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
        let mut sorted = node_ids.clone();
        sorted.sort_unstable();
        assert_eq!(node_ids, sorted);
        assert!(node_ids.contains(&"auth.py"));
        // Exactly one RESOLVED edge: payments.py -> auth.py; the external hashlib
        // import produces none.
        let edges = g["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "only the resolved edge is emitted");
        assert_eq!(edges[0]["source"], "payments.py");
        assert_eq!(edges[0]["target"], "auth.py");
        assert_eq!(edges[0]["type"], "import");
    }

    #[test]
    fn external_imports_produce_no_edges() {
        // The samples corpus: every import (os, fs, std, …) is external, so edges=[].
        let (idx, _) = Index::build_from_dir(&samples(), &HashEmbedder);
        let a = Artifact::from_index(&idx, meta());
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let graph_line = text.lines().nth(a.manifest.chunk_count + 1).unwrap();
        let g: Value = serde_json::from_str(graph_line).unwrap();
        assert!(g["edges"].as_array().unwrap().is_empty(), "no external edges");
        assert_eq!(g["nodes"].as_array().unwrap().len(), 7);
    }

    #[test]
    fn checksum_is_deterministic_and_recomputes() {
        let a = built();
        let b = built();
        assert_eq!(a.manifest.checksum, b.manifest.checksum);
        assert_eq!(a.manifest.checksum, a.computed_checksum());
        assert_eq!(a.to_bytes(), b.to_bytes(), "artifact bytes are byte-identical");
        assert_eq!(a.manifest.checksum.len(), 64);
    }

    #[test]
    fn round_trips_bytes_losslessly() {
        let a = built();
        let bytes = a.to_bytes();
        let parsed = Artifact::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.manifest, a.manifest);
        // The byte round-trip is exact (re-serializing the parsed artifact reproduces
        // the input), which is the real cross-engine guarantee.
        assert_eq!(parsed.to_bytes(), bytes);
        assert_eq!(parsed.chunks.len(), a.chunks.len());
        for (x, y) in parsed.chunks.iter().zip(a.chunks.iter()) {
            assert_eq!(x, y);
        }
    }

    #[test]
    fn import_reconstructs_a_graph_with_identical_expansion_behaviour() {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder);
        let a = Artifact::from_index(&idx, meta());
        // Import from the SERIALIZED bytes, so file_imports is reconstructed from the
        // resolved graph edges (dropping external imports is behaviour-preserving).
        let restored = Artifact::from_bytes(&a.to_bytes()).unwrap().into_index();
        assert_eq!(restored.chunks.len(), idx.chunks.len());
        assert_eq!(restored.file_tokens, idx.file_tokens);
        assert_eq!(restored.embedder_name, "hash");
        // The resolved import graph — what drives search expansion — is identical.
        assert_eq!(restored.graph.out_pairs(), idx.graph.out_pairs());
        assert_eq!(
            restored.graph.out_pairs(),
            vec![("payments.py".to_string(), "auth.py".to_string())]
        );
    }

    #[test]
    fn from_bytes_rejects_tampered_checksum() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let tampered = text.replacen(&a.manifest.checksum, &"0".repeat(64), 1);
        let err = Artifact::from_bytes(tampered.as_bytes()).unwrap_err();
        assert!(err.contains("checksum mismatch"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_truncated_stream() {
        let err = Artifact::from_bytes(b"{}\n").unwrap_err();
        assert!(err.contains("manifest") || err.contains("too short"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_wrong_chunk_count() {
        let a = built();
        let mut lines: Vec<String> =
            String::from_utf8(a.to_bytes()).unwrap().lines().map(|s| s.to_string()).collect();
        lines.remove(1);
        let text = lines.join("\n") + "\n";
        let err = Artifact::from_bytes(text.as_bytes()).unwrap_err();
        assert!(err.contains("line count"), "got: {err}");
    }

    #[test]
    fn from_bytes_rejects_non_utf8() {
        let err = Artifact::from_bytes(&[0xff, 0xfe, 0x00]).unwrap_err();
        assert!(err.contains("not UTF-8"), "got: {err}");
    }

    #[test]
    fn parse_manifest_rejects_missing_field() {
        // Missing `file_tokens`.
        let line = "{\"cce_version\":\"2.3\",\"checksum\":\"x\",\"chunk_count\":0,\"embedder\":\"hash\",\"pack_set_id\":\"p\",\"repo_id\":\"r\",\"sha\":\"s\"}";
        let err = parse_manifest(line).unwrap_err();
        assert!(err.contains("file_tokens"), "got: {err}");
    }

    #[test]
    fn imports_from_graph_rejects_missing_edges() {
        let g: Value = serde_json::from_str("{\"nodes\":[]}").unwrap();
        assert!(imports_from_graph(&g).is_err());
    }

    #[test]
    fn chunk_from_value_rejects_missing_field() {
        let v: Value = serde_json::from_str(
            "{\"chunk_type\":\"module\",\"embedding\":\"\",\"end_line\":1,\"file_path\":\"a\",\"id\":\"x\",\"kind\":\"module\",\"language\":\"plaintext\",\"start_line\":1,\"token_count\":1}",
        )
        .unwrap();
        assert!(chunk_from_value(&v).is_err());
    }

    /// The **shared golden** (SPEC-SYNC-RECONCILE.md): index `test/fixture/samples`
    /// and build the artifact with the forced identity `repo_id = "cce/demo"`,
    /// `sha = "0"*40`. Both engines MUST reproduce this checksum, and the raw bytes
    /// are written to `/tmp/cce_artifact_rust.cce` for a byte-for-byte diff against
    /// Ruby. A diff here is a breaking-format decision, not a test to "fix".
    #[test]
    fn shared_golden_checksum_for_samples() {
        let (idx, _) = Index::build_from_dir(&samples(), &HashEmbedder);
        let meta = ManifestMeta {
            repo_id: "cce/demo".to_string(),
            sha: "0000000000000000000000000000000000000000".to_string(),
        };
        let a = Artifact::from_index(&idx, meta);
        // Emit the raw bytes for the cross-engine diff (best-effort; ignore errors
        // on platforms without /tmp).
        let _ = std::fs::write("/tmp/cce_artifact_rust.cce", a.to_bytes());
        assert_eq!(a.manifest.repo_id, "cce/demo");
        assert_eq!(
            a.manifest.checksum,
            "581cbd0ff682a38d7d1250f3eec44f4ce456bdd660d4cb29aaaadd9e95072f48"
        );
    }

    /// Builder-independence, end to end (issue #24, generalised repro). Two builds
    /// of the same tree at the same sha — one on a "dev machine" that has the
    /// gitignored, auto-generated file on disk, one from a "clean CI checkout" that
    /// does not — MUST yield the IDENTICAL artifact checksum. Before the walker
    /// honored `.gitignore`, the dev build indexed the extra file and diverged,
    /// which is exactly what made `cce sync verify` false-fail.
    #[test]
    fn artifact_checksum_is_independent_of_gitignored_files_on_disk() {
        let build = |with_generated: bool| -> String {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            // A Next.js-style tree: the generated `next-env.d.ts` is git-ignored.
            std::fs::write(root.join(".gitignore"), "next-env.d.ts\n").unwrap();
            std::fs::write(
                root.join("index.ts"),
                "export function greet(name: string): string {\n  return `hi ${name}`;\n}\n",
            )
            .unwrap();
            if with_generated {
                std::fs::write(
                    root.join("next-env.d.ts"),
                    "/// <reference types=\"next\" />\n/// <reference types=\"next/image-types/global\" />\n",
                )
                .unwrap();
            }
            let (idx, _) = Index::build_from_dir(root, &HashEmbedder);
            Artifact::from_index(&idx, meta()).manifest.checksum
        };
        assert_eq!(
            build(true),
            build(false),
            "a gitignored file present on disk must not change the artifact checksum"
        );
    }
}
