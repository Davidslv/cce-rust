//! # mcp::init — `cce init`: plug-and-play editor wiring (SPEC-MCP §"cce init")
//!
//! **Why this file exists:** For MCP to be plug-and-play, one command must ensure
//! the project has an index, write the editor's MCP server config, and drop a
//! `CLAUDE.md` block that steers the agent to prefer `context_search` over
//! Read/Grep — all idempotently, so a re-run is safe. That is `cce init`.
//!
//! **What it is / does:** Ensures an index (via `cce sync pull --latest` when a
//! remote is configured/passed, else a local `cce index` / workspace index), then
//! merges a `cce` server entry into `<dir>/.mcp.json` and a marker-bounded block
//! into `<dir>/CLAUDE.md`, and prints next steps. Re-running adds no duplicates.
//!
//! **Responsibilities:**
//! - Own index-ensuring, `.mcp.json` merge, and the bounded `CLAUDE.md` block.
//! - Reuse `sync` for the remote path; never reimplement pull.
//! - It does NOT run the server (that is `server`) nor touch the network unless a
//!   remote is configured/requested.

use crate::embedder::HashEmbedder;
use crate::store::{default_store_path, Index};
use crate::sync::commands::{cmd_init as sync_init, cmd_pull, PullTarget};
use crate::sync::config::SyncConfig;
use crate::workspace::{build_graph, manifest_path, Manifest};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The stable markers bounding the CCE block in `CLAUDE.md` so it can be updated or
/// removed without disturbing the rest of the file (SPEC-MCP §"cce init").
const CLAUDE_BEGIN: &str = "<!-- BEGIN CCE MCP -->";
const CLAUDE_END: &str = "<!-- END CCE MCP -->";

/// Resolved options for `cce init`.
pub struct InitOptions {
    /// The project directory to wire up (defaults to cwd at the CLI layer).
    pub dir: PathBuf,
    /// The target agent. v1 supports only `claude`.
    pub agent: String,
    /// A sync remote to pull the CI-built index from (`--remote <url>`).
    pub remote: Option<String>,
    /// Overwrite/force the index refresh (a `--force` pull past a sha mismatch).
    pub force: bool,
}

/// Run `cce init` and return a human-readable report (SPEC-MCP §"cce init").
pub fn run(opts: &InitOptions) -> Result<String, String> {
    if opts.agent != "claude" {
        return Err(format!(
            "unsupported --agent `{}` — v1 targets Claude Code (pass `--agent claude`)",
            opts.agent
        ));
    }
    let dir = opts.dir.as_path();
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let is_workspace = manifest_path(dir).exists();

    let index_line = ensure_index(opts, is_workspace)?;
    let mcp_path = write_mcp_json(dir, is_workspace)?;
    let claude_path = write_claude_md(dir)?;

    let mode = if is_workspace { " (workspace)" } else { "" };
    let mut out = String::new();
    out.push_str(&format!("CCE is wired up for Claude Code{mode}.\n"));
    out.push_str(&format!("  {index_line}\n"));
    out.push_str(&format!("  .mcp.json : {} (server \"cce\")\n", mcp_path.display()));
    out.push_str(&format!("  CLAUDE.md : {} (context_search guidance)\n", claude_path.display()));
    out.push_str("\nNext steps:\n");
    out.push_str("  1. Restart your editor (Claude Code) so it loads .mcp.json.\n");
    out.push_str("  2. Ask a question about this codebase — the agent calls context_search.\n");
    out.push_str("  3. Confirm it was used: cce dashboard\n");
    Ok(out)
}

/// Ensure the project has an index, returning the report line describing how.
fn ensure_index(opts: &InitOptions, is_workspace: bool) -> Result<String, String> {
    let dir = opts.dir.as_path();
    let cfg = SyncConfig::load(dir);
    let want_remote = opts.remote.is_some() || cfg.remote.is_some();

    if want_remote {
        // A freshly-passed remote is configured first (writes .cce/config + clone).
        if let Some(remote) = &opts.remote {
            sync_init(dir, remote, true, None)?;
        }
        match cmd_pull(dir, PullTarget::Latest, opts.force, is_workspace) {
            Ok(_) => {
                return Ok(
                    "index     : pulled from sync remote (cce sync pull --latest)".to_string()
                )
            }
            Err(e) => {
                // An explicitly-requested remote that fails is a hard error; a merely
                // configured-but-unreachable one falls back to a local index.
                if opts.remote.is_some() {
                    return Err(format!("sync pull failed: {e}"));
                }
            }
        }
    }

    if is_workspace {
        let (files, chunks) = build_workspace_index(dir)?;
        Ok(format!("index     : built {chunks} chunk(s) from {files} file(s) across the workspace"))
    } else {
        let (idx, stats) = Index::build_protected(dir, &HashEmbedder, |_| true, true);
        idx.save(&default_store_path(dir)).map_err(|e| e.to_string())?;
        Ok(format!(
            "index     : built {} chunk(s) from {} file(s)",
            stats.total_chunks, stats.files_indexed
        ))
    }
}

/// Index every workspace member into its own store and rebuild the cross-member
/// graph. Returns (total files, total chunks).
fn build_workspace_index(dir: &Path) -> Result<(usize, usize), String> {
    let manifest = Manifest::load(dir)?;
    let emb = HashEmbedder;
    let mut files = 0usize;
    let mut chunks = 0usize;
    for m in &manifest.members {
        let member_dir = dir.join(&m.path);
        let (idx, stats) = Index::build_protected(&member_dir, &emb, |_| true, true);
        idx.save(&default_store_path(&member_dir)).map_err(|e| e.to_string())?;
        files += stats.files_indexed;
        chunks += stats.total_chunks;
    }
    let graph = build_graph(dir, &manifest);
    graph.save(dir).map_err(|e| format!("could not write workspace graph: {e}"))?;
    Ok((files, chunks))
}

/// Write/merge `<dir>/.mcp.json` with a `cce` MCP server entry (idempotent). Any
/// existing servers are preserved; the `cce` entry is (re)written to the canonical
/// value, so a re-run produces byte-identical output.
fn write_mcp_json(dir: &Path, is_workspace: bool) -> Result<PathBuf, String> {
    let path = dir.join(".mcp.json");
    let args: Vec<Value> = if is_workspace {
        vec![json!("mcp"), json!("--workspace")]
    } else {
        vec![json!("mcp"), json!("--dir"), json!(".")]
    };
    let entry = json!({ "command": "cce", "args": args });

    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    {
        let obj = root.as_object_mut().expect("root is an object");
        let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
        if !servers.is_object() {
            *servers = json!({});
        }
        servers.as_object_mut().expect("servers is an object").insert("cce".to_string(), entry);
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// The marker-bounded CLAUDE.md block (SPEC-MCP §"cce init").
fn claude_block() -> String {
    format!(
        "{CLAUDE_BEGIN}\n\
## Code Context Engine (CCE)\n\
\n\
This project is indexed by CCE, exposed as MCP tools. Prefer them over reading or grepping files.\n\
\n\
- **PREFER `context_search`** to locate code, understand behaviour, or answer \"where is X / how does Y work\". It returns the most relevant code chunks (file:line + kind) from a hybrid vector + BM25 index, so you do not pay tokens for whole files.\n\
- Reserve file reads for opening a specific path `context_search` points you to.\n\
- Use `index_status` to check how fresh the index is, and `record_feedback` to rate a result.\n\
{CLAUDE_END}\n"
    )
}

/// Write/merge the bounded CCE block into `<dir>/CLAUDE.md` (idempotent). If the
/// markers already exist their region is replaced; otherwise the block is appended
/// (or the file is created). A re-run produces byte-identical output.
fn write_claude_md(dir: &Path) -> Result<PathBuf, String> {
    let path = dir.join("CLAUDE.md");
    let block = claude_block();
    let new_content = match std::fs::read_to_string(&path) {
        Ok(text) => {
            if let (Some(b), Some(e)) = (text.find(CLAUDE_BEGIN), text.find(CLAUDE_END)) {
                let end_idx = e + CLAUDE_END.len();
                let mut s = String::new();
                s.push_str(&text[..b]);
                s.push_str(block.trim_end());
                s.push_str(&text[end_idx..]);
                s
            } else {
                let mut s = text;
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push('\n');
                s.push_str(&block);
                s
            }
        }
        Err(_) => format!("# CLAUDE.md\n\n{block}"),
    };
    std::fs::write(&path, new_content).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tiny_repo(dir: &Path) {
        std::fs::write(dir.join("auth.py"), "def hash_password(pw):\n    return pw + 'salt'\n")
            .unwrap();
        std::fs::write(
            dir.join("payments.py"),
            "import auth\n\ndef process_payment(amount):\n    return amount\n",
        )
        .unwrap();
    }

    fn opts(dir: &Path) -> InitOptions {
        InitOptions {
            dir: dir.to_path_buf(),
            agent: "claude".to_string(),
            remote: None,
            force: false,
        }
    }

    #[test]
    fn init_builds_index_and_writes_config_and_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        let report = run(&opts(tmp.path())).unwrap();
        assert!(report.contains("wired up for Claude Code"));
        assert!(report.contains("Restart your editor"));

        assert!(default_store_path(tmp.path()).exists(), "index not built");
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["cce"]["command"], "cce");
        assert_eq!(mcp["mcpServers"]["cce"]["args"], json!(["mcp", "--dir", "."]));

        let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(claude.contains(CLAUDE_BEGIN));
        assert!(claude.contains("PREFER `context_search`"));
        assert!(claude.contains(CLAUDE_END));
    }

    #[test]
    fn init_is_idempotent_no_duplicate_blocks_or_servers() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        run(&opts(tmp.path())).unwrap();
        let mcp1 = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let claude1 = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();

        run(&opts(tmp.path())).unwrap();
        let mcp2 = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        let claude2 = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();

        assert_eq!(mcp1, mcp2, ".mcp.json must be idempotent");
        assert_eq!(claude1, claude2, "CLAUDE.md must be idempotent");
        // Exactly one CCE block.
        assert_eq!(claude2.matches(CLAUDE_BEGIN).count(), 1);
    }

    #[test]
    fn init_merges_into_existing_mcp_json_preserving_other_servers() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        std::fs::write(
            tmp.path().join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"foo","args":[]}}}"#,
        )
        .unwrap();
        run(&opts(tmp.path())).unwrap();
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["other"]["command"], "foo");
        assert_eq!(mcp["mcpServers"]["cce"]["command"], "cce");
    }

    #[test]
    fn init_appends_block_to_existing_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "# My Project\n\nExisting notes.\n").unwrap();
        run(&opts(tmp.path())).unwrap();
        let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("# My Project"));
        assert!(claude.contains("Existing notes."));
        assert!(claude.contains(CLAUDE_BEGIN));
        // A second run still keeps exactly one block and preserves the preamble.
        run(&opts(tmp.path())).unwrap();
        let claude2 = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert_eq!(claude2.matches(CLAUDE_BEGIN).count(), 1);
        assert!(claude2.starts_with("# My Project"));
    }

    #[test]
    fn init_rejects_non_claude_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut o = opts(tmp.path());
        o.agent = "cursor".to_string();
        let err = run(&o).unwrap_err();
        assert!(err.contains("unsupported --agent"), "got: {err}");
    }

    #[test]
    fn init_rejects_non_directory() {
        let o = InitOptions {
            dir: PathBuf::from("/no/such/dir/xyz"),
            agent: "claude".to_string(),
            remote: None,
            force: false,
        };
        assert!(run(&o).unwrap_err().contains("not a directory"));
    }

    /// Build a two-member JS workspace and write its manifest under `.cce/`.
    fn write_workspace(dir: &Path) {
        for name in ["alpha", "beta"] {
            let m = dir.join(name);
            std::fs::create_dir_all(m.join("src")).unwrap();
            std::fs::write(m.join("package.json"), format!("{{\"name\":\"{name}\"}}")).unwrap();
            std::fs::write(m.join("src/index.js"), format!("function {name}() {{ return 1; }}\n"))
                .unwrap();
        }
        crate::workspace::build_manifest(dir).save(dir).unwrap();
    }

    #[test]
    fn init_workspace_mode_indexes_members_and_writes_workspace_config() {
        let _lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path()); // hermetic: no real global config
        let tmp = tempfile::tempdir().unwrap();
        write_workspace(tmp.path());

        let report = run(&opts(tmp.path())).unwrap();
        std::env::remove_var("CCE_HOME");
        assert!(report.contains("(workspace)"), "got: {report}");
        assert!(report.contains("across the workspace"));

        // Each member has its own store, and the cross-member graph exists.
        assert!(default_store_path(&tmp.path().join("alpha")).exists());
        assert!(default_store_path(&tmp.path().join("beta")).exists());
        assert!(crate::workspace::graph_path(tmp.path()).exists());

        // .mcp.json points at `cce mcp --workspace`.
        let mcp: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert_eq!(mcp["mcpServers"]["cce"]["args"], json!(["mcp", "--workspace"]));
    }

    #[test]
    fn init_falls_back_to_local_when_a_configured_remote_is_unreachable() {
        let _lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        // A configured-but-unreachable remote (not passed via --remote): init must
        // try to pull, fail, and fall back to building a local index.
        SyncConfig {
            remote: Some("file:///definitely/not/here.git".to_string()),
            lfs: false,
            repo_id: Some("example.com__acme__demo".to_string()),
            auto_pull: false,
            retention: crate::sync::config::Retention::All,
        }
        .save(tmp.path())
        .unwrap();

        let report = run(&opts(tmp.path())).unwrap();
        std::env::remove_var("CCE_HOME");
        // Fell back to a local build (not "pulled from sync remote").
        assert!(report.contains("built"), "got: {report}");
        assert!(default_store_path(tmp.path()).exists());
    }

    #[test]
    fn init_with_an_explicit_unreachable_remote_is_a_hard_error() {
        let _lock = crate::sync::test_support::env_lock();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("CCE_HOME", home.path());
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        let mut o = opts(tmp.path());
        o.remote = Some("file:///definitely/not/here.git".to_string());
        let err = run(&o).unwrap_err();
        std::env::remove_var("CCE_HOME");
        // An explicitly-requested remote that cannot be reached is fatal.
        assert!(!err.is_empty(), "expected an error for an unreachable --remote");
    }
}
