//! # Code Context Engine (CCE) — library root
//!
//! **Why this file exists:** It is the crate root that wires together every
//! module of the engine and exposes them to the binary (`main.rs`) and the test
//! suite. Keeping a thin `lib.rs` lets both the CLI and the tests depend on the
//! same, fully-tested library code.
//!
//! **What it is / does:** Declares the module tree and re-exports the handful of
//! types most consumers need (`Chunk`, `Index`, retrieval results).
//!
//! **Responsibilities:**
//! - Own the module list and the public surface of the library.
//! - It deliberately does NOT contain algorithm logic; each concern lives in its
//!   own file per SPEC §2.

pub mod atomic;
pub mod config;
pub mod tokenizer;
pub mod pricing;
pub mod savings;
pub mod eval;
pub mod embedder;
pub mod packs;
pub mod chunker;
pub mod markdown;
pub mod knowledge;
pub mod compress;
pub mod sensitive;
pub mod redactor;
pub mod vector_store;
pub mod keyword_store;
pub mod graph_store;
pub mod store;
pub mod memory;
pub mod session;
pub mod walker;
pub mod retriever;
pub mod grammar;
pub mod bench;
pub mod conformance;
pub mod stats;
pub mod relevance;
pub mod metrics;
pub mod aggregator;
pub mod usage;
pub mod dashboard;
pub mod workspace;
pub mod federation;
pub mod sync;
pub mod mcp;
pub mod update;
pub mod fingerprint;
pub mod doctor;

pub use chunker::Chunk;
pub use retriever::SearchResult;
pub use store::Index;
