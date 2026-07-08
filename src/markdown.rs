//! # markdown — the knowledge heading-chunker (SPEC-V2.6 §2, M1)
//!
//! **Why this file exists:** CCE indexes any `.md` as ONE whole-file chunk, which
//! buries a big policy doc or epic — it becomes a single chunk that misses on its
//! own topics. This module splits markdown into **heading-section** chunks, the way
//! the AST chunker splits code by function/class, so each section is a precise,
//! retrievable unit. It is used ONLY by the knowledge ingest (M3); the code index's
//! `.md` handling is untouched, so `conformance.json` and the Sync golden stay
//! byte-identical (SPEC-V2.6 §1.2). It is therefore deliberately NOT a registered
//! `LanguagePack` in `default_registry`.
//!
//! **What it is / does:** Parses markdown with the **tree-sitter-markdown** block
//! grammar (robust to code fences — a `#` inside a fence is NOT a heading — and to
//! nesting), collects every heading in source order with its level, and forms
//! chunks by the same-or-higher rule: a chunk is a heading plus its content down to
//! (not including) the next heading of the same-or-higher level. A deeper heading
//! rolls into its parent UNLESS the parent section's byte-estimated token count
//! exceeds `max_section_tokens`, in which case the section splits at its deeper
//! headings. Content before the first heading is a leading (preamble) chunk. Every
//! output is deterministic and byte-pinned.
//!
//! **Responsibilities:**
//! - Own `MarkdownChunk`, heading collection, and the byte-pinned boundary/split rule.
//! - Reuse the shared `chunk_id` and `token_count` (`cce.tokens/v1`) so ids and
//!   counts agree with the rest of the engine to the byte.
//! - It does NOT redact, attach facets, embed, or persist — the knowledge store
//!   (M3) wires those in. It knows nothing about the `cce.knowledge/v1` contract.

use crate::chunker::{chunk_id, token_count};
use crate::config::DEFAULT_MARKDOWN_MAX_SECTION_TOKENS;
use tree_sitter::{Node, Parser};

/// The breadcrumb segment separator: SPACE, U+203A SINGLE RIGHT-POINTING ANGLE
/// QUOTATION MARK, SPACE. Byte-pinned so both engines emit identical breadcrumbs.
pub const BREADCRUMB_SEP: &str = " \u{203a} ";

/// The `kind`/`name` of the leading chunk that holds content before the first
/// heading (SPEC-V2.6 §2). Byte-pinned sentinel.
pub const PREAMBLE_KIND: &str = "(preamble)";

/// A single heading-section chunk of a markdown document. Mirrors the code
/// `Chunk`'s identity fields (SPEC-V2.6 §2) but adds a `name` breadcrumb and carries
/// no embedding/facets — the knowledge store attaches those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownChunk {
    /// `SHA-256(path:start:end:prefix)`, the existing content-addressed scheme.
    pub chunk_id: String,
    /// The synthetic document path this chunk belongs to (the record id, in M3).
    pub file_path: String,
    /// 1-based line of the heading (or line 1 for the preamble chunk).
    pub start_line: usize,
    /// 1-based line of the last content line in this chunk (trailing blanks trimmed).
    pub end_line: usize,
    /// The heading text (raw inline markdown, trimmed), or `PREAMBLE_KIND`.
    pub kind: String,
    /// The breadcrumb name, e.g. `# Title › ## Section` (markers reconstructed from
    /// level). `PREAMBLE_KIND` for the leading chunk.
    pub name: String,
    /// The section's markdown bytes (heading + content), trailing whitespace trimmed.
    pub content: String,
    /// `token_count(content)` per the shared `cce.tokens/v1` estimator.
    pub token_count: usize,
}

/// A heading collected from the parse: level, byte offset, 1-based line, and text.
struct Heading {
    level: usize,
    start_byte: usize,
    start_line: usize,
    text: String,
}

/// Chunk `content` (markdown) for the document identified by `file_path`, splitting
/// oversized sections at `max_section_tokens` (see the module docs for the exact
/// boundary rule). Deterministic and byte-pinned. Empty/whitespace-only input yields
/// no chunks.
pub fn chunk_markdown(
    file_path: &str,
    content: &str,
    max_section_tokens: usize,
) -> Vec<MarkdownChunk> {
    // Fail-loud nesting guard (issue #49): tree-sitter-md's external scanner
    // serializes its open-block stack into tree-sitter's fixed 1024-byte buffer
    // WITHOUT a bounds check (scanner.c `serialize`: 5 state bytes + 4 bytes per
    // open block). At ~255 simultaneously open blocks — e.g. one line of 255 `>`
    // characters — a debug build dies on the parser.c assert and a release build
    // overruns the buffer (memory corruption / SIGSEGV). This cannot be caught
    // from Rust, so pathological nesting is detected UP FRONT and the document
    // degrades to the existing whole-doc fallback chunk instead of being parsed.
    if block_nesting_estimate(content) > MAX_BLOCK_NESTING {
        return whole_doc_fallback(file_path, content);
    }
    let mut parser = Parser::new();
    // The block grammar shares the engine's tree-sitter ABI; loading cannot fail for
    // a pinned grammar, but on any trouble we degrade to a single whole-doc chunk.
    if parser.set_language(&tree_sitter_md::LANGUAGE.into()).is_err() {
        return whole_doc_fallback(file_path, content);
    }
    let Some(tree) = parser.parse(content, None) else {
        return whole_doc_fallback(file_path, content);
    };

    let bytes = content.as_bytes();
    let headings = collect_headings(tree.root_node(), bytes);

    let mut out: Vec<MarkdownChunk> = Vec::new();

    // Preamble: any content before the first heading (or the whole doc if headings
    // is empty) becomes a leading chunk when it is non-blank.
    let first_heading_start = headings.first().map(|h| h.start_byte).unwrap_or(bytes.len());
    if let Some(chunk) = make_chunk(
        file_path,
        content,
        0,
        first_heading_start,
        1,
        PREAMBLE_KIND.to_string(),
        PREAMBLE_KIND.to_string(),
    ) {
        out.push(chunk);
    }

    // Heading sections, processed by the same-or-higher rule with budget splitting.
    let n = headings.len();
    let mut i = 0usize;
    while i < n {
        i = emit(file_path, content, &headings, i, n, &[], max_section_tokens, &mut out);
    }
    out
}

/// Emit heading `i` and all of its descendants, returning the index of the next
/// heading that is a sibling-or-higher of `i` (the first `j > i` with `level ≤ L[i]`,
/// else `n`). `ancestors` are the breadcrumb segments of the enclosing headings.
#[allow(clippy::too_many_arguments)]
fn emit(
    file_path: &str,
    content: &str,
    headings: &[Heading],
    i: usize,
    n: usize,
    ancestors: &[String],
    budget: usize,
    out: &mut Vec<MarkdownChunk>,
) -> usize {
    let bytes = content.as_bytes();
    let lvl = headings[i].level;

    // section_end_idx = first heading after i with level ≤ lvl (the section closes).
    let mut section_end_idx = i + 1;
    while section_end_idx < n && headings[section_end_idx].level > lvl {
        section_end_idx += 1;
    }
    let section_byte_end =
        headings.get(section_end_idx).map(|h| h.start_byte).unwrap_or(bytes.len());

    // This heading's breadcrumb segment and full crumb.
    let mut crumb: Vec<String> = ancestors.to_vec();
    crumb.push(format!("{} {}", "#".repeat(lvl), headings[i].text));
    let name = crumb.join(BREADCRUMB_SEP);
    let kind = headings[i].text.clone();

    // Does the section have any deeper heading inside it?
    let has_child = section_end_idx > i + 1;
    let section_text = content.get(headings[i].start_byte..section_byte_end).unwrap_or("");
    let fits = token_count(section_text.trim_end()) <= budget;

    if fits || !has_child {
        // Whole section is one chunk (deeper headings, if any, roll in).
        if let Some(chunk) = make_chunk(
            file_path,
            content,
            headings[i].start_byte,
            section_byte_end,
            headings[i].start_line,
            kind,
            name,
        ) {
            out.push(chunk);
        }
        return section_end_idx;
    }

    // Split: the head part is the heading + its direct content, up to the first
    // deeper heading; then each deeper heading is recursed as its own section.
    let first_child_start = headings[i + 1].start_byte;
    if let Some(chunk) = make_chunk(
        file_path,
        content,
        headings[i].start_byte,
        first_child_start,
        headings[i].start_line,
        kind,
        name,
    ) {
        out.push(chunk);
    }
    let mut j = i + 1;
    while j < section_end_idx {
        j = emit(file_path, content, headings, j, n, &crumb, budget, out);
    }
    section_end_idx
}

/// Build a `MarkdownChunk` for the byte range `[start, end)`, or `None` when the
/// trimmed content is empty. `start_line` is the 1-based line of `start`; `end_line`
/// is derived from the trimmed content's newline count so trailing blank lines are
/// never counted.
fn make_chunk(
    file_path: &str,
    content: &str,
    start: usize,
    end: usize,
    start_line: usize,
    kind: String,
    name: String,
) -> Option<MarkdownChunk> {
    let raw = content.get(start..end)?;
    let trimmed = raw.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let end_line = start_line + trimmed.bytes().filter(|&b| b == b'\n').count();
    let content_bytes = trimmed.as_bytes();
    Some(MarkdownChunk {
        chunk_id: chunk_id(file_path, start_line, end_line, content_bytes),
        file_path: file_path.to_string(),
        start_line,
        end_line,
        kind,
        name,
        content: trimmed.to_string(),
        token_count: token_count(trimmed),
    })
}

/// The degraded path: one whole-document chunk (used only if the grammar fails to
/// load or parse, which cannot happen for the pinned grammar). Deterministic.
fn whole_doc_fallback(file_path: &str, content: &str) -> Vec<MarkdownChunk> {
    make_chunk(
        file_path,
        content,
        0,
        content.len(),
        1,
        PREAMBLE_KIND.to_string(),
        PREAMBLE_KIND.to_string(),
    )
    .into_iter()
    .collect()
}

/// The maximum conservatively-estimated open-block nesting depth handed to the
/// tree-sitter-md parser. Its external scanner's serialization buffer hard-fails
/// (corrupts, in release) at ~255 simultaneously open blocks; 192 leaves a wide
/// safety margin (heading sections, estimate slack) while being far beyond any
/// real document. Estimated-deeper input degrades to the whole-doc fallback.
/// Public so the store fingerprint (issue #62) records it as part of the
/// chunker identity.
pub const MAX_BLOCK_NESTING: usize = 192;

/// A conservative UPPER BOUND on the deepest open-block stack the tree-sitter-md
/// scanner could reach for `content`. Per CommonMark, opening a container block
/// (blockquote/list) at depth D requires the opening line to carry the full
/// prefix for all D levels: each blockquote is at least one `>` and each list
/// level at least two columns of marker/indentation (a tab is four columns).
/// So the bound is per-line: `(count of '>') + (other prefix columns)/2 + 1`,
/// maximised over all lines. It over-counts (e.g. long `---` rules, marker-like
/// text inside code fences) — an over-count only sends a pathological-looking
/// document down the deterministic whole-doc fallback; it can never under-count
/// and let a crashing input through.
fn block_nesting_estimate(content: &str) -> usize {
    content
        .lines()
        .map(|line| {
            let mut quotes = 0usize;
            let mut columns = 0usize;
            for ch in line.chars() {
                match ch {
                    '>' => quotes += 1,
                    '\t' => columns += 4,
                    ' ' | '-' | '+' | '*' | '.' | ')' | '0'..='9' => columns += 1,
                    _ => break,
                }
            }
            quotes + columns / 2 + 1
        })
        .max()
        .unwrap_or(0)
}

/// Collect every `atx_heading` / `setext_heading` in source order with its level,
/// byte offset, 1-based line, and trimmed inline text.
///
/// Iterative `TreeCursor` walk (issue #49): a recursive per-node walk overflows
/// the stack on deeply nested trees (nested blockquotes/lists). Headings are
/// never descended into — a heading cannot contain another heading.
fn collect_headings(root: Node, src: &[u8]) -> Vec<Heading> {
    let mut headings: Vec<Heading> = Vec::new();
    let mut cursor = root.walk();
    'outer: loop {
        let node = cursor.node();
        let kind = node.kind();
        let mut descend = true;
        if kind == "atx_heading" || kind == "setext_heading" {
            if let Some(level) = heading_level(node) {
                headings.push(Heading {
                    level,
                    start_byte: node.start_byte(),
                    start_line: node.start_position().row + 1,
                    text: heading_text(node, src),
                });
            }
            // A heading never contains another heading; do not descend into it.
            descend = false;
        }
        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                break 'outer;
            }
        }
    }
    headings.sort_by_key(|h| h.start_byte);
    headings
}

/// The heading level (1..=6): from the `atx_hN_marker` child for ATX headings, or
/// from the `setext_h1_underline` / `setext_h2_underline` child for setext.
fn heading_level(node: Node) -> Option<usize> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "atx_h1_marker" => return Some(1),
            "atx_h2_marker" => return Some(2),
            "atx_h3_marker" => return Some(3),
            "atx_h4_marker" => return Some(4),
            "atx_h5_marker" => return Some(5),
            "atx_h6_marker" => return Some(6),
            "setext_h1_underline" => return Some(1),
            "setext_h2_underline" => return Some(2),
            _ => {}
        }
    }
    None
}

/// The heading's text: the first `inline` descendant's raw source, trimmed. An empty
/// heading (`##` with nothing after) yields `""`. Iterative pre-order search
/// (issue #49): no walk in this module recurses per node.
fn heading_text(node: Node, src: &[u8]) -> String {
    let mut cursor = node.walk();
    'outer: loop {
        let n = cursor.node();
        if n.kind() == "inline" {
            let text = std::str::from_utf8(&src[n.start_byte()..n.end_byte()]).unwrap_or("");
            return text.trim().to_string();
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                return String::new();
            }
        }
    }
}

/// The byte-pinned default section-split budget (`markdown.max_section_tokens`),
/// re-exported for callers that do not carry a config.
pub const DEFAULT_MAX_SECTION_TOKENS: usize = DEFAULT_MARKDOWN_MAX_SECTION_TOKENS;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preamble_before_first_heading_is_its_own_chunk() {
        let doc = "intro line\nmore intro\n\n# Title\n\nbody\n";
        let chunks = chunk_markdown("doc", doc, 400);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].kind, PREAMBLE_KIND);
        assert_eq!(chunks[0].name, PREAMBLE_KIND);
        assert_eq!(chunks[0].content, "intro line\nmore intro");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 2);
        assert_eq!(chunks[1].kind, "Title");
        assert_eq!(chunks[1].name, "# Title");
        assert_eq!(chunks[1].content, "# Title\n\nbody");
        assert_eq!(chunks[1].start_line, 4);
    }

    #[test]
    fn no_preamble_when_doc_starts_with_heading() {
        let doc = "# Title\n\nbody\n";
        let chunks = chunk_markdown("doc", doc, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "Title");
    }

    #[test]
    fn nested_headings_roll_into_parent_when_within_budget() {
        // A small section keeps its deeper heading in one chunk.
        let doc = "# Title\n\nintro\n\n## Sub\n\ndetail\n";
        let chunks = chunk_markdown("doc", doc, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "Title");
        assert!(chunks[0].content.contains("## Sub"));
    }

    #[test]
    fn oversized_section_splits_at_deeper_headings() {
        // A tiny budget forces the parent to split at its `##` children.
        let doc = "# Title\n\nintro\n\n## Alpha\n\naaa\n\n## Beta\n\nbbb\n";
        let chunks = chunk_markdown("doc", doc, 1);
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, vec!["Title", "Alpha", "Beta"]);
        // The head part holds only the title + its own intro, not the children.
        assert_eq!(chunks[0].content, "# Title\n\nintro");
        assert_eq!(chunks[1].content, "## Alpha\n\naaa");
        assert_eq!(chunks[2].content, "## Beta\n\nbbb");
        // Breadcrumbs reflect the ancestor chain.
        assert_eq!(chunks[1].name, "# Title › ## Alpha");
        assert_eq!(chunks[2].name, "# Title › ## Beta");
    }

    #[test]
    fn hash_inside_code_fence_is_not_a_heading() {
        let doc = "# Real\n\n```sh\n# not a heading\necho hi\n```\n\ntail\n";
        let chunks = chunk_markdown("doc", doc, 400);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "Real");
        // The fenced `#` line survives inside the single section's content.
        assert!(chunks[0].content.contains("# not a heading"));
    }

    #[test]
    fn deep_heading_skipping_a_level_keeps_parent_breadcrumb() {
        let doc = "# Title\n\nintro\n\n### Deep\n\nx\n";
        let chunks = chunk_markdown("doc", doc, 1);
        assert_eq!(chunks[0].kind, "Title");
        assert_eq!(chunks[1].kind, "Deep");
        assert_eq!(chunks[1].name, "# Title › ### Deep");
    }

    #[test]
    fn setext_headings_become_separate_chunks() {
        let doc = "Title\n=====\n\nbody\n\nSub\n---\n\nmore\n";
        let chunks = chunk_markdown("doc", doc, 1);
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, vec!["Title", "Sub"]);
    }

    #[test]
    fn chunk_ids_are_deterministic_and_16_hex() {
        let doc = "# A\n\nx\n\n# B\n\ny\n";
        let a = chunk_markdown("doc", doc, 400);
        let b = chunk_markdown("doc", doc, 400);
        assert_eq!(a, b);
        for c in &a {
            assert_eq!(c.chunk_id.len(), 16);
            assert!(c.chunk_id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
    }

    #[test]
    fn empty_input_yields_no_chunks() {
        assert!(chunk_markdown("doc", "", 400).is_empty());
        assert!(chunk_markdown("doc", "   \n\n  \n", 400).is_empty());
    }

    #[test]
    fn oversized_flat_section_stays_one_chunk() {
        // No deeper heading to split at ⇒ one chunk even over budget.
        let doc = "# Title\n\nlots of content here and more and more and more\n";
        let chunks = chunk_markdown("doc", doc, 1);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "Title");
    }

    #[test]
    fn end_line_ignores_trailing_blank_lines() {
        let doc = "# A\n\nline\n\n\n# B\n\nz\n";
        let chunks = chunk_markdown("doc", doc, 1);
        // Section A: heading line 1, content "line" line 3 ⇒ end_line 3 (blanks trimmed).
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        assert_eq!(chunks[0].content, "# A\n\nline");
    }
}
