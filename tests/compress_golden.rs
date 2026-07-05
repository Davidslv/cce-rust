//! # tests/compress_golden — byte-pinned L2 compact-chunk goldens (SPEC-V2.5 §2/§7)
//!
//! **Why this file exists:** L2 chunk compression is a byte-pinned, deterministic
//! transform that cce-ruby must later reconcile to. This suite freezes the exact
//! `signature` and `compact` bytes (plus a SHA-256 checksum) for a **function
//! (leaf)** chunk AND a **container (class/struct/…)** chunk in each of the six
//! languages, proves compression actually reduces tokens, verifies the round-trip
//! (`compress(Full)` == the stored body), and checks the `chunk_compression` ledger
//! math on a real index. Container goldens pin the STRUCTURAL compact
//! (SPEC-V2.5-TUNING §A): the header plus every direct member trimmed to its
//! signature line, so a Ruby model's associations/validations survive compact. The
//! goldens are authored from cce-rust and are the target for Ruby's catch-up.
//!
//! **What it is / does:** Chunks each `test/fixture/compress/<lang>` (leaf) and
//! `test/fixture/compress/containers/<lang>` (container) file with its pack, takes
//! the first `function`/`class` chunk, and asserts every compressed form to the
//! byte. Uses only the public library surface.
//!
//! **Responsibilities:**
//! - Own the per-language leaf AND container compact-chunk goldens + checksums.
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

/// One container golden (SPEC-V2.5-TUNING §A): a `test/fixture/compress/containers`
/// file, its expected header-only `signature`, its expected STRUCTURAL `compact`
/// bytes, and the SHA-256 of those bytes (cce-ruby must reproduce every one).
struct ContainerGolden {
    file: &'static str,
    signature: &'static str,
    compact: &'static str,
    compact_sha256: &'static str,
}

const CONTAINER_GOLDENS: &[ContainerGolden] = &[
    // The regression case: a Rails model. Associations, validations, scope and enum
    // MUST all survive compact so the agent can answer "what are its associations"
    // without re-searching. The one method is trimmed to its `def` line.
    ContainerGolden {
        file: "ruby.rb",
        signature: "class Case < ApplicationRecord",
        compact: "class Case < ApplicationRecord\nbelongs_to :assignee, class_name: \"User\"\nhas_many :comments, dependent: :destroy\nhas_one :summary\nvalidates :title, presence: true\nscope :open, -> { where(closed: false) }\nenum status: { open: 0, closed: 1 }\ndef close!\n… (+3 lines)",
        compact_sha256: "5a7b0f812779173dcbb343d7c7814910da80174989fccf11dd2a22e9e6108d0f",
    },
    // Rust struct: every field listed (single-line members, no elision).
    ContainerGolden {
        file: "rust.rs",
        signature: "pub struct Store",
        compact: "pub struct Store\ndata: HashMap<String, u32>\nname: String\ndirty: bool",
        compact_sha256: "0a09c515cec828cacd0e5498173fecaf0e206e48f3e05112fbeddf6ad979e3e0",
    },
    // TS class: a private field, a typed field, and a method trimmed to its header.
    ContainerGolden {
        file: "typescript.ts",
        signature: "class Service",
        compact: "class Service\nprivate repo: Repo\nlimit: number = 10\nfind(id: string): Case {\n… (+2 lines)",
        compact_sha256: "0fe7104a4c2694a1003b678782ab63dc3af194a24baf75098e8b42e7bde43879",
    },
    // JS class: a field and two methods, each method trimmed with its body elided.
    ContainerGolden {
        file: "javascript.js",
        signature: "class Service",
        compact: "class Service\ncount = 0\nfind(id) {\n… (+2 lines)\nsave(x) {\n… (+2 lines)",
        compact_sha256: "0cc473af35eeea971669cf5ac98546d25e1bc1dc87117b47210f608fc6a6684d",
    },
    // C struct: every field listed (with its trailing `;`), no methods.
    ContainerGolden {
        file: "c.c",
        signature: "struct Point",
        compact: "struct Point\nint x;\ndouble y;\nchar *label;",
        compact_sha256: "b41ce20f777a6e6d6e47b23d518d677cf069cca836a6899d54cbc44397673212",
    },
    // Python class: the leading docstring is kept, then each method's `def` line.
    ContainerGolden {
        file: "python.py",
        signature: "class Service:",
        compact: "class Service:\n\"\"\"A tiny service over a repo.\"\"\"\ndef find(self, id):\n… (+1 lines)\ndef save(self, x):\n… (+1 lines)",
        compact_sha256: "56731d1e78f1a0f6c646a76e52efbba573c4d368bdd138b8991ad33fbc911c80",
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

/// The first `class` chunk of a `containers/` fixture file (by start_line).
fn first_container_chunk(name: &str) -> Chunk {
    let reg = default_registry();
    let pack = reg.pack_for(name).unwrap();
    let src = std::fs::read_to_string(fixture_dir().join("containers").join(name)).unwrap();
    let fc = chunk_with_pack(pack, name, &src);
    let mut cs: Vec<Chunk> = fc.chunks.into_iter().filter(|c| c.chunk_type == "class").collect();
    cs.sort_by_key(|c| c.start_line);
    cs.into_iter().next().expect("a class chunk")
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
fn per_language_structural_container_goldens_are_byte_pinned() {
    let reg = default_registry();
    for g in CONTAINER_GOLDENS {
        let chunk = first_container_chunk(g.file);

        // signature: the container header only (no body).
        assert_eq!(
            compress(&reg, g.file, &chunk.content, DetailLevel::Signature),
            g.signature,
            "container signature golden drift for {}",
            g.file
        );

        // compact: header + leading doc (if any) + every direct member trimmed to
        // its signature line, bodies elided (SPEC-V2.5-TUNING §A).
        let compact = compress(&reg, g.file, &chunk.content, DetailLevel::Compact);
        assert_eq!(compact, g.compact, "container compact golden drift for {}", g.file);
        assert_eq!(
            sha256_hex(compact.as_bytes()),
            g.compact_sha256,
            "container compact checksum drift for {}",
            g.file
        );

        // full round-trips to the exact stored body (Layer 7 recovers this).
        assert_eq!(
            compress(&reg, g.file, &chunk.content, DetailLevel::Full),
            chunk.content,
            "full must equal the stored body for {}",
            g.file
        );

        // Structural compact must still be smaller than full (precision-over-volume).
        assert!(
            estimate_tokens(&compact) < estimate_tokens(&chunk.content),
            "structural compact must be smaller than full for {}",
            g.file
        );
    }
}

#[test]
fn ruby_model_associations_survive_structural_compact() {
    // The exact behaviour that regressed (SPEC-V2.5-TUNING §A): a model's DSL lines
    // MUST be visible in compact, so a data-model question is answered without a
    // re-search storm.
    let reg = default_registry();
    let chunk = first_container_chunk("ruby.rb");
    let compact = compress(&reg, "ruby.rb", &chunk.content, DetailLevel::Compact);
    for dsl in
        ["belongs_to :assignee", "has_many :comments", "has_one :summary", "validates :title"]
    {
        assert!(compact.contains(dsl), "structural compact dropped `{dsl}`:\n{compact}");
    }
}

#[test]
fn container_compact_round_trips_to_full_via_stored_body() {
    // The container compact is a lossy view; the FULL bytes are recovered verbatim
    // from the stored chunk (what `expand_chunk(scope:body)` returns), not by
    // inverting the transform.
    let reg = default_registry();
    for g in CONTAINER_GOLDENS {
        let chunk = first_container_chunk(g.file);
        let full = compress(&reg, g.file, &chunk.content, DetailLevel::Full);
        assert_eq!(full, chunk.content, "round-trip drift for {}", g.file);
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
