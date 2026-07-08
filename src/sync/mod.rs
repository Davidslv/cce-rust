//! # sync — CCE Sync: a distributed, offline-first cache for code indexes
//!
//! **Why this file exists:** SPEC-SYNC layers an *optional* content-addressed
//! cache on top of the local-first core: "git remotes for the index." This module
//! root wires the sub-parts together and owns the small, pure identity helpers
//! that every sub-part shares — the ones that MUST be byte-identical across the
//! Ruby and Rust engines (content address, `pack_set_id`, `cce_version`), plus the
//! location of the local working clone.
//!
//! **What it is / does:** Declares the sync sub-modules (`artifact`, `config`,
//! `git`, `remote`, `commands`) and exposes the deterministic key/identity
//! functions. Nothing here touches the network or the filesystem except to resolve
//! the sync home directory from the environment.
//!
//! **Responsibilities:**
//! - Own `SYNC_FORMAT_VERSION`, `pack_set_id`, `normalize_repo_id`,
//!   `content_address`, `pointer_address`, `sync_home`, `remote_slug`.
//! - Guarantee those are pure and deterministic (cross-language identical).
//! - It does NOT export/import artifacts, drive git, or parse config — the
//!   sub-modules do.

pub mod artifact;
pub mod commands;
pub mod config;
pub mod git;
pub mod remote;

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The only shareable embedder id (SPEC-SYNC §1/§3): the deterministic hash
/// embedder. Ollama/semantic indexes are non-reproducible and never pushed.
pub const HASH_EMBEDDER: &str = "hash";

/// The default remote ref a `--latest` pull resolves against (SPEC-SYNC §4/§5).
pub const DEFAULT_REF: &str = "main";

/// Lowercase-hex of a byte slice (shared with the artifact checksum).
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The **sync artifact format version** stamped in the content address and the
/// manifest `cce_version` field (SPEC-SYNC §3 — a format-compatible window).
///
/// This is deliberately **decoupled from the crate/app version**: it names the
/// *artifact format*, not the release. Only bump it when the artifact bytes actually
/// change shape. An additive app release (e.g. v2.4 CCE MCP, which does not touch the
/// artifact format) must NOT move it — otherwise every release invalidates everyone's
/// cache and the two engines' artifacts diverge, breaking the cross-engine
/// byte-identity. Both the Ruby and Rust engines pin the same value.
pub const SYNC_FORMAT_VERSION: &str = "2.3";

/// A deterministic id for the active pack set (SPEC-SYNC §2 manifest, reconciled):
/// the **sorted, comma-joined lowercase pack names** verbatim (e.g.
/// `c,javascript,python,ruby,rust,typescript`). Both engines register the same
/// language packs, so both produce the same string.
pub fn pack_set_id() -> String {
    let registry = crate::packs::default_registry();
    let mut names: Vec<String> =
        registry.all().iter().map(|p| p.name().to_ascii_lowercase()).collect();
    names.sort_unstable();
    names.join(",")
}

/// Normalize a git origin URL (or an already-normalized id) into a filesystem- and
/// path-safe `repo_id`: `host__org__repo` (SPEC-SYNC §3). Handles `https://`,
/// `ssh://`, scp-style `git@host:org/repo.git`, and bare paths. A trailing `.git`
/// is stripped. Characters outside `[A-Za-z0-9._-]` collapse to `_`.
pub fn normalize_repo_id(origin: &str) -> String {
    let s = origin.trim();
    let s = s.strip_suffix(".git").unwrap_or(s);

    // Split into (host, path) across the supported URL shapes.
    let (host, path): (String, String) = if let Some(rest) = s.split_once("://") {
        // scheme://[user@]host/path
        let after = rest.1;
        let (authority, path) = match after.split_once('/') {
            Some((a, p)) => (a, p),
            None => (after, ""),
        };
        let host = authority.rsplit('@').next().unwrap_or(authority);
        (host.to_string(), path.to_string())
    } else if let Some((auth, path)) = s.split_once(':') {
        // scp-style git@host:org/repo  (but not a bare Windows drive / plain path)
        if auth.contains('@') || (!auth.contains('/') && !path.starts_with('/')) {
            let host = auth.rsplit('@').next().unwrap_or(auth);
            (host.to_string(), path.to_string())
        } else {
            (String::new(), s.to_string())
        }
    } else {
        (String::new(), s.to_string())
    };

    let mut parts: Vec<String> = Vec::new();
    if !host.is_empty() {
        parts.push(host);
    }
    for seg in path.split('/') {
        if !seg.is_empty() {
            parts.push(seg.to_string());
        }
    }
    let joined = parts.join("__");
    sanitize_id(&joined)
}

/// Replace every character outside `[A-Za-z0-9._-]` with `_` (keeps the id safe as
/// a path segment on every OS and as a git path).
fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The content address (path within the remote) for an artifact (SPEC-SYNC §3):
/// `<embedder>/<cce_ver>/<repo_id>/<sha>.cce`.
pub fn content_address(embedder: &str, cce_ver: &str, repo_id: &str, sha: &str) -> String {
    format!("{embedder}/{cce_ver}/{repo_id}/{sha}.cce")
}

/// The pointer address for a ref (SPEC-SYNC §4 `latest`): a small file holding the
/// latest sha pushed for `<ref>`: `<embedder>/<cce_ver>/<repo_id>/refs/<ref>`.
pub fn pointer_address(embedder: &str, cce_ver: &str, repo_id: &str, git_ref: &str) -> String {
    format!("{embedder}/{cce_ver}/{repo_id}/refs/{}", sanitize_id(git_ref))
}

/// The published workspace-manifest address (#55, self-describing cache):
/// `<embedder>/<cce_ver>/<base_repo_id>/workspace.yml`. `push --workspace` puts the
/// root `.cce/workspace.yml` bytes here, under the workspace's **base** repo_id — the
/// prefix its members' `<base>__<member>` repo_ids are derived from. **Additive by
/// construction**: the key is neither a `<sha>.cce` artifact nor a `refs/<ref>`
/// pointer, so no existing key or old-client read path can collide with it.
pub fn workspace_manifest_address(embedder: &str, cce_ver: &str, base_repo_id: &str) -> String {
    format!("{embedder}/{cce_ver}/{base_repo_id}/workspace.yml")
}

/// The published workspace-graph address (#55):
/// `<embedder>/<cce_ver>/<base_repo_id>/workspace-graph.json` — the cross-member
/// dependency edges beside the published manifest (same additivity argument).
pub fn workspace_graph_address(embedder: &str, cce_ver: &str, base_repo_id: &str) -> String {
    format!("{embedder}/{cce_ver}/{base_repo_id}/workspace-graph.json")
}

/// The base directory that holds every remote's local working clone. It is
/// `$CCE_HOME/sync` when `CCE_HOME` is set (used by hermetic tests), else
/// `~/.cce/sync` (SPEC-SYNC §4). Falls back to `./.cce/sync` if no home is known.
pub fn sync_home() -> PathBuf {
    if let Ok(dir) = std::env::var("CCE_HOME") {
        return PathBuf::from(dir).join("sync");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cce").join("sync");
    }
    PathBuf::from(".cce").join("sync")
}

/// A stable per-remote slug for the working-clone directory name: the first 16 hex
/// chars of SHA-256 over the remote URL. Deterministic and collision-resistant.
pub fn remote_slug(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    hex_lower(&digest)[..16].to_string()
}

/// Test-only serialization of the process-global `CCE_HOME`/`HOME` env vars, which
/// several sync tests mutate. Cargo runs tests in parallel threads within one
/// process, so a shared lock keeps env reads/writes from racing across modules.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    /// Acquire the process-wide env lock (poison-tolerant).
    pub fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_format_version_is_decoupled_and_stable() {
        // The artifact format version is pinned, NOT derived from the app version
        // (which is 2.4.x). It only moves when the artifact bytes change shape, so an
        // additive release does not invalidate caches or diverge from Ruby.
        assert_eq!(SYNC_FORMAT_VERSION, "2.3");
    }

    #[test]
    fn pack_set_id_is_the_sorted_comma_joined_pack_names() {
        // The canonical reconciled value: sorted, comma-joined, lowercase.
        assert_eq!(pack_set_id(), "c,javascript,python,ruby,rust,typescript");
    }

    #[test]
    fn normalizes_https_origin() {
        assert_eq!(
            normalize_repo_id("https://github.com/acme/billing.git"),
            "github.com__acme__billing"
        );
    }

    #[test]
    fn normalizes_scp_style_origin() {
        assert_eq!(
            normalize_repo_id("git@github.com:acme/billing.git"),
            "github.com__acme__billing"
        );
    }

    #[test]
    fn normalizes_ssh_scheme_origin() {
        assert_eq!(
            normalize_repo_id("ssh://git@github.com/acme/billing"),
            "github.com__acme__billing"
        );
    }

    #[test]
    fn normalizes_bare_path_and_sanitizes() {
        // A bare path (no host) keeps its segments; odd chars collapse to `_`.
        assert_eq!(normalize_repo_id("acme/bill ing"), "acme__bill_ing");
    }

    #[test]
    fn content_and_pointer_addresses() {
        assert_eq!(
            content_address("hash", "2.3", "github.com__acme__billing", "9f1c2a"),
            "hash/2.3/github.com__acme__billing/9f1c2a.cce"
        );
        assert_eq!(
            pointer_address("hash", "2.3", "github.com__acme__billing", "main"),
            "hash/2.3/github.com__acme__billing/refs/main"
        );
    }

    #[test]
    fn remote_slug_is_16_hex_and_stable() {
        let s = remote_slug("file:///tmp/remote.git");
        assert_eq!(s.len(), 16);
        assert_eq!(s, remote_slug("file:///tmp/remote.git"));
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sync_home_honours_cce_home() {
        let _lock = test_support::env_lock();
        std::env::set_var("CCE_HOME", "/tmp/cce-test-home");
        assert_eq!(sync_home(), PathBuf::from("/tmp/cce-test-home/sync"));
        std::env::remove_var("CCE_HOME");
    }
}
