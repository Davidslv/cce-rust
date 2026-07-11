//! # fingerprint — the store's build-configuration fingerprint (issue #62)
//!
//! **Why this file exists:** A store is only as good as the configuration that
//! built it. We were bitten twice by silent config drift: #30 (an index whose
//! vectors came from a different embedding space than the query's — cosine
//! scores were meaningless with no signal) and the first #59 design (verifying
//! artifacts by re-deriving bytes with the *current* code, false-failing
//! artifacts built by older versions). Nothing recorded the full build
//! configuration in the store, so nothing could *detect* the drift before it
//! degraded retrieval. This module records it.
//!
//! **What it is / does:** A small, deterministic metadata block written to
//! `fingerprint.json` **beside** the store file (the same "lives beside the
//! index" discipline as `metrics.jsonl` and `.cce/synced.json`). It captures
//! the engine version, the embedder id + dimensions, the chunker identity
//! (language-pack set, markdown split budget, nesting limit), the tokenizer
//! rule id, and whether redaction was on — plus a SHA-256 over the canonical
//! serialization of those fields (self-integrity) and a SHA-256 of the exact
//! store bytes it describes (binding: a store rebuilt by a binary that does
//! not know about fingerprints is detected as stale, never trusted).
//!
//! **Additive by construction:** the fingerprint is a *separate file* that old
//! readers never open, the store's own bytes (`index.json`) are untouched, and
//! every byte-pinned golden (conformance, sync artifact, knowledge artifact)
//! is computed from surfaces this file does not exist on. All recorded values
//! derive from pinned constants, so the fingerprint itself is deterministic
//! for a given configuration.
//!
//! **Responsibilities:**
//! - Own the `Fingerprint` type, its canonical serialization + self-checksum,
//!   and the read/write helpers keyed off the store path.
//! - It does NOT judge drift — `doctor` compares a recorded fingerprint with
//!   the running binary's equivalents and reports.

use crate::store::Index;
use crate::sync::hex_lower;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};

/// The pinned fingerprint schema id.
pub const FINGERPRINT_SCHEMA: &str = "cce.fingerprint/v1";

/// The fingerprint filename, written beside the store file (for the default
/// store `<root>/.cce/index.json` that is `<root>/.cce/fingerprint.json`).
pub const FINGERPRINT_FILE: &str = "fingerprint.json";

/// The build-configuration fingerprint recorded beside every store (#62).
///
/// Field order is alphabetical and IS the canonical serialization order:
/// `sha256` is SHA-256 (lowercase hex) over `serde_json::to_string` of this
/// struct with `sha256` set to `""` — the same probe-with-empty-checksum rule
/// the sync artifact uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// The markdown chunker's fail-loud nesting guard (issue #49) in effect at
    /// build time — part of the chunker identity.
    pub block_nesting_limit: usize,
    /// Embedding dimensions: the pinned `EMBED_DIM` for the hash embedder,
    /// else the observed vector length of the first embedded chunk.
    pub embed_dim: usize,
    /// The embedder id the store was built with (`hash` / `ollama`).
    pub embedder: String,
    /// The cce version that built the store (informational — drift in the
    /// pinned fields below is what matters, not the release number).
    pub engine_version: String,
    /// The markdown heading-chunker's default split budget (SPEC-V2.6 §8) —
    /// the markdown chunker identity.
    pub markdown_section_tokens: usize,
    /// The chunker identity: the sorted, comma-joined language-pack set (the
    /// same `pack_set_id` the sync artifact manifest carries).
    pub pack_set: String,
    /// Whether SPEC-V2.1 secret protection (Layer 1 + 2 redaction) was on.
    pub redaction: bool,
    /// The pinned schema id (`cce.fingerprint/v1`).
    pub schema: String,
    /// SHA-256 (lowercase hex) over the canonical serialization of every other
    /// field (with this one set to `""`) — the self-integrity check.
    pub sha256: String,
    /// The store's `spec_version` tag.
    pub spec_version: String,
    /// SHA-256 (lowercase hex) of the exact store bytes this fingerprint
    /// describes — binds the fingerprint to the store file so a store
    /// rebuilt/overwritten without a fingerprint update is detected as stale.
    pub store_sha256: String,
    /// The pinned token-estimator rule id (`cce.tokens/v1`).
    pub tokenizer: String,
}

impl Fingerprint {
    /// Build the fingerprint for `index` as persisted in `store_bytes`, with
    /// redaction on/off as the build ran. Every configuration field comes from
    /// a pinned constant (or the index's own recorded embedder), so the result
    /// is deterministic; the `sha256` self-checksum is sealed here.
    pub fn for_index(index: &Index, store_bytes: &[u8], redaction: bool) -> Fingerprint {
        let embed_dim = if index.embedder_name == crate::sync::HASH_EMBEDDER {
            crate::config::EMBED_DIM
        } else {
            index.chunks.iter().map(|c| c.embedding.len()).find(|&n| n > 0).unwrap_or(0)
        };
        let mut fp = Fingerprint {
            block_nesting_limit: crate::markdown::MAX_BLOCK_NESTING,
            embed_dim,
            embedder: index.embedder_name.clone(),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            markdown_section_tokens: crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS,
            pack_set: crate::sync::pack_set_id(),
            redaction,
            schema: FINGERPRINT_SCHEMA.to_string(),
            sha256: String::new(),
            spec_version: crate::config::SPEC_VERSION.to_string(),
            store_sha256: hex_lower(&Sha256::digest(store_bytes)),
            tokenizer: crate::tokenizer::TOKEN_ESTIMATOR_ID.to_string(),
        };
        fp.sha256 = fp.computed_sha256();
        fp
    }

    /// Recompute the self-checksum: SHA-256 over the canonical serialization
    /// with `sha256` set to `""` (the artifact-checksum probe rule).
    pub fn computed_sha256(&self) -> String {
        let mut probe = self.clone();
        probe.sha256 = String::new();
        let canonical = serde_json::to_string(&probe).unwrap_or_default();
        hex_lower(&Sha256::digest(canonical.as_bytes()))
    }

    /// Re-seal the self-checksum after a field change (test fixtures use this
    /// to simulate a store recorded under a *different* configuration without
    /// tripping the self-integrity check).
    pub fn seal(&mut self) {
        self.sha256 = self.computed_sha256();
    }

    /// Persist beside `store_path` (single-line JSON, like `.cce/synced.json`).
    pub fn save_beside_store(&self, store_path: &Path) -> io::Result<PathBuf> {
        let path = beside_store(store_path);
        // #101: same atomic temp-file + rename as the store it sits beside, so a
        // torn write can't leave `cce doctor` reading a truncated fingerprint.
        let json = serde_json::to_string(self).map_err(io::Error::other)?;
        crate::atomic::atomic_write(&path, json.as_bytes())?;
        Ok(path)
    }

    /// Load the fingerprint recorded beside `store_path`.
    ///
    /// `Ok(None)` = no fingerprint file (a store built before fingerprints
    /// existed — a notice, never an error); `Err` = the file exists but does
    /// not parse (corruption).
    pub fn load_beside_store(store_path: &Path) -> Result<Option<Fingerprint>, String> {
        let path = beside_store(store_path);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        serde_json::from_str::<Fingerprint>(&text)
            .map(Some)
            .map_err(|e| format!("could not parse {}: {e}", path.display()))
    }
}

/// The fingerprint path for a store file: `FINGERPRINT_FILE` in the store's
/// directory (mirrors how the metrics log resolves beside the store).
pub fn beside_store(store_path: &Path) -> PathBuf {
    match store_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(FINGERPRINT_FILE),
        _ => PathBuf::from(FINGERPRINT_FILE),
    }
}

/// Write the fingerprint for a just-saved store: hash the exact bytes on disk
/// at `store_path` (the same read-back-what-was-written discipline as #55's
/// `installed_sha256`) and persist beside it. Call sites treat a failure as
/// best-effort (a warning): a missing fingerprint only disables drift
/// detection; it never invalidates the store itself.
pub fn write_for_store(store_path: &Path, index: &Index, redaction: bool) -> io::Result<PathBuf> {
    let bytes = std::fs::read(store_path)?;
    Fingerprint::for_index(index, &bytes, redaction).save_beside_store(store_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
    }

    fn built_store(dir: &Path) -> (Index, PathBuf) {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let store = dir.join("index.json");
        idx.save(&store).unwrap();
        (idx, store)
    }

    #[test]
    fn fingerprint_records_the_pinned_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let (idx, store) = built_store(tmp.path());
        let bytes = std::fs::read(&store).unwrap();
        let fp = Fingerprint::for_index(&idx, &bytes, true);
        assert_eq!(fp.schema, "cce.fingerprint/v1");
        assert_eq!(fp.embedder, "hash");
        assert_eq!(fp.embed_dim, 256);
        assert_eq!(fp.pack_set, "c,javascript,python,ruby,rust,typescript");
        assert_eq!(fp.tokenizer, "cce.tokens/v1");
        assert_eq!(fp.block_nesting_limit, 192);
        assert_eq!(fp.markdown_section_tokens, 400);
        assert_eq!(fp.spec_version, "1.0");
        assert!(fp.redaction);
        assert_eq!(fp.engine_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(fp.store_sha256, hex_lower(&Sha256::digest(&bytes)));
    }

    #[test]
    fn fingerprint_is_deterministic_and_self_checksummed() {
        let tmp = tempfile::tempdir().unwrap();
        let (idx, store) = built_store(tmp.path());
        let bytes = std::fs::read(&store).unwrap();
        let a = Fingerprint::for_index(&idx, &bytes, true);
        let b = Fingerprint::for_index(&idx, &bytes, true);
        assert_eq!(a, b, "same config + bytes must fingerprint identically");
        assert_eq!(a.sha256, a.computed_sha256());
        assert_eq!(a.sha256.len(), 64);
        // A field change breaks the seal until re-sealed.
        let mut edited = a.clone();
        edited.pack_set = "python".to_string();
        assert_ne!(edited.sha256, edited.computed_sha256());
        edited.seal();
        assert_eq!(edited.sha256, edited.computed_sha256());
    }

    #[test]
    fn write_for_store_round_trips_beside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (idx, store) = built_store(tmp.path());
        let path = write_for_store(&store, &idx, true).unwrap();
        assert_eq!(path, tmp.path().join(FINGERPRINT_FILE));
        let loaded = Fingerprint::load_beside_store(&store).unwrap().expect("written");
        assert_eq!(loaded.sha256, loaded.computed_sha256());
        assert_eq!(loaded.embedder, "hash");
    }

    #[test]
    fn load_is_none_for_pre_fingerprint_stores_and_err_for_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("index.json");
        // No fingerprint file at all: Ok(None), never an error (old stores).
        assert_eq!(Fingerprint::load_beside_store(&store).unwrap(), None);
        std::fs::write(tmp.path().join(FINGERPRINT_FILE), "not json").unwrap();
        assert!(Fingerprint::load_beside_store(&store).is_err());
    }

    #[test]
    fn non_hash_embedder_records_observed_dimensions() {
        // An "ollama" store's dimensions are model-dependent: record what the
        // chunks actually carry, not the hash constant.
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let ollama_like = Index::from_parts(
            idx.chunks.clone(),
            idx.file_imports.clone(),
            idx.file_tokens.clone(),
            "ollama".to_string(),
        );
        let fp = Fingerprint::for_index(&ollama_like, b"{}", true);
        assert_eq!(fp.embedder, "ollama");
        assert_eq!(fp.embed_dim, 256, "observed from the chunk vectors");
    }
}
