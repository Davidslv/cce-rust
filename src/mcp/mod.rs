//! # mcp — CCE MCP: a Model Context Protocol server + `cce init` (SPEC-MCP)
//!
//! **Why this file exists:** SPEC-MCP closes the last gap between the clean-room
//! CCE and the original: the *agent* integration. Instead of hoping an agent shells
//! out to `cce search`, `cce mcp` exposes CCE as a first-class MCP tool the agent
//! auto-invokes, and `cce init` wires an editor (Claude Code) up so it is
//! plug-and-play. This module root declares the sub-parts and owns the small,
//! shared protocol constants both engines must agree on.
//!
//! **What it is / does:** Declares `protocol` (JSON-RPC 2.0 framing), `server`
//! (the stdio dispatch loop + store resolution + sync auto-pull), `tools` (the
//! three tools with their exact cross-language schemas), and `init` (the editor
//! wiring). Re-exports the handful of types the binary drives.
//!
//! **Responsibilities:**
//! - Own the pinned MCP protocol version and the server identity strings.
//! - It does NOT implement retrieval (that is `retriever`/`federation`), metrics
//!   (that is `metrics`), or sync (that is `sync`) — it composes them, read-only.

pub mod init;
pub mod protocol;
pub mod server;
pub mod tools;

pub use init::InitOptions;
pub use server::McpServer;

/// The MCP protocol revision this server speaks, pinned per SPEC-MCP §"The server".
/// `2025-06-18` is the current stable revision of the Model Context Protocol; the
/// server advertises it in the `initialize` response. Both engines pin the same
/// value so an agent negotiates an identical protocol regardless of backend.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// The MCP `serverInfo.name` (SPEC-MCP): the tool surface is branded `cce` in the
/// editor's tool list, identical across the Ruby and Rust engines.
pub const SERVER_NAME: &str = "cce";

/// The MCP `serverInfo.version`: the crate version, so an agent (and the editor's
/// diagnostics) can see which CCE build is serving.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The default `top_k` for `context_search` when the caller omits it (SPEC-MCP
/// input schema: `"top_k": { "default": 8 }`). This is deliberately smaller than
/// the CLI's `DEFAULT_TOP_K` (10): an agent pays per token, so a tighter default
/// keeps the returned context lean.
pub const MCP_DEFAULT_TOP_K: usize = 8;
