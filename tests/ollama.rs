//! # tests/ollama — optional Ollama embedder integration test
//!
//! **Why this file exists:** SPEC §11 allows a minimal Ollama integration test
//! that is skipped gracefully when no server is present, keeping the default
//! suite hermetic (no network).
//!
//! **What it is / does:** Marked `#[ignore]` so it never runs in the default
//! suite. When explicitly run and a local Ollama is reachable, it embeds two
//! texts and checks the returned vectors are non-empty and equal-length.
//!
//! **Responsibilities:**
//! - Own the opt-in network test for the Ollama backend.

use cce::embedder::{Embedder, OllamaEmbedder};

#[test]
#[ignore = "requires a local Ollama server; run with --ignored"]
fn ollama_embeds_when_available() {
    let oll = OllamaEmbedder::default();
    if !oll.healthy() {
        eprintln!("skipping: no Ollama server at {}", oll.base_url);
        return;
    }
    let vecs = oll.embed_batch(&["hello world".to_string(), "goodbye".to_string()]);
    assert_eq!(vecs.len(), 2);
    assert!(!vecs[0].is_empty());
    assert_eq!(vecs[0].len(), vecs[1].len());
    // single embed path
    assert!(!oll.embed("single").is_empty());
}
