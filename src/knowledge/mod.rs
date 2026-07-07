//! # knowledge — Knowledge Sources: the ingest contract + snapshot store (SPEC-V2.6)
//!
//! **Why this file exists:** Code says *what/how*; epics, issues, and policy docs say
//! *why*. This module is CCE's generic way to feed that non-code knowledge in — a
//! neutral `cce.knowledge/v1` contract any adapter emits (`contract`), and a separate,
//! snapshot-keyed knowledge store that heading-chunks and indexes it (`store`). CCE
//! owns the engine (the contract + the chunker + the store), never the integrations.
//!
//! **What it is / does:** Re-exports the contract types/parsing and the store/ingest
//! API. Phase A (M1–M3) builds the store end-to-end; retrieval blend (M4) is Phase B.
//!
//! **Responsibilities:**
//! - Own the `knowledge` module tree and its public surface.
//! - It contains no algorithm itself; each concern lives in its submodule.

pub mod contract;
pub mod retrieval;
pub mod store;

pub use contract::{parse_ndjson, render_document, KnowledgeRecord, KNOWLEDGE_SCHEMA_ID};
pub use retrieval::{
    is_merged_pr_link, provenance_line, same_document_sections, search_knowledge, KnowledgeHit,
    LoadedKnowledge,
};
pub use store::{
    ingest, ingest_default, ingest_file, snapshot_id, IngestSummary, KnowledgeChunk, KnowledgeStore,
};
