//! # tests/markdown_golden — byte-pinned knowledge heading-chunk goldens (SPEC-V2.6 §2/§9)
//!
//! **Why this file exists:** The M1 markdown-heading chunker is a byte-pinned,
//! deterministic transform that cce-ruby must later reconcile to. This suite freezes
//! the exact heading-chunk bytes (plus a SHA-256 over a canonical serialization) for
//! fixtures covering the four cases the spec names: a code fence (a `#` inside a fence
//! is NOT a heading), nested headings (`###` rolling into `##`), pre-heading preamble,
//! and an oversized section that splits at its deeper headings. It also proves the
//! `compact → expand → full` round-trip works on a knowledge section via the shared
//! L2/L7 machinery.
//!
//! **What it is / does:** Runs `chunk_markdown` over each `test/fixture/markdown/*.md`
//! at a pinned budget and asserts the chunk kinds/breadcrumbs/line-spans/content and a
//! canonical checksum to the byte. Uses only the public library surface.
//!
//! **Responsibilities:**
//! - Own the per-fixture markdown heading-chunk goldens + checksums.
//! - It does NOT touch `conformance.json` or the Sync artifact — the code index's `.md`
//!   handling is unchanged (proven byte-identical in tests/knowledge_ingest.rs).

use cce::compress::{compress, DetailLevel};
use cce::markdown::{chunk_markdown, MarkdownChunk};
use cce::packs::default_registry;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn read(name: &str) -> String {
    let p = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/markdown")).join(name);
    std::fs::read_to_string(p).unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Canonical serialization of a chunk list: one record per chunk, fields
/// tab-separated (`chunk_id, kind, name, start, end, token_count, content`), chunks
/// newline-separated, embedded newlines escaped as `\n`. cce-ruby must reproduce this
/// byte-for-byte from the same fixtures.
fn canonical(chunks: &[MarkdownChunk]) -> String {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            c.chunk_id,
            c.kind,
            c.name,
            c.start_line,
            c.end_line,
            c.token_count,
            c.content.replace('\n', "\\n")
        ));
    }
    out
}

/// One fixture golden: file, split budget, expected chunk count, and the canonical
/// checksum. The structural facts are asserted separately below.
struct Golden {
    file: &'static str,
    budget: usize,
    chunks: usize,
    canonical_sha256: &'static str,
}

const GOLDENS: &[Golden] = &[
    // A `#` inside a fenced code block is NOT a heading ⇒ one whole-doc chunk.
    Golden {
        file: "code_fence.md",
        budget: 400,
        chunks: 1,
        canonical_sha256: "14a94ff03573a0567a582c0d875acbe30bcfc0a3e3fa840f72324cd37660371a",
    },
    // Pre-heading preamble becomes its own leading chunk before the first heading.
    Golden {
        file: "preamble.md",
        budget: 400,
        chunks: 2,
        canonical_sha256: "5a31d985af312ffc7a3b68be79f78fb44af69e4c4e4c387d915c00328c1ae164",
    },
    // Within budget, `###` rolls into `##` rolls into `#` ⇒ one chunk.
    Golden {
        file: "nested.md",
        budget: 400,
        chunks: 1,
        canonical_sha256: "ecff4f0927d76ff770578c9399fb8fc631d018a69efd572bbc898645b8069598",
    },
    // Over budget, the `#` section splits at its `##` children; the `###` still rolls
    // into its (in-budget) `## Alpha` parent.
    Golden {
        file: "oversized.md",
        budget: 30,
        chunks: 3,
        canonical_sha256: "fa864f7e1af325c13174f876ee7a1f5197a8663464fa7d8dcf8895a5677ccdb5",
    },
];

#[test]
fn per_fixture_goldens_are_byte_pinned() {
    for g in GOLDENS {
        let chunks = chunk_markdown(g.file, &read(g.file), g.budget);
        assert_eq!(chunks.len(), g.chunks, "{}: chunk count", g.file);
        let got = sha256_hex(canonical(&chunks).as_bytes());
        assert_eq!(got, g.canonical_sha256, "{}: canonical checksum drift", g.file);
        // Determinism: identical input ⇒ identical output.
        assert_eq!(
            chunk_markdown(g.file, &read(g.file), g.budget),
            chunks,
            "{}: nondeterministic",
            g.file
        );
    }
}

#[test]
fn code_fence_hash_is_not_a_heading() {
    let chunks = chunk_markdown("code_fence.md", &read("code_fence.md"), 400);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "Install");
    // The `#`-prefixed line inside the fence stays part of the section content.
    assert!(chunks[0].content.contains("# comment, not a heading"));
}

#[test]
fn preamble_is_a_leading_chunk() {
    let chunks = chunk_markdown("preamble.md", &read("preamble.md"), 400);
    assert_eq!(chunks[0].kind, "(preamble)");
    assert_eq!(chunks[0].name, "(preamble)");
    assert_eq!(chunks[0].start_line, 1);
    assert_eq!(chunks[0].end_line, 2);
    assert_eq!(chunks[1].kind, "Heading One");
    assert_eq!(chunks[1].name, "# Heading One");
    assert_eq!(chunks[1].start_line, 4);
}

#[test]
fn nested_rolls_up_within_budget() {
    let chunks = chunk_markdown("nested.md", &read("nested.md"), 400);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, "API");
    assert!(chunks[0].content.contains("## Auth"));
    assert!(chunks[0].content.contains("### Refresh"));
}

#[test]
fn oversized_splits_and_deep_heading_rolls_into_its_parent() {
    let chunks = chunk_markdown("oversized.md", &read("oversized.md"), 30);
    let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
    assert_eq!(kinds, vec!["Guide", "Alpha", "Beta"]);
    // The Guide head part holds only the intro, not its children.
    assert_eq!(chunks[0].content, "# Guide\n\nIntro paragraph for the guide.");
    // `### Alpha Detail` rolls into the (in-budget) `## Alpha` chunk.
    assert!(chunks[1].content.contains("### Alpha Detail"));
    assert_eq!(chunks[1].name, "# Guide › ## Alpha");
    assert_eq!(chunks[2].name, "# Guide › ## Beta");
}

/// A knowledge/markdown section round-trips through the shared L2/L7 machinery: a
/// `.md` chunk has no registered pack, so `compress` uses the language-neutral rule —
/// `compact` = the heading line + a `… (+N lines)` elision, `full` recovers the exact
/// stored bytes. `expand_chunk` (L7) then re-serves that `full` view verbatim.
#[test]
fn compact_expand_full_round_trip_on_a_section() {
    let reg = default_registry();
    let chunks = chunk_markdown("nested.md", &read("nested.md"), 400);
    let section = &chunks[0];

    // full == the stored bytes, to the byte (what expand_chunk re-serves).
    let full = compress(&reg, "nested.md", &section.content, DetailLevel::Full);
    assert_eq!(full, section.content);

    // compact reduces to the heading line + the elision marker for the rest.
    let compact = compress(&reg, "nested.md", &section.content, DetailLevel::Compact);
    assert_eq!(compact, "# API\n… (+10 lines)");

    // signature is the heading line alone.
    let sig = compress(&reg, "nested.md", &section.content, DetailLevel::Signature);
    assert_eq!(sig, "# API");

    // The round-trip: expanding the section (re-fetching full) recovers the original.
    let expanded = compress(&reg, "nested.md", &section.content, DetailLevel::Full);
    assert_eq!(expanded, section.content);
}
