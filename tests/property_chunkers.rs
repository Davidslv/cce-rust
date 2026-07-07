//! # tests/property_chunkers — property-based invariants for the chunkers (#33)
//!
//! **Why this file exists:** The chunkers and the byte-pinned token rule are
//! guarded by fixed goldens over seven curated fixtures — exactly the inputs a
//! tree-sitter offset bug would never touch. `chunk_id` is the content-addressed
//! anchor sync verify, `expand_chunk`, and conformance stand on, so a silently
//! shifted offset on an adversarial-but-legal input (CRLF, unicode identifiers,
//! empty or comment-only files, deep nesting, markdown heading edge cases) would
//! corrupt the whole system without failing any golden. These properties assert
//! what the chunkers PROMISE on arbitrary input, not what one fixture produces.
//!
//! **What it is / does:** Generates plausible-to-hostile source per language
//! (composed fragments: functions, classes, comments, nested definitions, raw
//! garbage; mutated with CRLF, trailing whitespace, missing final newline) and
//! markdown (headings of random depth, setext, preambles, fenced `#` lines), then
//! asserts, for every pack and the markdown chunker: in-bounds line ranges,
//! exact-slice round-trip (which also proves char-boundary-safe slicing — a lossy
//! decode would break substring equality), pre-order/nesting order, determinism,
//! recomputable `chunk_id`s, and the pinned `max(1, floor(bytes/4))` token rule.
//!
//! **Responsibilities:**
//! - Own the generator strategies and the invariant checkers for the code and
//!   markdown chunkers plus the `cce.tokens/v1` estimator and SPEC §4.1 tokenizer.
//! - It does NOT pin bytes (the goldens do) and does NOT test retrieval/stores.
//! - Proptest persists any found failure under `proptest-regressions/` — commit
//!   that file so a shrunk counterexample becomes a permanent regression test.

use cce::chunker::{chunk_id, Chunk, Chunker};
use cce::markdown::{chunk_markdown, MarkdownChunk, PREAMBLE_KIND};
use cce::tokenizer::{estimate_tokens, tokenize};
use proptest::prelude::*;

/// One representative path per shipped pack (the six languages of SPEC-V2 §2).
const LANGS: &[(&str, &str)] = &[
    ("c", "gen/src.c"),
    ("javascript", "gen/src.js"),
    ("python", "gen/src.py"),
    ("ruby", "gen/src.rb"),
    ("rust", "gen/src.rs"),
    ("typescript", "gen/src.ts"),
];

fn newline_count(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

/// The pinned `cce.tokens/v1` rule (SPEC-V2.5 §4): max(1, floor(byte_length / 4)).
fn pinned_token_rule(s: &str) -> usize {
    (s.len() / 4).max(1)
}

fn assert_hex16(id: &str) {
    assert_eq!(id.len(), 16, "chunk_id must be 16 chars: {id:?}");
    assert!(
        id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "chunk_id must be lowercase hex: {id:?}"
    );
}

// --- Generators: language-neutral fragments rendered per language ---

/// A language-neutral source fragment; `render` turns it into concrete syntax so
/// the SAME generated structure exercises all six packs.
#[derive(Debug, Clone)]
enum Frag {
    Function {
        name: String,
        param: String,
        body: String,
    },
    Class {
        name: String,
        method: String,
    },
    Comment(String),
    Blank,
    /// `depth` nested definitions/blocks — exercises nested-chunk emission and
    /// deep recursion in the tree walk.
    Nested {
        name: String,
        depth: usize,
    },
    Statement(String),
    /// Arbitrary printable unicode — the chunker must be robust to ANY input.
    Garbage(String),
}

/// Identifiers: mostly ASCII, deliberately salted with multi-byte unicode so
/// byte-vs-char offset bugs near chunk boundaries surface.
fn ident() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => "[a-z][a-z0-9_]{0,9}",
        1 => Just("café".to_string()),
        1 => Just("δelta".to_string()),
        1 => Just("имя_x".to_string()),
        1 => Just("名前".to_string()),
    ]
}

/// Free text for comments/string bodies (no quotes/backslashes, so it stays
/// inside a string literal; `Garbage` covers the truly hostile shapes).
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "[a-z0-9 .,;:!?_-]{0,24}",
        1 => Just("naïve ☃ text — ①".to_string()),
    ]
}

fn frag() -> impl Strategy<Value = Frag> {
    prop_oneof![
        3 => (ident(), ident(), text())
            .prop_map(|(name, param, body)| Frag::Function { name, param, body }),
        2 => (ident(), ident()).prop_map(|(name, method)| Frag::Class { name, method }),
        2 => text().prop_map(Frag::Comment),
        1 => Just(Frag::Blank),
        1 => (ident(), 1usize..=8).prop_map(|(name, depth)| Frag::Nested { name, depth }),
        2 => ident().prop_map(Frag::Statement),
        1 => "\\PC{0,40}".prop_map(Frag::Garbage),
    ]
}

/// Uppercase the first character (Ruby constants must be capitalized for the
/// class to parse; other languages tolerate it).
fn constantize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => "K".to_string(),
    }
}

fn render(lang: &str, frags: &[Frag]) -> String {
    let mut out = String::new();
    for f in frags {
        out.push_str(&render_frag(lang, f));
    }
    out
}

fn render_frag(lang: &str, frag: &Frag) -> String {
    match frag {
        Frag::Function { name, param, body } => match lang {
            "c" => format!("int {name}(int {param}) {{\n  const char *s = \"{body}\";\n  return {param} + 1;\n}}\n"),
            "javascript" => format!("function {name}({param}) {{\n  return \"{body}\";\n}}\n"),
            "python" => format!("def {name}({param}):\n    x = \"{body}\"\n    return x\n"),
            "ruby" => format!("def {name}({param})\n  \"{body}\"\nend\n"),
            "rust" => format!("fn {name}({param}: u32) -> u32 {{\n    let _s = \"{body}\";\n    {param} + 1\n}}\n"),
            "typescript" => format!("function {name}({param}: string): string {{\n  return \"{body}\";\n}}\n"),
            _ => unreachable!(),
        },
        Frag::Class { name, method } => {
            let cls = constantize(name);
            match lang {
                "c" => format!("struct {name} {{\n  int x;\n}};\n"),
                "javascript" => format!("class {cls} {{\n  {method}() {{\n    return 1;\n  }}\n}}\n"),
                "python" => format!("class {cls}:\n    def {method}(self):\n        pass\n"),
                "ruby" => format!("class {cls}\n  def {method}\n  end\nend\n"),
                "rust" => format!("struct {cls} {{\n    x: u32,\n}}\n\nimpl {cls} {{\n    fn {method}(&self) -> u32 {{\n        self.x\n    }}\n}}\n"),
                "typescript" => format!("interface {cls}Shape {{\n  x: number;\n}}\nclass {cls} {{\n  {method}(): number {{\n    return 1;\n  }}\n}}\n"),
                _ => unreachable!(),
            }
        }
        Frag::Comment(t) => match lang {
            "c" => format!("/* {t} */\n"),
            "python" | "ruby" => format!("# {t}\n"),
            _ => format!("// {t}\n"),
        },
        Frag::Blank => "\n".to_string(),
        Frag::Nested { name, depth } => render_nested(lang, name, *depth),
        Frag::Statement(name) => match lang {
            "c" => format!("int {name}_v = 1;\n"),
            "javascript" => format!("const {name} = 1;\n"),
            "python" | "ruby" => format!("{name} = 1\n"),
            "rust" => format!("const {name}: u32 = 1;\n"),
            "typescript" => format!("const {name}: number = 1;\n"),
            _ => unreachable!(),
        },
        Frag::Garbage(g) => format!("{g}\n"),
    }
}

/// `depth` nested definitions (or blocks, for C) — parents and children are both
/// chunk candidates in most packs, so this exercises the nested-emission rule.
fn render_nested(lang: &str, name: &str, depth: usize) -> String {
    let mut s = String::new();
    match lang {
        "c" => {
            s.push_str(&format!("void {name}(void) {{\n"));
            for _ in 0..depth {
                s.push_str("{\n");
            }
            s.push_str("int x = 0;\n");
            for _ in 0..depth {
                s.push_str("}\n");
            }
            s.push_str("}\n");
        }
        "python" => {
            for i in 0..depth {
                s.push_str(&format!("{}def {name}{i}():\n", "    ".repeat(i)));
            }
            s.push_str(&format!("{}pass\n", "    ".repeat(depth)));
        }
        "ruby" => {
            for i in 0..depth {
                s.push_str(&format!("{}def {name}{i}\n", "  ".repeat(i)));
            }
            for i in (0..depth).rev() {
                s.push_str(&format!("{}end\n", "  ".repeat(i)));
            }
        }
        "rust" => {
            for i in 0..depth {
                s.push_str(&format!("{}fn {name}{i}() {{\n", "    ".repeat(i)));
            }
            s.push_str(&format!("{}let _x = 0;\n", "    ".repeat(depth)));
            for i in (0..depth).rev() {
                s.push_str(&format!("{}}}\n", "    ".repeat(i)));
            }
        }
        // javascript / typescript
        _ => {
            for i in 0..depth {
                s.push_str(&format!("{}function {name}{i}() {{\n", "  ".repeat(i)));
            }
            for i in (0..depth).rev() {
                s.push_str(&format!("{}}}\n", "  ".repeat(i)));
            }
        }
    }
    s
}

/// Input mutations: trailing whitespace per line, CRLF line endings, and a
/// stripped final newline (in that order, so CRLF applies to the padded text).
fn mutate(src: &str, crlf: bool, trailing_ws: bool, no_final_nl: bool) -> String {
    let mut s = src.to_string();
    if trailing_ws {
        s = s.replace('\n', " \t\n");
    }
    if crlf {
        s = s.replace('\n', "\r\n");
    }
    if no_final_nl {
        if s.ends_with("\r\n") {
            s.truncate(s.len() - 2);
        } else if s.ends_with('\n') {
            s.pop();
        }
    }
    s
}

// --- Invariant checkers ---

/// The code-chunker contract (src/chunker.rs), asserted for one input:
/// (a) line ranges 1-based, ordered, in-bounds for the input;
/// (b) pre-order emission — a later chunk is nested inside an earlier one or
///     starts at/after its end (nested chunks are documented and legal);
/// (c) determinism — two runs yield identical chunks, ids, and imports;
/// (d) `token_count` = the pinned max(1, floor(bytes/4)) rule over the content;
/// (e) round-trip — content is an exact byte slice of the input (a non-char
///     boundary would lossy-decode to U+FFFD and break substring equality), and
///     `chunk_id` is recomputable from the persisted fields.
fn check_code_invariants(chunker: &mut Chunker, path: &str, lang: &str, input: &str) {
    let a = chunker.chunk_file(path, input);
    let b = chunker.chunk_file(path, input);
    assert_eq!(a.chunks, b.chunks, "chunking must be deterministic ({lang})");
    assert_eq!(a.imports, b.imports, "import extraction must be deterministic ({lang})");
    assert!(!a.chunks.is_empty(), "every file yields at least one chunk ({lang})");

    let total_lines = newline_count(input) + 1;
    for c in &a.chunks {
        let ctx = format!("[{lang}] chunk {}..{} kind={:?}", c.start_line, c.end_line, c.kind);
        assert_eq!(c.file_path, path, "{ctx}");
        assert_eq!(c.language, lang, "{ctx}");
        assert!(c.start_line >= 1, "{ctx}: start_line is 1-based");
        assert!(c.start_line <= c.end_line, "{ctx}: start_line <= end_line");
        assert!(c.end_line <= total_lines, "{ctx}: end_line in-bounds of {total_lines} lines");
        assert!(input.contains(&c.content), "{ctx}: content must be an exact slice of the input");
        assert_eq!(
            newline_count(&c.content),
            c.end_line - c.start_line,
            "{ctx}: content newlines must match the line span"
        );
        assert_eq!(c.token_count, pinned_token_rule(&c.content), "{ctx}: pinned token rule");
        assert_eq!(
            c.chunk_id,
            chunk_id(path, c.start_line, c.end_line, c.content.as_bytes()),
            "{ctx}: chunk_id must be recomputable from the persisted fields"
        );
        assert_hex16(&c.chunk_id);
        assert!(
            matches!(c.chunk_type.as_str(), "function" | "class" | "module"),
            "{ctx}: unknown chunk_type {:?}",
            c.chunk_type
        );
        assert!(!c.kind.is_empty(), "{ctx}: kind is always set");
        assert!(c.embedding.is_empty(), "{ctx}: the chunker never embeds");
    }

    // The whole-file fallback is exactly one `module` chunk covering the file.
    if let Some(m) = a.chunks.iter().find(|c| c.chunk_type == "module") {
        assert_eq!(a.chunks.len(), 1, "[{lang}] module fallback must be the only chunk");
        assert_eq!(m.kind, "module");
        assert_eq!(m.start_line, 1);
        assert_eq!(m.end_line, total_lines, "[{lang}] fallback end_line = newline count + 1");
        assert_eq!(m.content, input, "[{lang}] fallback content is the whole file");
    }

    assert_preorder_nesting(lang, &a.chunks);
}

/// Pre-order emission: for every pair i < j, chunk j starts at or after chunk i,
/// and is either nested inside it or begins at/after its last line (siblings may
/// share a boundary line; a later chunk never *straddles* an earlier one).
fn assert_preorder_nesting(lang: &str, chunks: &[Chunk]) {
    for i in 0..chunks.len() {
        for j in (i + 1)..chunks.len() {
            let (ci, cj) = (&chunks[i], &chunks[j]);
            assert!(
                cj.start_line >= ci.start_line,
                "[{lang}] pre-order: chunk {j} ({}..{}) starts before chunk {i} ({}..{})",
                cj.start_line,
                cj.end_line,
                ci.start_line,
                ci.end_line
            );
            assert!(
                cj.end_line <= ci.end_line || cj.start_line >= ci.end_line,
                "[{lang}] chunk {j} ({}..{}) straddles chunk {i} ({}..{})",
                cj.start_line,
                cj.end_line,
                ci.start_line,
                ci.end_line
            );
        }
    }
}

/// The markdown heading-chunker contract (src/markdown.rs), asserted for one
/// input: determinism; blank input yields no chunks and non-blank input at least
/// one; every chunk's content is a non-empty, trailing-trimmed exact slice of the
/// input with in-bounds lines, `end_line = start_line + newlines(content)`, the
/// pinned token rule, and a recomputable `chunk_id`; sections are emitted in
/// order and never invert (a heading inside a blockquote/list may share its line
/// with the previous section's trimmed tail, so ranges may TOUCH, not overlap
/// beyond that boundary line); and every non-blank input line is covered by at
/// least one chunk.
fn check_markdown_invariants(path: &str, input: &str, budget: usize) {
    let a = chunk_markdown(path, input, budget);
    let b = chunk_markdown(path, input, budget);
    assert_eq!(a, b, "markdown chunking must be deterministic");

    if input.trim().is_empty() {
        assert!(a.is_empty(), "blank input must yield no chunks, got {a:?}");
        return;
    }
    assert!(!a.is_empty(), "non-blank input must yield at least one chunk: {input:?}");

    let total_lines = newline_count(input) + 1;
    for c in &a {
        let ctx = format!("chunk {}..{} kind={:?}", c.start_line, c.end_line, c.kind);
        assert_eq!(c.file_path, path, "{ctx}");
        assert!(!c.content.is_empty(), "{ctx}: content is never empty");
        assert_eq!(c.content, c.content.trim_end(), "{ctx}: content is trailing-trimmed");
        assert!(input.contains(&c.content), "{ctx}: content must be an exact slice of the input");
        assert!(c.start_line >= 1, "{ctx}: start_line is 1-based");
        assert!(c.start_line <= c.end_line, "{ctx}: start_line <= end_line");
        assert!(c.end_line <= total_lines, "{ctx}: end_line in-bounds of {total_lines} lines");
        assert_eq!(
            c.end_line - c.start_line,
            newline_count(&c.content),
            "{ctx}: end_line = start_line + newlines(content)"
        );
        assert_eq!(c.token_count, pinned_token_rule(&c.content), "{ctx}: pinned token rule");
        assert_eq!(
            c.chunk_id,
            chunk_id(path, c.start_line, c.end_line, c.content.as_bytes()),
            "{ctx}: chunk_id must be recomputable from the persisted fields"
        );
        assert_hex16(&c.chunk_id);
        if c.kind == PREAMBLE_KIND {
            assert_eq!(c.name, PREAMBLE_KIND, "{ctx}: preamble name is the sentinel");
        } else {
            assert!(
                c.name.ends_with(&c.kind),
                "{ctx}: breadcrumb {:?} must end with the heading text",
                c.name
            );
        }
    }

    assert_md_order_and_coverage(input, &a);
}

fn assert_md_order_and_coverage(input: &str, chunks: &[MarkdownChunk]) {
    // Ordered, non-inverting sections.
    for w in chunks.windows(2) {
        assert!(
            w[1].start_line >= w[0].end_line,
            "sections out of order: {}..{} then {}..{}",
            w[0].start_line,
            w[0].end_line,
            w[1].start_line,
            w[1].end_line
        );
    }
    // Every non-blank input line belongs to at least one section.
    for (idx, line) in input.split('\n').enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let ln = idx + 1;
        assert!(
            chunks.iter().any(|c| c.start_line <= ln && ln <= c.end_line),
            "non-blank line {ln} ({line:?}) is not covered by any chunk"
        );
    }
}

// --- Markdown generators ---

#[derive(Debug, Clone)]
enum MdFrag {
    Heading {
        level: usize,
        text: String,
    },
    Setext {
        level: usize,
        text: String,
    },
    Para(String),
    /// A fenced code block whose body contains `#`-leading lines — those must
    /// NOT become headings.
    Fence(String),
    Blank,
    Garbage(String),
}

fn md_text() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "[a-zA-Z0-9][a-zA-Z0-9 _.,#-]{0,23}",
        1 => Just("Café δ 名前 ☃".to_string()),
        1 => Just(String::new()), // an empty `##` heading is legal
    ]
}

fn md_frag() -> impl Strategy<Value = MdFrag> {
    prop_oneof![
        3 => (1usize..=6, md_text()).prop_map(|(level, text)| MdFrag::Heading { level, text }),
        1 => (1usize..=2, "[a-zA-Z][a-zA-Z0-9 ]{0,16}")
            .prop_map(|(level, text)| MdFrag::Setext { level, text }),
        3 => md_text().prop_map(MdFrag::Para),
        1 => md_text().prop_map(MdFrag::Fence),
        1 => Just(MdFrag::Blank),
        1 => "\\PC{0,40}".prop_map(MdFrag::Garbage),
    ]
}

fn render_md(frags: &[MdFrag]) -> String {
    let mut out = String::new();
    for f in frags {
        match f {
            MdFrag::Heading { level, text } => {
                if text.is_empty() {
                    out.push_str(&format!("{}\n", "#".repeat(*level)));
                } else {
                    out.push_str(&format!("{} {text}\n", "#".repeat(*level)));
                }
            }
            MdFrag::Setext { level, text } => {
                let underline = if *level == 1 { "====" } else { "----" };
                out.push_str(&format!("{text}\n{underline}\n"));
            }
            MdFrag::Para(t) => out.push_str(&format!("{t}\n\n")),
            MdFrag::Fence(t) => out.push_str(&format!("```sh\n# not a heading\n{t}\n```\n")),
            MdFrag::Blank => out.push('\n'),
            MdFrag::Garbage(g) => out.push_str(&format!("{g}\n")),
        }
    }
    out
}

fn md_budget() -> impl Strategy<Value = usize> {
    // 1 forces maximal splitting, 400 is the shipped default, the range walks
    // the split/roll-in boundary.
    prop_oneof![Just(1usize), Just(400usize), 1usize..=64]
}

// --- Properties ---

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// Invariants (a)-(e) for all six language packs over composed fragments and
    /// EOL/whitespace mutations. One generated structure drives every pack.
    #[test]
    fn code_chunker_invariants(
        frags in prop::collection::vec(frag(), 0..8),
        crlf in any::<bool>(),
        trailing_ws in any::<bool>(),
        no_final_nl in any::<bool>(),
    ) {
        let mut chunker = Chunker::new();
        for (lang, path) in LANGS {
            let input = mutate(&render(lang, &frags), crlf, trailing_ws, no_final_nl);
            check_code_invariants(&mut chunker, path, lang, &input);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The same invariants for the markdown heading-chunker, across heading
    /// depths, setext headings, preambles, fenced `#` lines, and split budgets.
    #[test]
    fn markdown_chunker_invariants(
        frags in prop::collection::vec(md_frag(), 0..10),
        budget in md_budget(),
        crlf in any::<bool>(),
        no_final_nl in any::<bool>(),
    ) {
        let input = mutate(&render_md(&frags), crlf, false, no_final_nl);
        check_markdown_invariants("knowledge/doc.md", &input, budget);
    }

    /// The pinned `cce.tokens/v1` estimator rule holds for ANY string, and
    /// `chunker::token_count` never diverges from it (they must agree to the byte
    /// — the persisted index, conformance, and the savings ledger all stand on it).
    #[test]
    fn token_estimator_matches_pinned_rule(s in "\\PC{0,64}") {
        prop_assert_eq!(estimate_tokens(&s), pinned_token_rule(&s) as u64);
        prop_assert_eq!(cce::chunker::token_count(&s) as u64, estimate_tokens(&s));
    }

    /// SPEC §4.1 tokenizer: tokens are non-empty maximal `[a-z0-9_]` runs (ASCII
    /// uppercase folded), present in the lowercased input, and deterministic.
    #[test]
    fn tokenizer_invariants(s in "\\PC{0,64}") {
        let toks = tokenize(&s);
        prop_assert_eq!(&toks, &tokenize(&s), "tokenize must be deterministic");
        let lower = s.to_ascii_lowercase();
        prop_assert_eq!(&toks, &tokenize(&lower), "tokenize is case-insensitive");
        for t in &toks {
            prop_assert!(!t.is_empty(), "no empty tokens");
            prop_assert!(
                t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "token {t:?} must be lowercase [a-z0-9_]"
            );
            prop_assert!(lower.contains(t.as_str()), "token {t:?} must appear in the input");
        }
    }
}

// --- Deterministic anchors for the corner cases the issue names explicitly ---

#[test]
fn empty_input_is_a_single_one_line_module_chunk_in_every_language() {
    let mut chunker = Chunker::new();
    for (lang, path) in LANGS {
        let fc = chunker.chunk_file(path, "");
        assert_eq!(fc.chunks.len(), 1, "[{lang}]");
        let c = &fc.chunks[0];
        assert_eq!(c.chunk_type, "module", "[{lang}]");
        assert_eq!((c.start_line, c.end_line), (1, 1), "[{lang}]");
        assert_eq!(c.content, "", "[{lang}]");
        assert_eq!(c.token_count, 1, "[{lang}] max(1, 0/4) = 1");
        assert_eq!(c.language, *lang);
    }
}

#[test]
fn comment_only_input_falls_back_to_a_whole_file_module_chunk() {
    let mut chunker = Chunker::new();
    for (lang, path) in LANGS {
        let input = match *lang {
            "c" => "/* only a comment */\n// and another\n",
            "python" | "ruby" => "# only a comment\n# and another\n",
            _ => "// only a comment\n// and another\n",
        };
        let fc = chunker.chunk_file(path, input);
        assert_eq!(fc.chunks.len(), 1, "[{lang}]");
        let c = &fc.chunks[0];
        assert_eq!(c.chunk_type, "module", "[{lang}]");
        assert_eq!(c.content, input, "[{lang}] fallback content is the whole file");
        assert_eq!(c.language, *lang);
        check_code_invariants(&mut chunker, path, lang, input);
    }
}

#[test]
fn markdown_heading_at_eof_without_newline_is_a_chunk() {
    let chunks = chunk_markdown("doc.md", "para\n# Tail", 400);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[1].kind, "Tail");
    assert_eq!(chunks[1].content, "# Tail");
    assert_eq!((chunks[1].start_line, chunks[1].end_line), (2, 2));
    check_markdown_invariants("doc.md", "para\n# Tail", 400);
}
