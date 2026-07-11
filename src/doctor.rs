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
            Err(e) => {
                // #126: distinguish an ABSENT manifest (simply not a workspace —
                // fall through to single-dir mode, as before) from one that is
                // PRESENT but unreadable/corrupt. The old `Err(_)` arm conflated
                // the two, silently degrading a broken workspace to single-dir
                // mode: the manifest corruption was never surfaced and — when the
                // root also held its own store — doctor reported the broken
                // workspace HEALTHY (exit 0), skipping every member store. A
                // corrupt/unreadable workspace.yml is DEFINITE corruption; report
                // it as a failure (exit 1) instead of degrading past it.
                if crate::workspace::manifest_path(&root).exists() {
                    r.line(&format!("cce doctor — {}", root.display()));
                    r.fail(&format!(
                        "workspace.yml is present but unreadable/corrupt: {e}\n\
                         fix or remove {} — doctor will not fall back to single-directory \
                         mode past a corrupt manifest (that hides the corruption and skips \
                         every member store).",
                        crate::workspace::manifest_path(&root).display()
                    ));
                    0
                } else {
                    let store_path = default_store_path(&root);
                    r.line(&format!("cce doctor — {}", root.display()));
                    let has_knowledge =
                        crate::knowledge::store::KnowledgeStore::current_pointer_path(&root)
                            .exists();
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
            check_knowledge_scrub(&store, r);
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

/// #144: scan every PERSISTED free-text facet (and the record id) of the loaded
/// knowledge store through the Layer-2 redactor. `redact` is the identity on clean
/// text, so `redact(value) != value` means the value is STILL secret-shaped: the
/// store predates the #111/#112 facet-redaction fix (a local re-index will scrub
/// it), or it was tampered with. If it was `push`ed, the `.cck` on the shared
/// remote is NOT scrubbed by a local re-index — the fix must also re-push.
///
/// Advisory, not a failure (exit stays 0): a pre-#111 store is legitimately old,
/// not corrupt, and doctor reserves the non-zero exit for definite
/// corruption/mismatch. The finding names the affected facet(s) — never their raw
/// value — so the advisory itself never prints a secret.
///
/// The `record_id` is reported SEPARATELY: it is an addressing key that CANNOT be
/// redacted at rest (chunk ids and the document path derive from it), so a secret
/// there needs the SOURCE ADAPTER to fix the id — a re-index will not scrub it. It
/// is shown in its REDACTED display form, so this advisory does not leak it either.
fn check_knowledge_scrub(store: &crate::knowledge::store::KnowledgeStore, r: &mut Report) {
    use std::collections::BTreeSet;
    let dirty = |v: &str| crate::redactor::redact(v) != v;
    let mut dirty_facets: BTreeSet<&'static str> = BTreeSet::new();
    let mut dirty_ids: BTreeSet<String> = BTreeSet::new();

    for c in &store.chunks {
        // The record id is scanned but reported separately (it cannot be scrubbed
        // by re-index). Key the set by its REDACTED form so the advisory below can
        // never print the raw secret.
        if dirty(&c.record_id) {
            dirty_ids.insert(crate::redactor::redact(&c.record_id));
        }
        // Every OTHER persisted free-text facet SHOULD already be redacted (#111).
        let mut facets: Vec<(&'static str, &str)> = vec![
            ("content", c.content.as_str()),
            ("kind", c.kind.as_str()),
            ("name", c.name.as_str()),
            ("source", c.source.as_str()),
            ("title", c.title.as_str()),
        ];
        for (name, opt) in [
            ("url", &c.url),
            ("state", &c.state),
            ("state_reason", &c.state_reason),
            ("updated_at", &c.updated_at),
            ("group", &c.group),
        ] {
            if let Some(v) = opt.as_deref() {
                facets.push((name, v));
            }
        }
        for (name, value) in facets {
            if dirty(value) {
                dirty_facets.insert(name);
            }
        }
        if c.labels.iter().any(|l| dirty(l)) {
            dirty_facets.insert("labels");
        }
        if c.links.iter().any(|l| dirty(l)) {
            dirty_facets.insert("links");
        }
    }

    if !dirty_facets.is_empty() {
        let names: Vec<&str> = dirty_facets.iter().copied().collect();
        r.advisory(&format!(
            "knowledge store carries un-redacted content in facet(s): {} — it predates the \
             redaction fix (or was tampered with); re-index the feed, and if it was pushed, \
             re-push (a local re-index does NOT scrub the remote .cck)",
            names.join(", ")
        ));
    }
    for id in &dirty_ids {
        r.advisory(&format!(
            "knowledge record id `{id}` contains secret-shaped content; record ids are addressing \
             keys and cannot be redacted by re-index — fix the source adapter to remove the secret \
             from the id"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embedder::HashEmbedder;
    use crate::sync::hex_lower;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    fn fixture() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/base"))
    }

    /// Index the fixture into `<root>/.cce/index.json` and stamp its
    /// fingerprint — exactly what `cce index <root>` produces.
    fn indexed_root(root: &Path) -> Index {
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let store = default_store_path(root);
        idx.save(&store).unwrap();
        crate::fingerprint::write_for_store(&store, &idx, true).unwrap();
        idx
    }

    /// Load, edit, re-seal, and rewrite the fingerprint beside `root`'s store
    /// — simulates a store recorded under a DIFFERENT build configuration
    /// without tripping the self-integrity check.
    fn doctor_fingerprint(root: &Path, edit: impl FnOnce(&mut Fingerprint)) {
        let store = default_store_path(root);
        let mut fp = Fingerprint::load_beside_store(&store).unwrap().unwrap();
        edit(&mut fp);
        fp.seal();
        fp.save_beside_store(&store).unwrap();
    }

    fn run(root: &Path) -> DoctorOutcome {
        cmd_doctor(Some(root.to_path_buf()), None).unwrap()
    }

    #[test]
    fn healthy_store_reports_ok_and_no_failures() {
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert_eq!(out.advisories, 0, "report:\n{}", out.report);
        assert!(out.report.contains("store parses: 7 chunks"), "{}", out.report);
        assert!(out.report.contains("fingerprint matches this binary"), "{}", out.report);
        assert!(out.report.contains("provenance: local build"), "{}", out.report);
        assert!(out.report.contains("summary: 1 store checked · 0 failures"), "{}", out.report);
    }

    #[test]
    fn pre_fingerprint_store_is_a_notice_not_a_failure() {
        // Issue #62 acceptance: an old store without the fingerprint gets a
        // graceful notice and exit 0.
        let tmp = tempfile::tempdir().unwrap();
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        idx.save(&default_store_path(tmp.path())).unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "an unfingerprinted store must not fail:\n{}", out.report);
        assert_eq!(out.advisories, 1);
        assert!(out.report.contains(NO_FINGERPRINT_NOTICE), "{}", out.report);
    }

    #[test]
    fn chunker_pack_set_mismatch_is_a_definite_failure() {
        // Issue #62 acceptance: chunker-version mismatch (recorded field edited
        // in a fixture) — chunk_ids may not be reproducible.
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        doctor_fingerprint(tmp.path(), |fp| fp.pack_set = "python,ruby".to_string());
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("chunker changed"), "{}", out.report);
        assert!(out.report.contains("chunk_ids may not be reproducible"), "{}", out.report);
        assert!(out.report.contains("re-index to realign"), "{}", out.report);
    }

    #[test]
    fn embedder_drift_hash_vs_ollama_is_a_definite_failure() {
        // Issue #62 acceptance: store built with a different embedder. The
        // fingerprint says `ollama`, the store declares `hash` — the vectors'
        // provenance is unknown (#30).
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        doctor_fingerprint(tmp.path(), |fp| fp.embedder = "ollama".to_string());
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("embedder drift"), "{}", out.report);
        assert!(out.report.contains("#30"), "{}", out.report);
    }

    #[test]
    fn consistent_ollama_store_is_advisory_only() {
        // A store honestly built with ollama (fingerprint and store agree) is
        // legitimate: doctor notes the operational caveats, exit 0. Hermetic —
        // doctor never embeds, so no server is contacted.
        let tmp = tempfile::tempdir().unwrap();
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let ollama_like = Index::from_parts(
            idx.chunks.clone(),
            idx.file_imports.clone(),
            idx.file_tokens.clone(),
            "ollama".to_string(),
        );
        let store = default_store_path(tmp.path());
        ollama_like.save(&store).unwrap();
        crate::fingerprint::write_for_store(&store, &ollama_like, true).unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("ollama-built store"), "{}", out.report);
        assert!(out.advisories >= 1);
    }

    #[test]
    fn embed_dim_drift_is_a_definite_failure() {
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        doctor_fingerprint(tmp.path(), |fp| fp.embed_dim = 128);
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("embedding dimensions changed"), "{}", out.report);
    }

    #[test]
    fn tokenizer_and_nesting_drift_are_definite_failures() {
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        doctor_fingerprint(tmp.path(), |fp| {
            fp.tokenizer = "cce.tokens/v0".to_string();
            fp.block_nesting_limit = 64;
        });
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("tokenizer rule changed"), "{}", out.report);
        assert!(out.report.contains("markdown nesting limit changed"), "{}", out.report);
    }

    #[test]
    fn allow_secrets_build_is_advisory_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let store = default_store_path(tmp.path());
        idx.save(&store).unwrap();
        crate::fingerprint::write_for_store(&store, &idx, false).unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("--allow-secrets"), "{}", out.report);
    }

    #[test]
    fn corrupted_store_is_a_definite_failure() {
        // Issue #62 acceptance: corrupted store.
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        std::fs::write(default_store_path(tmp.path()), "{ not json").unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("store cannot be parsed"), "{}", out.report);
        // The binding hash also flags the fingerprint as no longer matching.
        assert!(out.report.contains("do not match the fingerprint"), "{}", out.report);
    }

    #[test]
    fn empty_embedding_store_is_a_definite_failure() {
        // Issue #62 acceptance: empty-embedding store — the #30 tripwire.
        let tmp = tempfile::tempdir().unwrap();
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let mut chunks = idx.chunks.clone();
        for c in chunks.iter_mut().take(2) {
            c.embedding = Vec::new();
        }
        let broken = Index::from_parts(
            chunks,
            idx.file_imports.clone(),
            idx.file_tokens.clone(),
            "hash".to_string(),
        );
        let store = default_store_path(tmp.path());
        broken.save(&store).unwrap();
        crate::fingerprint::write_for_store(&store, &broken, true).unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("2 chunks carry EMPTY embeddings"), "{}", out.report);
        assert!(out.report.contains("#30"), "{}", out.report);
    }

    #[test]
    fn edited_fingerprint_without_reseal_fails_self_checksum() {
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        let store = default_store_path(tmp.path());
        let mut fp = Fingerprint::load_beside_store(&store).unwrap().unwrap();
        fp.pack_set = "python".to_string(); // no seal(): sha256 is now stale
        fp.save_beside_store(&store).unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("fingerprint self-checksum mismatch"), "{}", out.report);
    }

    #[test]
    fn stale_fingerprint_after_foreign_rebuild_is_a_definite_failure() {
        // A store rebuilt WITHOUT updating the fingerprint (older binary, or by
        // hand): store bytes no longer match `store_sha256`.
        let tmp = tempfile::tempdir().unwrap();
        let idx = indexed_root(tmp.path());
        // Re-save the store (bytes change trivially via a different chunk order?
        // no — identical build is byte-identical). Append whitespace instead:
        // still-valid JSON, different bytes.
        let store = default_store_path(tmp.path());
        let mut bytes = std::fs::read(&store).unwrap();
        bytes.push(b'\n');
        std::fs::write(&store, &bytes).unwrap();
        assert!(Index::load(&store).is_ok(), "still parses");
        let _ = idx;
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("store bytes do not match the fingerprint"), "{}", out.report);
    }

    #[test]
    fn pulled_store_corruption_is_caught_by_the_installed_bytes_check() {
        // REUSES the #55 verify --checksum-only machinery: a synced.json marker
        // records the installed hash; modified store bytes are a hard failure.
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        let store = default_store_path(tmp.path());
        let installed = hex_lower(&Sha256::digest(std::fs::read(&store).unwrap()));
        std::fs::write(
            tmp.path().join(".cce").join("synced.json"),
            format!(
                "{{\"repo_id\":\"example.com__acme__demo\",\"sha\":\"{}\",\"checksum\":\"{}\",\
                 \"installed_sha256\":\"{installed}\"}}",
                "0".repeat(40),
                "f".repeat(64)
            ),
        )
        .unwrap();
        // Intact: doctor passes and reports the match.
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("match the install record"), "{}", out.report);
        // Corrupt the store (keep JSON valid so ONLY the byte check trips is
        // not possible here — the fingerprint binding also trips; both are
        // definite failures naming the same corruption).
        let mut bytes = std::fs::read(&store).unwrap();
        bytes.push(b' ');
        std::fs::write(&store, &bytes).unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("verify FAILED (checksum-only)"), "{}", out.report);
    }

    #[test]
    fn old_marker_without_install_hash_is_advisory() {
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        std::fs::write(
            tmp.path().join(".cce").join("synced.json"),
            format!(
                "{{\"repo_id\":\"example.com__acme__demo\",\"sha\":\"{}\",\"checksum\":\"{}\"}}",
                "0".repeat(40),
                "f".repeat(64)
            ),
        )
        .unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "an old marker is a notice, not a failure:\n{}", out.report);
        assert!(out.report.contains("no install checksum recorded"), "{}", out.report);
    }

    #[test]
    fn workspace_mode_checks_every_member_and_summarizes() {
        use crate::workspace::{Manifest, Member, MemberType};
        let tmp = tempfile::tempdir().unwrap();
        for name in ["api", "web"] {
            let member = tmp.path().join(name);
            std::fs::create_dir_all(&member).unwrap();
            indexed_root(&member);
        }
        // `web` is a StoreOnly member (a pulled, source-less consumer member).
        let manifest = Manifest {
            version: 1,
            name: "demo".to_string(),
            members: vec![
                Member {
                    name: "api".to_string(),
                    path: "api".to_string(),
                    member_type: MemberType::RubyGem,
                    package: "api".to_string(),
                },
                Member {
                    name: "web".to_string(),
                    path: "web".to_string(),
                    member_type: MemberType::StoreOnly,
                    package: "web".to_string(),
                },
            ],
        };
        manifest.save(tmp.path()).unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("workspace demo (2 members)"), "{}", out.report);
        assert!(out.report.contains("api:"), "{}", out.report);
        assert!(out.report.contains("web:"), "{}", out.report);
        assert!(out.report.contains("summary: 2 stores checked · 0 failures"), "{}", out.report);
        // One drifted member fails the whole run, naming the member section.
        doctor_fingerprint(&tmp.path().join("web"), |fp| fp.pack_set = "ruby".to_string());
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("chunker changed"), "{}", out.report);
    }

    #[test]
    fn corrupt_workspace_manifest_is_a_definite_failure_not_silent_single_dir() {
        // #126: doctor's `Err(_)` arm on Manifest::load conflated an ABSENT
        // manifest (not a workspace) with a PRESENT-but-corrupt one, silently
        // degrading to single-dir mode — so a broken workspace whose root also
        // holds a store was reported HEALTHY (exit 0) and the manifest
        // corruption never surfaced. A corrupt workspace.yml is definite
        // corruption: it must fail the run.
        use crate::workspace::{manifest_path, Manifest, Member, MemberType};
        let tmp = tempfile::tempdir().unwrap();
        // A root-level store: the single-dir fallback WOULD find it healthy.
        indexed_root(tmp.path());

        // A valid workspace.yml stays healthy (member `.` re-uses the root store).
        let manifest = Manifest {
            version: 1,
            name: "demo".to_string(),
            members: vec![Member {
                name: "api".to_string(),
                path: ".".to_string(),
                member_type: MemberType::StoreOnly,
                package: "api".to_string(),
            }],
        };
        manifest.save(tmp.path()).unwrap();
        assert!(run(tmp.path()).healthy(), "a valid workspace must stay healthy");

        // Corrupt it: unparseable YAML. doctor must NOT report healthy.
        std::fs::write(manifest_path(tmp.path()), "version: 1\nname: demo\nmembers: [oops\n")
            .unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy(), "a corrupt workspace.yml must fail the run:\n{}", out.report);
        assert!(
            out.report.contains("workspace.yml"),
            "the finding must name the manifest:\n{}",
            out.report
        );

        // With NO workspace.yml the same tree is unaffected (single-dir, healthy).
        std::fs::remove_file(manifest_path(tmp.path())).unwrap();
        assert!(run(tmp.path()).healthy(), "an absent manifest → single-dir, unaffected");
    }

    #[test]
    fn knowledge_store_reports_contract_and_flags_a_mismatch() {
        use crate::knowledge::store::KnowledgeStore;
        let tmp = tempfile::tempdir().unwrap();
        indexed_root(tmp.path());
        let store = KnowledgeStore {
            schema: crate::knowledge::contract::KNOWLEDGE_SCHEMA_ID.to_string(),
            snapshot: "abcdef0123456789".to_string(),
            records: 0,
            chunks: Vec::new(),
        };
        store.save(tmp.path()).unwrap();
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("contract cce.knowledge/v1"), "{}", out.report);
        assert!(out.report.contains("data as-of unknown"), "{}", out.report);
        // A store speaking a different contract is a definite mismatch.
        let alien = KnowledgeStore { schema: "cce.knowledge/v9".to_string(), ..store };
        alien.save(tmp.path()).unwrap();
        let out = run(tmp.path());
        assert!(!out.healthy());
        assert!(out.report.contains("knowledge contract mismatch"), "{}", out.report);
    }

    // Secret-shaped inputs are assembled from split fragments via `concat!` so no
    // committed source file carries a contiguous secret literal (GitHub push
    // protection); the redactor still sees the full value at runtime.
    const SCRUB_AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");

    /// Ingest a one-record clean feed into `root`'s knowledge store, then apply
    /// `mutate` to the in-memory store BEFORE it is persisted — so a facet can be
    /// dirtied AFTER the ingest-time redaction, simulating a pre-#111 (or tampered)
    /// store that carries raw secrets on disk.
    fn save_dirty_knowledge_store(
        root: &Path,
        mutate: impl FnOnce(&mut crate::knowledge::store::KnowledgeStore),
    ) {
        let feed = "{\"id\":\"kn:1\",\"title\":\"Login policy\",\"body\":\"## Rule\\n\\nStore only a salted hash.\",\"source\":\"handbook\"}\n";
        let recs = crate::knowledge::contract::parse_ndjson(feed).unwrap();
        let mut store = crate::knowledge::store::ingest(&recs, feed.as_bytes(), 400);
        mutate(&mut store);
        store.save(root).unwrap();
    }

    #[test]
    fn doctor_flags_a_knowledge_store_with_an_unredacted_facet() {
        // #144 Part B (RED on pre-fix code, which never scanned facets): a store
        // whose persisted `title` facet still carries a raw secret (a pre-#111 or
        // tampered store) is flagged — advisory (exit 0), naming the facet, never
        // printing the raw value.
        let tmp = tempfile::tempdir().unwrap();
        save_dirty_knowledge_store(tmp.path(), |store| {
            for c in &mut store.chunks {
                c.title = format!("Rotate leaked key {SCRUB_AWS_KEY} now");
            }
        });
        let out = run(tmp.path());
        assert!(out.healthy(), "scrub scan must stay advisory (exit 0):\n{}", out.report);
        assert!(
            out.report.contains("un-redacted content in facet(s): title"),
            "advisory must name the dirty facet:\n{}",
            out.report
        );
        assert!(out.report.contains("re-push"), "advisory must mention re-push:\n{}", out.report);
        assert!(
            !out.report.contains(SCRUB_AWS_KEY),
            "the advisory must NOT print the raw secret:\n{}",
            out.report
        );
    }

    #[test]
    fn doctor_flags_a_secret_bearing_record_id_separately() {
        // #144 Part B: a record id carrying a secret is flagged on its OWN advisory
        // — ids are addressing keys, unscrubbable by re-index — and is shown only in
        // its redacted display form, so the advisory never leaks the raw secret.
        let tmp = tempfile::tempdir().unwrap();
        save_dirty_knowledge_store(tmp.path(), |store| {
            for c in &mut store.chunks {
                c.record_id = format!("gh:{SCRUB_AWS_KEY}");
            }
        });
        let out = run(tmp.path());
        assert!(out.healthy(), "id scan must stay advisory (exit 0):\n{}", out.report);
        assert!(
            out.report.contains("record id `gh:[REDACTED:AWS_ACCESS_KEY]` contains secret-shaped"),
            "advisory must name the redacted id:\n{}",
            out.report
        );
        assert!(
            out.report.contains("fix the source adapter"),
            "advisory must point at the source adapter:\n{}",
            out.report
        );
        assert!(
            !out.report.contains(SCRUB_AWS_KEY),
            "the advisory must NOT print the raw secret id:\n{}",
            out.report
        );
    }

    #[test]
    fn doctor_is_silent_on_the_scrub_scan_for_a_clean_store() {
        // #144 Part B control: a secret-free store trips NO scrub finding (redaction
        // is the identity on clean text).
        let tmp = tempfile::tempdir().unwrap();
        save_dirty_knowledge_store(tmp.path(), |_| {});
        let out = run(tmp.path());
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(
            !out.report.contains("un-redacted content"),
            "clean store must not trip the facet scan:\n{}",
            out.report
        );
        assert!(
            !out.report.contains("secret-shaped"),
            "clean store must not trip the id scan:\n{}",
            out.report
        );
        assert_eq!(
            out.advisories, 0,
            "clean locally-indexed store has no advisories:\n{}",
            out.report
        );
    }

    #[test]
    fn missing_store_is_a_usage_error_and_store_flag_targets_one_file() {
        let tmp = tempfile::tempdir().unwrap();
        // No store anywhere: a clear usage error, not a report.
        let err = cmd_doctor(Some(tmp.path().to_path_buf()), None).unwrap_err();
        assert!(err.contains("no store at"), "{err}");
        // --store mode: check exactly one file.
        let (idx, _) = Index::build_from_dir(&fixture(), &HashEmbedder).unwrap();
        let store = tmp.path().join("custom.json");
        idx.save(&store).unwrap();
        crate::fingerprint::write_for_store(&store, &idx, true).unwrap();
        let out = cmd_doctor(None, Some(store.clone())).unwrap();
        assert!(out.healthy(), "report:\n{}", out.report);
        assert!(out.report.contains("fingerprint matches"), "{}", out.report);
        // A missing --store path errors clearly.
        let err = cmd_doctor(None, Some(tmp.path().join("nope.json"))).unwrap_err();
        assert!(err.contains("no store at"), "{err}");
    }
}
