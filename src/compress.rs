//! # compress — L2 chunk compression (SPEC-V2.5 §2, the headline layer)
//!
//! **Why this file exists:** Returning full chunk bodies can cost more than a
//! targeted grep+read; the real win is serving **signatures + a docstring + the
//! first body line** and letting the agent expand on demand (Layer 7). This module
//! owns that deterministic, AST-driven reduction. It is a **retrieval/serialization-
//! time transform ONLY** — the index and store keep FULL chunk bodies; compression
//! happens on the way OUT. So `conformance.json`, `token_count`, `file_tokens`, and
//! the Sync artifact are untouched, and `expand_chunk` (Layer 7) recovers the exact
//! `full` bytes by re-fetching the stored chunk, not by inverting this transform.
//!
//! **What it is / does:** Defines `DetailLevel` (`signature` | `compact` | `full`)
//! and `compress`, which re-parses a chunk's OWN body with its language pack's
//! grammar, finds the outermost definition node, and — using the node-type sets the
//! pack declares (`body_node_types`, `doc_node_types`) — extracts the signature
//! header, an optional leading doc, and the first non-trivial body line. A chunk
//! with no resolvable pack, or one whose body does not re-parse to a definition
//! (e.g. a bare JS/TS class method, which is not a valid standalone program), falls
//! back to a language-neutral first-line rule. Every output is byte-pinned.
//!
//! **Responsibilities:**
//! - Own `DetailLevel`, the byte-pinned `ELISION_MARKER` grammar, and `compress`.
//! - Reuse each pack's declared node types — it names NO language itself.
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

    // Compact = signature + leading doc (if any) + first non-trivial body line +
    // the elision marker for the lines neither shown.
    let mut lines: Vec<String> = vec![signature.to_string()];
    let mut shown = line_span(signature);

    let body = match body_child {
        Some(b) => b,
        // No body to elide: compact == signature.
        None => return Some(signature.to_string()),
    };

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

    Some(with_elision(lines, shown, content))
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
        assert_eq!(
            compress(&reg, "s.rs", s, DetailLevel::Compact),
            "pub struct Store\ndata: u32\n… (+2 lines)"
        );
        let imp = "impl Store {\n    pub fn get(&self) -> u32 {\n        0\n    }\n}";
        assert_eq!(compress(&reg, "s.rs", imp, DetailLevel::Signature), "impl Store");
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
