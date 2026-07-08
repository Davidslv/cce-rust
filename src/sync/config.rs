//! # sync::config — the `sync.*` configuration (SPEC-SYNC §8)
//!
//! **Why this file exists:** Sync is opt-in: absent config ⇒ pure local CCE
//! (SPEC-SYNC §9.1). When present, a project records which git remote is its cache,
//! whether to use git-LFS, an optional `repo_id` override, and a retention policy.
//! `cce sync init` writes this; every other sync command reads it. Keeping it in one
//! typed place means the offline-first default is the natural default.
//!
//! **What it is / does:** Reads/writes a small YAML file with a single `sync:` block.
//! The per-project file is `<root>/.cce/config`; a global `~/.cce/config.yml` is a
//! fallback for the remote when a project sets none. All keys are optional.
//!
//! **Responsibilities:**
//! - Own `SyncConfig`/`Retention`, their YAML (de)serialization, and load/save.
//! - Default `lfs = true`, `auto_pull = false`, `retention = all` (SPEC-SYNC §8).
//! - It does NOT drive git or export artifacts.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Cache retention policy (SPEC-SYNC §4/§8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retention {
    /// Keep every sha ever pushed (the default).
    All,
    /// Keep only the most recent `N` shas per repo (older `*.cce` are pruned).
    KeepLast(usize),
}

impl Retention {
    /// Parse the config string form: `all` or `keep-last-<n>`.
    pub fn parse(s: &str) -> Retention {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("keep-last-") {
            if let Ok(n) = rest.parse::<usize>() {
                return Retention::KeepLast(n);
            }
        }
        Retention::All
    }

    /// The config string form.
    pub fn as_str(&self) -> String {
        match self {
            Retention::All => "all".to_string(),
            Retention::KeepLast(n) => format!("keep-last-{n}"),
        }
    }
}

/// The resolved `sync.*` configuration for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    pub remote: Option<String>,
    pub lfs: bool,
    pub repo_id: Option<String>,
    pub auto_pull: bool,
    pub retention: Retention,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            remote: None,
            lfs: true,
            repo_id: None,
            auto_pull: false,
            retention: Retention::All,
        }
    }
}

/// The per-project config path: `<root>/.cce/config`.
pub fn config_path(root: &Path) -> PathBuf {
    root.join(".cce").join("config")
}

/// The global config path: `$CCE_HOME/config.yml` or `~/.cce/config.yml`.
fn global_config_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CCE_HOME") {
        return Some(PathBuf::from(dir).join("config.yml"));
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".cce").join("config.yml"))
}

// --- YAML shapes (reading is tolerant; writing is our canonical form) ---

#[derive(Deserialize)]
struct RawSync {
    remote: Option<String>,
    lfs: Option<bool>,
    repo_id: Option<String>,
    auto_pull: Option<bool>,
    retention: Option<String>,
}

#[derive(Deserialize)]
struct RawRoot {
    sync: Option<RawSync>,
}

impl SyncConfig {
    /// Parse a `SyncConfig` from YAML text. A file with no `sync:` block yields the
    /// default (all-local) config.
    pub fn from_yaml(text: &str) -> Result<SyncConfig, String> {
        let raw: RawRoot =
            serde_yaml::from_str(text).map_err(|e| format!("invalid sync config: {e}"))?;
        let mut cfg = SyncConfig::default();
        if let Some(s) = raw.sync {
            cfg.remote = s.remote.filter(|r| !r.is_empty());
            if let Some(lfs) = s.lfs {
                cfg.lfs = lfs;
            }
            cfg.repo_id = s.repo_id.filter(|r| !r.is_empty());
            if let Some(ap) = s.auto_pull {
                cfg.auto_pull = ap;
            }
            if let Some(r) = s.retention {
                cfg.retention = Retention::parse(&r);
            }
        }
        Ok(cfg)
    }

    /// Serialize to the canonical `sync:` YAML block.
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str("sync:\n");
        match &self.remote {
            Some(r) => s.push_str(&format!("  remote: {}\n", yaml_scalar(r))),
            None => s.push_str("  remote: null\n"),
        }
        s.push_str(&format!("  lfs: {}\n", self.lfs));
        match &self.repo_id {
            Some(r) => s.push_str(&format!("  repo_id: {}\n", yaml_scalar(r))),
            None => s.push_str("  repo_id: null\n"),
        }
        s.push_str(&format!("  auto_pull: {}\n", self.auto_pull));
        s.push_str(&format!("  retention: {}\n", self.retention.as_str()));
        s
    }

    /// Load the config for `root`: the per-project `.cce/config` if it exists, else
    /// the global config, else the default. When the project file exists but sets no
    /// remote, the global remote (if any) is inherited.
    pub fn load(root: &Path) -> SyncConfig {
        let project = std::fs::read_to_string(config_path(root))
            .ok()
            .and_then(|t| SyncConfig::from_yaml(&t).ok());
        let global = global_config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| SyncConfig::from_yaml(&t).ok());

        match (project, global) {
            (Some(mut p), Some(g)) => {
                if p.remote.is_none() {
                    p.remote = g.remote;
                }
                p
            }
            (Some(p), None) => p,
            (None, Some(g)) => g,
            (None, None) => SyncConfig::default(),
        }
    }

    /// Write the config to `<root>/.cce/config`, creating `.cce/`.
    pub fn save(&self, root: &Path) -> std::io::Result<()> {
        let path = config_path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_yaml())
    }
}

/// The resolved `knowledge.sync.*` configuration (SPEC-SYNC-KNOWLEDGE §8). All
/// keys optional; absent ⇒ knowledge sync off, pure local knowledge exactly as
/// today. These keys are written by hand or by the adapter job — there is no
/// `cce knowledge init` (`cce sync init` owns the remote clone setup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeSyncConfig {
    /// The §4.1 corpus identity; required to push (or pass `--corpus`).
    pub corpus_id: Option<String>,
    /// Per-corpus remote override (§4.3); default = the project's `sync.remote`.
    pub remote: Option<String>,
    /// Per-corpus retention (§4.5): `all` | `keep-last-<n>`; default `all`.
    pub retention: Retention,
}

impl Default for KnowledgeSyncConfig {
    fn default() -> Self {
        KnowledgeSyncConfig { corpus_id: None, remote: None, retention: Retention::All }
    }
}

// The `knowledge: sync:` YAML shape. Reading is tolerant: a missing block, or a
// `knowledge:` block without `sync:`, yields the default. Sibling keys the
// runtime `KnowledgeConfig` owns (`enabled`, `min_score`, …) are untouched.
#[derive(Deserialize)]
struct RawKnowledgeSync {
    corpus_id: Option<String>,
    remote: Option<String>,
    retention: Option<String>,
}

#[derive(Deserialize)]
struct RawKnowledgeBlock {
    sync: Option<RawKnowledgeSync>,
}

#[derive(Deserialize)]
struct RawKnowledgeRoot {
    knowledge: Option<RawKnowledgeBlock>,
}

impl KnowledgeSyncConfig {
    /// Parse from `.cce/config` YAML text. Tolerant: no `knowledge:` block, no
    /// `sync:` sub-block, or unparsable YAML all yield the default (sync off).
    pub fn from_yaml(text: &str) -> KnowledgeSyncConfig {
        let mut cfg = KnowledgeSyncConfig::default();
        let Ok(raw) = serde_yaml::from_str::<RawKnowledgeRoot>(text) else {
            return cfg;
        };
        if let Some(s) = raw.knowledge.and_then(|k| k.sync) {
            cfg.corpus_id = s.corpus_id.filter(|c| !c.is_empty());
            cfg.remote = s.remote.filter(|r| !r.is_empty());
            if let Some(r) = s.retention {
                cfg.retention = Retention::parse(&r);
            }
        }
        cfg
    }

    /// Load the knowledge sync config for `root` from the per-project
    /// `.cce/config`; absent file ⇒ default.
    pub fn load(root: &Path) -> KnowledgeSyncConfig {
        std::fs::read_to_string(config_path(root))
            .map(|t| KnowledgeSyncConfig::from_yaml(&t))
            .unwrap_or_default()
    }
}

/// Quote a YAML scalar only when needed (mirrors the workspace manifest writer).
fn yaml_scalar(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | ':'))
        && !s.starts_with('-');
    if safe {
        s.to_string()
    } else {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_all_local_with_lfs_on() {
        let c = SyncConfig::default();
        assert_eq!(c.remote, None);
        assert!(c.lfs);
        assert!(!c.auto_pull);
        assert_eq!(c.retention, Retention::All);
    }

    #[test]
    fn retention_parse_and_render() {
        assert_eq!(Retention::parse("all"), Retention::All);
        assert_eq!(Retention::parse("keep-last-5"), Retention::KeepLast(5));
        assert_eq!(Retention::parse("nonsense"), Retention::All);
        assert_eq!(Retention::KeepLast(3).as_str(), "keep-last-3");
        assert_eq!(Retention::All.as_str(), "all");
    }

    #[test]
    fn yaml_round_trips() {
        let c = SyncConfig {
            remote: Some("file:///tmp/remote.git".to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            auto_pull: true,
            retention: Retention::KeepLast(10),
        };
        let yaml = c.to_yaml();
        let parsed = SyncConfig::from_yaml(&yaml).unwrap();
        assert_eq!(parsed, c);
    }

    #[test]
    fn missing_sync_block_yields_default() {
        let c = SyncConfig::from_yaml("other: 1\n").unwrap();
        assert_eq!(c, SyncConfig::default());
    }

    #[test]
    fn save_then_load_from_disk() {
        let _lock = crate::sync::test_support::env_lock();
        let iso = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", iso.path());
        let tmp = tempfile::tempdir().unwrap();
        let c = SyncConfig {
            remote: Some("file:///tmp/r.git".to_string()),
            lfs: true,
            repo_id: None,
            auto_pull: false,
            retention: Retention::All,
        };
        c.save(tmp.path()).unwrap();
        let loaded = SyncConfig::load(tmp.path());
        std::env::remove_var("CCE_HOME");
        assert_eq!(loaded.remote, c.remote);
        assert!(loaded.lfs);
    }

    #[test]
    fn knowledge_sync_defaults_to_off() {
        let c = KnowledgeSyncConfig::default();
        assert_eq!(c.corpus_id, None);
        assert_eq!(c.remote, None);
        assert_eq!(c.retention, Retention::All);
    }

    #[test]
    fn knowledge_sync_reads_the_nested_block_and_tolerates_junk() {
        let c = KnowledgeSyncConfig::from_yaml(
            "knowledge:\n  enabled: true\n  sync:\n    corpus_id: internal-tickets\n    remote: file:///tmp/k.git\n    retention: keep-last-10\n",
        );
        assert_eq!(c.corpus_id.as_deref(), Some("internal-tickets"));
        assert_eq!(c.remote.as_deref(), Some("file:///tmp/k.git"));
        assert_eq!(c.retention, Retention::KeepLast(10));
        // Tolerant reading: absent/partial/unparsable blocks yield the default.
        assert_eq!(KnowledgeSyncConfig::from_yaml("sync:\n  lfs: true\n"), Default::default());
        assert_eq!(
            KnowledgeSyncConfig::from_yaml("knowledge:\n  enabled: false\n"),
            Default::default()
        );
        assert_eq!(KnowledgeSyncConfig::from_yaml("not: yaml: ["), Default::default());
        // Empty strings are treated as unset.
        let empty = KnowledgeSyncConfig::from_yaml("knowledge:\n  sync:\n    corpus_id: \"\"\n");
        assert_eq!(empty.corpus_id, None);
    }

    #[test]
    fn knowledge_sync_coexists_with_the_runtime_knowledge_config() {
        // The same `.cce/config` text parses for BOTH readers: the runtime
        // `knowledge.*` keys and the nested `knowledge.sync.*` keys (§8: the
        // existing keys are untouched).
        let text = "knowledge:\n  enabled: false\n  min_score: 0.5\n  sync:\n    corpus_id: c1\n";
        let sync = KnowledgeSyncConfig::from_yaml(text);
        assert_eq!(sync.corpus_id.as_deref(), Some("c1"));
        let runtime = crate::config::KnowledgeConfig::from_yaml(text);
        assert!(!runtime.enabled);
        assert_eq!(runtime.min_score, 0.5);
    }

    #[test]
    fn load_absent_is_default() {
        let _lock = crate::sync::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        // No config anywhere (CCE_HOME points at the empty temp dir).
        std::env::set_var("CCE_HOME", tmp.path());
        let loaded = SyncConfig::load(tmp.path());
        std::env::remove_var("CCE_HOME");
        assert_eq!(loaded.remote, None);
    }
}
