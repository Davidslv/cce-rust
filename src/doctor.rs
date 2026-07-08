//! # doctor — store health checks and config-drift detection (issue #62)
//!
//! **Why this file exists:** A store silently built by a *different*
//! configuration than the binary reading it degrades retrieval with no signal
//! (#30: embedding-space mismatch → meaningless cosine scores; #59
//! first-design: version-skewed byte re-derivation false-failing intact
//! artifacts). `cce doctor` reads a store, compares its recorded build
//! fingerprint (`fingerprint` module) with the running binary's pinned
//! equivalents, and reports every mismatch with what it means — *before* it
//! costs anyone retrieval quality.
//!
//! **What it is / does:** A read-only report over one store, a project root,
//! or a whole workspace (per member, `MemberType::StoreOnly` included):
//! fingerprint drift, store parse health (chunk count, the #30
//! empty-embedding tripwire), the #55 installed-bytes corruption check
//! (REUSING `verify --checksum-only`'s machinery verbatim), and the knowledge
//! store's contract version + snapshot freshness.
//!
//! **Responsibilities:**
//! - Own the doctor checks, their severities, and the rendered report.
//! - **Never mutate anything.** Exit non-zero ONLY on definite
//!   corruption/mismatch; soft findings render as distinct `advisory` lines
//!   and keep exit 0 (an old store without a fingerprint is a notice, not a
//!   failure).
//! - It does NOT write fingerprints (`fingerprint`) nor verify remote
//!   artifacts (`sync::commands`).

use crate::fingerprint::Fingerprint;
use crate::store::{default_store_path, Index};
use crate::sync::commands::{
    verify_knowledge_checksum, verify_store_checksum, ChecksumVerify, KnowledgeChecksumVerify,
    KNOWLEDGE_NO_RECORD_NOTICE, NO_RECORD_NOTICE,
};
use crate::sync::hex_lower;
use crate::workspace::Manifest;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The graceful notice for a store that predates fingerprints (#62): exit 0.
pub const NO_FINGERPRINT_NOTICE: &str =
    "no fingerprint recorded (store built before cce v2.8) — re-index to enable drift detection";

/// Accumulates the rendered report plus the finding counts that decide the
/// exit code (failures) and the summary line (advisories).
struct Report {
    out: String,
    advisories: usize,
    failures: usize,
}

impl Report {
    fn new() -> Report {
        Report { out: String::new(), advisories: 0, failures: 0 }
    }
    fn line(&mut self, s: &str) {
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn ok(&mut self, msg: &str) {
        self.line(&format!("  ok        {msg}"));
    }
    fn advisory(&mut self, msg: &str) {
        self.advisories += 1;
        self.line(&format!("  advisory  {msg}"));
    }
    fn fail(&mut self, msg: &str) {
        self.failures += 1;
        // A multi-line reused message (the #55 verify machinery) keeps its
        // shape; continuation lines are indented under the FAIL marker.
        let mut lines = msg.lines();
        if let Some(first) = lines.next() {
            self.line(&format!("  FAIL      {first}"));
        }
        for rest in lines {
            self.line(&format!("            {}", rest.trim_start()));
        }
    }
}

/// A completed doctor run: the rendered report plus the counts that decide
/// the exit code. `Err` from [`cmd_doctor`] is reserved for USAGE errors (no
/// directory, no store anywhere) — a failing check is an `Ok` outcome whose
/// `failures > 0`, so the report always renders in full.
#[derive(Debug, Clone)]
pub struct DoctorOutcome {
    pub report: String,
    pub failures: usize,
    pub advisories: usize,
}

impl DoctorOutcome {
    /// True when no definite corruption/mismatch was found (advisories allowed).
    pub fn healthy(&self) -> bool {
        self.failures == 0
    }
}

/// `cce doctor [--dir <root>|--store <path>]` (#62). Read-only.
pub fn cmd_doctor(dir: Option<PathBuf>, store: Option<PathBuf>) -> Result<DoctorOutcome, String> {
    let mut r = Report::new();
    let checked = if let Some(store_path) = store {
        // Explicit-store mode: just that file (+ its fingerprint beside it).
        r.line(&format!("cce doctor — store {}", store_path.display()));
        if !store_path.exists() {
            return Err(format!(
                "no store at {} — index first (`cce index <dir>`)",
                store_path.display()
            ));
        }
        check_store(&store_path, &mut r);
        1usize
    } else {
        let root = dir.unwrap_or_else(|| PathBuf::from("."));
        if !root.is_dir() {
            return Err(format!("not a directory: {}", root.display()));
        }
        match Manifest::load(&root) {
            Ok(manifest) => {
                // Workspace mode: every member (StoreOnly members included —
                // a pulled, source-less member is still a store to examine).
                r.line(&format!(
                    "cce doctor — workspace {} ({} member{})",
                    manifest.name,
                    manifest.members.len(),
                    if manifest.members.len() == 1 { "" } else { "s" }
                ));
                for m in &manifest.members {
                    r.line(&format!("{}:", m.name));
                    let member_dir = root.join(&m.path);
                    check_store(&default_store_path(&member_dir), &mut r);
                    check_sync_marker(&member_dir, &mut r);
                }
                check_knowledge(&root, &mut r);
                manifest.members.len()
            }
            Err(_) => {
                let store_path = default_store_path(&root);
                r.line(&format!("cce doctor — {}", root.display()));
                let has_knowledge =
                    crate::knowledge::store::KnowledgeStore::current_pointer_path(&root).exists();
                if !store_path.exists() && !has_knowledge {
                    return Err(format!(
                        "no store at {} — index first (`cce index {}`)",
                        store_path.display(),
                        root.display()
                    ));
                }
                if store_path.exists() {
                    check_store(&store_path, &mut r);
                    check_sync_marker(&root, &mut r);
                }
                check_knowledge(&root, &mut r);
                1
            }
        }
    };

    let summary = format!(
        "summary: {checked} store{} checked · {} failure{} · {} advisor{}",
        if checked == 1 { "" } else { "s" },
        r.failures,
        if r.failures == 1 { "" } else { "s" },
        r.advisories,
        if r.advisories == 1 { "y" } else { "ies" }
    );
    r.line(&summary);
    Ok(DoctorOutcome { report: r.out, failures: r.failures, advisories: r.advisories })
}

/// Parse health + fingerprint drift for one store file. Never mutates.
fn check_store(store_path: &Path, r: &mut Report) {
    let bytes = match std::fs::read(store_path) {
        Ok(b) => b,
        Err(e) => {
            r.fail(&format!("store could not be read ({}): {e}", store_path.display()));
            return;
        }
    };

    // --- Parse health + the #30 empty-embedding tripwire ---
    let index = match Index::load(store_path) {
        Ok(idx) => Some(idx),
        Err(e) => {
            r.fail(&format!(
                "store cannot be parsed ({}): {e} — corruption; re-index (or `cce sync pull \
                 --force`)",
                store_path.display()
            ));
            None
        }
    };
    if let Some(idx) = &index {
        r.ok(&format!(
            "store parses: {} chunk{} across {} file{} (embedder {})",
            idx.chunks.len(),
            if idx.chunks.len() == 1 { "" } else { "s" },
            idx.files().len(),
            if idx.files().len() == 1 { "" } else { "s" },
            idx.embedder_name
        ));
        let empty = idx.chunks.iter().filter(|c| c.embedding.is_empty()).count();
        if empty > 0 {
            r.fail(&format!(
                "{empty} chunk{} carr{} EMPTY embeddings — dead vector signal (the #30 \
                 regression); re-index",
                if empty == 1 { "" } else { "s" },
                if empty == 1 { "ies" } else { "y" }
            ));
        }
        let dims: Vec<usize> = idx
            .chunks
            .iter()
            .map(|c| c.embedding.len())
            .filter(|&n| n > 0)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if dims.len() > 1 {
            r.fail(&format!(
                "mixed embedding dimensions in one store ({:?}) — vectors are not comparable; \
                 re-index",
                dims
            ));
        } else if idx.embedder_name == crate::sync::HASH_EMBEDDER {
            if let Some(&d) = dims.first() {
                if d != crate::config::EMBED_DIM {
                    r.fail(&format!(
                        "hash-embedder store has {d}-dim vectors but this binary embeds queries \
                         at {} — cosine scores would be meaningless (#30); re-index",
                        crate::config::EMBED_DIM
                    ));
                }
            }
        }
    }

    // --- Fingerprint vs this binary's pinned equivalents ---
    match Fingerprint::load_beside_store(store_path) {
        Err(e) => r.fail(&format!("{e} — corruption; re-index to rewrite the fingerprint")),
        Ok(None) => r.advisory(NO_FINGERPRINT_NOTICE),
        Ok(Some(fp)) => check_fingerprint(&fp, &bytes, index.as_ref(), r),
    }
}

/// Compare a recorded fingerprint field-by-field with the running binary's
/// pinned constants, explaining what each mismatch means.
fn check_fingerprint(fp: &Fingerprint, store_bytes: &[u8], index: Option<&Index>, r: &mut Report) {
    if fp.sha256 != fp.computed_sha256() {
        r.fail(
            "fingerprint self-checksum mismatch — the fingerprint file was edited or corrupted; \
             its fields cannot be trusted. Re-index to rewrite it",
        );
        return;
    }
    if fp.store_sha256 != hex_lower(&Sha256::digest(store_bytes)) {
        r.fail(
            "store bytes do not match the fingerprint — the store was rebuilt or modified \
             without updating it (an older cce, or by hand); drift detection is blind here. \
             Re-index to realign",
        );
    }
    let mut drift = 0usize;
    if let Some(idx) = index {
        if fp.embedder != idx.embedder_name {
            drift += 1;
            r.fail(&format!(
                "embedder drift: the fingerprint records `{}` but the store declares `{}` — the \
                 vectors may come from a different embedding space, making cosine scores \
                 meaningless (#30); re-index",
                fp.embedder, idx.embedder_name
            ));
        }
    }
    if fp.embedder == crate::sync::HASH_EMBEDDER && fp.embed_dim != crate::config::EMBED_DIM {
        drift += 1;
        r.fail(&format!(
            "embedding dimensions changed: built at {} dims, this binary embeds at {} — vector \
             recall would compare across spaces (#30); re-index",
            fp.embed_dim,
            crate::config::EMBED_DIM
        ));
    }
    let current_packs = crate::sync::pack_set_id();
    if fp.pack_set != current_packs {
        drift += 1;
        r.fail(&format!(
            "chunker changed: the store was built with packs `{}`, this binary registers `{}` — \
             chunk_ids may not be reproducible; re-index to realign",
            fp.pack_set, current_packs
        ));
    }
    if fp.tokenizer != crate::tokenizer::TOKEN_ESTIMATOR_ID {
        drift += 1;
        r.fail(&format!(
            "tokenizer rule changed (`{}` vs `{}`) — token counts, chunk budgets, and the \
             savings ledger drift; re-index to realign",
            fp.tokenizer,
            crate::tokenizer::TOKEN_ESTIMATOR_ID
        ));
    }
    if fp.block_nesting_limit != crate::markdown::MAX_BLOCK_NESTING {
        drift += 1;
        r.fail(&format!(
            "markdown nesting limit changed ({} vs {}) — deeply nested documents chunk \
             differently; re-index to realign",
            fp.block_nesting_limit,
            crate::markdown::MAX_BLOCK_NESTING
        ));
    }
    if fp.markdown_section_tokens != crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS {
        r.advisory(&format!(
            "markdown split budget differs ({} vs the default {}) — knowledge sections may \
             split at other boundaries",
            fp.markdown_section_tokens,
            crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS
        ));
    }
    if !fp.redaction {
        r.advisory(
            "built with --allow-secrets (redaction OFF) — sensitive files and raw secrets may \
             be stored verbatim",
        );
    }
    if fp.embedder == "ollama" {
        r.advisory(
            "ollama-built store: searching needs a reachable Ollama server, and the index is \
             not shareable via `cce sync` (non-reproducible vectors)",
        );
    }
    if drift == 0 {
        let version_note = if fp.engine_version == env!("CARGO_PKG_VERSION") {
            String::new()
        } else {
            format!(" — built by cce {}, config unchanged", fp.engine_version)
        };
        r.ok(&format!(
            "fingerprint matches this binary ({}/{} · packs `{}` · {}{})",
            fp.embedder, fp.embed_dim, fp.pack_set, fp.tokenizer, version_note
        ));
    }
}

/// The #55 installed-bytes corruption check, REUSING `verify --checksum-only`'s
/// machinery: a pulled store's on-disk bytes vs the `installed_sha256` its
/// pull recorded in `.cce/synced.json`. A local (never-pulled) store has no
/// marker — that is provenance information, not a finding.
fn check_sync_marker(dir: &Path, r: &mut Report) {
    if crate::sync::commands::SyncState::load(dir).is_none() {
        r.ok("provenance: local build (no sync marker)");
        return;
    }
    match verify_store_checksum(dir, "this store") {
        Ok(ChecksumVerify::Ok(state, checksum)) => r.ok(&format!(
            "pulled store bytes match the install record ({}@{} · {})",
            state.repo_id,
            state.sha,
            &checksum[..12]
        )),
        Ok(ChecksumVerify::NoRecord(_)) => r.advisory(NO_RECORD_NOTICE),
        Err(e) => r.fail(&e),
    }
}

/// Knowledge store health (SPEC-V2.6 / SPEC-SYNC-KNOWLEDGE): contract version,
/// snapshot identity + data freshness, and the pulled-snapshot install-bytes
/// check. Silent when the root has no knowledge store at all.
fn check_knowledge(root: &Path, r: &mut Report) {
    use crate::knowledge::store::KnowledgeStore;
    let pointer = KnowledgeStore::current_pointer_path(root);
    if !pointer.exists() {
        return;
    }
    r.line("knowledge:");
    match KnowledgeStore::load_current(root) {
        Err(e) => r.fail(&format!("knowledge store cannot be loaded: {e} — re-ingest or re-pull")),
        Ok(store) => {
            if store.schema == crate::knowledge::contract::KNOWLEDGE_SCHEMA_ID {
                let as_of = crate::sync::knowledge_artifact::data_as_of(&store.chunks)
                    .unwrap_or_else(|| "unknown".to_string());
                r.ok(&format!(
                    "contract {} · snapshot {} · {} record{} · {} chunk{} · data as-of {}",
                    store.schema,
                    store.snapshot,
                    store.records,
                    if store.records == 1 { "" } else { "s" },
                    store.chunks.len(),
                    if store.chunks.len() == 1 { "" } else { "s" },
                    as_of
                ));
            } else {
                r.fail(&format!(
                    "knowledge contract mismatch: the store says `{}`, this binary speaks `{}` — \
                     re-ingest the feed (or re-pull the corpus)",
                    store.schema,
                    crate::knowledge::contract::KNOWLEDGE_SCHEMA_ID
                ));
            }
        }
    }
    match verify_knowledge_checksum(root) {
        None => {}
        Some(Ok(KnowledgeChecksumVerify::Ok(state, checksum))) => r.ok(&format!(
            "pulled snapshot bytes match the install record ({}@{} · {})",
            state.corpus_id,
            state.snapshot,
            &checksum[..12]
        )),
        Some(Ok(KnowledgeChecksumVerify::NoRecord(_))) => r.advisory(KNOWLEDGE_NO_RECORD_NOTICE),
        Some(Err(e)) => r.fail(&e),
    }
}
