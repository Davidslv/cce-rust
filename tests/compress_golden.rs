//! # tests/compress_golden — byte-pinned L2 compact-chunk goldens (SPEC-V2.5 §2/§7)
//!
//! **Why this file exists:** L2 chunk compression is a byte-pinned, deterministic
//! transform that cce-ruby must later reconcile to. This suite freezes the exact
//! `signature` and `compact` bytes (plus a SHA-256 checksum) for a function chunk in
//! each of the six languages, proves compression actually reduces tokens, verifies
//! the round-trip (`compress(Full)` == the stored body), and checks the
//! `chunk_compression` ledger math on a real index. The goldens are authored from
//! cce-rust and are the target for Ruby's catch-up.
//!
//! **What it is / does:** Chunks each `test/fixture/compress/<lang>` file with its
//! pack, takes the first `function` chunk, and asserts every compressed form to the
//! byte. Uses only the public library surface.
//!
//! **Responsibilities:**
//! - Own the per-language compact-chunk goldens + checksums.
//! - It does NOT touch `conformance.json`, the Sync artifact, or the store — L2 is a
//!   serialization-time transform, so those stay byte-identical (proven elsewhere).

use cce::chunker::{chunk_with_pack, Chunk};
use cce::compress::{compress, DetailLevel};
use cce::embedder::HashEmbedder;
use cce::packs::default_registry;
use cce::retriever::{build_search_record, search};
use cce::store::Index;
use cce::tokenizer::estimate_tokens;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/compress"))
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixture_dir().join(name)).unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// One language golden: file, expected signature, expected compact bytes, and the
/// SHA-256 of the compact bytes (cce-ruby must reproduce every one).
struct Golden {
    file: &'static str,
    signature: &'static str,
    compact: &'static str,
    compact_sha256: &'static str,
}

const GOLDENS: &[Golden] = &[
    Golden {
        file: "python.py",
        signature: "def parse_amount(raw):",
        compact: "def parse_amount(raw):\n\"\"\"Parse a currency amount from raw text.\"\"\"\ncleaned = raw.strip().replace(\"$\", \"\")\n… (+2 lines)",
        compact_sha256: "ac1c3202b4d42008b5036e54c95fd148a879e91178098e9644a6caf94673a433",
    },
    Golden {
        file: "ruby.rb",
        signature: "def parse_amount(raw)",
        compact: "def parse_amount(raw)\ncleaned = raw.strip.delete(\"$\")\n… (+3 lines)",
        compact_sha256: "e19ed20076d49d0535991935d7e4c6846d772931667697fa06f57f79224421aa",
    },
    Golden {
        file: "rust.rs",
        signature: "pub fn parse_amount(raw: &str) -> f64",
        compact: "pub fn parse_amount(raw: &str) -> f64\nlet cleaned = raw.trim().replace('$', \"\");\n… (+3 lines)",
        compact_sha256: "322dabfc73efa40205c210e919912dd6f59b5c311f79b531918c0927656526b8",
    },
    Golden {
        file: "typescript.ts",
        signature: "function parseAmount(raw: string): number",
        compact: "function parseAmount(raw: string): number\nconst cleaned = raw.trim().replace(\"$\", \"\");\n… (+3 lines)",
        compact_sha256: "c69e8a6a2fd20e9f157d80aa4fd2fcf9344f02a225de693f77d1af53023f68cf",
    },
    Golden {
        file: "javascript.js",
        signature: "function parseAmount(raw)",
        compact: "function parseAmount(raw)\nconst cleaned = raw.trim().replace(\"$\", \"\");\n… (+3 lines)",
        compact_sha256: "997dc69371022ac1ef442031fc8c982d5a483fc51603a299210894423f1fa1b8",
    },
    Golden {
        file: "c.c",
        signature: "double parse_amount(const char *raw)",
        compact: "double parse_amount(const char *raw)\ndouble value = atof(raw);\n… (+3 lines)",
        compact_sha256: "917fee8aa08d8e7b18f0cbb4eaae863f7066293614159bcd6ec4910b15316928",
    },
];

/// The first `function` chunk of a fixture file (deterministic by start_line).
fn first_function_chunk(name: &str) -> Chunk {
    let reg = default_registry();
    let pack = reg.pack_for(name).unwrap();
    let src = read(name);
    let fc = chunk_with_pack(pack, name, &src);
    let mut fns: Vec<Chunk> =
        fc.chunks.into_iter().filter(|c| c.chunk_type == "function").collect();
    fns.sort_by_key(|c| c.start_line);
    fns.into_iter().next().expect("a function chunk")
}

#[test]
fn per_language_compact_chunk_goldens_are_byte_pinned() {
    let reg = default_registry();
    for g in GOLDENS {
        let chunk = first_function_chunk(g.file);

        // signature: declaration line(s) only.
        assert_eq!(
            compress(&reg, g.file, &chunk.content, DetailLevel::Signature),
            g.signature,
            "signature golden drift for {}",
            g.file
        );

        // compact: signature + doc (if any) + first body line + byte-pinned elision.
        let compact = compress(&reg, g.file, &chunk.content, DetailLevel::Compact);
        assert_eq!(compact, g.compact, "compact golden drift for {}", g.file);
        assert_eq!(
            sha256_hex(compact.as_bytes()),
            g.compact_sha256,
            "compact checksum drift for {}",
            g.file
        );

        // full round-trips to the exact stored body (Layer 7 recovers this).
        assert_eq!(
            compress(&reg, g.file, &chunk.content, DetailLevel::Full),
            chunk.content,
            "full must equal the stored body for {}",
            g.file
        );

        // Compression must actually reduce tokens (precision-over-volume, §1.6).
        assert!(
            estimate_tokens(&compact) < estimate_tokens(&chunk.content),
            "compact must be smaller than full for {}",
            g.file
        );
    }
}

#[test]
fn every_compact_golden_elides_and_marks_it() {
    // Each golden function is long enough to trigger the `… (+N lines)` marker.
    for g in GOLDENS {
        assert!(g.compact.contains("… (+"), "expected an elision marker for {}", g.file);
    }
}

#[test]
fn chunk_compression_ledger_math_on_a_real_index() {
    // Build a real index over the six compress fixtures, run a search, and prove the
    // `chunk_compression` bucket equals Σ(full − compressed) with baseline Σ(full),
    // computed with the one `cce.tokens/v1` estimator (SPEC-V2.5 §2/§4).
    let reg = default_registry();
    let (index, _) = Index::build_from_dir(&fixture_dir(), &HashEmbedder);
    let results = search(&index, &HashEmbedder, "parse amount currency", 8, false);
    assert!(!results.is_empty(), "the query must return results");

    let mut expected_baseline = 0u64;
    let mut expected_saved = 0u64;
    for r in &results {
        let full = estimate_tokens(&r.content);
        let served =
            estimate_tokens(&compress(&reg, &r.file_path, &r.content, DetailLevel::Compact));
        expected_baseline += full;
        expected_saved += full - served;
    }

    let rec = build_search_record(
        &index,
        &results,
        "parse amount currency",
        8,
        false,
        0.0,
        "mcp",
        DetailLevel::Compact,
    );
    assert_eq!(rec.chunk_baseline_tokens, expected_baseline);
    assert_eq!(rec.chunk_saved_tokens, expected_saved);
    // Real savings on this corpus, not a tautology.
    assert!(rec.chunk_saved_tokens > 0, "compression should save tokens on this corpus");

    // At detail:full nothing is compressed → the bucket is zero.
    let rec_full = build_search_record(
        &index,
        &results,
        "parse amount currency",
        8,
        false,
        0.0,
        "mcp",
        DetailLevel::Full,
    );
    assert_eq!(rec_full.chunk_baseline_tokens, 0);
    assert_eq!(rec_full.chunk_saved_tokens, 0);
}
