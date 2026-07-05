//! # sync::artifact — the portable, byte-exact interchange format (SPEC-SYNC §2)
//!
//! **Why this file exists:** Ruby stores in SQLite, Rust in JSON, so the cache
//! cannot be either native store. SPEC-SYNC §2 defines a *canonical, deterministic*
//! interchange artifact both engines export and import. It must be **byte-identical
//! across people and across both engines** for the same `repo@sha` — that identity
//! is what makes the cache content-addressable and `--verify` meaningful. This file
//! owns that format down to the last byte.
//!
//! **What it is / does:** A UTF-8, LF-terminated, newline-delimited stream:
//!   line 1        = the manifest JSON,
//!   lines 2..N+1  = one JSON object per chunk, sorted by `(file_path, start_line,
//!                   chunk_id)` (N = `chunk_count`),
//!   line N+2      = the graph JSON (`file_imports` + `file_tokens`).
//! Every object uses **sorted keys and compact separators** (serde_json's default
//! `Map` is a `BTreeMap`, so `to_string` yields sorted, whitespace-free JSON).
//! Embeddings are encoded as **base64 of 256 little-endian IEEE-754 `f64` bytes**
//! (NOT decimals), so the bytes match across languages regardless of float→string
//! formatting. `checksum` = lowercase-hex SHA-256 over the canonical stream with the
//! manifest's `checksum` field omitted. `built_at` is the commit date of `sha` and
//! `built_by` a neutral constant, so both are deterministic and cross-language.
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

/// A neutral, engine-independent `built_by` so the artifact bytes are identical
/// whether Ruby, Rust, CI, or a teammate produced them (SPEC-SYNC §10).
pub const BUILT_BY: &str = "cce";

/// The artifact manifest (SPEC-SYNC §2, line 1). Every field is deterministic for
/// a given `repo@sha` and pack set, so the whole artifact is reproducible.
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
    /// Lowercase-hex SHA-256 checksum (filled in by `Artifact::build`).
    pub checksum: String,
    /// Commit date of `sha` (RFC 3339), or empty when unknown. Deterministic.
    pub built_at: String,
    /// Neutral builder tag (`BUILT_BY`).
    pub built_by: String,
}

/// The metadata needed to stamp a manifest, supplied by the caller (the CLI reads
/// `sha`/`built_at` from git; `repo_id` from config or the origin).
#[derive(Debug, Clone)]
pub struct ManifestMeta {
    pub repo_id: String,
    pub sha: String,
    pub built_at: String,
}

/// A fully-materialized artifact: its manifest plus the content it carries.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub manifest: Manifest,
    /// Chunks in canonical `(file_path, start_line, chunk_id)` order.
    pub chunks: Vec<Chunk>,
    pub file_imports: BTreeMap<String, Vec<String>>,
    pub file_tokens: BTreeMap<String, usize>,
}

/// Encode a 256-d embedding as base64 of its little-endian IEEE-754 `f64` bytes
/// (SPEC-SYNC §2). An empty vector encodes to the empty string.
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
    let mut out = Vec::with_capacity(bytes.len() / 8);
    for chunk in bytes.chunks_exact(8) {
        let arr: [u8; 8] = chunk.try_into().expect("chunks_exact(8) yields 8 bytes");
        out.push(f64::from_le_bytes(arr));
    }
    Ok(out)
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

/// Canonical JSON `Value` for the graph line: the import map plus the whole-file
/// token counts (both deterministic; the token counts keep the round-trip lossless
/// for the dashboard's baseline counterfactual).
fn graph_value(
    file_imports: &BTreeMap<String, Vec<String>>,
    file_tokens: &BTreeMap<String, usize>,
) -> Value {
    json!({ "file_imports": file_imports, "file_tokens": file_tokens })
}

/// The manifest JSON `Value`. When `with_checksum` is false the `checksum` key is
/// omitted — that is the exact form the checksum is computed over.
fn manifest_value(m: &Manifest, with_checksum: bool) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("built_at".into(), json!(m.built_at));
    obj.insert("built_by".into(), json!(m.built_by));
    obj.insert("cce_version".into(), json!(m.cce_version));
    if with_checksum {
        obj.insert("checksum".into(), json!(m.checksum));
    }
    obj.insert("chunk_count".into(), json!(m.chunk_count));
    obj.insert("embedder".into(), json!(m.embedder));
    obj.insert("pack_set_id".into(), json!(m.pack_set_id));
    obj.insert("repo_id".into(), json!(m.repo_id));
    obj.insert("sha".into(), json!(m.sha));
    Value::Object(obj)
}

/// Sort chunks into the canonical `(file_path, start_line, chunk_id)` order.
fn sort_chunks(chunks: &mut [Chunk]) {
    chunks.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.chunk_id.cmp(&b.chunk_id))
    });
}

/// Assemble the canonical byte stream. `with_checksum` selects whether line 1
/// carries the `checksum` field (false = the checksummed pre-image).
fn stream_bytes(
    manifest: &Manifest,
    chunks: &[Chunk],
    file_imports: &BTreeMap<String, Vec<String>>,
    file_tokens: &BTreeMap<String, usize>,
    with_checksum: bool,
) -> Vec<u8> {
    let mut out = String::new();
    out.push_str(&manifest_value(manifest, with_checksum).to_string());
    out.push('\n');
    for c in chunks {
        out.push_str(&chunk_value(c).to_string());
        out.push('\n');
    }
    out.push_str(&graph_value(file_imports, file_tokens).to_string());
    out.push('\n');
    out.into_bytes()
}

impl Artifact {
    /// Export a local `Index` to a canonical artifact (SPEC-SYNC §2). The index
    /// MUST have been built with the hash embedder — the caller enforces that; here
    /// we only stamp `embedder = "hash"` and compute everything deterministically.
    pub fn from_index(index: &Index, meta: ManifestMeta) -> Artifact {
        let mut chunks = index.chunks.clone();
        sort_chunks(&mut chunks);
        let mut manifest = Manifest {
            repo_id: meta.repo_id,
            sha: meta.sha,
            cce_version: crate::sync::cce_version_minor(),
            embedder: crate::sync::HASH_EMBEDDER.to_string(),
            pack_set_id: crate::sync::pack_set_id(),
            chunk_count: chunks.len(),
            checksum: String::new(),
            built_at: meta.built_at,
            built_by: BUILT_BY.to_string(),
        };
        // Compute the checksum over the pre-image (manifest without `checksum`).
        let pre = stream_bytes(&manifest, &chunks, &index.file_imports, &index.file_tokens, false);
        manifest.checksum = hex_lower(&Sha256::digest(&pre));
        Artifact {
            manifest,
            chunks,
            file_imports: index.file_imports.clone(),
            file_tokens: index.file_tokens.clone(),
        }
    }

    /// The canonical artifact bytes (line 1 includes the checksum).
    pub fn to_bytes(&self) -> Vec<u8> {
        stream_bytes(&self.manifest, &self.chunks, &self.file_imports, &self.file_tokens, true)
    }

    /// Recompute the checksum from the current content (the SHA-256 over the
    /// checksum-omitted pre-image). Used by `verify` and by `from_bytes` validation.
    pub fn computed_checksum(&self) -> String {
        let pre = stream_bytes(
            &self.manifest,
            &self.chunks,
            &self.file_imports,
            &self.file_tokens,
            false,
        );
        hex_lower(&Sha256::digest(&pre))
    }

    /// Parse a canonical artifact from bytes, validating the structure and the
    /// stored checksum. Rejects a truncated stream, a bad chunk count, or a
    /// checksum that does not match the recomputed value.
    pub fn from_bytes(bytes: &[u8]) -> Result<Artifact, String> {
        let text = std::str::from_utf8(bytes).map_err(|e| format!("artifact is not UTF-8: {e}"))?;
        // The stream is LF-terminated; the trailing newline yields a final empty
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
        // Expect: 1 manifest + n chunks + 1 graph.
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
        let file_imports = parse_imports(&graph)?;
        let file_tokens = parse_tokens(&graph)?;

        let artifact = Artifact { manifest, chunks, file_imports, file_tokens };
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
            self.file_tokens,
            crate::sync::HASH_EMBEDDER.to_string(),
        )
    }
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
    Ok(Manifest {
        repo_id: s("repo_id")?,
        sha: s("sha")?,
        cce_version: s("cce_version")?,
        embedder: s("embedder")?,
        pack_set_id: s("pack_set_id")?,
        chunk_count,
        checksum: s("checksum")?,
        built_at: s("built_at")?,
        built_by: s("built_by")?,
    })
}

/// Parse `file_imports` out of the graph value.
fn parse_imports(graph: &Value) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let obj = graph
        .get("file_imports")
        .and_then(|x| x.as_object())
        .ok_or_else(|| "graph missing object `file_imports`".to_string())?;
    for (k, v) in obj {
        let arr = v.as_array().ok_or_else(|| format!("file_imports[{k}] is not an array"))?;
        let mut imports = Vec::with_capacity(arr.len());
        for item in arr {
            imports.push(
                item.as_str()
                    .ok_or_else(|| format!("file_imports[{k}] has a non-string"))?
                    .to_string(),
            );
        }
        out.insert(k.clone(), imports);
    }
    Ok(out)
}

/// Parse `file_tokens` out of the graph value.
fn parse_tokens(graph: &Value) -> Result<BTreeMap<String, usize>, String> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    let obj = graph
        .get("file_tokens")
        .and_then(|x| x.as_object())
        .ok_or_else(|| "graph missing object `file_tokens`".to_string())?;
    for (k, v) in obj {
        let n = v.as_u64().ok_or_else(|| format!("file_tokens[{k}] is not an integer"))?;
        out.insert(k.clone(), n as usize);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
    }

    fn meta() -> ManifestMeta {
        ManifestMeta {
            repo_id: "example.com__acme__demo".to_string(),
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            built_at: "2026-07-05T12:00:00+00:00".to_string(),
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
        // 3 bytes -> "AAAA"... craft base64 of 3 bytes (not multiple of 8).
        let enc = B64.encode([1u8, 2, 3]);
        assert!(decode_embedding(&enc).is_err());
    }

    #[test]
    fn manifest_line_has_sorted_keys_and_no_whitespace() {
        let a = built();
        let bytes = a.to_bytes();
        let text = String::from_utf8(bytes).unwrap();
        let first = text.lines().next().unwrap();
        // Sorted keys, compact separators.
        assert!(first.starts_with(
            "{\"built_at\":\"2026-07-05T12:00:00+00:00\",\"built_by\":\"cce\",\"cce_version\":\"2.3\",\"checksum\":\""
        ));
        assert!(!first.contains(", "));
        assert!(!first.contains(": "));
        // Keys appear in alphabetical order.
        let idx_repo = first.find("\"repo_id\"").unwrap();
        let idx_sha = first.find("\"sha\"").unwrap();
        assert!(idx_repo < idx_sha);
    }

    #[test]
    fn stream_shape_is_manifest_chunks_graph() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), a.manifest.chunk_count + 2);
        assert!(lines[0].contains("\"chunk_count\":"));
        assert!(lines.last().unwrap().starts_with("{\"file_imports\":"));
    }

    #[test]
    fn checksum_is_deterministic_and_recomputes() {
        let a = built();
        let b = built();
        assert_eq!(a.manifest.checksum, b.manifest.checksum);
        assert_eq!(a.manifest.checksum, a.computed_checksum());
        assert_eq!(a.to_bytes(), b.to_bytes(), "artifact bytes are byte-identical");
    }

    #[test]
    fn round_trips_bytes_losslessly() {
        let a = built();
        let bytes = a.to_bytes();
        let parsed = Artifact::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.manifest, a.manifest);
        assert_eq!(parsed.to_bytes(), bytes);
        // Chunk embeddings survive exactly.
        assert_eq!(parsed.chunks.len(), a.chunks.len());
        for (x, y) in parsed.chunks.iter().zip(a.chunks.iter()) {
            assert_eq!(x, y);
        }
    }

    #[test]
    fn import_reconstructs_a_functionally_identical_index() {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder);
        let a = Artifact::from_index(&idx, meta());
        let restored = a.into_index();
        assert_eq!(restored.chunks.len(), idx.chunks.len());
        assert_eq!(restored.file_imports, idx.file_imports);
        assert_eq!(restored.file_tokens, idx.file_tokens);
        assert_eq!(restored.embedder_name, "hash");
    }

    #[test]
    fn from_bytes_rejects_tampered_checksum() {
        let a = built();
        let text = String::from_utf8(a.to_bytes()).unwrap();
        // Flip one hex digit of the checksum by replacing the whole value.
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
        // Drop one chunk line so the count no longer matches the manifest.
        lines.remove(1);
        let text = lines.join("\n") + "\n";
        let err = Artifact::from_bytes(text.as_bytes()).unwrap_err();
        assert!(err.contains("line count"), "got: {err}");
    }

    #[test]
    fn decode_rejects_invalid_base64() {
        // `!` is outside the base64 alphabet.
        assert!(decode_embedding("!!!!").is_err());
    }

    #[test]
    fn from_bytes_rejects_non_utf8() {
        let err = Artifact::from_bytes(&[0xff, 0xfe, 0x00]).unwrap_err();
        assert!(err.contains("not UTF-8"), "got: {err}");
    }

    #[test]
    fn parse_manifest_rejects_missing_field() {
        // Missing `chunk_count`.
        let line = "{\"built_at\":\"\",\"built_by\":\"cce\",\"cce_version\":\"2.3\",\"checksum\":\"x\",\"embedder\":\"hash\",\"pack_set_id\":\"p\",\"repo_id\":\"r\",\"sha\":\"s\"}";
        let err = parse_manifest(line).unwrap_err();
        assert!(err.contains("chunk_count"), "got: {err}");
    }

    #[test]
    fn parse_graph_rejects_missing_sections() {
        let g: Value = serde_json::from_str("{\"file_imports\":{}}").unwrap();
        assert!(parse_imports(&g).is_ok());
        // Missing file_tokens.
        assert!(parse_tokens(&g).is_err());
        // A non-object file_imports.
        let bad: Value = serde_json::from_str("{\"file_imports\":[]}").unwrap();
        assert!(parse_imports(&bad).is_err());
    }

    #[test]
    fn chunk_from_value_rejects_missing_field() {
        // No `content` key.
        let v: Value = serde_json::from_str(
            "{\"chunk_type\":\"module\",\"embedding\":\"\",\"end_line\":1,\"file_path\":\"a\",\"id\":\"x\",\"kind\":\"module\",\"language\":\"plaintext\",\"start_line\":1,\"token_count\":1}",
        )
        .unwrap();
        assert!(chunk_from_value(&v).is_err());
    }

    #[test]
    fn built_at_is_carried_verbatim_into_the_manifest() {
        let a = built();
        assert_eq!(a.manifest.built_at, "2026-07-05T12:00:00+00:00");
        assert_eq!(a.manifest.built_by, "cce");
        assert_eq!(a.manifest.embedder, "hash");
    }

    /// A committed **golden checksum** (SPEC-SYNC §11): the artifact for the `base`
    /// fixture built with a fixed identity. This is the cross-language anchor — the
    /// Ruby engine, exporting the same fixture with the same `(repo_id, sha,
    /// built_at)`, MUST produce this exact checksum. If this value changes, the wire
    /// format changed and every cache in the wild is invalidated: treat a diff here
    /// as a breaking-format decision, not a test to "fix".
    #[test]
    fn golden_checksum_for_base_fixture() {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder);
        let meta = ManifestMeta {
            repo_id: "example.com__acme__demo".to_string(),
            sha: "0000000000000000000000000000000000000000".to_string(),
            built_at: "2026-01-01T00:00:00+00:00".to_string(),
        };
        let a = Artifact::from_index(&idx, meta);
        assert_eq!(a.manifest.chunk_count, 7);
        assert_eq!(
            a.manifest.checksum,
            "48d8066cec52668fef75811bcd9cbd6c3e6ed5bcabe8bbbfef5f667463db61ee"
        );
    }
}
