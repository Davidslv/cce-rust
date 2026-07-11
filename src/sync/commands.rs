//! # sync::commands — the `cce sync …` command orchestration (SPEC-SYNC §5)
//!
//! **Why this file exists:** The CLI surface (`init`, `push`, `pull`, `status`,
//! `verify`) is thin argument parsing; the actual work — resolve config, read git
//! facts, export/import the artifact, drive the remote, and enforce the safety
//! rules (refuse a dirty tree or a non-hash index; never break local work) — lives
//! here so it is unit-testable against a local bare git remote without spawning the
//! binary.
//!
//! **What it is / does:** Each `cmd_*` returns `Result<String, String>` (a
//! human-readable report, or a clear error). It composes `config`, `git`, `remote`,
//! and `artifact`, and keeps a small `.cce/synced.json` marker so `status`/`verify`
//! know which sha the local cache came from (SPEC-SYNC §9.4).
//!
//! **Responsibilities:**
//! - Own `cmd_init/push/pull/status/verify` and the `--workspace` fan-out.
//! - Enforce §5 rules and §9 offline-first guarantees (best-effort, never fatal to
//!   local state).
//! - It does NOT parse CLI args (main.rs) nor define the artifact bytes (artifact).

use crate::embedder::HashEmbedder;
use crate::store::{default_store_path, Index};
use crate::sync::artifact::{Artifact, ManifestMeta};
use crate::sync::config::SyncConfig;
use crate::sync::remote::{GitRemote, SyncRemote};
use crate::sync::{
    content_address, git, hex_lower, normalize_repo_id, pointer_address, valid_repo_id,
    workspace_graph_address, workspace_manifest_address, HASH_EMBEDDER, SYNC_FORMAT_VERSION,
};
use crate::workspace::{Manifest, Member, MemberType};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Which sha a `pull` targets (SPEC-SYNC §5).
#[derive(Debug, Clone)]
pub enum PullTarget {
    /// The working tree's current HEAD (the default).
    Head,
    /// An explicit `--commit <sha>`.
    Commit(String),
    /// The remote's latest pushed sha for the default ref (`--latest`).
    Latest,
}

/// The `.cce/synced.json` marker recording what the local cache was pulled from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncState {
    pub repo_id: String,
    pub sha: String,
    pub checksum: String,
    /// SHA-256 (lowercase hex) of the exact `index.json` bytes written at
    /// install time (#55, `verify --checksum-only`). Recorded from the
    /// **installed file on disk**, not re-derived through any export path, so
    /// the later re-hash is version-independent: an artifact pushed by an
    /// older cce verifies against what THIS pull actually wrote, never against
    /// a byte shape the current code would produce. **Additive:** markers
    /// written by older binaries lack it (`verify --checksum-only` then
    /// reports a clear re-pull notice, not a false failure), and older
    /// binaries reading a new marker ignore the extra field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

impl SyncState {
    fn path(root: &Path) -> PathBuf {
        root.join(".cce").join("synced.json")
    }
    pub(crate) fn load(root: &Path) -> Option<SyncState> {
        let text = std::fs::read_to_string(Self::path(root)).ok()?;
        serde_json::from_str(&text).ok()
    }
    fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = Self::path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string(self).unwrap_or_default())
    }
}

/// The short 12-char prefix of a checksum for display, sliced on a **byte- and
/// char-boundary-safe** basis (#134). `&s[..12]` panics when `s` is shorter than
/// 12 bytes or when byte 12 splits a multi-byte char — and the checksum can come
/// from an untrusted `.cce/synced.json` marker (an older/sibling engine or a
/// hand-edit), so the diagnostic `status`/`verify` commands must render it
/// without panicking. A value under 12 bytes renders whole.
fn short_checksum(s: &str) -> &str {
    s.get(..12).unwrap_or(s)
}

/// Resolve the `repo_id` for `root`: the config override, else the normalized git
/// origin, else an error (we cannot address the cache without one).
fn resolve_repo_id(root: &Path, cfg: &SyncConfig) -> Result<String, String> {
    let id = match &cfg.repo_id {
        Some(id) => id.clone(),
        None => match git::origin_url(root) {
            Some(url) => normalize_repo_id(&url),
            None => {
                return Err(
                    "cannot determine repo_id: no `sync.repo_id` configured and no git origin \
                     remote. Set one with `cce sync init --repo-id <id>`."
                        .to_string(),
                )
            }
        },
    };
    // #141: the single chokepoint every caller (push, pull, status, init
    // override) inherits — reject a `repo_id` that is not a single cache path
    // segment (`.`, `..`, embedded `/`) BEFORE it is built into a content or
    // pointer address, mirroring the #121 corpus_id fix.
    if !valid_repo_id(&id) {
        return Err(format!(
            "invalid repo_id `{id}`: must be non-empty, charset [A-Za-z0-9._-], and a single \
             path segment — `.` and `..` are rejected (it is a path segment on the cache, so a \
             traversal token would escape the repo namespace)"
        ));
    }
    Ok(id)
}

/// Open the configured remote, or a clear "no remote" error (offline-first: this is
/// the only place that can fail for lack of a remote, and it fails cleanly).
fn open_remote(cfg: &SyncConfig) -> Result<GitRemote, String> {
    let url = cfg.remote.as_deref().ok_or_else(|| {
        "no sync remote configured — run `cce sync init --remote <git-url>`".to_string()
    })?;
    GitRemote::open(url, cfg.lfs)
}

/// Build a fresh hash-embedder index for `root`'s working tree, for export at `sha`.
///
/// **Content-address invariant (SPEC-SYNC §1/§3):** the cache is keyed by sha and the
/// invariant is `artifact == build(sha)`. Push therefore MUST rebuild from the current
/// source tree and export THAT — it must **never** re-export a pre-existing
/// `.cce/index.json`. That file may be stale, foreign, or (after `cce sync pull`) a
/// cache built by an older or sibling engine; re-publishing its bytes under the sha key
/// would launder a non-`build(sha)` index into the content-addressed cache and make
/// `cce sync verify` (which correctly rebuilds) fail against it.
///
/// If a store already exists we still read it to enforce the non-hash refusal
/// (SPEC-SYNC §1: only the deterministic hash embedder produces shareable caches), but
/// we never reuse its bytes — we always rebuild.
fn ensure_hash_index(root: &Path, sha: &str) -> Result<Index, String> {
    let store = default_store_path(root);
    if store.exists() {
        let idx =
            Index::load(&store).map_err(|e| format!("could not load {}: {e}", store.display()))?;
        if idx.embedder_name != HASH_EMBEDDER {
            return Err(format!(
                "refusing to push a non-hash index (embedder = `{}`). Only the deterministic \
                 hash embedder produces shareable caches; re-index with `cce index <dir>`.",
                idx.embedder_name
            ));
        }
    }
    // Always rebuild from the working tree so the export is exactly build(sha), never a
    // stale/foreign/pulled index.json. Use the same build path `cce index`/verify use.
    eprintln!("rebuilding index for {sha}");
    let (idx, _) = Index::build_protected(root, &HashEmbedder, |_| true, true)?;
    Ok(idx)
}

/// `cce sync init` (SPEC-SYNC §5): write `sync.*` config and set up the local clone.
pub fn cmd_init(
    root: &Path,
    remote_url: &str,
    lfs: bool,
    repo_id_override: Option<String>,
) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()));
    }
    let mut cfg = SyncConfig::load(root);
    cfg.remote = Some(remote_url.to_string());
    cfg.lfs = lfs;
    if let Some(id) = repo_id_override {
        cfg.repo_id = Some(id);
    } else if cfg.repo_id.is_none() {
        // Best effort: record the normalized origin so the id is visible/stable.
        if let Some(url) = git::origin_url(root) {
            cfg.repo_id = Some(normalize_repo_id(&url));
        }
    }
    cfg.save(root).map_err(|e| format!("could not write config: {e}"))?;

    // Set up (or reuse) the working clone; this is where LFS attributes are written.
    let remote = open_remote(&cfg)?;

    let mut out = String::new();
    out.push_str(&format!("Configured sync remote: {remote_url}\n"));
    out.push_str(&format!(
        "  git-LFS       : {}\n",
        if lfs { "enabled (*.cce)" } else { "disabled" }
    ));
    if let Some(id) = &cfg.repo_id {
        out.push_str(&format!("  repo_id       : {id}\n"));
    } else {
        out.push_str("  repo_id       : (resolved from git origin at push time)\n");
    }
    out.push_str(&format!("  working clone : {}\n", remote.dir().display()));
    out.push_str(&format!(
        "  config        : {}\n",
        crate::sync::config::config_path(root).display()
    ));
    Ok(out)
}

/// Resolve the sha to push: `--commit` or HEAD; refuse a dirty tree (SPEC-SYNC §5).
///
/// **`--commit` is a sanity assertion, not a backfill selector (#116).** Push always
/// rebuilds the artifact from the *working tree* (`ensure_hash_index`), so the only
/// sha it can honestly publish is HEAD — `artifact == build(HEAD)`. An explicit
/// `--commit <sha>` that resolves to something other than HEAD would launder
/// build(HEAD) into that sha's content-address key *and* rewind `refs/<branch>` to
/// it, poisoning the shared cache. We therefore reject any `--commit` that is not a
/// valid commit, or that does not resolve to the current HEAD.
fn resolve_push_sha(root: &Path, commit: Option<String>) -> Result<String, String> {
    if git::is_dirty(root) {
        return Err(
            "refusing to push: the working tree is dirty. Commit your changes and push a clean \
             sha (a cache is content-addressed by commit)."
                .to_string(),
        );
    }
    let head = git::head_sha(root).ok_or_else(|| {
        "cannot determine HEAD sha — is this a git repository with a commit?".to_string()
    })?;
    match commit {
        None => Ok(head),
        Some(sha) => {
            let resolved = git::resolve_commit(root, &sha).ok_or_else(|| {
                format!("--commit {sha} is not a valid commit in this repository")
            })?;
            if resolved != head {
                return Err(format!(
                    "--commit {sha} does not match HEAD {head}; push builds the artifact from the \
                     working tree, so it can only publish HEAD. (To publish an old commit you must \
                     check it out first.)"
                ));
            }
            Ok(head)
        }
    }
}

/// Export a single repo's index at `sha` to an artifact and put it on the remote,
/// updating the ref pointer. Returns the artifact checksum.
fn push_one(
    root: &Path,
    remote: &dyn SyncRemote,
    repo_id: &str,
    sha: &str,
) -> Result<(String, String), String> {
    let index = ensure_hash_index(root, sha)?;
    let meta = ManifestMeta { repo_id: repo_id.to_string(), sha: sha.to_string() };
    let artifact = Artifact::from_index(&index, meta);
    let bytes = artifact.to_bytes();
    let ver = SYNC_FORMAT_VERSION.to_string();
    let key = content_address(HASH_EMBEDDER, &ver, repo_id, sha);

    // Update the ref pointer to this sha alongside the artifact, in one commit/push.
    let branch = git::current_branch(root).unwrap_or_else(|| crate::sync::DEFAULT_REF.to_string());
    let pointer_key = pointer_address(HASH_EMBEDDER, &ver, repo_id, &branch);
    remote.put_many(&[(key.clone(), bytes), (pointer_key, format!("{sha}\n").into_bytes())])?;
    Ok((key, artifact.manifest.checksum))
}

/// `cce sync push` (SPEC-SYNC §5): export the hash-index for HEAD/sha and put it.
pub fn cmd_push(root: &Path, commit: Option<String>, workspace: bool) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let remote = open_remote(&cfg)?;

    if workspace {
        return push_workspace(root, &cfg, &remote);
    }

    let sha = resolve_push_sha(root, commit)?;
    let repo_id = resolve_repo_id(root, &cfg)?;
    let (key, checksum) = push_one(root, &remote, &repo_id, &sha)?;
    Ok(format!("Pushed {repo_id}@{sha}\n  key      : {key}\n  checksum : {checksum}\n"))
}

/// Push every workspace member, each keyed by its own `repo_id@sha`, then
/// publish the workspace metadata under the **base** repo_id (#55, the
/// self-describing cache): the canonical `workspace.yml` bytes and a freshly
/// derived `workspace-graph.json` at the well-known
/// `workspace_manifest_address`/`workspace_graph_address` keys. Publishing is
/// **additive** — neither key is a `<sha>.cce` artifact nor a `refs/<ref>`
/// pointer, so existing artifact keys, ref pointers, and old-client pulls are
/// untouched. The graph is re-derived from the source manifests on disk (the
/// same `build_graph` `cce index --workspace` uses) rather than re-uploading a
/// possibly stale `.cce/workspace-graph.json`.
fn push_workspace(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
) -> Result<String, String> {
    let manifest = Manifest::load(root)?;
    let base = resolve_repo_id(root, cfg)?;
    let sha = resolve_push_sha(root, None)?;
    let mut out = format!("Pushing workspace {} @ {sha}\n", manifest.name);
    for m in &manifest.members {
        let member_dir = root.join(&m.path);
        let repo_id = format!("{base}__{}", m.name);
        let (key, checksum) = push_one(&member_dir, remote, &repo_id, &sha)?;
        out.push_str(&format!("  {:<16} {key}  ({})\n", m.name, short_checksum(&checksum)));
    }
    let ver = SYNC_FORMAT_VERSION.to_string();
    let graph = crate::workspace::build_graph(root, &manifest);
    remote.put_many(&[
        (workspace_manifest_address(HASH_EMBEDDER, &ver, &base), manifest.to_yaml().into_bytes()),
        (workspace_graph_address(HASH_EMBEDDER, &ver, &base), graph.to_json().into_bytes()),
    ])?;
    out.push_str(&format!(
        "  metadata         workspace.yml + workspace-graph.json under {base} ({} edge{})\n",
        graph.edges.len(),
        if graph.edges.len() == 1 { "" } else { "s" }
    ));
    Ok(out)
}

/// The names of every `refs/<name>` pointer a repo carries, sorted — ONE
/// listing call over the repo's `refs/` directory (#72), never N pointer
/// reads. Nested junk under `refs/` is skipped (the #37 graceful-skip rule);
/// a repo with no pointers lists as empty.
fn list_ref_names(
    remote: &dyn SyncRemote,
    ver: &str,
    repo_id: &str,
) -> Result<Vec<String>, String> {
    let prefix = format!("{HASH_EMBEDDER}/{ver}/{repo_id}/refs");
    let dir = format!("{prefix}/");
    Ok(remote
        .list_keys_with_suffix(&prefix, "")?
        .iter()
        .filter_map(|k| k.strip_prefix(dir.as_str()))
        .filter(|n| !n.is_empty() && !n.contains('/'))
        .map(str::to_string)
        .collect())
}

/// Read a `refs/<name>` pointer's sha via the artifact-read path (`get`, which
/// any `SyncRemote` backend supports). The sha comes back trimmed; an absent
/// pointer is the clear per-ref "no `--latest` pointer" error.
fn read_pointer(
    remote: &dyn SyncRemote,
    ver: &str,
    repo_id: &str,
    name: &str,
) -> Result<String, String> {
    let pointer = pointer_address(HASH_EMBEDDER, ver, repo_id, name);
    let bytes = remote
        .get(&pointer)
        .map_err(|_| format!("no `--latest` pointer for {repo_id} on `{name}`"))?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// Resolve the sha a pull should install. For `--latest` the resolution order
/// is (#72): an explicit ref (the CLI `--ref`, else the project's `sync.ref`
/// config), else `refs/main`, else the **single-fallback rule** — when exactly
/// one other `refs/<name>` pointer exists it wins; several are an error naming
/// them (the operator must choose a ref); none keeps today's error verbatim.
/// The second tuple element is the ref the sha came from, `Some` ONLY when it
/// is not `main` — so every `refs/main`-resolved report stays byte-identical.
fn resolve_pull_sha(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
    repo_id: &str,
    target: &PullTarget,
    ref_override: Option<&str>,
) -> Result<(String, Option<String>), String> {
    match target {
        PullTarget::Commit(sha) => Ok((sha.clone(), None)),
        PullTarget::Head => git::head_sha(root).map(|sha| (sha, None)).ok_or_else(|| {
            "cannot determine HEAD sha — pass --commit <sha> or --latest".to_string()
        }),
        PullTarget::Latest => {
            let ver = SYNC_FORMAT_VERSION.to_string();
            // Explicit ref: the CLI `--ref` wins, else the `sync.ref` config (#72).
            if let Some(name) = ref_override.map(str::to_string).or_else(|| cfg.git_ref.clone()) {
                let sha = read_pointer(remote, &ver, repo_id, &name)?;
                let noted = (name != crate::sync::DEFAULT_REF).then_some(name);
                return Ok((sha, noted));
            }
            if let Ok(sha) = read_pointer(remote, &ver, repo_id, crate::sync::DEFAULT_REF) {
                return Ok((sha, None));
            }
            // refs/main is absent — the #72 single-fallback rule.
            let refs = list_ref_names(remote, &ver, repo_id)?;
            match refs.as_slice() {
                [only] => Ok((read_pointer(remote, &ver, repo_id, only)?, Some(only.clone()))),
                [] => Err(format!(
                    "no `--latest` pointer for {repo_id} on `{}`",
                    crate::sync::DEFAULT_REF
                )),
                several => Err(format!(
                    "no `--latest` pointer for {repo_id} on `{}`; available refs: {} — pass \
                     --ref <name> (or set `sync.ref` in .cce/config)",
                    crate::sync::DEFAULT_REF,
                    several.join(", ")
                )),
            }
        }
    }
}

/// Install an artifact's bytes into `root`'s `.cce/` store; record the sync marker.
fn install_artifact(root: &Path, bytes: &[u8]) -> Result<Artifact, String> {
    let artifact = Artifact::from_bytes(bytes)?;
    let index = artifact.clone().into_index();
    let store = default_store_path(root);
    index.save(&store).map_err(|e| format!("could not write {}: {e}", store.display()))?;
    // #55: hash the EXACT bytes just installed (read back from disk) so
    // `verify --checksum-only` has a version-independent baseline — "has this
    // file changed since pull", regardless of which cce version pushed the
    // artifact. Best-effort: an unreadable store leaves the field absent and
    // verify reports the re-pull notice.
    let installed_sha256 = std::fs::read(&store).ok().map(|b| hex_lower(&Sha256::digest(&b)));
    // #62: stamp a build fingerprint beside the installed store so `cce doctor`
    // can drift-check pulled stores too. Shareable artifacts are always
    // hash-embedded and redaction-protected (push refuses otherwise), and the
    // remaining fields are pinned by the artifact format. Best-effort — a
    // failed write only disables drift detection, never the pull.
    if let Err(e) = crate::fingerprint::write_for_store(&store, &index, true) {
        eprintln!("warning: could not write the store fingerprint: {e}");
    }
    SyncState {
        repo_id: artifact.manifest.repo_id.clone(),
        sha: artifact.manifest.sha.clone(),
        checksum: artifact.manifest.checksum.clone(),
        installed_sha256,
    }
    .save(root)
    .map_err(|e| format!("could not write sync marker: {e}"))?;

    // Best-effort: record a `sync-pull` index event so the dashboard's freshness
    // panel shows the pulled provenance (purely log-derived — the dashboard makes no
    // live lookup). Never fatal to the pull.
    let clock = crate::metrics::SystemClock;
    let ids = crate::metrics::HexIdSource::default();
    let writer = crate::metrics::MetricsWriter::new(
        crate::store::default_metrics_path(root),
        &clock,
        &ids,
        true,
    );
    writer.log_index(&crate::metrics::IndexRecord {
        files_indexed: index.files().len(),
        chunks: index.chunks.len(),
        index_bytes: bytes.len() as u64,
        duration_ms: 0.0,
        embedder: HASH_EMBEDDER.to_string(),
        full: true,
        sha: Some(artifact.manifest.sha.clone()),
        source: "sync-pull".to_string(),
        sensitive_skipped: 0,
    });

    Ok(artifact)
}

/// `cce sync pull` (SPEC-SYNC §5): fetch the cache for a sha and install it.
/// `git_ref` is the explicit `--ref <name>` a `--latest` pull resolves against
/// (#72); `None` uses the `sync.ref` config, else main-else-single-fallback.
pub fn cmd_pull(
    root: &Path,
    target: PullTarget,
    force: bool,
    workspace: bool,
    git_ref: Option<String>,
) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let remote = open_remote(&cfg)?;

    if workspace {
        return pull_workspace(root, &cfg, &remote, &target, git_ref.as_deref());
    }

    let repo_id = resolve_repo_id(root, &cfg)?;
    let (sha, noted_ref) =
        resolve_pull_sha(root, &cfg, &remote, &repo_id, &target, git_ref.as_deref())?;

    // §9.4: do not silently overwrite a newer local index for a different sha.
    if !force {
        if let Some(state) = SyncState::load(root) {
            if state.sha != sha {
                return Err(format!(
                    "local cache is at {} but you are pulling {sha}. Pass --force to overwrite.",
                    state.sha
                ));
            }
        }
    }

    let ver = SYNC_FORMAT_VERSION.to_string();
    let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
    let bytes = remote.get(&key)?;
    let artifact = install_artifact(root, &bytes)?;

    let mut out = format!("Pulled {repo_id}@{sha}\n");
    // #72: a `--latest` resolved off `refs/main` (single-fallback, `--ref`, or
    // `sync.ref`) names the ref; a main-resolved pull stays byte-identical.
    if let Some(name) = &noted_ref {
        out.push_str(&format!("  ref      : {name}\n"));
    }
    out.push_str(&format!(
        "  chunks   : {}\n  checksum : {}\n  store    : {}\n",
        artifact.manifest.chunk_count,
        artifact.manifest.checksum,
        default_store_path(root).display()
    ));
    // §7 v1: if the working tree differs from the pulled sha, note the fallback.
    match git::head_sha(root) {
        Some(head) if head == sha && !git::is_dirty(root) => {
            out.push_str("  tree     : matches — pulled index used as-is\n");
        }
        Some(_) => {
            out.push_str(
                "  tree     : differs from the pulled sha — `cce index` locally for a WIP index\n",
            );
        }
        None => {
            // Consumer mode (#54): no source checkout at all. The pulled index IS
            // the corpus — nothing to compare, nothing to re-index.
            out.push_str("  tree     : (no source checkout — consumer mode; the pulled index is the corpus)\n");
        }
    }
    Ok(out)
}

/// Fetch the published workspace metadata for `base` — the manifest and (when
/// present) the cross-member graph — if the cache carries them (#55, the
/// self-describing cache). **Best-effort by design:** an old cache that never
/// published metadata returns `None` and the caller proceeds exactly as before
/// (#55 is additive); unparsable metadata is treated the same as absent.
fn fetch_published_workspace(
    remote: &dyn SyncRemote,
    ver: &str,
    base: &str,
) -> Option<(Manifest, Option<crate::workspace::WorkspaceGraph>)> {
    let yaml = remote.get(&workspace_manifest_address(HASH_EMBEDDER, ver, base)).ok()?;
    let manifest = Manifest::from_yaml(std::str::from_utf8(&yaml).ok()?).ok()?;
    let graph = remote
        .get(&workspace_graph_address(HASH_EMBEDDER, ver, base))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|t| crate::workspace::WorkspaceGraph::from_json(&t).ok());
    Some((manifest, graph))
}

/// Merge the published member metadata into the local manifest (#55). Members
/// are matched **by name**; the local `path` — the consumer's actual on-disk
/// layout — always wins, while `type` and `package` adopt the published truth
/// (a repo-less consumer has no source to re-detect them from). Returns how
/// many members were enriched.
fn merge_published_members(local: &mut Manifest, published: &Manifest) -> usize {
    let mut merged = 0usize;
    for m in &mut local.members {
        if let Some(p) = published.member(&m.name) {
            m.member_type = p.member_type;
            m.package = p.package.clone();
            merged += 1;
        }
    }
    merged
}

/// Pull every workspace member from its own `repo_id@sha` cache, then install
/// the published workspace metadata (#55) so a repo-less consumer keeps the
/// real member types/packages AND the cross-member dependency edges that drive
/// federated graph expansion. With no local manifest at all, the published one
/// bootstraps the layout (its member paths become the consumer directories).
fn pull_workspace(
    root: &Path,
    cfg: &SyncConfig,
    remote: &dyn SyncRemote,
    target: &PullTarget,
    git_ref: Option<&str>,
) -> Result<String, String> {
    let base = resolve_repo_id(root, cfg)?;
    let ver = SYNC_FORMAT_VERSION.to_string();
    let published = fetch_published_workspace(remote, &ver, &base);
    let (manifest, bootstrapped) = match Manifest::load(root) {
        Ok(m) => (m, false),
        // #55: a repo-less consumer with only a config — bootstrap the layout
        // from the published manifest. Without one, the original error stands.
        Err(load_err) => match &published {
            Some((p, _)) => {
                p.save(root).map_err(|e| format!("could not write workspace manifest: {e}"))?;
                (p.clone(), true)
            }
            None => return Err(load_err),
        },
    };
    let mut out = format!("Pulling workspace {}\n", manifest.name);
    if bootstrapped {
        out.push_str(&format!(
            "  manifest         installed from the published metadata under {base}\n"
        ));
    }
    for m in &manifest.members {
        let member_dir = root.join(&m.path);
        let repo_id = format!("{base}__{}", m.name);
        let (sha, noted_ref) =
            resolve_pull_sha(&member_dir, cfg, remote, &repo_id, target, git_ref)?;
        let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
        let bytes = remote.get(&key)?;
        let artifact = install_artifact(&member_dir, &bytes)?;
        // #72: note a non-main ref per member; main rows stay byte-identical.
        let note = noted_ref.map(|n| format!("  (ref {n})")).unwrap_or_default();
        out.push_str(&format!(
            "  {:<16} {sha}  chunks {}  ({}){note}\n",
            m.name,
            artifact.manifest.chunk_count,
            short_checksum(&artifact.manifest.checksum)
        ));
    }
    // #55: adopt the published member metadata and install the published graph.
    // Absent metadata (an old cache) changes nothing — the pre-#55 behaviour.
    if let Some((p, graph)) = published {
        let mut merged_manifest = manifest;
        merge_published_members(&mut merged_manifest, &p);
        merged_manifest
            .save(root)
            .map_err(|e| format!("could not write workspace manifest: {e}"))?;
        if let Some(g) = graph {
            g.save(root).map_err(|e| format!("could not write workspace graph: {e}"))?;
            out.push_str(&format!(
                "  metadata         workspace.yml + workspace-graph.json installed ({} cross-member edge{})\n",
                g.edges.len(),
                if g.edges.len() == 1 { "" } else { "s" }
            ));
        } else {
            out.push_str("  metadata         workspace.yml installed (no published graph)\n");
        }
    }
    Ok(out)
}

/// `cce sync status` (SPEC-SYNC §5): remote, local cache sha, remote latest, match.
pub fn cmd_status(root: &Path) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let mut out = String::new();
    match &cfg.remote {
        Some(r) => out.push_str(&format!("remote        : {r}\n")),
        None => {
            out.push_str("remote        : (none — pure local CCE)\n");
            return Ok(out);
        }
    }
    out.push_str(&format!("git-LFS       : {}\n", if cfg.lfs { "on" } else { "off" }));

    let repo_id = match resolve_repo_id(root, &cfg) {
        Ok(id) => id,
        Err(e) => {
            out.push_str(&format!("repo_id       : (unresolved: {e})\n"));
            return Ok(out);
        }
    };
    out.push_str(&format!("repo_id       : {repo_id}\n"));

    match SyncState::load(root) {
        Some(state) => out.push_str(&format!(
            "local cache   : {} ({})\n",
            state.sha,
            short_checksum(&state.checksum)
        )),
        None => out.push_str("local cache   : (none pulled yet)\n"),
    }

    // Remote latest is best-effort: offline ⇒ we still print everything else.
    match open_remote(&cfg) {
        Ok(remote) => {
            let ver = SYNC_FORMAT_VERSION.to_string();
            let pointer = pointer_address(HASH_EMBEDDER, &ver, &repo_id, crate::sync::DEFAULT_REF);
            match remote.get(&pointer) {
                Ok(bytes) => out.push_str(&format!(
                    "remote latest : {} (ref {})\n",
                    String::from_utf8_lossy(&bytes).trim(),
                    crate::sync::DEFAULT_REF
                )),
                Err(_) => out.push_str("remote latest : (no pointer yet)\n"),
            }
        }
        Err(e) => out.push_str(&format!("remote latest : (unreachable: {e})\n")),
    }

    match git::head_sha(root) {
        Some(head) => {
            let dirty = if git::is_dirty(root) { " (dirty)" } else { "" };
            out.push_str(&format!("working tree  : {head}{dirty}\n"));
        }
        None => out.push_str("working tree  : (not a git checkout)\n"),
    }
    Ok(out)
}

/// One repo's aggregate row in a cache listing (`cce sync list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListing {
    pub repo_id: String,
    /// The repo's latest pointer — the same source of truth `pull --latest`
    /// resolves: `refs/<DEFAULT_REF>`, else the #72 single-fallback ref. `None`
    /// when no pointer resolves (none pushed yet, or several non-main refs).
    pub latest_sha: Option<String>,
    /// The ref `latest_sha` came from, `Some` ONLY when it is not `main` (the
    /// #72 single-fallback) — a main-resolved row renders byte-identically.
    pub latest_ref: Option<String>,
    /// Every `refs/<name>` pointer present, sorted — the multi-ref skip warning
    /// names these so the operator can pick one (`--ref` / `sync.ref`).
    pub refs: Vec<String>,
    /// Distinct artifact shas cached for this repo.
    pub artifacts: usize,
    /// Total artifact bytes (LFS-aware: the pointer's recorded size, not the pointer).
    pub bytes: u64,
}

/// One knowledge corpus's aggregate row in a cache listing
/// (SPEC-SYNC-KNOWLEDGE §6). `current`, `data_as_of`, and `pushed_at` are
/// nullable — a field never disappears within a row; the latter two come from
/// the best-effort published `corpus.json` and degrade to `None` when it is
/// absent or unparsable (§4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeListing {
    pub corpus_id: String,
    /// The corpus `current` pointer's snapshot — the same source of truth
    /// `knowledge pull` resolves. `None` when no pointer exists yet.
    pub current: Option<String>,
    /// Distinct `<snapshot>.cck` keys cached for this corpus.
    pub snapshots: usize,
    /// Total artifact bytes (LFS-aware: the pointer's recorded size).
    pub bytes: u64,
    /// The corpus's deterministic data age, from `corpus.json`.
    pub data_as_of: Option<String>,
    /// When the corpus was last published, from `corpus.json`.
    pub pushed_at: Option<String>,
}

/// The whole cache listing: the resolved remote plus one row per `repo_id`,
/// sorted by `repo_id` (deterministic output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheListing {
    pub remote: String,
    pub repos: Vec<RepoListing>,
    /// Base repo_ids carrying **published workspace metadata** (#55): a
    /// `workspace.yml` blob at the well-known `workspace_manifest_address`.
    /// Sorted (repo_id order). A prefix that is *pure metadata* — no artifacts
    /// and no latest pointer — lists here and NOT in `repos`, so the rendered
    /// `sync list` output (byte-pinned `cce.synclist/v1`) is unchanged and
    /// `pull --all` never warn-skips a metadata prefix as an unpullable repo.
    pub workspaces: Vec<String>,
    /// Knowledge corpora the cache carries (SPEC-SYNC-KNOWLEDGE §6), sorted by
    /// `corpus_id`. Empty on a knowledge-free cache — the rendered listing is
    /// then byte-identical to the pre-M5 shape (human and JSON alike).
    pub knowledge: Vec<KnowledgeListing>,
}

/// `cce sync list` (#53): enumerate what a cache holds — one row per `repo_id`
/// with its latest sha, artifact count, and total artifact bytes. **Read-only**:
/// it never writes to the cache or the local `.cce/`, and it needs no local
/// store, source checkout, or config — a bare directory plus `--remote` works.
///
/// The remote resolves exactly as `cce sync status` does: the explicit
/// `remote_override` (`--remote`), else `.cce/config`'s `sync.remote`.
pub fn cmd_list(root: &Path, remote_override: Option<String>) -> Result<CacheListing, String> {
    let cfg = SyncConfig::load(root);
    let url = remote_override.or_else(|| cfg.remote.clone()).ok_or_else(|| {
        "no sync remote configured — run `cce sync init --remote <git-url>` or pass \
         `--remote <url>`"
            .to_string()
    })?;
    // Always open with LFS off: `open(url, true)` would write, commit, and push a
    // `.gitattributes` — and `list` must never mutate the cache. Reading needs no
    // smudge (artifact bytes are never checked out here).
    let remote = GitRemote::open(&url, false)?;

    let ver = SYNC_FORMAT_VERSION.to_string();
    let base = format!("{HASH_EMBEDDER}/{ver}");
    let mut repos = Vec::new();
    let mut workspaces = Vec::new();
    for repo_id in remote.list_dirs(&base)? {
        let prefix = format!("{base}/{repo_id}");
        // The pinned #37 walk: `.cce` shas only, junk entries skipped gracefully.
        let shas = remote.list(&prefix)?;
        let bytes = remote.list_artifact_sizes(&prefix)?.iter().map(|(_, b)| b).sum();
        // #72: `refs/main` when present, else the sole other ref (annotated),
        // else no latest (0 or several refs) — the same rule `pull --latest`
        // applies, so `list` and consumer pulls can never disagree.
        let refs = list_ref_names(&remote, &ver, &repo_id)?;
        let chosen: Option<&str> = if refs.iter().any(|r| r == crate::sync::DEFAULT_REF) {
            Some(crate::sync::DEFAULT_REF)
        } else {
            match refs.as_slice() {
                [only] => Some(only.as_str()),
                _ => None,
            }
        };
        let latest_sha = chosen.and_then(|name| {
            let pointer = pointer_address(HASH_EMBEDDER, &ver, &repo_id, name);
            remote.read_blob_text(&pointer).ok().filter(|s| !s.is_empty())
        });
        let latest_ref = match (&latest_sha, chosen) {
            (Some(_), Some(name)) if name != crate::sync::DEFAULT_REF => Some(name.to_string()),
            _ => None,
        };
        // #55: a prefix carrying published workspace metadata is a workspace base.
        let has_manifest =
            remote.has(&workspace_manifest_address(HASH_EMBEDDER, &ver, &repo_id)).unwrap_or(false);
        if has_manifest {
            workspaces.push(repo_id.clone());
            if shas.is_empty() && latest_sha.is_none() {
                // Pure metadata — nothing pullable lives here; not a repo row.
                continue;
            }
        }
        repos.push(RepoListing {
            repo_id,
            latest_sha,
            latest_ref,
            refs,
            artifacts: shas.len(),
            bytes,
        });
    }
    let knowledge = list_knowledge(&remote)?;
    // `list_dirs` already sorts, so rows, workspaces, and corpora are
    // deterministic (repo_id / corpus_id order).
    Ok(CacheListing { remote: url, repos, workspaces, knowledge })
}

/// Enumerate the cache's knowledge corpora (SPEC-SYNC-KNOWLEDGE §6): the
/// `knowledge/<contract_version>/<corpus_id>/` prefixes, each with its `current`
/// pointer, distinct `.cck` count, LFS-aware bytes, and the best-effort
/// `corpus.json` freshness fields. Reading stays cheap and read-only: only plain
/// text blobs (`current`, `corpus.json`) plus key/size enumeration — no LFS
/// smudge, no cache mutation (the same no-mutation posture as `cmd_list`).
fn list_knowledge(remote: &GitRemote) -> Result<Vec<KnowledgeListing>, String> {
    let kver = crate::sync::knowledge_contract_version();
    let base = format!("knowledge/{kver}");
    let mut out = Vec::new();
    for corpus_id in remote.list_dirs(&base)? {
        let prefix = format!("{base}/{corpus_id}");
        let snapshots = remote.list_keys_with_suffix(&prefix, ".cck")?.len();
        let bytes = remote.list_sizes_with_suffix(&prefix, ".cck")?.iter().map(|(_, b)| b).sum();
        let current = remote
            .read_blob_text(&crate::sync::knowledge_pointer_address(kver, &corpus_id))
            .ok()
            .filter(|s| !s.is_empty());
        // corpus.json is best-effort display metadata: absent or unparsable
        // degrades to null fields, never an error (§4.4).
        let meta: Option<serde_json::Value> = remote
            .read_blob_text(&crate::sync::knowledge_corpus_meta_address(kver, &corpus_id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let meta_str =
            |k: &str| -> Option<String> { meta.as_ref()?.get(k)?.as_str().map(str::to_string) };
        out.push(KnowledgeListing {
            corpus_id,
            current,
            snapshots,
            bytes,
            data_as_of: meta_str("data_as_of"),
            pushed_at: meta_str("pushed_at"),
        });
    }
    Ok(out)
}

/// Render a cache listing as the human table: a `remote` header line in the
/// `status` label style, aligned columns sorted by repo_id (a `-` marks a repo
/// with no latest pointer), and a total line. An empty cache is a friendly
/// message, not an error.
pub fn render_list_human(listing: &CacheListing) -> String {
    let mut out = format!("remote        : {}\n", listing.remote);
    if listing.repos.is_empty() {
        if listing.knowledge.is_empty() {
            out.push_str("\nThe cache is empty — nothing has been pushed yet.\n");
        } else {
            out.push_str("\nNo code repos cached — the cache carries only knowledge corpora.\n");
            out.push_str(&render_knowledge_section(&listing.knowledge));
        }
        return out;
    }
    // #72: a single-fallback row annotates its ref — `<sha> (master)` — inside
    // the `latest` column, so alignment stays deterministic (the column widths
    // are computed over the rendered values). Main-resolved rows are unchanged.
    let latest_of = |r: &RepoListing| match (&r.latest_sha, &r.latest_ref) {
        (Some(sha), Some(name)) => format!("{sha} ({name})"),
        (Some(sha), None) => sha.clone(),
        (None, _) => "-".to_string(),
    };
    let id_w =
        listing.repos.iter().map(|r| r.repo_id.len()).chain(["repo_id".len()]).max().unwrap();
    let sha_w =
        listing.repos.iter().map(|r| latest_of(r).len()).chain(["latest".len()]).max().unwrap();
    let bytes_w = listing
        .repos
        .iter()
        .map(|r| r.bytes.to_string().len())
        .chain(["bytes".len()])
        .max()
        .unwrap();
    out.push('\n');
    out.push_str(&format!(
        "{:<id_w$}  {:<sha_w$}  {:>9}  {:>bytes_w$}\n",
        "repo_id", "latest", "artifacts", "bytes"
    ));
    let (mut total_artifacts, mut total_bytes) = (0usize, 0u64);
    for r in &listing.repos {
        total_artifacts += r.artifacts;
        total_bytes += r.bytes;
        out.push_str(&format!(
            "{:<id_w$}  {:<sha_w$}  {:>9}  {:>bytes_w$}\n",
            r.repo_id,
            latest_of(r),
            r.artifacts,
            r.bytes
        ));
    }
    let repos_n = listing.repos.len();
    out.push_str(&format!(
        "\ntotal         : {repos_n} repo{}, {total_artifacts} artifact{}, {total_bytes} bytes\n",
        if repos_n == 1 { "" } else { "s" },
        if total_artifacts == 1 { "" } else { "s" },
    ));
    out.push_str(&render_knowledge_section(&listing.knowledge));
    out
}

/// The human knowledge block (SPEC-SYNC-KNOWLEDGE §6): one aligned row per
/// corpus after the repos table, rendered ONLY when the cache carries at least
/// one corpus — a knowledge-free cache's listing stays byte-identical.
fn render_knowledge_section(corpora: &[KnowledgeListing]) -> String {
    if corpora.is_empty() {
        return String::new();
    }
    let current_of = |k: &KnowledgeListing| k.current.clone().unwrap_or_else(|| "-".to_string());
    let as_of = |k: &KnowledgeListing| k.data_as_of.clone().unwrap_or_else(|| "-".to_string());
    let id_w = corpora.iter().map(|k| k.corpus_id.len()).chain(["corpus_id".len()]).max().unwrap();
    let cur_w = corpora.iter().map(|k| current_of(k).len()).chain(["current".len()]).max().unwrap();
    let bytes_w =
        corpora.iter().map(|k| k.bytes.to_string().len()).chain(["bytes".len()]).max().unwrap();
    let mut out = String::from("\nknowledge:\n");
    out.push_str(&format!(
        "{:<id_w$}  {:<cur_w$}  {:>9}  {:>bytes_w$}  {}\n",
        "corpus_id", "current", "snapshots", "bytes", "data as-of"
    ));
    for k in corpora {
        out.push_str(&format!(
            "{:<id_w$}  {:<cur_w$}  {:>9}  {:>bytes_w$}  {}\n",
            k.corpus_id,
            current_of(k),
            k.snapshots,
            k.bytes,
            as_of(k)
        ));
    }
    out
}

/// Render a cache listing as the stable, versioned `cce.synclist/v1` JSON shape
/// (`--json`). Same grammar discipline as the other `--json` surfaces: pretty-
/// printed, two-space indent, serde_json's alphabetical key order, one trailing
/// newline. A missing latest pointer is JSON `null` — the field never disappears.
pub fn render_list_json(listing: &CacheListing) -> String {
    let repos: Vec<serde_json::Value> = listing
        .repos
        .iter()
        .map(|r| {
            let mut row = serde_json::json!({
                "repo_id": r.repo_id,
                "latest_sha": r.latest_sha,
                "artifacts": r.artifacts,
                "bytes": r.bytes,
            });
            // #72: an OPTIONAL `ref` field, present ONLY when the latest sha
            // was resolved via the single-ref fallback — every main-resolved
            // row stays byte-identical (tolerant-reader additivity, the
            // SPEC-SYNC §3 rule; the schema stays `cce.synclist/v1`).
            if let Some(name) = &r.latest_ref {
                row.as_object_mut()
                    .expect("repo row is an object")
                    .insert("ref".to_string(), serde_json::json!(name));
            }
            row
        })
        .collect();
    let mut body = serde_json::json!({
        "schema": "cce.synclist/v1",
        "remote": listing.remote,
        "repos": repos,
    });
    // SPEC-SYNC-KNOWLEDGE §6: the schema STAYS `cce.synclist/v1` and gains an
    // OPTIONAL `knowledge` array, emitted only when the cache carries at least
    // one corpus — a knowledge-free listing is byte-identical to the pre-M5
    // shape (tolerant-reader additivity, the SPEC-SYNC §3 rule). Nullable
    // fields stay present as JSON `null` — a field never disappears in a row.
    if !listing.knowledge.is_empty() {
        let corpora: Vec<serde_json::Value> = listing
            .knowledge
            .iter()
            .map(|k| {
                serde_json::json!({
                    "corpus_id": k.corpus_id,
                    "current": k.current,
                    "snapshots": k.snapshots,
                    "bytes": k.bytes,
                    "data_as_of": k.data_as_of,
                    "pushed_at": k.pushed_at,
                })
            })
            .collect();
        body.as_object_mut()
            .expect("synclist body is an object")
            .insert("knowledge".to_string(), serde_json::Value::Array(corpora));
    }
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// The last `__` segment of a `repo_id` — the human-friendly member name
/// (`github.com__acme__billing` → `billing`). A repo_id with no `__` is its own
/// short name.
fn short_member_name(repo_id: &str) -> String {
    repo_id.rsplit("__").next().filter(|s| !s.is_empty()).unwrap_or(repo_id).to_string()
}

/// Resolve a collision-free member name with the workspace `-2`/`-3` convention
/// (the first taker keeps the bare name; see `workspace::detect_members`).
fn dedup_member_name(base: &str, used: &BTreeSet<String>) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{base}-{n}");
        if !used.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// `cce sync pull --all --into <dir>` (#54): the one-command repo-less consumer
/// workspace. Enumerates the cache (the #53 `cmd_list` machinery), pulls every
/// repo_id's `--latest` artifact into `<dir>/<member>/.cce/`, and synthesizes
/// `<dir>/.cce/workspace.yml` (+ per-member and root `.cce/config`) so
/// `cce search --workspace <dir>` and `cce mcp --workspace --dir <dir>` work
/// immediately — no source checkout anywhere.
///
/// Rules:
/// - A repo with **no latest pointer** cannot be pulled `--latest`: it is warned
///   and skipped; the run continues (one unpullable repo never fails the rest).
/// - **Idempotent refresh**: a member whose installed sha (the `.cce/synced.json`
///   marker `install_artifact` writes) equals the cache's latest pointer is
///   reported `up-to-date` and not re-fetched; a moved pointer re-pulls exactly
///   that member; new repo_ids gain new members; a member whose repo_id vanished
///   from the cache is warned about but never deleted.
/// - **Naming**: member name/directory = the repo_id's last `__` segment,
///   collision-suffixed `-2`/`-3` in repo_id order; the full repo_id lives in the
///   member's `.cce/config` (`sync.repo_id`), so per-member `cce sync pull
///   --latest` refreshes also work.
/// - Members are federated **at independent shas** — there is no one-workspace-sha
///   assumption on the pull side.
/// - **Knowledge** (SPEC-SYNC-KNOWLEDGE §7): after the member pulls, the cache's
///   corpus (if any) is installed into the consumer root's `.cce/knowledge/` —
///   `--corpus <id>` wins; a cache carrying exactly one corpus installs it; with
///   several and no flag the run warns and skips knowledge, naming the ids. The
///   install reuses `cce knowledge pull` verbatim, so the store is byte-identical
///   to a direct pull, and the marker makes the refresh idempotent (an unmoved
///   remote `current` is `up-to-date`, not re-fetched — the member rule).
pub fn cmd_pull_all(
    into: &Path,
    remote_override: Option<String>,
    corpus: Option<String>,
) -> Result<String, String> {
    std::fs::create_dir_all(into).map_err(|e| format!("cannot create {}: {e}", into.display()))?;
    let listing = cmd_list(into, remote_override)?;
    let mut out = format!("remote        : {}\n", listing.remote);
    if listing.repos.is_empty() {
        if listing.knowledge.is_empty() {
            out.push_str("\nThe cache is empty — nothing has been pushed yet.\n");
        } else {
            // A knowledge-only cache: no members to pull, but the corpus still
            // installs (a knowledge-only consumer is a valid §7 shape).
            out.push_str("\nNo code repos cached — the cache carries only knowledge corpora.\n\n");
            pull_all_knowledge(into, &listing, corpus.as_deref(), &mut out);
        }
        return Ok(out);
    }
    // The same read-only open `cmd_list` uses: never write LFS attributes into
    // the cache from a consumer.
    let remote = GitRemote::open(&listing.remote, false)?;
    let ver = SYNC_FORMAT_VERSION.to_string();

    // A refresh run starts from the existing synthesized manifest; members map to
    // cache repos via the `sync.repo_id` their `.cce/config` records.
    let existing_manifest = Manifest::load(into).ok();
    let workspace_name = existing_manifest
        .as_ref()
        .map(|m| m.name.clone())
        .or_else(|| {
            into.canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        })
        .unwrap_or_else(|| ".".to_string());
    let mut members: Vec<Member> = existing_manifest.map(|m| m.members).unwrap_or_default();
    let mut used_names: BTreeSet<String> = members.iter().map(|m| m.name.clone()).collect();
    let mut by_repo_id: BTreeMap<String, usize> = BTreeMap::new();
    for (i, m) in members.iter().enumerate() {
        if let Some(id) = SyncConfig::load(&into.join(&m.path)).repo_id {
            by_repo_id.insert(id, i);
        }
    }

    let (mut pulled, mut up_to_date, mut skipped) = (0usize, 0usize, 0usize);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    out.push('\n');
    for r in &listing.repos {
        seen.insert(r.repo_id.clone());
        // #72: an existing member whose `.cce/config` names a `sync.ref`
        // resolves against THAT pointer — the per-member override the multi-ref
        // warning points at — instead of the listing's main-else-fallback sha.
        let member_ref = by_repo_id
            .get(&r.repo_id)
            .and_then(|&i| SyncConfig::load(&into.join(&members[i].path)).git_ref);
        let (sha, noted_ref) = if let Some(name) = &member_ref {
            let pointer = pointer_address(HASH_EMBEDDER, &ver, &r.repo_id, name);
            match remote.read_blob_text(&pointer).ok().filter(|s| !s.is_empty()) {
                Some(sha) => (sha, (name != crate::sync::DEFAULT_REF).then(|| name.clone())),
                None => {
                    out.push_str(&format!(
                        "  warning: skipped {} — no pointer on `{name}` (the member's \
                         `sync.ref`)\n",
                        r.repo_id
                    ));
                    skipped += 1;
                    continue;
                }
            }
        } else if let Some(sha) = &r.latest_sha {
            (sha.clone(), r.latest_ref.clone())
        } else if r.refs.len() > 1 {
            // #72 multi-ref rule: no refs/main and SEVERAL other pointers —
            // skip as before, but NAME the refs so the operator can choose.
            out.push_str(&format!(
                "  warning: skipped {} — no refs/{}; available refs: {} — set `sync.ref` in \
                 the member's .cce/config or pull it with `cce sync pull --latest --ref <name>`\n",
                r.repo_id,
                crate::sync::DEFAULT_REF,
                r.refs.join(", ")
            ));
            skipped += 1;
            continue;
        } else {
            // A repo without a latest pointer (rendered `-` by `sync list`) has
            // nothing `--latest` can resolve: warn, count, continue.
            out.push_str(&format!(
                "  warning: skipped {} — no latest pointer on `{}` (nothing pushed for the ref yet)\n",
                r.repo_id,
                crate::sync::DEFAULT_REF
            ));
            skipped += 1;
            continue;
        };
        // The per-member ref note (#72): empty for main-resolved rows, so the
        // pre-#72 report stays byte-identical.
        let note = noted_ref.map(|n| format!("  (ref {n})")).unwrap_or_default();
        let sha = &sha;
        // The member this repo maps to: the config-recorded repo_id (the stable
        // mapping), else — live-review finding — RE-ADOPT an orphaned member: a
        // manifest entry with this repo's short name whose directory lost its
        // `.cce/config` (e.g. the store dir was deleted by hand). Without this
        // a refresh would create `<name>-2` beside the orphaned `<name>` dir.
        let existing = by_repo_id.get(&r.repo_id).copied().or_else(|| {
            let short = short_member_name(&r.repo_id);
            let orphan = members.iter().position(|m| {
                m.name == short && SyncConfig::load(&into.join(&m.path)).repo_id.is_none()
            });
            if let Some(i) = orphan {
                out.push_str(&format!(
                    "  note: re-adopting existing member `{}` for {} (its config was missing — \
                     rewritten)\n",
                    members[i].name, r.repo_id
                ));
                by_repo_id.insert(r.repo_id.clone(), i);
            }
            orphan
        });
        let name = match existing {
            Some(i) => members[i].name.clone(),
            None => dedup_member_name(&short_member_name(&r.repo_id), &used_names),
        };
        let member_dir = match existing {
            Some(i) => into.join(&members[i].path),
            None => into.join(&name),
        };
        // The member config a later per-member `cce sync pull --latest`/`--commit`
        // needs: the remote and the full repo_id. `lfs: false` keeps every
        // consumer read from writing `.gitattributes` into the cache.
        let member_cfg = SyncConfig {
            remote: Some(listing.remote.clone()),
            lfs: false,
            repo_id: Some(r.repo_id.clone()),
            // #72: a hand-set `sync.ref` survives every refresh rewrite.
            git_ref: member_ref.clone(),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        };

        // Idempotent refresh: `install_artifact` records the installed sha in the
        // `.cce/synced.json` marker; an unmoved latest pointer means no fetch.
        let installed_sha = SyncState::load(&member_dir).map(|s| s.sha);
        if installed_sha.as_deref() == Some(sha.as_str()) {
            member_cfg
                .save(&member_dir)
                .map_err(|e| format!("could not write {} config: {e}", member_dir.display()))?;
            if existing.is_none() {
                used_names.insert(name.clone());
                by_repo_id.insert(r.repo_id.clone(), members.len());
                members.push(Member {
                    name: name.clone(),
                    path: name.clone(),
                    member_type: MemberType::StoreOnly,
                    package: name.clone(),
                });
            }
            out.push_str(&format!("  {name:<16} up-to-date  {}@{sha}{note}\n", r.repo_id));
            up_to_date += 1;
            continue;
        }

        let key = content_address(HASH_EMBEDDER, &ver, &r.repo_id, sha);
        let installed = remote.get(&key).and_then(|bytes| install_artifact(&member_dir, &bytes));
        match installed {
            Ok(artifact) => {
                member_cfg
                    .save(&member_dir)
                    .map_err(|e| format!("could not write {} config: {e}", member_dir.display()))?;
                if existing.is_none() {
                    used_names.insert(name.clone());
                    by_repo_id.insert(r.repo_id.clone(), members.len());
                    members.push(Member {
                        name: name.clone(),
                        path: name.clone(),
                        member_type: MemberType::StoreOnly,
                        package: name.clone(),
                    });
                }
                out.push_str(&format!(
                    "  {name:<16} pulled      {}@{sha}  chunks {}  ({}){note}\n",
                    r.repo_id,
                    artifact.manifest.chunk_count,
                    short_checksum(&artifact.manifest.checksum)
                ));
                pulled += 1;
            }
            Err(e) => {
                out.push_str(&format!("  warning: skipped {} — {e}\n", r.repo_id));
                skipped += 1;
            }
        }
    }

    // Warn — never delete — for members whose repo_id vanished from the cache.
    for m in &members {
        if let Some(id) = SyncConfig::load(&into.join(&m.path)).repo_id {
            if !seen.contains(&id) {
                out.push_str(&format!(
                    "  warning: {} ({id}) is no longer in the cache — left in place\n",
                    m.name
                ));
            }
        }
    }

    // SPEC-SYNC-KNOWLEDGE §7: install the cache's corpus into the consumer ROOT
    // (`<into>/.cce/knowledge/` — where the MCP server loads knowledge from).
    // Best-effort like every other row: a knowledge problem warns, never fails
    // the member pulls.
    pull_all_knowledge(into, &listing, corpus.as_deref(), &mut out);

    // #55: apply the published workspace metadata (the self-describing cache).
    // Each published manifest applies only to its OWN members — the ones whose
    // repo_id is `<base>__<published-name>` — enriching the synthesized entries
    // with the real type/package. Members covered by no manifest keep the #54
    // synthesis. Consumer names/paths never change (the #54 short-name layout is
    // the stable, refresh-idempotent one), so the published graphs' member
    // references are REWRITTEN to the consumer names; the collision rule is
    // therefore #54's: the first taker in repo_id order keeps the bare name, a
    // later workspace's same-named member stays at its `-2`/`-3` name — warned.
    let mut edge_set: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut manifests_applied = 0usize;
    for ws_base in &listing.workspaces {
        let Some((published, graph)) = fetch_published_workspace(&remote, &ver, ws_base) else {
            continue; // unparsable — treated as absent (best-effort, additive)
        };
        manifests_applied += 1;
        // This workspace's mapping: published member name -> consumer member name.
        let mut name_map: BTreeMap<String, String> = BTreeMap::new();
        for pm in &published.members {
            let repo_id = format!("{ws_base}__{}", pm.name);
            let Some(&i) = by_repo_id.get(&repo_id) else { continue };
            members[i].member_type = pm.member_type;
            members[i].package = pm.package.clone();
            name_map.insert(pm.name.clone(), members[i].name.clone());
            if members[i].name != pm.name {
                out.push_str(&format!(
                    "  warning: workspace {ws_base}: member name `{}` was taken by an earlier \
                     repo — kept as `{}` (first in repo_id order wins)\n",
                    pm.name, members[i].name
                ));
            }
        }
        if let Some(g) = graph {
            for e in &g.edges {
                if let (Some(from), Some(to)) = (name_map.get(&e.from), name_map.get(&e.to)) {
                    edge_set.insert((from.clone(), to.clone(), e.via.clone()));
                }
            }
        }
    }

    // Synthesize the workspace manifest + root config. Members sort by path (the
    // manifest's deterministic order); the root config records the remote so a
    // refresh run needs no `--remote`.
    if members.is_empty() {
        out.push_str("\nNothing pullable — no workspace written.\n");
        return Ok(out);
    }
    members.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest { version: 1, name: workspace_name, members };
    manifest.save(into).map_err(|e| format!("could not write workspace manifest: {e}"))?;

    // #55: install the merged cross-member graph when any manifest was applied.
    // With none published this writes nothing — exactly the #54 behaviour.
    if manifests_applied > 0 {
        let graph = crate::workspace::WorkspaceGraph {
            members: manifest.members.iter().map(|m| m.name.clone()).collect(),
            edges: edge_set
                .into_iter()
                .map(|(from, to, via)| crate::workspace::Edge { from, to, via })
                .collect(),
        };
        graph.save(into).map_err(|e| format!("could not write workspace graph: {e}"))?;
        out.push_str(&format!(
            "\nmetadata      : {manifests_applied} published workspace manifest{} applied · {} \
             cross-member edge{} installed",
            if manifests_applied == 1 { "" } else { "s" },
            graph.edges.len(),
            if graph.edges.len() == 1 { "" } else { "s" }
        ));
    }
    let mut root_cfg = std::fs::read_to_string(crate::sync::config::config_path(into))
        .ok()
        .and_then(|t| SyncConfig::from_yaml(&t).ok())
        .unwrap_or(SyncConfig {
            remote: None,
            lfs: false,
            repo_id: None,
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        });
    root_cfg.remote = Some(listing.remote.clone());
    root_cfg.save(into).map_err(|e| format!("could not write root config: {e}"))?;

    out.push_str(&format!(
        "\nworkspace     : {} ({} member{})\n",
        crate::workspace::manifest_path(into).display(),
        manifest.members.len(),
        if manifest.members.len() == 1 { "" } else { "s" }
    ));
    out.push_str(&format!(
        "summary       : {pulled} pulled · {up_to_date} up-to-date · {skipped} skipped\n"
    ));
    Ok(out)
}

/// The `pull --all` knowledge step (SPEC-SYNC-KNOWLEDGE §7). Selection: an
/// explicit `--corpus` wins; a cache carrying exactly ONE corpus installs it;
/// with several and no flag the run warns and skips knowledge, listing the
/// corpus ids so the user can choose (one active corpus per root is the v1
/// invariant — blending is deferred). Every failure is a warning line: knowledge
/// never fails the member pulls. The install itself is `cmd_knowledge_pull`
/// verbatim, so the store bytes and marker are identical to a direct
/// `cce knowledge pull`, and an unmoved remote `current` short-circuits to
/// `up-to-date` with no fetch (the #54 member-refresh rule, via the marker).
fn pull_all_knowledge(into: &Path, listing: &CacheListing, corpus: Option<&str>, out: &mut String) {
    if listing.knowledge.is_empty() {
        return;
    }
    let ids =
        || listing.knowledge.iter().map(|k| k.corpus_id.as_str()).collect::<Vec<_>>().join(", ");
    let selected = match corpus {
        Some(id) => match listing.knowledge.iter().find(|k| k.corpus_id == id) {
            Some(k) => k,
            None => {
                out.push_str(&format!(
                    "  warning: skipped knowledge — corpus `{id}` is not in the cache (it \
                     carries: {})\n",
                    ids()
                ));
                return;
            }
        },
        None if listing.knowledge.len() == 1 => &listing.knowledge[0],
        None => {
            out.push_str(&format!(
                "  warning: skipped knowledge — the cache carries {} corpora ({}); one active \
                 corpus per root — pass --corpus <id> to install one\n",
                listing.knowledge.len(),
                ids()
            ));
            return;
        }
    };
    let Some(current) = selected.current.as_deref() else {
        out.push_str(&format!(
            "  warning: skipped knowledge corpus {} — no `current` pointer on the remote\n",
            selected.corpus_id
        ));
        return;
    };
    // Idempotent refresh: the knowledge sync marker records what was installed;
    // an unmoved `current` means nothing to fetch.
    if let Some(marker) = crate::sync::knowledge_commands::KnowledgeSyncState::load(into) {
        if marker.corpus_id == selected.corpus_id && marker.snapshot == current {
            out.push_str(&format!(
                "  {:<16} up-to-date  {}@{current}\n",
                "knowledge", selected.corpus_id
            ));
            return;
        }
    }
    match crate::sync::knowledge_commands::cmd_knowledge_pull(
        into,
        Some(selected.corpus_id.to_string()),
        None,
        false,
        Some(listing.remote.clone()),
    ) {
        Ok(_) => {
            let snapshot = crate::sync::knowledge_commands::KnowledgeSyncState::load(into)
                .map(|s| s.snapshot)
                .unwrap_or_else(|| current.to_string());
            out.push_str(&format!(
                "  {:<16} pulled      {}@{snapshot}  → .cce/knowledge/\n",
                "knowledge", selected.corpus_id
            ));
        }
        Err(e) => {
            out.push_str(&format!(
                "  warning: skipped knowledge corpus {} — {e}\n",
                selected.corpus_id
            ));
        }
    }
}

/// `cce sync verify` (SPEC-SYNC §5): re-index locally and confirm the pulled
/// artifact's checksum by rebuilding it byte-for-byte.
pub fn cmd_verify(root: &Path, commit: Option<String>) -> Result<String, String> {
    let cfg = SyncConfig::load(root);
    let repo_id = resolve_repo_id(root, &cfg)?;

    // The expected checksum: from the local sync marker, else fetched from the remote.
    let sha = match &commit {
        Some(s) => s.clone(),
        None => match SyncState::load(root) {
            Some(state) => state.sha,
            None => git::head_sha(root)
                .ok_or_else(|| "nothing to verify: no pulled cache and no HEAD sha".to_string())?,
        },
    };

    let expected = match SyncState::load(root) {
        Some(state) if state.sha == sha => state.checksum,
        _ => {
            // Fetch the artifact from the remote to learn its checksum.
            let remote = open_remote(&cfg)?;
            let ver = SYNC_FORMAT_VERSION.to_string();
            let key = content_address(HASH_EMBEDDER, &ver, &repo_id, &sha);
            let bytes = remote.get(&key)?;
            Artifact::from_bytes(&bytes)?.manifest.checksum
        }
    };

    // Rebuild locally from the working tree and export at the same identity.
    let (index, _) = Index::build_protected(root, &HashEmbedder, |_| true, true)?;
    let meta = ManifestMeta { repo_id: repo_id.clone(), sha: sha.clone() };
    let rebuilt = Artifact::from_index(&index, meta);
    let actual = rebuilt.manifest.checksum;

    if actual == expected {
        Ok(format!("verify OK: {repo_id}@{sha}\n  checksum : {actual}\n"))
    } else {
        Err(format!(
            "verify FAILED for {repo_id}@{sha}\n  expected : {expected}\n  rebuilt  : {actual}\n\
             The pulled cache does not match a local rebuild (working tree differs from the sha, \
             or the cache is not trustworthy)."
        ))
    }
}

/// One pulled store's checksum-only outcome (#55): verified against the
/// recorded install hash, or no hash recorded (a marker written by an older
/// cce) — the latter is a clear re-pull notice, never a false failure.
pub(crate) enum ChecksumVerify {
    /// The on-disk bytes match the recorded install hash (carried for the report).
    Ok(SyncState, String),
    /// The marker predates `installed_sha256`; verification is unavailable
    /// until a re-pull records it.
    NoRecord(SyncState),
}

/// Verify one pulled store by re-hash alone (#55): SHA-256 the on-disk
/// `index.json` bytes and compare against the `installed_sha256` the pull
/// recorded in `.cce/synced.json`. **No export path is involved**, so the check
/// is version-independent — an artifact pushed by ANY older cce version
/// verifies against what this machine's pull actually wrote ("has this file
/// changed since pull"), never against a byte shape the current code would
/// produce. `label` names the store in messages (a member name, or the repo_id
/// for a single pull).
pub(crate) fn verify_store_checksum(dir: &Path, label: &str) -> Result<ChecksumVerify, String> {
    let state = SyncState::load(dir).ok_or_else(|| {
        format!("nothing to verify for {label}: no `.cce/synced.json` marker (not a pulled store)")
    })?;
    let Some(expected) = state.installed_sha256.clone() else {
        return Ok(ChecksumVerify::NoRecord(state));
    };
    let store = default_store_path(dir);
    let bytes = std::fs::read(&store).map_err(|e| {
        format!(
            "verify FAILED (checksum-only) for {label} ({}@{})\n  the pulled store could not \
             be read: {e}\n  Re-pull it with `cce sync pull --force`.",
            state.repo_id, state.sha
        )
    })?;
    let actual = hex_lower(&Sha256::digest(&bytes));
    if actual != expected {
        return Err(format!(
            "verify FAILED (checksum-only) for {label} ({}@{})\n  expected : {expected}\n  \
             actual   : {actual}\n  The pulled store does not match the bytes recorded at pull \
             time — corruption or local modification of the store. Re-pull with `cce sync pull \
             --force`.\n  (Checksum-only detects corruption, not a malicious build — true \
             `artifact == build(sha)` verification needs the source and stays with the \
             source-holders/CI: `cce sync verify`.)",
            state.repo_id, state.sha
        ));
    }
    Ok(ChecksumVerify::Ok(state, actual))
}

/// The re-pull notice for a marker without an install hash (written by an
/// older cce). A notice, not a failure: the store is not known-bad, it is
/// unverifiable until a re-pull records the hash.
pub(crate) const NO_RECORD_NOTICE: &str =
    "no install checksum recorded (pulled by an older cce) — re-pull with `cce sync pull \
     --force` to enable checksum verification";

/// The knowledge analogue of [`NO_RECORD_NOTICE`] (SPEC-SYNC-KNOWLEDGE §7): a
/// knowledge sync marker without `installed_sha256` is the same explicit
/// notice + exit 0, never a false failure.
pub(crate) const KNOWLEDGE_NO_RECORD_NOTICE: &str =
    "no install checksum recorded (pulled by an older cce) — re-pull with `cce knowledge pull` \
     to enable checksum verification";

/// One pulled knowledge store's checksum-only outcome (SPEC-SYNC-KNOWLEDGE §7)
/// — the knowledge sibling of [`ChecksumVerify`].
pub(crate) enum KnowledgeChecksumVerify {
    /// The on-disk snapshot bytes match the recorded install hash.
    Ok(crate::sync::knowledge_commands::KnowledgeSyncState, String),
    /// The marker predates `installed_sha256`; unverifiable until a re-pull.
    NoRecord(crate::sync::knowledge_commands::KnowledgeSyncState),
}

/// Verify the root's pulled knowledge store by re-hash alone (SPEC-SYNC-KNOWLEDGE
/// §7): SHA-256 the on-disk `.cce/knowledge/<snapshot>.json` bytes and compare
/// against the `installed_sha256` the pull recorded in the knowledge sync
/// marker — the exact #55 mechanism, version-independent by construction.
/// Returns `None` when the root carries no marker at all (a knowledge-free or
/// local-ingest-only root verifies exactly as today — no knowledge row, no
/// error). A mismatch names the corpus and carries the sharpened §4.2 caveat:
/// knowledge has NO full-`verify` escalation path at all.
pub(crate) fn verify_knowledge_checksum(
    root: &Path,
) -> Option<Result<KnowledgeChecksumVerify, String>> {
    let state = crate::sync::knowledge_commands::KnowledgeSyncState::load(root)?;
    let Some(expected) = state.installed_sha256.clone() else {
        return Some(Ok(KnowledgeChecksumVerify::NoRecord(state)));
    };
    let store = crate::knowledge::store::KnowledgeStore::snapshot_path(root, &state.snapshot);
    let bytes = match std::fs::read(&store) {
        Ok(b) => b,
        Err(e) => {
            return Some(Err(format!(
                "verify FAILED (checksum-only) for knowledge corpus `{}` (@{})\n  the pulled \
                 knowledge store could not be read: {e}\n  Re-pull it with `cce knowledge pull \
                 --corpus {}`.",
                state.corpus_id, state.snapshot, state.corpus_id
            )))
        }
    };
    let actual = hex_lower(&Sha256::digest(&bytes));
    if actual != expected {
        return Some(Err(format!(
            "verify FAILED (checksum-only) for knowledge corpus `{}` (@{})\n  expected : \
             {expected}\n  actual   : {actual}\n  The pulled knowledge store does not match the \
             bytes recorded at pull time — corruption or local modification. Re-pull with `cce \
             knowledge pull --corpus {}`.\n  (Checksum-only detects corruption, not a malicious \
             build — and a knowledge corpus has NO rebuild-verify escalation path at all: the \
             puller lacks the source feed. Trust stays with the pusher and the git host's \
             access control.)",
            state.corpus_id, state.snapshot, state.corpus_id
        )));
    }
    Some(Ok(KnowledgeChecksumVerify::Ok(state, actual)))
}

/// `cce sync verify --checksum-only` (#55): re-hash the PULLED store's on-disk
/// bytes against the SHA-256 **recorded from the installed bytes at pull time**
/// (`installed_sha256` in `.cce/synced.json`) — zero source checkout, zero
/// rebuild, zero remote access, so it works for repo-less consumers, where full
/// `verify` (which rebuilds from the working tree) inherently cannot.
///
/// **Version-independent by construction:** the baseline is hashed from the
/// exact file the pull wrote, so artifacts pushed by older cce versions verify
/// exactly like current ones. (An earlier design re-exported the installed
/// index with the CURRENT code and compared against the artifact-manifest
/// checksum computed at PUSH time — any byte-level difference between the two
/// versions' export shapes false-failed intact pulls. Live-verified against a
/// mixed-version cache.)
///
/// **Old markers:** a `.cce/synced.json` written before `installed_sha256`
/// existed cannot be verified; that is reported as an explicit *notice* with
/// **exit 0** — the store is not known-bad, it is unverifiable until a re-pull
/// records the hash. Only a real mismatch (or unreadable store) is a non-zero
/// failure, naming the member.
///
/// **Honest caveat (documented):** this detects *corruption, not a malicious
/// build*. `artifact == build(sha)` verification requires the source and stays
/// with source-holders (CI); the repo-less trust posture is CI-as-canonical-
/// pusher plus the git host's access control.
///
/// Workspace-aware: with a workspace manifest at `root`, every member's pulled
/// store is verified and a failure names the member; without one, the root
/// store is verified. Exit codes and message shapes mirror full `verify`.
pub fn cmd_verify_checksum_only(root: &Path) -> Result<String, String> {
    // SPEC-SYNC-KNOWLEDGE §7: when the verified root carries a knowledge sync
    // marker, the report gains a knowledge row — same pass/fail/notice
    // semantics as members. A hard mismatch propagates here (non-zero, naming
    // the corpus); a root without a marker gains nothing.
    let knowledge = verify_knowledge_checksum(root).transpose()?;
    let knowledge_row = |k: &KnowledgeChecksumVerify| match k {
        KnowledgeChecksumVerify::Ok(state, checksum) => format!(
            "  {:<16} {}@{}  ({})\n",
            "knowledge",
            state.corpus_id,
            state.snapshot,
            short_checksum(checksum)
        ),
        KnowledgeChecksumVerify::NoRecord(_) => {
            format!("  {:<16} {KNOWLEDGE_NO_RECORD_NOTICE}\n", "knowledge")
        }
    };
    match Manifest::load(root) {
        Ok(manifest) => {
            let mut out = String::new();
            let (mut verified, mut unrecorded) = (0usize, 0usize);
            for m in &manifest.members {
                let label = format!("member `{}`", m.name);
                match verify_store_checksum(&root.join(&m.path), &label)? {
                    ChecksumVerify::Ok(state, checksum) => {
                        out.push_str(&format!(
                            "  {:<16} {}@{}  ({})\n",
                            m.name,
                            state.repo_id,
                            state.sha,
                            short_checksum(&checksum)
                        ));
                        verified += 1;
                    }
                    ChecksumVerify::NoRecord(_) => {
                        out.push_str(&format!("  {:<16} {NO_RECORD_NOTICE}\n", m.name));
                        unrecorded += 1;
                    }
                }
            }
            if let Some(k) = &knowledge {
                out.push_str(&knowledge_row(k));
                if matches!(k, KnowledgeChecksumVerify::NoRecord(_)) {
                    unrecorded += 1;
                }
            }
            let header = if unrecorded == 0 {
                format!(
                    "verify OK (checksum-only): {verified} member{}",
                    if verified == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "verify OK (checksum-only): {verified} member{} verified · {unrecorded} \
                     without a recorded install checksum (re-pull to enable)",
                    if verified == 1 { "" } else { "s" }
                )
            };
            Ok(format!("{header}\n{out}"))
        }
        Err(_) => {
            // A root with ONLY a pulled knowledge corpus (no code store, no
            // manifest) still verifies its knowledge (§7); with neither marker
            // the original clear error stands.
            if SyncState::load(root).is_none() {
                if let Some(k) = &knowledge {
                    return Ok(match k {
                        KnowledgeChecksumVerify::Ok(state, checksum) => format!(
                            "verify OK (checksum-only): knowledge corpus {}@{}\n  checksum : \
                             {checksum}\n",
                            state.corpus_id, state.snapshot
                        ),
                        KnowledgeChecksumVerify::NoRecord(state) => format!(
                            "verify (checksum-only): knowledge corpus {}@{}: \
                             {KNOWLEDGE_NO_RECORD_NOTICE}\n",
                            state.corpus_id, state.snapshot
                        ),
                    });
                }
            }
            let mut out = match verify_store_checksum(root, "this store")? {
                ChecksumVerify::Ok(state, checksum) => format!(
                    "verify OK (checksum-only): {}@{}\n  checksum : {checksum}\n",
                    state.repo_id, state.sha
                ),
                ChecksumVerify::NoRecord(state) => format!(
                    "verify (checksum-only): {}@{}: {NO_RECORD_NOTICE}\n",
                    state.repo_id, state.sha
                ),
            };
            if let Some(k) = &knowledge {
                out.push_str(&knowledge_row(k));
            }
            Ok(out)
        }
    }
}

/// Where the local index came from (SPEC-MCP: `index_status` freshness).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexSource {
    /// Built locally by `cce index` — no sync marker present.
    Local,
    /// Installed by `cce sync pull` — a `.cce/synced.json` marker is present.
    Pulled,
}

/// A read-only freshness summary of the local index (SPEC-MCP §"Freshness is
/// observable"). Pure of side effects; the remote lookup is best-effort and
/// offline-safe (any error leaves `remote_latest = None`, `behind_remote = false`).
#[derive(Debug, Clone)]
pub struct Freshness {
    /// Local vs pulled.
    pub source: IndexSource,
    /// The pulled sha, when the index came from the cache.
    pub sha: Option<String>,
    /// The remote's latest pushed sha for the default ref, if reachable.
    pub remote_latest: Option<String>,
    /// True only when both shas are known and differ (the local index is stale).
    pub behind_remote: bool,
}

/// Summarise the local index's freshness for `root` (SPEC-MCP). Reads the sync
/// marker for source/sha, then best-effort resolves the remote's latest sha to
/// decide "behind remote." With no remote configured this touches no network and
/// reports `Local`. Never errors — MCP's `index_status` must always answer.
pub fn freshness(root: &Path) -> Freshness {
    let state = SyncState::load(root);
    let (source, sha) = match &state {
        Some(s) => (IndexSource::Pulled, Some(s.sha.clone())),
        None => (IndexSource::Local, None),
    };

    let cfg = SyncConfig::load(root);
    let remote_latest = if cfg.remote.is_some() {
        resolve_repo_id(root, &cfg).ok().and_then(|repo_id| {
            open_remote(&cfg).ok().and_then(|remote| {
                let ver = SYNC_FORMAT_VERSION.to_string();
                let pointer =
                    pointer_address(HASH_EMBEDDER, &ver, &repo_id, crate::sync::DEFAULT_REF);
                remote.get(&pointer).ok().map(|b| String::from_utf8_lossy(&b).trim().to_string())
            })
        })
    } else {
        None
    };

    let behind_remote = match (&sha, &remote_latest) {
        (Some(local), Some(remote)) => local != remote,
        _ => false,
    };

    Freshness { source, sha, remote_latest, behind_remote }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::remote::GitRemote;

    /// A source repo with committed content, on branch `main`.
    fn source_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run_commit(d, &["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(d.join("auth.py"), "def login(u):\n    return hash(u)\n").unwrap();
        std::fs::write(d.join("app.py"), "import auth\n\ndef run():\n    return auth.login('x')\n")
            .unwrap();
        git::run_commit(d, &["add", "-A"]).unwrap();
        git::run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    fn bare_remote() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        git::run_commit(tmp.path(), &["init", "--bare", "-q", "-b", "main"]).unwrap();
        let url = format!("file://{}", tmp.path().to_string_lossy());
        (tmp, url)
    }

    #[allow(dead_code)]
    struct HomeGuard {
        home: tempfile::TempDir,
        lock: std::sync::MutexGuard<'static, ()>,
    }
    /// Point CCE_HOME at a temp dir under the process-wide env lock.
    fn set_home() -> HomeGuard {
        let lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        HomeGuard { home, lock }
    }

    fn init_cfg(root: &Path, url: &str) {
        SyncConfig {
            remote: Some(url.to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(root)
        .unwrap();
    }

    #[test]
    fn push_then_pull_round_trip_is_functionally_identical() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);

        // Push from the source repo.
        let report = cmd_push(src.path(), None, false).unwrap();
        assert!(report.contains("Pushed example.com__acme__demo@"));

        // A fresh consumer checkout (same sha) pulls it.
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        let out = cmd_pull(dst.path(), PullTarget::Head, false, false, None).unwrap();
        assert!(out.contains("Pulled example.com__acme__demo@"));
        assert!(out.contains("matches — pulled index used as-is"), "got: {out}");

        // The pulled store is byte-identical to a fresh local hash index of src.
        let (local, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let pulled = Index::load(&default_store_path(dst.path())).unwrap();
        assert_eq!(pulled.chunks.len(), local.chunks.len());
        assert_eq!(pulled.file_imports, local.file_imports);
        std::env::remove_var("CCE_HOME");
    }

    /// Clone the source repo's committed tree into a new dir at the same sha.
    fn source_repo_clone(src: &tempfile::TempDir) -> tempfile::TempDir {
        let dst = tempfile::tempdir().unwrap();
        let src_url = format!("file://{}", src.path().to_string_lossy());
        git::run_commit(Path::new("."), &["clone", "-q", &src_url, &dst.path().to_string_lossy()])
            .unwrap();
        dst
    }

    #[test]
    fn pull_latest_uses_the_ref_pointer() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        let out = cmd_pull(dst.path(), PullTarget::Latest, false, false, None).unwrap();
        assert!(out.contains(&sha), "latest should resolve to {sha}: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_matches_after_pull() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();

        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false, None).unwrap();
        let out = cmd_verify(dst.path(), None).unwrap();
        assert!(out.contains("verify OK"), "got: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_refuses_dirty_tree() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // Make a real (non-.cce) change.
        std::fs::write(src.path().join("auth.py"), "def login(u):\n    return 0\n").unwrap();
        let err = cmd_push(src.path(), None, false).unwrap_err();
        assert!(err.contains("working tree is dirty"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_refuses_non_hash_index() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // Plant an ollama-embedder store so ensure_hash_index refuses.
        let (mut idx, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        idx.embedder_name = "ollama".to_string();
        idx.save(&default_store_path(src.path())).unwrap();
        let err = cmd_push(src.path(), None, false).unwrap_err();
        assert!(err.contains("refusing to push a non-hash index"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn pull_refuses_overwriting_a_different_sha_without_force() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false, None).unwrap();

        // Now pretend to pull a different sha: should refuse without --force.
        let err =
            cmd_pull(dst.path(), PullTarget::Commit("deadbeef".to_string()), false, false, None)
                .unwrap_err();
        assert!(err.contains("--force to overwrite"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn no_remote_configured_errors_clearly() {
        let _home = set_home();
        let work = tempfile::tempdir().unwrap();
        let err = cmd_push(work.path(), None, false).unwrap_err();
        assert!(err.contains("no sync remote configured"), "got: {err}");
        // Status with no remote is not an error — it reports the local-only state.
        let s = cmd_status(work.path()).unwrap();
        assert!(s.contains("pure local CCE"), "got: {s}");
    }

    #[test]
    fn cache_miss_on_pull_reports_clearly() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        // No push happened, so the artifact is absent.
        let err = cmd_pull(src.path(), PullTarget::Head, false, false, None).unwrap_err();
        assert!(err.contains("cache miss"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_remote_and_local_cache() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false, None).unwrap();
        let s = cmd_status(dst.path()).unwrap();
        assert!(s.contains("remote        : file://"));
        assert!(s.contains("local cache   :"));
        assert!(s.contains("remote latest :"));
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn resolve_repo_id_rejects_path_traversal_ids() {
        // #141: a config `sync.repo_id` (or `--repo-id` override) of `.`/`..`/an
        // embedded separator flowed verbatim into content/pointer addresses,
        // escaping the repo namespace. It must be refused at the resolve
        // chokepoint every command inherits.
        let work = tempfile::tempdir().unwrap();
        let cfg_with = |id: &str| SyncConfig {
            remote: Some("file:///x".into()),
            lfs: false,
            repo_id: Some(id.to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        };
        for bad in ["..", ".", "a/b"] {
            let err = resolve_repo_id(work.path(), &cfg_with(bad)).unwrap_err();
            assert!(err.contains("invalid repo_id"), "`{bad}` got: {err}");
        }
        assert_eq!(
            resolve_repo_id(work.path(), &cfg_with("example.com__acme__demo")).unwrap(),
            "example.com__acme__demo"
        );
    }

    #[test]
    fn status_renders_a_short_checksum_marker_without_panicking() {
        // #134: `.cce/synced.json` can carry a checksum shorter than 12 bytes
        // (an older/sibling engine such as cce-ruby, or a hand-edit). `cce sync
        // status` — the very command run to inspect a suspect marker — used to
        // panic with "byte index 12 is out of bounds" on `&state.checksum[..12]`.
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let work = tempfile::tempdir().unwrap();
        init_cfg(work.path(), &url);
        std::fs::write(
            work.path().join(".cce").join("synced.json"),
            "{\"repo_id\":\"example.com__acme__demo\",\"sha\":\"abc123\",\"checksum\":\"short\"}",
        )
        .unwrap();
        let s = cmd_status(work.path()).unwrap();
        assert!(s.contains("local cache   : abc123 (short)"), "got: {s}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn init_writes_config_and_clone() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        let out =
            cmd_init(src.path(), &url, false, Some("example.com__acme__demo".to_string())).unwrap();
        assert!(out.contains("Configured sync remote"));
        let cfg = SyncConfig::load(src.path());
        assert_eq!(cfg.remote.as_deref(), Some(url.as_str()));
        assert_eq!(cfg.repo_id.as_deref(), Some("example.com__acme__demo"));
        assert!(GitRemote::clone_dir(&url).join(".git").is_dir());
        std::env::remove_var("CCE_HOME");
    }

    /// A git workspace with two detectable JS members, committed on `main`.
    /// `alpha` declares a dependency on `beta` (one cross-member edge, #55).
    fn workspace_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run_commit(d, &["init", "-q", "-b", "main"]).unwrap();
        for name in ["alpha", "beta"] {
            let m = d.join(name);
            std::fs::create_dir_all(m.join("src")).unwrap();
            let pkg = if name == "alpha" {
                format!("{{\"name\":\"{name}\",\"dependencies\":{{\"beta\":\"1.0.0\"}}}}")
            } else {
                format!("{{\"name\":\"{name}\"}}")
            };
            std::fs::write(m.join("package.json"), pkg).unwrap();
            std::fs::write(m.join("src/index.js"), format!("function {name}() {{ return 1; }}\n"))
                .unwrap();
        }
        // Write the workspace manifest and commit everything.
        let manifest = crate::workspace::build_manifest(d);
        manifest.save(d).unwrap();
        git::run_commit(d, &["add", "-A"]).unwrap();
        git::run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    #[test]
    fn workspace_push_then_pull_over_members() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        // Config with a base repo_id; members are keyed <base>__<member>.
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();

        let report = cmd_push(src.path(), None, true).unwrap();
        assert!(report.contains("Pushing workspace"));
        assert!(report.contains("alpha"));
        assert!(report.contains("beta"));

        // Clone the whole workspace and pull every member.
        let dst = source_repo_clone(&src);
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(dst.path())
        .unwrap();
        let out = cmd_pull(dst.path(), PullTarget::Head, false, true, None).unwrap();
        assert!(out.contains("Pulling workspace"));
        // Each member now has its own store.
        assert!(dst.path().join("alpha/.cce/index.json").exists());
        assert!(dst.path().join("beta/.cce/index.json").exists());
        std::env::remove_var("CCE_HOME");
    }

    /// #55 Part 2: `pull --workspace` installs the published metadata into the
    /// root `.cce/` — the graph verbatim (names resolve against the manifest)
    /// and the manifest's member types/packages merged over the local one.
    #[test]
    fn workspace_pull_installs_published_manifest_and_graph() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        let cfg = SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        };
        cfg.save(src.path()).unwrap();
        cmd_push(src.path(), None, true).unwrap();

        let dst = source_repo_clone(&src);
        cfg.save(dst.path()).unwrap();
        let out = cmd_pull(dst.path(), PullTarget::Head, false, true, None).unwrap();
        assert!(
            out.contains("workspace.yml + workspace-graph.json installed (1 cross-member edge)"),
            "got: {out}"
        );
        // The installed graph is the published one — cross-member expansion has
        // its alpha -> beta edge without any source derivation.
        let manifest = Manifest::load(dst.path()).unwrap();
        let graph = crate::workspace::WorkspaceGraph::load_or_empty(dst.path(), &manifest);
        assert_eq!(graph.targets_from("alpha"), vec!["beta"]);
        // The merged manifest carries the real (published) types/packages.
        assert_eq!(manifest.member("alpha").unwrap().member_type, MemberType::Javascript);
        assert_eq!(manifest.member("alpha").unwrap().package, "alpha");
        std::env::remove_var("CCE_HOME");
    }

    /// #55 Part 2: a REPO-LESS `pull --workspace --latest` — a bare directory
    /// with only a `.cce/config` — bootstraps the layout from the published
    /// manifest (its member paths become the consumer directories).
    #[test]
    fn workspace_pull_bootstraps_repo_less_from_the_published_manifest() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        let cfg = SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        };
        cfg.save(src.path()).unwrap();
        cmd_push(src.path(), None, true).unwrap();

        // A bare consumer dir: no git checkout, no manifest — only the config.
        let bare_dir = tempfile::tempdir().unwrap();
        cfg.save(bare_dir.path()).unwrap();
        let out = cmd_pull(bare_dir.path(), PullTarget::Latest, false, true, None).unwrap();
        assert!(
            out.contains("manifest         installed from the published metadata"),
            "got: {out}"
        );
        assert!(bare_dir.path().join("alpha/.cce/index.json").exists());
        assert!(bare_dir.path().join("beta/.cce/index.json").exists());
        let manifest = Manifest::load(bare_dir.path()).unwrap();
        let graph = crate::workspace::WorkspaceGraph::load_or_empty(bare_dir.path(), &manifest);
        assert_eq!(graph.targets_from("alpha"), vec!["beta"]);
        std::env::remove_var("CCE_HOME");
    }

    /// #55 additivity: against a cache with NO published metadata (seeded the
    /// pre-#55 way), `pull --workspace` behaves exactly as before — no metadata
    /// lines, no graph file, and a missing local manifest is still the original
    /// clear error.
    #[test]
    fn workspace_pull_without_published_metadata_is_the_pre_55_behaviour() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        let cfg = SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        };
        cfg.save(src.path()).unwrap();
        // Seed the cache the pre-#55 way: member artifacts + pointers only.
        let sha = git::head_sha(src.path()).unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        let remote = GitRemote::open(&url, false).unwrap();
        for m in &manifest.members {
            let repo_id = format!("example.com__acme__mono__{}", m.name);
            push_one(&src.path().join(&m.path), &remote, &repo_id, &sha).unwrap();
        }

        let dst = source_repo_clone(&src);
        cfg.save(dst.path()).unwrap();
        let out = cmd_pull(dst.path(), PullTarget::Head, false, true, None).unwrap();
        assert!(!out.contains("metadata"), "got: {out}");
        assert!(!crate::workspace::graph_path(dst.path()).exists());

        // Repo-less with no manifest and no published metadata: the original error.
        let bare_dir = tempfile::tempdir().unwrap();
        cfg.save(bare_dir.path()).unwrap();
        let err = cmd_pull(bare_dir.path(), PullTarget::Latest, false, true, None).unwrap_err();
        assert!(err.contains("no workspace manifest"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    /// #55 Part 1: `push --workspace` additionally publishes the canonical
    /// workspace manifest + a freshly derived cross-member graph at the
    /// well-known keys under the **base** repo_id — additively (the member
    /// artifact keys and ref pointers are exactly the pre-#55 ones).
    #[test]
    fn workspace_push_publishes_manifest_and_graph_under_the_base_repo_id() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        let base = "example.com__acme__mono";
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some(base.to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();

        let report = cmd_push(src.path(), None, true).unwrap();
        assert!(report.contains("metadata"), "got: {report}");

        let ver = SYNC_FORMAT_VERSION.to_string();
        let remote = GitRemote::open(&url, false).unwrap();
        // The published manifest is the canonical serialization of the root
        // manifest, byte-for-byte.
        let yaml = remote.get(&workspace_manifest_address(HASH_EMBEDDER, &ver, base)).unwrap();
        let manifest = Manifest::load(src.path()).unwrap();
        assert_eq!(String::from_utf8(yaml).unwrap(), manifest.to_yaml());
        // The published graph carries the alpha -> beta edge.
        let json = remote.get(&workspace_graph_address(HASH_EMBEDDER, &ver, base)).unwrap();
        let graph =
            crate::workspace::WorkspaceGraph::from_json(&String::from_utf8(json).unwrap()).unwrap();
        assert_eq!(graph.members, vec!["alpha", "beta"]);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, "alpha");
        assert_eq!(graph.edges[0].to, "beta");
        // Additive: the member keys and pointers are exactly the pre-#55 shape.
        let sha = git::head_sha(src.path()).unwrap();
        for m in ["alpha", "beta"] {
            let repo_id = format!("{base}__{m}");
            assert!(remote.has(&content_address(HASH_EMBEDDER, &ver, &repo_id, &sha)).unwrap());
            assert_eq!(
                remote
                    .read_blob_text(&pointer_address(HASH_EMBEDDER, &ver, &repo_id, "main"))
                    .unwrap(),
                sha
            );
        }
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_fails_when_working_tree_differs() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        cmd_pull(dst.path(), PullTarget::Head, false, false, None).unwrap();

        // Mutate a tracked file so the local rebuild no longer matches the cache.
        std::fs::write(dst.path().join("auth.py"), "def login(u):\n    return 999\n").unwrap();
        let err = cmd_verify(dst.path(), None).unwrap_err();
        assert!(err.contains("verify FAILED"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_fetches_checksum_from_remote_when_no_marker() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        // A clean clone with no synced.json: verify must fetch the artifact to learn
        // its checksum, then confirm the local rebuild matches.
        let dst = source_repo_clone(&src);
        init_cfg(dst.path(), &url);
        assert!(!SyncState::path(dst.path()).exists());
        let out = cmd_verify(dst.path(), Some(sha)).unwrap();
        assert!(out.contains("verify OK"), "got: {out}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_unreachable_remote() {
        let _home = set_home();
        let src = source_repo();
        // A configured but non-existent remote: status still succeeds and flags it.
        SyncConfig {
            remote: Some("file:///definitely/not/here.git".to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();
        let s = cmd_status(src.path()).unwrap();
        assert!(s.contains("remote latest : (unreachable"), "got: {s}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn status_reports_unresolved_repo_id() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        // A non-git directory with a remote but no repo_id and no git origin.
        let dir = tempfile::tempdir().unwrap();
        SyncConfig {
            remote: Some(url),
            lfs: false,
            repo_id: None,
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(dir.path())
        .unwrap();
        let s = cmd_status(dir.path()).unwrap();
        assert!(s.contains("repo_id       : (unresolved"), "got: {s}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn push_with_explicit_commit_flag() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let report = cmd_push(src.path(), Some(sha.clone()), false).unwrap();
        assert!(report.contains(&sha), "got: {report}");
        std::env::remove_var("CCE_HOME");
    }

    /// Advance `src` by one commit and return its new (v2) HEAD sha.
    fn commit_v2(src: &tempfile::TempDir) -> String {
        std::fs::write(src.path().join("extra.py"), "GREETING = 'hi'\n").unwrap();
        git::run_commit(src.path(), &["add", "-A"]).unwrap();
        git::run_commit(src.path(), &["commit", "-q", "-m", "v2"]).unwrap();
        git::head_sha(src.path()).unwrap()
    }

    /// #116: `push --commit <sha>` with `sha != HEAD` must be REJECTED. Push builds
    /// the artifact from the working tree (build(HEAD)); publishing it under a
    /// different sha's key would launder build(HEAD) into the v1 slot AND rewind the
    /// ref pointer, poisoning the shared cache. Assert it publishes NOTHING and does
    /// NOT move the ref.
    #[test]
    fn push_rejects_commit_that_is_not_head() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let v1 = git::head_sha(src.path()).unwrap();
        let v2 = commit_v2(&src);
        assert_ne!(v1, v2, "HEAD must have advanced to v2");

        // Clean tree at HEAD=v2, but ask to push the old v1 sha.
        let err = cmd_push(src.path(), Some(v1.clone()), false).unwrap_err();
        assert!(err.contains("does not match HEAD"), "got: {err}");
        assert!(err.contains(&v1) && err.contains(&v2), "error should name both shas: {err}");

        // Nothing was published under the v1 key and no ref pointer was created/moved.
        let ver = SYNC_FORMAT_VERSION.to_string();
        let repo_id = "example.com__acme__demo";
        let remote = GitRemote::open(&url, false).unwrap();
        let key = content_address(HASH_EMBEDDER, &ver, repo_id, &v1);
        assert!(remote.get(&key).is_err(), "must publish NOTHING under the v1 key");
        let pointer = pointer_address(HASH_EMBEDDER, &ver, repo_id, "main");
        assert!(remote.get(&pointer).is_err(), "must NOT create or move the ref pointer");
        std::env::remove_var("CCE_HOME");
    }

    /// #116: a `--commit` value that is not a real commit in the repo (garbage /
    /// nonexistent sha) must be rejected before any build/put.
    #[test]
    fn push_rejects_nonexistent_commit() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);

        let err = cmd_push(src.path(), Some("0".repeat(40)), false).unwrap_err();
        assert!(err.contains("not a valid commit"), "got: {err}");

        let err2 = cmd_push(src.path(), Some("not-a-sha".to_string()), false).unwrap_err();
        assert!(err2.contains("not a valid commit"), "got: {err2}");
        std::env::remove_var("CCE_HOME");
    }

    /// #116 hardening: a leading-dash `--commit` value (e.g. `--output=x`) must be
    /// rejected as an invalid commit, never resolved or smuggled into git as a flag.
    #[test]
    fn push_rejects_dash_leading_commit() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let err = cmd_push(src.path(), Some("--output=x".to_string()), false).unwrap_err();
        assert!(err.contains("not a valid commit"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn init_rejects_non_directory() {
        let _home = set_home();
        let err = cmd_init(Path::new("/no/such/dir/here"), "file:///x", false, None).unwrap_err();
        assert!(err.contains("not a directory"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    /// Regression (fix/sync-push-rebuild): push must REBUILD the index from the
    /// working tree and export `build(sha)`, never re-export a pre-existing (stale or
    /// foreign) `.cce/index.json`. We plant an index whose bytes differ from a real
    /// build and assert the *published* artifact's checksum equals a fresh rebuild's,
    /// not the planted file's.
    #[test]
    fn push_rebuilds_and_ignores_a_stale_planted_index() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let repo_id = "example.com__acme__demo";
        let ver = SYNC_FORMAT_VERSION.to_string();

        // The correct artifact checksum: a fresh build of the working tree.
        let (fresh, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let fresh_checksum = Artifact::from_index(
            &fresh,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;

        // Plant a FOREIGN/stale index.json whose bytes differ from a real build
        // (mutate a chunk's content so its content-address checksum differs), but keep
        // it a *hash* index so the non-hash refusal does not short-circuit the push.
        // (The deterministic hash build reproduces `fresh` exactly, so this is a
        // faithful copy with one chunk perturbed.)
        let (mut foreign, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        assert!(!foreign.chunks.is_empty());
        foreign.chunks[0].content.push_str("\n# stale foreign bytes from an older engine\n");
        let planted_checksum = Artifact::from_index(
            &foreign,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;
        assert_ne!(planted_checksum, fresh_checksum, "planted bytes must differ from build(sha)");
        assert_eq!(foreign.embedder_name, HASH_EMBEDDER);
        foreign.save(&default_store_path(src.path())).unwrap();

        // Push: must rebuild from the tree, NOT re-export the planted file.
        let report = cmd_push(src.path(), None, false).unwrap();

        // The PUBLISHED artifact's checksum equals a fresh rebuild, not the planted file.
        let key = content_address(HASH_EMBEDDER, &ver, repo_id, &sha);
        let bytes = GitRemote::open(&url, false).unwrap().get(&key).unwrap();
        let published = Artifact::from_bytes(&bytes).unwrap().manifest.checksum;
        assert_eq!(
            published, fresh_checksum,
            "push must publish build(sha), not the planted index"
        );
        assert_ne!(published, planted_checksum, "push must NOT launder the stale planted bytes");
        assert!(
            report.contains(&fresh_checksum),
            "report should show the rebuilt checksum: {report}"
        );
        std::env::remove_var("CCE_HOME");
    }

    /// Regression: the reproduction from the bug report — a consumer `pull`s a stale
    /// cache (built by an older engine, bytes ≠ build(sha)), then `push`es. Push must
    /// republish `build(sha)`; a fresh consumer that then `pull`s the republished
    /// artifact must `verify` GREEN. Proves pull → push → verify is laundering-proof.
    #[test]
    fn pull_then_push_then_verify_is_green_after_stale_cache() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        let sha = git::head_sha(src.path()).unwrap();
        let repo_id = "example.com__acme__demo";
        let ver = SYNC_FORMAT_VERSION.to_string();
        let key = content_address(HASH_EMBEDDER, &ver, repo_id, &sha);
        let pointer = pointer_address(HASH_EMBEDDER, &ver, repo_id, crate::sync::DEFAULT_REF);

        // The correct build(sha) checksum.
        let (fresh, _) = Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        let correct = Artifact::from_index(
            &fresh,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        )
        .manifest
        .checksum;

        // Seed the remote with a STALE artifact under the sha key (simulating a cache
        // built by an older/sibling engine: bytes that differ from build(sha)). The
        // deterministic hash build reproduces `fresh`; we perturb one chunk.
        let (mut stale_idx, _) =
            Index::build_protected(src.path(), &HashEmbedder, |_| true, true).unwrap();
        stale_idx.chunks[0].content.push_str("\n# stale cache from an older cce version\n");
        let stale_artifact = Artifact::from_index(
            &stale_idx,
            ManifestMeta { repo_id: repo_id.to_string(), sha: sha.clone() },
        );
        let stale_checksum = stale_artifact.manifest.checksum.clone();
        assert_ne!(stale_checksum, correct);
        GitRemote::open(&url, false)
            .unwrap()
            .put_many(&[
                (key.clone(), stale_artifact.to_bytes()),
                (pointer, format!("{sha}\n").into_bytes()),
            ])
            .unwrap();

        // Consumer B pulls the stale cache (installs the stale index + marker)...
        let b = source_repo_clone(&src);
        init_cfg(b.path(), &url);
        let pulled = cmd_pull(b.path(), PullTarget::Head, false, false, None).unwrap();
        assert!(pulled.contains(&stale_checksum), "B pulled the stale cache: {pulled}");
        // ...then B pushes: must REBUILD and republish build(sha), not the stale file.
        cmd_push(b.path(), None, false).unwrap();

        // The remote now holds build(sha), not the stale bytes.
        let republished =
            Artifact::from_bytes(&GitRemote::open(&url, false).unwrap().get(&key).unwrap())
                .unwrap()
                .manifest
                .checksum;
        assert_eq!(republished, correct, "push must republish build(sha)");
        assert_ne!(republished, stale_checksum, "the stale bytes must not survive a push");

        // A fresh consumer C pulls the republished artifact → verify is GREEN.
        let c = source_repo_clone(&src);
        init_cfg(c.path(), &url);
        cmd_pull(c.path(), PullTarget::Head, false, false, None).unwrap();
        let out = cmd_verify(c.path(), None).unwrap();
        assert!(out.contains("verify OK"), "pull→push→verify must be green: {out}");
        std::env::remove_var("CCE_HOME");
    }

    // --- cmd_list / render_list_*: the `cce sync list` contract (#53) ---

    #[test]
    fn cmd_list_aggregates_per_repo_with_latest_pointer_and_junk_skipped() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        // Seed the cache directly: repo `aaa` has two artifacts + a latest pointer
        // and junk entries (the #37 fixture); repo `bbb` has one artifact and NO
        // pointer; a stray top-level blob is not a repo_id.
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("hash/2.3/aaa__one/1111111.cce".to_string(), b"AA\n".to_vec()),
                ("hash/2.3/aaa__one/2222222.cce".to_string(), b"BBBB\n".to_vec()),
                ("hash/2.3/aaa__one/refs/main".to_string(), b"2222222\n".to_vec()),
                ("hash/2.3/aaa__one/README.md".to_string(), b"junk\n".to_vec()),
                ("hash/2.3/aaa__one/no-extension".to_string(), b"junk\n".to_vec()),
                ("hash/2.3/bbb__two/3333333.cce".to_string(), b"C\n".to_vec()),
                ("hash/2.3/stray-blob".to_string(), b"junk\n".to_vec()),
            ])
            .unwrap();

        // A bare directory + --remote: no config, no store, no git checkout.
        let bare_dir = tempfile::tempdir().unwrap();
        let listing = cmd_list(bare_dir.path(), Some(url.clone())).unwrap();
        assert_eq!(listing.remote, url);
        assert_eq!(
            listing.repos,
            vec![
                RepoListing {
                    repo_id: "aaa__one".to_string(),
                    latest_sha: Some("2222222".to_string()),
                    latest_ref: None,
                    refs: vec!["main".to_string()],
                    artifacts: 2,
                    bytes: 8,
                },
                RepoListing {
                    repo_id: "bbb__two".to_string(),
                    latest_sha: None,
                    latest_ref: None,
                    refs: Vec::new(),
                    artifacts: 1,
                    bytes: 2,
                },
            ]
        );
        // Read-only: the local bare directory gained no `.cce/`.
        assert!(!bare_dir.path().join(".cce").exists());
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn cmd_list_uses_the_configured_remote_when_no_override() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        let listing = cmd_list(src.path(), None).unwrap();
        assert_eq!(listing.remote, url);
        assert_eq!(listing.repos.len(), 1);
        assert_eq!(listing.repos[0].repo_id, "example.com__acme__demo");
        // The latest pointer is the same source of truth `pull --latest` reads.
        assert_eq!(listing.repos[0].latest_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(listing.repos[0].artifacts, 1);
        assert!(listing.repos[0].bytes > 0);
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn cmd_list_empty_cache_and_no_remote_cases() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let dir = tempfile::tempdir().unwrap();
        // An empty cache lists zero repos (the CLI renders the friendly message).
        let listing = cmd_list(dir.path(), Some(url)).unwrap();
        assert!(listing.repos.is_empty());
        // No config and no --remote: the friendly guidance, as an error.
        let err = cmd_list(dir.path(), None).unwrap_err();
        assert!(err.contains("no sync remote configured"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn cmd_list_unreachable_remote_errors_clearly() {
        let _home = set_home();
        let dir = tempfile::tempdir().unwrap();
        let err = cmd_list(dir.path(), Some("file:///definitely/not/a/repo/here.git".to_string()))
            .unwrap_err();
        assert!(err.contains("could not clone"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    fn sample_listing() -> CacheListing {
        CacheListing {
            remote: "file:///srv/cache.git".to_string(),
            repos: vec![
                RepoListing {
                    repo_id: "github.com__acme__billing".to_string(),
                    latest_sha: Some("7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f".to_string()),
                    latest_ref: None,
                    refs: Vec::new(),
                    artifacts: 3,
                    bytes: 123456,
                },
                RepoListing {
                    repo_id: "github.com__acme__web".to_string(),
                    latest_sha: None,
                    latest_ref: None,
                    refs: Vec::new(),
                    artifacts: 1,
                    bytes: 2048,
                },
            ],
            workspaces: vec![],
            knowledge: vec![],
        }
    }

    /// Byte-pinned (the #32 discipline, same grammar rules as `results_json`):
    /// pretty-printed, two-space indent, serde_json's alphabetical key order, a
    /// single trailing newline, `latest_sha` present-as-null when absent. Scripts
    /// parse this — a field rename or reshape must fail here first.
    #[test]
    fn render_list_json_is_byte_pinned() {
        let s = render_list_json(&sample_listing());
        let golden = r#"{
  "remote": "file:///srv/cache.git",
  "repos": [
    {
      "artifacts": 3,
      "bytes": 123456,
      "latest_sha": "7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f",
      "repo_id": "github.com__acme__billing"
    },
    {
      "artifacts": 1,
      "bytes": 2048,
      "latest_sha": null,
      "repo_id": "github.com__acme__web"
    }
  ],
  "schema": "cce.synclist/v1"
}
"#;
        assert_eq!(s, golden);
    }

    #[test]
    fn render_list_json_empty_cache_is_byte_pinned() {
        let s = render_list_json(&CacheListing {
            remote: "file:///srv/cache.git".to_string(),
            repos: vec![],
            workspaces: vec![],
            knowledge: vec![],
        });
        let golden = "{\n  \"remote\": \"file:///srv/cache.git\",\n  \"repos\": [],\n  \"schema\": \"cce.synclist/v1\"\n}\n";
        assert_eq!(s, golden);
    }

    #[test]
    fn render_list_human_is_an_aligned_table_with_total() {
        let s = render_list_human(&sample_listing());
        let golden = "\
remote        : file:///srv/cache.git

repo_id                    latest                                    artifacts   bytes
github.com__acme__billing  7b9dec7dcbe86ca35b2b4ddeb8386d0595e3362f          3  123456
github.com__acme__web      -                                                 1    2048

total         : 2 repos, 4 artifacts, 125504 bytes
";
        assert_eq!(s, golden);
    }

    #[test]
    fn render_list_human_empty_cache_is_friendly() {
        let s = render_list_human(&CacheListing {
            remote: "file:///srv/cache.git".to_string(),
            repos: vec![],
            workspaces: vec![],
            knowledge: vec![],
        });
        assert_eq!(
            s,
            "remote        : file:///srv/cache.git\n\nThe cache is empty — nothing has been pushed yet.\n"
        );
    }

    // --- cmd_pull_all: the `cce sync pull --all` contract (#54) ---

    #[test]
    fn short_member_name_and_dedup_follow_the_workspace_convention() {
        assert_eq!(short_member_name("github.com__acme__billing"), "billing");
        assert_eq!(short_member_name("plain-id"), "plain-id");
        assert_eq!(short_member_name("trailing__"), "trailing__");
        let mut used: BTreeSet<String> = BTreeSet::new();
        assert_eq!(dedup_member_name("demo", &used), "demo");
        used.insert("demo".to_string());
        assert_eq!(dedup_member_name("demo", &used), "demo-2");
        used.insert("demo-2".to_string());
        assert_eq!(dedup_member_name("demo", &used), "demo-3");
    }

    #[test]
    fn pull_all_skips_pointerless_repos_pulls_the_rest_and_is_idempotent() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        // One real pushed repo (with a latest pointer)…
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();
        // …and one repo_id with an artifact but NO latest pointer (real caches
        // have these; `sync list` renders them `-`).
        GitRemote::open(&url, false)
            .unwrap()
            .put("hash/2.3/example.com__acme__nolatest/1234567.cce", b"raw\n")
            .unwrap();

        let into = tempfile::tempdir().unwrap();
        let out = cmd_pull_all(into.path(), Some(url.clone()), None).unwrap();
        assert!(
            out.contains("warning: skipped example.com__acme__nolatest — no latest pointer"),
            "got: {out}"
        );
        assert!(
            out.contains(&format!("demo             pulled      example.com__acme__demo@{sha}")),
            "got: {out}"
        );
        assert!(out.contains("summary       : 1 pulled · 0 up-to-date · 1 skipped"), "got: {out}");

        // The synthesized workspace: short-named member dir + store + config +
        // manifest that round-trips through the ordinary parser.
        let member = into.path().join("demo");
        assert!(member.join(".cce/index.json").exists());
        let mc = SyncConfig::load(&member);
        assert_eq!(mc.repo_id.as_deref(), Some("example.com__acme__demo"));
        assert_eq!(mc.remote.as_deref(), Some(url.as_str()));
        assert!(!mc.lfs, "consumer configs must never write LFS attributes into the cache");
        let manifest = Manifest::load(into.path()).unwrap();
        assert_eq!(manifest.members.len(), 1);
        assert_eq!(manifest.members[0].name, "demo");
        assert_eq!(manifest.members[0].path, "demo");
        assert_eq!(manifest.members[0].member_type, MemberType::StoreOnly);
        // The skipped repo left no member and no directory.
        assert!(!into.path().join("nolatest").exists());

        // Second run: nothing moved — everything reports up-to-date, no re-pull.
        let out2 = cmd_pull_all(into.path(), None, None).unwrap();
        assert!(
            out2.contains(&format!("demo             up-to-date  example.com__acme__demo@{sha}")),
            "got: {out2}"
        );
        assert!(
            out2.contains("summary       : 0 pulled · 1 up-to-date · 1 skipped"),
            "got: {out2}"
        );
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn pull_all_warns_but_keeps_members_whose_repo_id_vanished() {
        let _home = set_home();
        let (_bare_a, url_a) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url_a);
        cmd_push(src.path(), None, false).unwrap();

        let into = tempfile::tempdir().unwrap();
        cmd_pull_all(into.path(), Some(url_a), None).unwrap();
        assert!(into.path().join("demo/.cce/index.json").exists());

        // Point the same consumer dir at a different (empty-of-this-repo) cache:
        // the member is warned about, never deleted.
        let (_bare_b, url_b) = bare_remote();
        let src_b = source_repo();
        SyncConfig {
            remote: Some(url_b.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__other".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src_b.path())
        .unwrap();
        cmd_push(src_b.path(), None, false).unwrap();

        let out = cmd_pull_all(into.path(), Some(url_b), None).unwrap();
        assert!(
            out.contains(
                "warning: demo (example.com__acme__demo) is no longer in the cache — left in place"
            ),
            "got: {out}"
        );
        assert!(into.path().join("demo/.cce/index.json").exists(), "vanished members are kept");
        // The new cache's repo joined the same workspace.
        let manifest = Manifest::load(into.path()).unwrap();
        let names: Vec<&str> = manifest.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["demo", "other"]);
        std::env::remove_var("CCE_HOME");
    }

    // --- #55: published-metadata discovery + `pull --all` enrichment ---

    /// The #53 listing, extended (#55): a prefix carrying a `workspace.yml` is
    /// recorded in `workspaces`; a PURE metadata prefix (no artifacts, no
    /// pointer) is hidden from `repos`, so the rendered listing (byte-pinned)
    /// and `pull --all`'s warn-skip behaviour are unchanged by publication.
    #[test]
    fn cmd_list_records_workspace_prefixes_and_hides_pure_metadata_rows() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let remote = GitRemote::open(&url, false).unwrap();
        remote
            .put_many(&[
                ("hash/2.3/acme__mono/workspace.yml".to_string(), b"version: 1\n".to_vec()),
                ("hash/2.3/acme__mono__demo/1111111.cce".to_string(), b"AA\n".to_vec()),
                ("hash/2.3/acme__mono__demo/refs/main".to_string(), b"1111111\n".to_vec()),
            ])
            .unwrap();
        let bare_dir = tempfile::tempdir().unwrap();
        let listing = cmd_list(bare_dir.path(), Some(url)).unwrap();
        assert_eq!(listing.workspaces, vec!["acme__mono".to_string()]);
        // The metadata prefix is not a repo row; the member repo is.
        let ids: Vec<&str> = listing.repos.iter().map(|r| r.repo_id.as_str()).collect();
        assert_eq!(ids, vec!["acme__mono__demo"]);
        std::env::remove_var("CCE_HOME");
    }

    /// #55: `pull --all` applies the published manifest to the members it covers
    /// (real types/packages) and installs the published graph rewritten to the
    /// consumer member names — idempotently across a refresh run.
    #[test]
    fn pull_all_applies_published_metadata_and_installs_the_graph() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();
        cmd_push(src.path(), None, true).unwrap();

        let into = tempfile::tempdir().unwrap();
        let out = cmd_pull_all(into.path(), Some(url.clone()), None).unwrap();
        assert!(
            out.contains(
                "metadata      : 1 published workspace manifest applied · 1 cross-member edge installed"
            ),
            "got: {out}"
        );
        // Enriched members: the real published type/package, consumer layout paths.
        let manifest = Manifest::load(into.path()).unwrap();
        let alpha = manifest.member("alpha").unwrap();
        assert_eq!(alpha.member_type, MemberType::Javascript);
        assert_eq!(alpha.package, "alpha");
        assert_eq!(alpha.path, "alpha", "the #54 consumer layout is kept");
        // The installed graph resolves by consumer member names.
        let graph = crate::workspace::WorkspaceGraph::load_or_empty(into.path(), &manifest);
        assert_eq!(graph.targets_from("alpha"), vec!["beta"]);

        // Refresh run: byte-identical manifest + graph, everything up-to-date.
        let yaml1 = std::fs::read_to_string(crate::workspace::manifest_path(into.path())).unwrap();
        let graph1 = std::fs::read_to_string(crate::workspace::graph_path(into.path())).unwrap();
        let out2 = cmd_pull_all(into.path(), None, None).unwrap();
        assert!(
            out2.contains("summary       : 0 pulled · 2 up-to-date · 0 skipped"),
            "got: {out2}"
        );
        assert_eq!(
            std::fs::read_to_string(crate::workspace::manifest_path(into.path())).unwrap(),
            yaml1
        );
        assert_eq!(
            std::fs::read_to_string(crate::workspace::graph_path(into.path())).unwrap(),
            graph1
        );
        std::env::remove_var("CCE_HOME");
    }

    /// A single-member workspace source repo whose member is named `demo`.
    fn demo_workspace(package: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        git::run_commit(d, &["init", "-q", "-b", "main"]).unwrap();
        let m = d.join("demo");
        std::fs::create_dir_all(m.join("src")).unwrap();
        std::fs::write(m.join("package.json"), format!("{{\"name\":\"{package}\"}}")).unwrap();
        std::fs::write(m.join("src/index.js"), format!("function {package}() {{ return 1; }}\n"))
            .unwrap();
        crate::workspace::build_manifest(d).save(d).unwrap();
        git::run_commit(d, &["add", "-A"]).unwrap();
        git::run_commit(d, &["commit", "-q", "-m", "init"]).unwrap();
        tmp
    }

    /// #55, the documented multi-workspace rule: each published manifest applies
    /// to its OWN members; on a member-NAME collision across manifests the first
    /// (repo_id order) keeps the bare name and the later one stays at its
    /// deduped `-2` name, with a warning.
    #[test]
    fn pull_all_member_name_collision_across_workspaces_first_wins_with_warning() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        for (base, package) in
            [("example.com__acme__mono", "demo_acme"), ("example.com__zeta__mono", "demo_zeta")]
        {
            let src = demo_workspace(package);
            SyncConfig {
                remote: Some(url.clone()),
                lfs: false,
                repo_id: Some(base.to_string()),
                git_ref: None,
                auto_pull: false,
                retention: crate::sync::config::Retention::All,
            }
            .save(src.path())
            .unwrap();
            cmd_push(src.path(), None, true).unwrap();
        }

        let into = tempfile::tempdir().unwrap();
        let out = cmd_pull_all(into.path(), Some(url), None).unwrap();
        assert!(
            out.contains(
                "warning: workspace example.com__zeta__mono: member name `demo` was taken by an \
                 earlier repo — kept as `demo-2` (first in repo_id order wins)"
            ),
            "got: {out}"
        );
        // Each manifest enriched exactly its own member.
        let manifest = Manifest::load(into.path()).unwrap();
        assert_eq!(manifest.member("demo").unwrap().package, "demo_acme");
        assert_eq!(manifest.member("demo-2").unwrap().package, "demo_zeta");
        std::env::remove_var("CCE_HOME");
    }

    // --- #55: `verify --checksum-only` — integrity with zero source checkout ---

    #[test]
    fn verify_checksum_only_passes_on_an_intact_pull_and_fails_on_corruption() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();
        let sha = git::head_sha(src.path()).unwrap();

        // A repo-less consumer: bare dir + config only, `--latest` pull.
        let consumer = tempfile::tempdir().unwrap();
        init_cfg(consumer.path(), &url);
        cmd_pull(consumer.path(), PullTarget::Latest, false, false, None).unwrap();
        let out = cmd_verify_checksum_only(consumer.path()).unwrap();
        assert!(
            out.contains(&format!("verify OK (checksum-only): example.com__acme__demo@{sha}")),
            "got: {out}"
        );

        // Corrupt the pulled store (mutate one chunk's bytes) → loud failure.
        let store = default_store_path(consumer.path());
        let mut idx = Index::load(&store).unwrap();
        idx.chunks[0].content.push_str("\n# flipped bytes\n");
        idx.save(&store).unwrap();
        let err = cmd_verify_checksum_only(consumer.path()).unwrap_err();
        assert!(err.contains("verify FAILED (checksum-only)"), "got: {err}");
        assert!(err.contains("corruption"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_checksum_only_names_the_corrupted_member_in_a_workspace() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = workspace_repo();
        SyncConfig {
            remote: Some(url.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src.path())
        .unwrap();
        cmd_push(src.path(), None, true).unwrap();

        let into = tempfile::tempdir().unwrap();
        cmd_pull_all(into.path(), Some(url), None).unwrap();
        let out = cmd_verify_checksum_only(into.path()).unwrap();
        assert!(out.contains("verify OK (checksum-only): 2 members"), "got: {out}");
        assert!(out.contains("alpha") && out.contains("beta"), "got: {out}");

        // Corrupt exactly beta → the failure names the member.
        let store = default_store_path(&into.path().join("beta"));
        let mut idx = Index::load(&store).unwrap();
        idx.chunks[0].content.push('!');
        idx.save(&store).unwrap();
        let err = cmd_verify_checksum_only(into.path()).unwrap_err();
        assert!(err.contains("verify FAILED (checksum-only) for member `beta`"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    /// #55, the cross-version shape (live-review finding): a `.cce/synced.json`
    /// written by an OLDER cce has no `installed_sha256`. That is a clear,
    /// actionable NOTICE with exit-0 semantics (`Ok`), never a false failure —
    /// the earlier export-based design false-failed intact pulls of artifacts
    /// pushed by older versions.
    #[test]
    fn verify_checksum_only_old_marker_is_a_notice_not_a_false_failure() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();

        let consumer = tempfile::tempdir().unwrap();
        init_cfg(consumer.path(), &url);
        cmd_pull(consumer.path(), PullTarget::Latest, false, false, None).unwrap();
        // Rewrite the marker to the pre-#55 shape (no installed_sha256).
        let mut state = SyncState::load(consumer.path()).unwrap();
        assert!(state.installed_sha256.is_some(), "a fresh pull records the hash");
        state.installed_sha256 = None;
        state.save(consumer.path()).unwrap();

        let out = cmd_verify_checksum_only(consumer.path()).unwrap();
        assert!(
            out.contains("no install checksum recorded (pulled by an older cce)"),
            "got: {out}"
        );
        assert!(out.contains("re-pull"), "the notice must be actionable: {out}");

        // Workspace-mixed: one member verified, one on the notice path (a
        // fresh cache so only the two workspace members are pulled).
        let (_bare_ws, url_ws) = bare_remote();
        let src_ws = workspace_repo();
        SyncConfig {
            remote: Some(url_ws.clone()),
            lfs: false,
            repo_id: Some("example.com__acme__mono".to_string()),
            git_ref: None,
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(src_ws.path())
        .unwrap();
        cmd_push(src_ws.path(), None, true).unwrap();
        let into = tempfile::tempdir().unwrap();
        cmd_pull_all(into.path(), Some(url_ws), None).unwrap();
        let beta = into.path().join("beta");
        let mut state = SyncState::load(&beta).unwrap();
        state.installed_sha256 = None;
        state.save(&beta).unwrap();
        let out = cmd_verify_checksum_only(into.path()).unwrap();
        assert!(
            out.contains(
                "verify OK (checksum-only): 1 member verified · 1 without a recorded install \
                 checksum (re-pull to enable)"
            ),
            "got: {out}"
        );
        assert!(out.contains("beta"), "the notice names the member: {out}");
        std::env::remove_var("CCE_HOME");
    }

    /// Live-review finding: a member directory whose `.cce` (config + marker)
    /// was deleted must be RE-ADOPTED by the next `pull --all` — matched by its
    /// short name — not duplicated as `<name>-2` beside the orphan.
    #[test]
    fn pull_all_re_adopts_an_orphaned_member_dir_instead_of_duplicating() {
        let _home = set_home();
        let (_bare, url) = bare_remote();
        let src = source_repo();
        init_cfg(src.path(), &url);
        cmd_push(src.path(), None, false).unwrap();

        let into = tempfile::tempdir().unwrap();
        cmd_pull_all(into.path(), Some(url), None).unwrap();
        assert!(into.path().join("demo/.cce/index.json").exists());
        // Kill the member's whole .cce dir: config, marker, and store gone.
        std::fs::remove_dir_all(into.path().join("demo/.cce")).unwrap();

        let out = cmd_pull_all(into.path(), None, None).unwrap();
        assert!(
            out.contains("note: re-adopting existing member `demo` for example.com__acme__demo"),
            "got: {out}"
        );
        assert!(out.contains("summary       : 1 pulled · 0 up-to-date · 0 skipped"), "got: {out}");
        assert!(into.path().join("demo/.cce/index.json").exists(), "re-pulled into the same dir");
        assert_eq!(
            SyncConfig::load(&into.path().join("demo")).repo_id.as_deref(),
            Some("example.com__acme__demo"),
            "the config is rewritten"
        );
        assert!(!into.path().join("demo-2").exists(), "no duplicate member dir");
        let manifest = Manifest::load(into.path()).unwrap();
        assert_eq!(manifest.members.len(), 1, "no duplicate manifest entry");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn verify_checksum_only_without_a_marker_is_a_clear_error() {
        let _home = set_home();
        let dir = tempfile::tempdir().unwrap();
        let err = cmd_verify_checksum_only(dir.path()).unwrap_err();
        assert!(err.contains("no `.cce/synced.json` marker"), "got: {err}");
        std::env::remove_var("CCE_HOME");
    }

    #[test]
    fn resolve_repo_id_uses_git_origin_when_unconfigured() {
        let _home = set_home();
        let src = source_repo();
        // Give the source repo an origin so repo_id can be derived.
        git::run_commit(
            src.path(),
            &["remote", "add", "origin", "https://github.com/acme/demo.git"],
        )
        .unwrap();
        let cfg = SyncConfig { repo_id: None, ..SyncConfig::default() };
        assert_eq!(resolve_repo_id(src.path(), &cfg).unwrap(), "github.com__acme__demo");
        std::env::remove_var("CCE_HOME");
    }
}
