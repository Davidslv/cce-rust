//! # compress — L2 chunk compression (SPEC-V2.5 §2 + SPEC-V2.5-TUNING §A)
//!
//! **Why this file exists:** Returning full chunk bodies can cost more than a
//! targeted grep+read; the real win is serving a compressed view and letting the
//! agent expand on demand (Layer 7). This module owns that deterministic, AST-driven
//! reduction. It is a **retrieval/serialization-time transform ONLY** — the index and
//! store keep FULL chunk bodies; compression happens on the way OUT. So
//! `conformance.json`, `token_count`, `file_tokens`, and the Sync artifact are
//! untouched, and `expand_chunk` (Layer 7) recovers the exact `full` bytes by
//! re-fetching the stored chunk, not by inverting this transform.
//!
//! **What it is / does:** Defines `DetailLevel` (`signature` | `compact` | `full`)
//! and `compress`, which re-parses a chunk's OWN body with its language pack's
//! grammar and finds the outermost definition node. The `compact` view depends on
//! whether that node is a **container** (class / module / struct / impl / trait /
//! interface / enum) or a **leaf** (function / method):
//! - **container** → the STRUCTURAL compact (SPEC-V2.5-TUNING §A): the header + a
//!   leading doc + a deterministic list of the container's DIRECT members, each
//!   trimmed to its first (signature) line with the rest of that member elided by
//!   the byte-pinned `… (+N lines)` marker. Members are the pack's `function_types`
//!   (methods) plus its declared `member_node_types` (Rust fields/variants, TS/JS/C
//!   members, constants) plus any direct child whose leading token is in
//!   `member_line_prefixes` (the Ruby model DSL). The agent sees the whole table of
//!   contents — every method and association — without the bodies.
//! - **leaf** → signature + optional leading doc + the first non-trivial body line +
//!   the elision marker (unchanged from the first cut).
//!
//! A chunk with no resolvable pack, or one whose body does not re-parse to a
//! definition (e.g. a bare JS/TS class method, which is not a valid standalone
//! program), falls back to a language-neutral first-line rule. Every output is
//! byte-pinned.
//!
//! **Responsibilities:**
//! - Own `DetailLevel`, the byte-pinned `ELISION_MARKER` grammar, and `compress`.
//! - Own the container-vs-leaf `compact` split and per-member rendering.
//! - Reuse each pack's declared node types/prefixes — it names NO language itself.
//! - It does NOT read the store, rank, or expand — callers wire those in.

use crate::packs::{visit_pre, LanguagePack, Registry};
use tree_sitter::{Node, Parser};

/// The L2 detail level a `context_search` result is served at (SPEC-V2.5 §2/§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLevel {
    /// Declaration line(s) only — the header before the body block.
    Signature,
    /// Signature + leading doc (if any) + first non-trivial body line + the elision
    /// marker. The config default (SPEC-V2.5 §5, `retrieval.detail`).
    Compact,
    /// Today's whole-chunk-body behaviour (no reduction).
    Full,
}

impl DetailLevel {
    /// Parse the config/tool string form (case-insensitive). Unknown ⇒ `None`.
    pub fn parse(s: &str) -> Option<DetailLevel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "signature" => Some(DetailLevel::Signature),
            "compact" => Some(DetailLevel::Compact),
            "full" => Some(DetailLevel::Full),
            _ => None,
        }
    }

    /// The canonical string form.
    pub const fn as_str(&self) -> &'static str {
        match self {
            DetailLevel::Signature => "signature",
            DetailLevel::Compact => "compact",
            DetailLevel::Full => "full",
        }
    }
}

/// The byte-pinned elision marker prefix. The full marker for `n` elided lines is
/// `… (+N lines)` — a leading U+2026 HORIZONTAL ELLIPSIS, then `" (+"`, the count,
/// and `" lines)"`. Both engines emit these exact bytes. See `elision_marker`.
pub const ELISION_MARKER_PREFIX: &str = "… (+";

/// Build the byte-pinned elision marker for `n` elided (omitted) lines: the exact
/// string `… (+N lines)`. The word is always `lines` (no singular special case).
pub fn elision_marker(n: usize) -> String {
    format!("{ELISION_MARKER_PREFIX}{n} lines)")
}

/// Compress a chunk's FULL `content` to the requested `level`, resolving the
/// language pack from `file_path` by extension. Pure and deterministic; `Full`
/// returns the content verbatim (so it round-trips to the byte).
pub fn compress(registry: &Registry, file_path: &str, content: &str, level: DetailLevel) -> String {
    if level == DetailLevel::Full {
        return content.to_string();
    }
    match registry.pack_for(file_path) {
        Some(pack) => {
            compress_with_pack(pack, content, level).unwrap_or_else(|| generic(content, level))
        }
        None => generic(content, level),
    }
}

/// AST-driven compression against a resolved pack. Returns `None` (⇒ generic
/// fallback) when the body does not re-parse to a definition node.
fn compress_with_pack(
    pack: &dyn LanguagePack,
    content: &str,
    level: DetailLevel,
) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&pack.grammar()).ok()?;
    let tree = parser.parse(content, None)?;
    let def = find_def(tree.root_node(), pack)?;

    // Signature = the header from the definition's start up to its first body child
    // (the block/member-list), right-trimmed. When there is no body child, the whole
    // definition is the signature.
    let body_child = first_child_in(def, pack.body_node_types());
    let sig_end = body_child.map(|b| b.start_byte()).unwrap_or(def.end_byte());
    let signature = content.get(def.start_byte()..sig_end)?.trim_end();
    if level == DetailLevel::Signature {
        return Some(signature.to_string());
    }

    let body = match body_child {
        Some(b) => b,
        // No body to elide: compact == signature.
        None => return Some(signature.to_string()),
    };

    // The compact view splits on shape: a container keeps its members (its table of
    // contents); a leaf keeps its first body line (SPEC-V2.5-TUNING §A).
    if pack.class_types().contains(&def.kind()) {
        Some(compact_container(pack, content, signature, body))
    } else {
        Some(compact_leaf(pack, content, signature, body))
    }
}

/// STRUCTURAL container compact (SPEC-V2.5-TUNING §A): the header, a leading doc if
/// present, then every DIRECT member trimmed to its signature line (bodies elided).
fn compact_container(
    pack: &dyn LanguagePack,
    content: &str,
    signature: &str,
    body: Node,
) -> String {
    let named: Vec<Node> = {
        let mut cur = body.walk();
        body.named_children(&mut cur).collect()
    };
    let mut lines: Vec<String> = vec![signature.to_string()];

    // Leading doc: the first named body element (unwrapping a Python-style
    // `expression_statement`), when its kind is one the pack declares as a doc. It is
    // then skipped by the member pass.
    let mut start = 0usize;
    if let Some(first) = named.first() {
        let doc_node = unwrap_doc(*first);
        if pack.doc_node_types().contains(&doc_node.kind()) {
            if let Some(text) = node_first_line(doc_node, content) {
                lines.push(text);
                start = 1;
            }
        }
    }

    // Members: each direct child kept by kind (methods via `function_types`, other
    // members via `member_node_types`) or by leading token (`member_line_prefixes`),
    // rendered as its trimmed first line plus a `… (+N lines)` marker for the rest.
    for member in &named[start..] {
        if let Some(rendered) = render_member(pack, *member, content) {
            lines.extend(rendered);
        }
    }

    lines.join("\n")
}

/// LEAF compact (unchanged): signature + a leading doc (if any) + the first
/// non-trivial body line + the elision marker for the lines neither shown.
fn compact_leaf(pack: &dyn LanguagePack, content: &str, signature: &str, body: Node) -> String {
    let mut lines: Vec<String> = vec![signature.to_string()];
    let mut shown = line_span(signature);

    let named: Vec<Node> = {
        let mut cur = body.walk();
        body.named_children(&mut cur).collect()
    };

    // Leading doc: the first named body element, unwrapping a Python-style
    // `expression_statement` wrapper, when its kind is one the pack declares.
    let mut next = 0usize;
    if let Some(first) = named.first() {
        let doc_node = unwrap_doc(*first);
        if pack.doc_node_types().contains(&doc_node.kind()) {
            if let Some(text) = node_first_line(doc_node, content) {
                lines.push(text);
                shown += 1;
                next = 1;
            }
        }
    }

    // First non-trivial body line: the first named body element after any doc.
    if let Some(stmt) = named.get(next) {
        if let Some(text) = node_first_line(*stmt, content) {
            lines.push(text);
            shown += 1;
        }
    }

    with_elision(lines, shown, content)
}

/// Render one direct-child member of a container for the structural compact, or
/// `None` when the child is not a kept member. A kept member is rendered as its
/// trimmed first line; if the member spans more than one line, the byte-pinned
/// `… (+N lines)` marker for the remaining `span − 1` lines follows on its own line.
fn render_member(pack: &dyn LanguagePack, node: Node, content: &str) -> Option<Vec<String>> {
    if !is_member(pack, node, content) {
        return None;
    }
    let first = node_first_line(node, content)?;
    let span = node_line_span(node);
    let mut out = vec![first];
    if span > 1 {
        out.push(elision_marker(span - 1));
    }
    Some(out)
}

/// Whether `node` — a direct child of a container body — is a kept member: a method
/// (`function_types`), a declared non-method member (`member_node_types`), or a
/// statement whose leading token is a declared `member_line_prefix` (the Ruby DSL).
fn is_member(pack: &dyn LanguagePack, node: Node, content: &str) -> bool {
    let kind = node.kind();
    if pack.function_types().contains(&kind) || pack.member_node_types().contains(&kind) {
        return true;
    }
    if pack.member_line_prefixes().is_empty() {
        return false;
    }
    match node_first_line(node, content) {
        Some(line) => {
            let token: String =
                line.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            pack.member_line_prefixes().contains(&token.as_str())
        }
        None => false,
    }
}

/// Number of physical source lines a node spans (inclusive): `end_row − start_row + 1`.
fn node_line_span(node: Node) -> usize {
    node.end_position().row - node.start_position().row + 1
}

/// Language-neutral fallback: the first non-blank line, plus (for `compact`) an
/// elision marker for the remaining lines. Used for chunks with no pack (module
/// fallbacks) or whose body does not re-parse to a definition.
fn generic(content: &str, level: DetailLevel) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let Some(i) = lines.iter().position(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    let first = lines[i].trim_end().to_string();
    if level == DetailLevel::Signature {
        return first;
    }
    let elided = lines.len().saturating_sub(i + 1);
    if elided > 0 {
        format!("{first}\n{}", elision_marker(elided))
    } else {
        first
    }
}

/// Join `lines` and append the elision marker for `total − shown` lines (omitted
/// when nothing was elided). No trailing newline.
fn with_elision(lines: Vec<String>, shown: usize, content: &str) -> String {
    let total = content.lines().count();
    let elided = total.saturating_sub(shown);
    let mut out = lines.join("\n");
    if elided > 0 {
        out.push('\n');
        out.push_str(&elision_marker(elided));
    }
    out
}

/// The first named node, in pre-order, whose kind is a function or class type the
/// pack declares (the outermost definition of a re-parsed chunk body).
fn find_def<'a>(root: Node<'a>, pack: &dyn LanguagePack) -> Option<Node<'a>> {
    let mut found: Option<Node<'a>> = None;
    visit_pre(root, &mut |n| {
        if found.is_none()
            && n.is_named()
            && (pack.function_types().contains(&n.kind()) || pack.class_types().contains(&n.kind()))
        {
            found = Some(n);
        }
    });
    found
}

/// The first direct child of `node` whose kind is in `kinds` (order-preserving).
fn first_child_in<'a>(node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    let mut cur = node.walk();
    let found = node.children(&mut cur).find(|c| kinds.contains(&c.kind()));
    found
}

/// Unwrap a docstring wrapper: a Python docstring is a `string` inside an
/// `expression_statement`, so descend one level for that kind; otherwise identity.
fn unwrap_doc(node: Node) -> Node {
    if node.kind() == "expression_statement" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    }
}

/// The first physical line of `node`'s text, both-side trimmed. `None` when empty.
fn node_first_line(node: Node, content: &str) -> Option<String> {
    let text = content.get(node.start_byte()..node.end_byte())?;
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Number of physical lines `text` spans (newline count + 1; empty ⇒ 1).
fn line_span(text: &str) -> usize {
    text.matches('\n').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::chunk_with_pack;
    use crate::packs::default_registry;

    #[test]
    fn detail_level_parse_and_render_round_trip() {
        for level in [DetailLevel::Signature, DetailLevel::Compact, DetailLevel::Full] {
            assert_eq!(DetailLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(DetailLevel::parse("COMPACT"), Some(DetailLevel::Compact));
        assert_eq!(DetailLevel::parse(" full "), Some(DetailLevel::Full));
        assert_eq!(DetailLevel::parse("verbose"), None);
    }

    #[test]
    fn elision_marker_is_byte_pinned() {
        assert_eq!(elision_marker(3), "… (+3 lines)");
        assert_eq!(elision_marker(0), "… (+0 lines)");
        assert_eq!(elision_marker(1), "… (+1 lines)"); // always plural, by design
    }

    #[test]
    fn full_round_trips_to_the_byte() {
        let reg = default_registry();
        let body = "pub fn f(x: u32) -> u32 {\n    x + 1\n}";
        assert_eq!(compress(&reg, "m.rs", body, DetailLevel::Full), body);
    }

    #[test]
    fn rust_function_signature_and_compact() {
        let reg = default_registry();
        let body =
            "pub fn build_index() -> HashMap<String, u32> {\n    let m = HashMap::new();\n    m\n}";
        assert_eq!(
            compress(&reg, "s.rs", body, DetailLevel::Signature),
            "pub fn build_index() -> HashMap<String, u32>"
        );
        assert_eq!(
            compress(&reg, "s.rs", body, DetailLevel::Compact),
            "pub fn build_index() -> HashMap<String, u32>\nlet m = HashMap::new();\n… (+2 lines)"
        );
    }

    #[test]
    fn rust_struct_and_impl_signatures() {
        let reg = default_registry();
        let s = "pub struct Store {\n    data: u32,\n    name: String,\n}";
        assert_eq!(compress(&reg, "s.rs", s, DetailLevel::Signature), "pub struct Store");
        // Structural compact lists EVERY field (the container's table of contents),
        // not just the first line (SPEC-V2.5-TUNING §A).
        assert_eq!(
            compress(&reg, "s.rs", s, DetailLevel::Compact),
            "pub struct Store\ndata: u32\nname: String"
        );
        let imp = "impl Store {\n    pub fn get(&self) -> u32 {\n        0\n    }\n}";
        assert_eq!(compress(&reg, "s.rs", imp, DetailLevel::Signature), "impl Store");
        // The impl's method is kept as its signature line, its body elided.
        assert_eq!(
            compress(&reg, "s.rs", imp, DetailLevel::Compact),
            "impl Store\npub fn get(&self) -> u32 {\n… (+2 lines)"
        );
    }

    #[test]
    fn rust_enum_variants_are_listed_in_structural_compact() {
        let reg = default_registry();
        let e = "pub enum Kind {\n    Alpha,\n    Beta(u32),\n}";
        assert_eq!(
            compress(&reg, "s.rs", e, DetailLevel::Compact),
            "pub enum Kind\nAlpha\nBeta(u32)"
        );
    }

    #[test]
    fn ruby_model_dsl_lines_appear_in_structural_compact() {
        // The exact regression case (SPEC-V2.5-TUNING §A): a model whose associations
        // and validations MUST survive compact so the agent need not re-search.
        let reg = default_registry();
        let model = "class Case < ApplicationRecord\n  belongs_to :assignee\n  has_many :comments\n  validates :title, presence: true\n\n  def close!\n    update!(closed: true)\n  end\nend";
        assert_eq!(
            compress(&reg, "case.rb", model, DetailLevel::Compact),
            "class Case < ApplicationRecord\nbelongs_to :assignee\nhas_many :comments\nvalidates :title, presence: true\ndef close!\n… (+2 lines)"
        );
    }

    #[test]
    fn leaf_function_compact_is_unchanged_by_the_container_split() {
        // A method inside a class re-parsed as a standalone chunk is a leaf, so it
        // keeps the signature + first-body-line form, not the structural form.
        let reg = default_registry();
        let body =
            "pub fn build_index() -> HashMap<String, u32> {\n    let m = HashMap::new();\n    m\n}";
        assert_eq!(
            compress(&reg, "s.rs", body, DetailLevel::Compact),
            "pub fn build_index() -> HashMap<String, u32>\nlet m = HashMap::new();\n… (+2 lines)"
        );
    }

    #[test]
    fn python_docstring_is_kept_in_compact() {
        let reg = default_registry();
        let body = "def f(x):\n    \"\"\"Doc line.\"\"\"\n    y = x + 1\n    return y";
        assert_eq!(
            compress(&reg, "m.py", body, DetailLevel::Compact),
            "def f(x):\n\"\"\"Doc line.\"\"\"\ny = x + 1\n… (+1 lines)"
        );
    }

    #[test]
    fn bare_js_method_falls_back_to_generic_first_line() {
        // A standalone `load() {…}` is not a valid JS program; the compressor uses
        // the language-neutral first-line rule rather than crashing.
        let reg = default_registry();
        let body = "load() {\n    return readConfig(\".\");\n  }";
        assert_eq!(compress(&reg, "a.js", body, DetailLevel::Signature), "load() {");
        assert_eq!(compress(&reg, "a.js", body, DetailLevel::Compact), "load() {\n… (+2 lines)");
    }

    #[test]
    fn module_fallback_chunk_uses_generic_rule() {
        let reg = default_registry();
        let body = "# Notes\nsome text\nmore text";
        assert_eq!(compress(&reg, "notes.md", body, DetailLevel::Signature), "# Notes");
        assert_eq!(compress(&reg, "notes.md", body, DetailLevel::Compact), "# Notes\n… (+2 lines)");
    }

    #[test]
    fn compress_matches_chunker_output_for_a_real_chunk() {
        // Compress operates on the exact bytes the chunker persisted (round-trip
        // safety): drive it from a real chunk of the Ruby pack's own sample.
        let reg = default_registry();
        let pack = reg.pack_for("x.rb").unwrap();
        let fc = chunk_with_pack(pack, "x.rb", pack.sample());
        let method = fc.chunks.iter().find(|c| c.chunk_type == "function").unwrap();
        // Signature is the `def …` header with no body.
        let sig = compress(&reg, "x.rb", &method.content, DetailLevel::Signature);
        assert!(sig.starts_with("def "), "got: {sig}");
        assert!(!sig.contains('\n'), "signature should be the header only: {sig}");
        // Full is byte-identical to what the store holds.
        assert_eq!(compress(&reg, "x.rb", &method.content, DetailLevel::Full), method.content);
    }
}
