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
//! into `<dir>/CLAUDE.md`, and prints next steps. The CLAUDE.md block carries an
//! L4 output-compression section whose text is chosen by the configured
//! `output.level` (SPEC-V2.5 §2 Layer 4, §5) — each level's block is static and
//! byte-pinned. Re-running adds no duplicates.
//!
//! **Responsibilities:**
//! - Own index-ensuring, `.mcp.json` merge, and the bounded `CLAUDE.md` block
//!   (including the byte-pinned per-level output-compression section).
//! - Reuse `sync` for the remote path; never reimplement pull.
//! - It does NOT run the server (that is `server`) nor touch the network unless a
//!   remote is configured/requested.

use crate::config::{OutputConfig, OutputLevel};
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

// --- L4 output-compression section (SPEC-V2.5 §2 Layer 4, §5) ---
//
// The four level blocks below are STATIC and BYTE-PINNED: the exact bytes `cce init`
// writes into the CLAUDE.md block for each `output.level`, defined verbatim so every
// run — and cce-ruby's later catch-up — emits identical text. Each non-`off` block is
// a self-contained markdown subsection (leading blank line + `### Output compression`
// heading + one instruction paragraph + trailing newline); `off` contributes nothing,
// leaving the agent's default verbosity untouched. Chosen by `output_rules_block`.

/// `off`: no output rules — the section is omitted entirely.
const OUTPUT_BLOCK_OFF: &str = "";

/// `lite`: be concise; drop filler/preamble/postamble.
const OUTPUT_BLOCK_LITE: &str =
    "\n### Output compression\n\nBe concise; drop filler, preamble, and postamble.\n";

/// `standard` (default): fewest correct words; code as minimal diffs, never whole
/// files; no preamble or postamble.
const OUTPUT_BLOCK_STANDARD: &str = "\n### Output compression\n\nAnswer in the fewest words that \
are correct; when editing code show ONLY the changed lines (a minimal diff), never reprint whole \
files; no preamble or postamble.\n";

/// `max`: standard + telegraphic prose; code as minimal diffs only.
const OUTPUT_BLOCK_MAX: &str = "\n### Output compression\n\nAnswer in the fewest words that are \
correct; when editing code show ONLY the changed lines (a minimal diff), never reprint whole \
files; no preamble or postamble. Telegraphic prose; code as minimal diffs only.\n";

/// The byte-pinned output-compression section for `level` (SPEC-V2.5 §2 Layer 4).
/// A pure function of the level, so both engines emit identical bytes.
pub const fn output_rules_block(level: OutputLevel) -> &'static str {
    match level {
        OutputLevel::Off => OUTPUT_BLOCK_OFF,
        OutputLevel::Lite => OUTPUT_BLOCK_LITE,
        OutputLevel::Standard => OUTPUT_BLOCK_STANDARD,
        OutputLevel::Max => OUTPUT_BLOCK_MAX,
    }
}

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
    // The CLAUDE.md block honours the configured L4 output level (SPEC-V2.5 §5).
    let output_level = OutputConfig::load(dir).level;
    let claude_path = write_claude_md(dir, output_level)?;
    // Team-wide fix for issue #24: keep cce's own cache out of the tree so nobody
    // commits their local artifacts (which would then be honored via `.gitignore`
    // on other machines only if committed — but more importantly, they must never
    // pollute the index). Committing `.cce/` to `.gitignore` is the canonical,
    // machine-independent way to ensure that.
    let gitignore_path = ensure_cce_gitignored(dir)?;

    let mode = if is_workspace { " (workspace)" } else { "" };
    let mut out = String::new();
    out.push_str(&format!("CCE is wired up for Claude Code{mode}.\n"));
    out.push_str(&format!("  {index_line}\n"));
    out.push_str(&format!("  .mcp.json : {} (server \"cce\")\n", mcp_path.display()));
    out.push_str(&format!("  CLAUDE.md : {} (context_search guidance)\n", claude_path.display()));
    if let Some(gi) = &gitignore_path {
        out.push_str(&format!("  .gitignore: {} (ignores .cce/ cache)\n", gi.display()));
    }
    out.push_str("\nNext steps:\n");
    out.push_str("  1. Restart your editor (Claude Code) so it loads .mcp.json.\n");
    out.push_str("  2. Ask a question about this codebase — the agent calls context_search.\n");
    out.push_str("  3. Confirm it was used: cce usage (or cce dashboard)\n");
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
        match cmd_pull(dir, PullTarget::Latest, opts.force, is_workspace, None) {
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
        let (idx, stats) = Index::build_protected(dir, &HashEmbedder, |_| true, true)?;
        let store = default_store_path(dir);
        idx.save(&store).map_err(|e| e.to_string())?;
        // Best-effort build fingerprint for `cce doctor` (#62); never fatal.
        if let Err(e) = crate::fingerprint::write_for_store(&store, &idx, true) {
            eprintln!("warning: could not write the store fingerprint: {e}");
        }
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
        let (idx, stats) = Index::build_protected(&member_dir, &emb, |_| true, true)?;
        let store = default_store_path(&member_dir);
        idx.save(&store).map_err(|e| e.to_string())?;
        // Best-effort per-member build fingerprint for `cce doctor` (#62).
        if let Err(e) = crate::fingerprint::write_for_store(&store, &idx, true) {
            eprintln!("warning: could not write the store fingerprint for {}: {e}", m.name);
        }
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
///
/// Fail-safe (#99): `.mcp.json` is user-owned and may hold other servers' config
/// (commands, env, tokens). A read/parse failure — or a root/`mcpServers` that is
/// not a JSON object — must NEVER cause a rebuild from scratch; init refuses with
/// an actionable error and leaves the existing file byte-untouched.
fn write_mcp_json(dir: &Path, is_workspace: bool) -> Result<PathBuf, String> {
    let path = dir.join(".mcp.json");
    let args: Vec<Value> = if is_workspace {
        vec![json!("mcp"), json!("--workspace")]
    } else {
        vec![json!("mcp"), json!("--dir"), json!(".")]
    };
    let entry = json!({ "command": "cce", "args": args });

    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e} — fix the file and re-run `cce init`; refusing to overwrite it", path.display()))?;
        serde_json::from_str(&text).map_err(|e| {
            format!(
                "{} is not valid JSON ({e}) — refusing to rewrite it because that would drop \
                 your existing MCP servers; fix the syntax and re-run `cce init`",
                path.display()
            )
        })?
    } else {
        json!({})
    };
    {
        let obj = root.as_object_mut().ok_or_else(|| {
            format!(
                "{} does not contain a JSON object at the top level — refusing to rewrite it; \
                 fix the file and re-run `cce init`",
                path.display()
            )
        })?;
        let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
        let servers = servers.as_object_mut().ok_or_else(|| {
            format!(
                "\"mcpServers\" in {} is not a JSON object — refusing to rewrite it; \
                 fix the file and re-run `cce init`",
                path.display()
            )
        })?;
        servers.insert("cce".to_string(), entry);
    }
    let text = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())? + "\n";
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// The marker-bounded CLAUDE.md block (SPEC-MCP §"cce init" + SPEC-V2.5 §2 Layer 4).
/// The trailing output-compression section is chosen by `level` and byte-pinned.
fn claude_block(level: OutputLevel) -> String {
    let output_section = output_rules_block(level);
    format!(
        "{CLAUDE_BEGIN}\n\
## Code Context Engine (CCE)\n\
\n\
This project is indexed by CCE, exposed as MCP tools. Prefer them over reading or grepping files.\n\
\n\
- **PREFER `context_search`** to locate code, understand behaviour, or answer \"where is X / how does Y work\". It returns the most relevant code chunks (file:line + kind) from a hybrid vector + BM25 index, so you do not pay tokens for whole files.\n\
- Reserve file reads for opening a specific path `context_search` points you to.\n\
- Use `index_status` to check how fresh the index is, and `record_feedback` to rate a result.\n\
{output_section}\
{CLAUDE_END}\n"
    )
}

/// The comment + rules appended to `.gitignore` for cce's own cache dir. A single
/// self-contained block so the append is deterministic and easy to detect.
///
/// The "ignore contents, keep one file" pattern: `.cce/*` ignores everything cce
/// writes locally (the index/cache) while `!.cce/workspace.yml` re-includes the
/// SHARED workspace definition so it stays git-committable. Git cannot re-include a
/// file whose parent DIRECTORY is ignored, so this must be `.cce/*` (ignore the
/// dir's contents) and NOT `.cce/` (ignore the dir itself) — otherwise the negation
/// would be inert and `.cce/workspace.yml` could never be committed.
const GITIGNORE_BLOCK: &str =
    "# cce local index/cache — never commit (but keep the shared workspace.yml)\n\
     .cce/*\n\
     !.cce/workspace.yml\n";

/// Ensure the repo ignores cce's own local cache while keeping the shared
/// `.cce/workspace.yml` committable. When `dir` is a git repo, add the `.cce/*` +
/// `!.cce/workspace.yml` block to `<dir>/.gitignore` if not already present, and
/// return the path; otherwise (not a git repo) do nothing and return `None`.
///
/// Idempotent: the block is not re-added when the file already contains `.cce/*`
/// OR a pre-existing blanket `.cce` / `.cce/` line (an older layout is left as-is —
/// migrating it is not required, just don't double-add). This is the team-wide,
/// committed fix so no one commits their local cache and pollutes a content-
/// addressed sync artifact (issue #24).
fn ensure_cce_gitignored(dir: &Path) -> Result<Option<PathBuf>, String> {
    // Only meaningful inside a git repository — the walker honors committed
    // `.gitignore`, and a non-repo has no sha to be builder-independent against.
    // `.git` is a directory in a normal checkout and a file in a worktree/submodule.
    if !dir.join(".git").exists() {
        return Ok(None);
    }
    let path = dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let already = existing
        .lines()
        .any(|l| matches!(l.trim(), ".cce/*" | ".cce" | ".cce/" | "/.cce" | "/.cce/"));
    if already {
        return Ok(Some(path));
    }
    let mut next = existing;
    if next.is_empty() {
        next.push_str(GITIGNORE_BLOCK);
    } else {
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next.push('\n');
        next.push_str(GITIGNORE_BLOCK);
    }
    std::fs::write(&path, next).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

/// Write/merge the bounded CCE block into `<dir>/CLAUDE.md` (idempotent). If the
/// markers already exist their region is replaced; otherwise the block is appended
/// (or the file is created). A re-run produces byte-identical output.
fn write_claude_md(dir: &Path, level: OutputLevel) -> Result<PathBuf, String> {
    let path = dir.join("CLAUDE.md");
    let block = claude_block(level);
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
    fn init_refuses_to_rewrite_malformed_mcp_json_preserving_user_servers() {
        // #99: a user-authored .mcp.json with a real server entry plus one
        // trailing comma (a JSONC-ism serde rejects). init used to rebuild the
        // file from `{}`, silently wiping every user server. It must instead
        // refuse with an error naming the parse problem and leave the file
        // byte-untouched.
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        let malformed =
            "{\n  \"mcpServers\": {\n    \"github\": {\"command\": \"gh-mcp\"},\n  }\n}\n";
        std::fs::write(tmp.path().join(".mcp.json"), malformed).unwrap();

        let err = run(&opts(tmp.path())).unwrap_err();
        assert!(err.contains(".mcp.json"), "error must name the file: {err}");
        let after = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
        assert_eq!(after, malformed, ".mcp.json must be left byte-untouched on a parse failure");
    }

    #[test]
    fn init_refuses_mcp_json_whose_root_or_servers_is_not_an_object() {
        // #99: a non-object root (or a non-object `mcpServers`) used to be
        // silently replaced with `{}`, causing the same wholesale loss as a
        // parse failure. Both must be refused, file untouched.
        for content in ["[1, 2, 3]\n", "{\n  \"mcpServers\": \"oops\"\n}\n"] {
            let tmp = tempfile::tempdir().unwrap();
            write_tiny_repo(tmp.path());
            std::fs::write(tmp.path().join(".mcp.json"), content).unwrap();

            let err = run(&opts(tmp.path())).unwrap_err();
            assert!(err.contains(".mcp.json"), "error must name the file: {err}");
            let after = std::fs::read_to_string(tmp.path().join(".mcp.json")).unwrap();
            assert_eq!(after, content, "input {content:?} must be left byte-untouched");
        }
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

    /// Write a `.cce/config` selecting an L4 output level.
    fn write_output_config(dir: &Path, level: &str) {
        let cce = dir.join(".cce");
        std::fs::create_dir_all(&cce).unwrap();
        std::fs::write(cce.join("config"), format!("output:\n  level: {level}\n")).unwrap();
    }

    #[test]
    fn output_blocks_are_byte_pinned() {
        // The four level blocks are STATIC and BYTE-PINNED (SPEC-V2.5 §2 Layer 4).
        // A change here is a cross-language format break and must be intentional.
        assert_eq!(output_rules_block(OutputLevel::Off), "");
        assert_eq!(
            output_rules_block(OutputLevel::Lite),
            "\n### Output compression\n\nBe concise; drop filler, preamble, and postamble.\n"
        );
        assert_eq!(
            output_rules_block(OutputLevel::Standard),
            "\n### Output compression\n\nAnswer in the fewest words that are correct; when editing \
             code show ONLY the changed lines (a minimal diff), never reprint whole files; no \
             preamble or postamble.\n"
        );
        assert_eq!(
            output_rules_block(OutputLevel::Max),
            "\n### Output compression\n\nAnswer in the fewest words that are correct; when editing \
             code show ONLY the changed lines (a minimal diff), never reprint whole files; no \
             preamble or postamble. Telegraphic prose; code as minimal diffs only.\n"
        );
        // `max` is `standard` plus the telegraphic sentence.
        let std_body = output_rules_block(OutputLevel::Standard).trim_end();
        assert!(output_rules_block(OutputLevel::Max).starts_with(std_body));
        assert!(output_rules_block(OutputLevel::Max)
            .contains("Telegraphic prose; code as minimal diffs only."));
    }

    #[test]
    fn output_block_lengths_are_pinned() {
        // Byte-length checksums — a cheap tamper-evident pin on the exact bytes.
        assert_eq!(output_rules_block(OutputLevel::Off).len(), 0);
        assert_eq!(output_rules_block(OutputLevel::Lite).len(), 75);
        assert_eq!(output_rules_block(OutputLevel::Standard).len(), 187);
        assert_eq!(output_rules_block(OutputLevel::Max).len(), 234);
    }

    #[test]
    fn init_writes_the_default_standard_output_block() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        run(&opts(tmp.path())).unwrap();
        let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        // No config ⇒ the default `standard` block is present.
        assert!(claude.contains("### Output compression"));
        assert!(claude.contains("Answer in the fewest words that are correct"));
        assert!(!claude.contains("Telegraphic prose"), "standard leaked max text");
    }

    #[test]
    fn init_honours_each_output_level_and_stays_idempotent() {
        for (level, present, absent) in [
            ("off", None, Some("### Output compression")),
            ("lite", Some("Be concise; drop filler"), Some("Telegraphic prose")),
            ("standard", Some("fewest words that are correct"), Some("Telegraphic prose")),
            ("max", Some("Telegraphic prose; code as minimal diffs only."), None),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            write_tiny_repo(tmp.path());
            write_output_config(tmp.path(), level);
            run(&opts(tmp.path())).unwrap();
            let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
            if let Some(p) = present {
                assert!(claude.contains(p), "level {level}: missing {p:?} in:\n{claude}");
            }
            if let Some(a) = absent {
                assert!(!claude.contains(a), "level {level}: unexpected {a:?} in:\n{claude}");
            }
            // Idempotent: a re-run leaves the file byte-identical, one block.
            let first = claude.clone();
            run(&opts(tmp.path())).unwrap();
            let second = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
            assert_eq!(first, second, "level {level}: re-run changed CLAUDE.md");
            assert_eq!(second.matches(CLAUDE_BEGIN).count(), 1, "level {level}: duplicate block");
        }
    }

    #[test]
    fn init_off_level_writes_no_output_section() {
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        write_output_config(tmp.path(), "off");
        run(&opts(tmp.path())).unwrap();
        let claude = std::fs::read_to_string(tmp.path().join("CLAUDE.md")).unwrap();
        assert!(claude.contains(CLAUDE_BEGIN));
        assert!(claude.contains("PREFER `context_search`"));
        assert!(!claude.contains("### Output compression"), "off must add no output rules");
    }

    #[test]
    fn init_adds_cce_to_gitignore_in_a_git_repo_and_is_idempotent() {
        // Issue #24 team-wide fix: `cce init` in a git repo writes the "ignore
        // contents, keep one file" block so no one commits their local cache while
        // the shared `.cce/workspace.yml` stays committable. Idempotent: a second
        // run must not duplicate the block.
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap(); // mark as a git repo

        run(&opts(tmp.path())).unwrap();
        let gi_path = tmp.path().join(".gitignore");
        let gi1 = std::fs::read_to_string(&gi_path).unwrap();
        assert!(gi1.lines().any(|l| l == ".cce/*"), "must ignore the cache contents (`.cce/*`)");
        assert!(
            gi1.lines().any(|l| l == "!.cce/workspace.yml"),
            "must re-include the shared workspace.yml"
        );

        run(&opts(tmp.path())).unwrap();
        let gi2 = std::fs::read_to_string(&gi_path).unwrap();
        assert_eq!(gi1, gi2, ".gitignore must be idempotent across runs");
        assert_eq!(gi2.matches(".cce/*").count(), 1, "must not duplicate the `.cce/*` rule");
        assert_eq!(
            gi2.matches("!.cce/workspace.yml").count(),
            1,
            "must not duplicate the workspace.yml re-include"
        );
    }

    #[test]
    fn init_gitignore_block_keeps_workspace_yml_committable_but_ignores_the_cache() {
        // Behavioural proof against a REAL, hermetic git repo: the `.cce/*` +
        // `!.cce/workspace.yml` block must let git commit the SHARED
        // `.cce/workspace.yml` while still ignoring the local cache
        // (`.cce/index.json`). Machine-local git config (which on some dev boxes
        // itself excludes `.cce/`) is neutralised so the written `.gitignore` alone
        // decides — the same reason `.cce/` blanket ignores broke workspace sync.
        use std::process::Command;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs")
        };
        assert!(git(&["init", "-q"]).status.success(), "git init");
        write_tiny_repo(root);
        run(&opts(root)).unwrap();

        // `git check-ignore -q` exits 0 when the path IS ignored, non-0 when NOT.
        std::fs::create_dir_all(root.join(".cce")).unwrap();
        std::fs::write(root.join(".cce/workspace.yml"), "members: []\n").unwrap();
        std::fs::write(root.join(".cce/index.json"), "{}\n").unwrap();

        assert!(
            !git(&["check-ignore", "-q", ".cce/workspace.yml"]).status.success(),
            ".cce/workspace.yml must stay committable (NOT ignored)"
        );
        assert!(
            git(&["check-ignore", "-q", ".cce/index.json"]).status.success(),
            ".cce/index.json (local cache) MUST be ignored"
        );
    }

    #[test]
    fn init_preserves_existing_gitignore_and_does_not_duplicate() {
        // An existing `.gitignore` is appended to, not clobbered; and if a blanket
        // `.cce` rule already exists it is treated as handled — no block is added
        // (migrating an older layout is not required, just don't double-add).
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "node_modules/\n.cce\n").unwrap();

        run(&opts(tmp.path())).unwrap();
        let gi = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert!(gi.contains("node_modules/"), "existing rules preserved");
        // A blanket `.cce` was already present → no `.cce/*` block appended.
        assert!(!gi.contains(".cce/*"), "must not add a new block when `.cce` already present");
    }

    #[test]
    fn init_does_not_write_gitignore_outside_a_git_repo() {
        // With no `.git`, there is no sha to be builder-independent against, so
        // `cce init` must not create a `.gitignore`.
        let tmp = tempfile::tempdir().unwrap();
        write_tiny_repo(tmp.path());
        run(&opts(tmp.path())).unwrap();
        assert!(!tmp.path().join(".gitignore").exists(), "no .gitignore outside a git repo");
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
            git_ref: None,
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
