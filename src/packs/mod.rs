//! # packs — the pluggable LanguagePack abstraction and registry
//!
//! **Why this file exists:** SPEC-V2 §1 requires the core chunker/importer to
//! hold *zero* language-specific knowledge. All of that knowledge moves into
//! self-contained **language packs**; the core only ever talks to a pack through
//! this interface. Adding a language then means: add one pack file, register it,
//! pass validation — no core edits.
//!
//! **What it is / does:** Declares the `LanguagePack` trait (what the engine
//! needs to know about a language), the `PackExpected` self-test contract, small
//! tree-walking / import helpers every pack reuses, and the `Registry` that owns
//! the packs and resolves a file path to its pack by extension.
//!
//! **Responsibilities:**
//! - Own the `LanguagePack` trait and `PackExpected`.
//! - Own the `Registry` (register with duplicate-extension rejection, resolve by
//!   extension, list all) and the canonical `default_registry` of six packs.
//! - Own the shared, language-neutral helpers (`visit_pre`, `push_unique`,
//!   `node_text`, `dedup`) — this file names NO language and no extension.
//! - It does NOT chunk, embed, rank, or persist, and it does NOT know any single
//!   language's node types (those live inside each pack).

pub mod validators;

mod c;
mod javascript;
mod python;
mod ruby;
mod rust;
mod typescript;

use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// What a pack's `sample` must produce, checked by the behavioural self-test
/// (SPEC-V2 §5 Layer 3). Counts are minimums; `kinds` must all be present; and
/// `imports` must match `extract_imports(sample)` **exactly**.
#[derive(Debug, Clone)]
pub struct PackExpected {
    /// Minimum number of `function` chunks the sample must yield.
    pub min_functions: usize,
    /// Minimum number of `class` chunks the sample must yield.
    pub min_classes: usize,
    /// Node-type `kind`s that must all appear among the sample's chunks.
    pub kinds: &'static [&'static str],
    /// The exact, ordered, de-duplicated import list `extract_imports` must return.
    pub imports: &'static [&'static str],
}

/// A first-class language pack: everything the engine needs to know about one
/// language, in one self-contained unit (SPEC-V2 §1). Every method is pure and
/// side-effect free; the pack carries no state.
pub trait LanguagePack {
    /// Unique lowercase id, e.g. `"ruby"`.
    fn name(&self) -> &'static str;

    /// File extensions this pack claims — leading dot, lowercase, e.g. `[".rb"]`.
    fn extensions(&self) -> &'static [&'static str];

    /// The tree-sitter grammar to parse this language with.
    fn grammar(&self) -> Language;

    /// AST node-type strings that become `function` chunks.
    fn function_types(&self) -> &'static [&'static str];

    /// AST node-type strings that become `class` chunks.
    fn class_types(&self) -> &'static [&'static str];

    /// AST node-type strings the pack inspects during import extraction. Declared
    /// so the grammar-binding lint (Layer 2) can verify they are real node kinds.
    fn import_node_types(&self) -> &'static [&'static str];

    /// AST node-type strings that are the *body* of a definition — the block or
    /// member list a declaration's header stops before. L2 chunk compression
    /// (SPEC-V2.5 §2) forms the signature view by keeping the bytes from a
    /// definition node's start up to its first body child, and eliding the rest.
    /// e.g. Rust `block`/`declaration_list`, Python `block`, Ruby `body_statement`.
    /// Declared here (not hard-coded in the compressor) so the grammar-binding lint
    /// verifies they are real node kinds. Default: none (chunk compression then
    /// falls back to the language-neutral first-line rule — see `crate::compress`).
    fn body_node_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// AST node-type strings that count as a *leading doc* when one is the first
    /// named element inside a definition body — the docstring / doc-comment L2
    /// keeps in the compact view (SPEC-V2.5 §2). A docstring wrapped in an
    /// `expression_statement` (Python) is unwrapped one level before matching, so a
    /// pack declares the inner kind (`string`). e.g. Ruby/JS/TS/C `comment`, Rust
    /// `line_comment`/`block_comment`, Python `string`. Default: none.
    fn doc_node_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// AST node-type strings that, as a **direct child** of a container's body, are
    /// kept as member declaration lines in the STRUCTURAL `compact` view of a
    /// container chunk (SPEC-V2.5-TUNING §A). Methods are already kept via
    /// `function_types` and need NOT be repeated here; this set declares the
    /// NON-method members a language wants in the skeleton — e.g. Rust
    /// `field_declaration`/`enum_variant`/`const_item`, TS `public_field_definition`/
    /// `method_signature`/`property_signature`/`enum_assignment`, JS `field_definition`,
    /// C `field_declaration`/`enumerator`. Declared here (not hard-coded in the
    /// compressor) so the grammar-binding lint (Layer 2) verifies they are real node
    /// kinds. Default: none (a container's skeleton then lists only its methods).
    fn member_node_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Leading-token line prefixes that mark a direct-child statement as a kept
    /// member in the STRUCTURAL `compact` view, for members that are ordinary calls
    /// with no distinguishing node kind — chiefly the **Ruby model DSL**
    /// (`has_many`, `belongs_to`, `validates`, `scope`, `enum`, `delegate`, …). A
    /// direct child whose trimmed first line's leading identifier equals one of these
    /// is kept (SPEC-V2.5-TUNING §A, "the lines a 'what are its associations'
    /// question needs"). These are source tokens, NOT node kinds, so the Layer-2
    /// grammar-binding lint does not check them. Default: none.
    fn member_line_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// Ordered, de-duplicated module/include names imported by `source`.
    /// Must never panic; on any trouble it returns what it has so far.
    fn extract_imports(&self, root: Node, source: &[u8]) -> Vec<String>;

    /// A small source snippet in this language — its self-test fixture (SPEC-V2 §6).
    fn sample(&self) -> &'static str;

    /// What `sample` must produce (SPEC-V2 §6), checked by the Layer-3 self-test.
    fn expected(&self) -> PackExpected;
}

/// The set of packs, and file→pack resolution by extension (SPEC-V2 §1.1).
pub struct Registry {
    packs: Vec<Box<dyn LanguagePack>>,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Registry { packs: Vec::new() }
    }

    /// Register a pack. Rejects a pack whose name is already taken or whose
    /// extension is already claimed by another pack (SPEC-V2 §1.1 / §5 Layer 1).
    pub fn register(&mut self, pack: Box<dyn LanguagePack>) -> Result<(), String> {
        for existing in &self.packs {
            if existing.name() == pack.name() {
                return Err(format!(
                    "[pack:{}] name is already registered; each pack name must be unique.",
                    pack.name()
                ));
            }
            for ext in pack.extensions() {
                if existing.extensions().contains(ext) {
                    return Err(format!(
                        "[pack:{}] extension \"{}\" already claimed by pack \"{}\"; each \
                         extension maps to exactly one pack.",
                        pack.name(),
                        ext,
                        existing.name()
                    ));
                }
            }
        }
        self.packs.push(pack);
        Ok(())
    }

    /// Resolve a file path to its pack by lowercased extension, or `None`.
    pub fn pack_for(&self, path: &str) -> Option<&dyn LanguagePack> {
        let ext = extension_of(path)?;
        self.packs.iter().find(|p| p.extensions().contains(&ext.as_str())).map(|p| p.as_ref())
    }

    /// All registered packs, in registration order.
    pub fn all(&self) -> &[Box<dyn LanguagePack>] {
        &self.packs
    }
}

impl Default for Registry {
    fn default() -> Self {
        default_registry()
    }
}

/// The canonical registry of the six shipped packs (SPEC-V2 §2), in a stable
/// order. Runs the cheap Layer-1 checks at construction (duplicate extension) via
/// `register`, and panics on a duplicate — that is a programming error, and the
/// fail-fast startup path (SPEC-V2 §5) surfaces it immediately.
pub fn default_registry() -> Registry {
    let mut reg = Registry::new();
    let packs: Vec<Box<dyn LanguagePack>> = vec![
        Box::new(python::PythonPack),
        Box::new(javascript::JavaScriptPack),
        Box::new(ruby::RubyPack),
        Box::new(rust::RustPack),
        Box::new(typescript::TypeScriptPack),
        Box::new(c::CPack),
    ];
    for p in packs {
        reg.register(p).expect("default packs must register without conflict");
    }
    reg
}

// --- Shared, language-neutral helpers (no language named below) ---

/// The lowercased extension of a path, with a leading dot, e.g. `"a/B.RB"` ->
/// `".rb"`. `None` when the final path segment has no `.`.
pub fn extension_of(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = name.rfind('.')?;
    if dot == 0 {
        // A dotfile like ".gitignore" has no extension.
        return None;
    }
    Some(name[dot..].to_ascii_lowercase())
}

/// Node text as a string slice from the source bytes (empty on invalid UTF-8).
pub fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// Depth-first pre-order visit: call `f` on `node`, then each child in order.
///
/// Deliberately iterative (a `TreeCursor` loop, not recursion): a pathological
/// input — thousands of nested parens/braces — yields a tree deep enough to
/// overflow the stack of a per-node recursive walk (a 2 MiB test thread dies
/// around depth 1–2k), killing the process with SIGSEGV (issue #49). tree-sitter
/// itself parses such input fine; only the walk must not recurse.
pub fn visit_pre<'a>(node: Node<'a>, f: &mut impl FnMut(Node<'a>)) {
    let mut cursor = node.walk();
    'outer: loop {
        f(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'outer;
            }
            if !cursor.goto_parent() {
                // Back at the walk root: every node has been visited.
                break 'outer;
            }
        }
    }
}

/// Push `name` onto `out` iff it is non-empty and not already seen (first-seen
/// order preserved). The `seen` set carries de-duplication across calls.
pub fn push_unique(out: &mut Vec<String>, seen: &mut HashSet<String>, name: &str) {
    if !name.is_empty() && seen.insert(name.to_string()) {
        out.push(name.to_string());
    }
}

/// The string module specifier of an ECMAScript-family `import … from "x"`
/// statement (shared by the JavaScript and TypeScript packs). Reads the `source`
/// field's inner `string_fragment` when present, else strips the surrounding
/// quotes from the raw text. `None` when the statement has no source.
pub fn import_source_string(import_node: Node, src: &[u8]) -> Option<String> {
    let source = import_node.child_by_field_name("source")?;
    let mut fragment: Option<String> = None;
    let mut cursor = source.walk();
    for child in source.children(&mut cursor) {
        if child.kind() == "string_fragment" {
            fragment = Some(node_text(child, src).to_string());
        }
    }
    Some(fragment.unwrap_or_else(|| {
        node_text(source, src).trim_matches(|c| c == '\'' || c == '"' || c == '`').to_string()
    }))
}

/// First path segment of an ECMAScript module specifier, ignoring `.`/`..`
/// (e.g. `"./store"` -> `store`, `"react"` -> `react`). A scoped package keeps
/// its scope (`"@scope/pkg"` -> `@scope/pkg`). Shared by JS and TypeScript.
pub fn first_specifier_segment(spec: &str) -> String {
    let segs: Vec<&str> =
        spec.split('/').filter(|s| !s.is_empty() && *s != "." && *s != "..").collect();
    match segs.first() {
        None => String::new(),
        Some(first) if first.starts_with('@') && segs.len() >= 2 => {
            format!("{first}/{}", segs[1])
        }
        Some(first) => (*first).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_lowercased_with_leading_dot() {
        assert_eq!(extension_of("a/B.RB"), Some(".rb".to_string()));
        assert_eq!(extension_of("main.rs"), Some(".rs".to_string()));
        assert_eq!(extension_of("dir/x.TSX"), Some(".tsx".to_string()));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of(".gitignore"), None);
    }

    #[test]
    fn default_registry_has_six_packs_with_unique_names() {
        let reg = default_registry();
        assert_eq!(reg.all().len(), 6);
        let mut names: Vec<&str> = reg.all().iter().map(|p| p.name()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["c", "javascript", "python", "ruby", "rust", "typescript"]);
    }

    #[test]
    fn resolves_files_to_packs_by_extension() {
        let reg = default_registry();
        assert_eq!(reg.pack_for("a/b/foo.rb").map(|p| p.name()), Some("ruby"));
        assert_eq!(reg.pack_for("main.RS").map(|p| p.name()), Some("rust"));
        assert_eq!(reg.pack_for("x.tsx").map(|p| p.name()), Some("typescript"));
        assert_eq!(reg.pack_for("y.h").map(|p| p.name()), Some("c"));
        assert_eq!(reg.pack_for("mod.mjs").map(|p| p.name()), Some("javascript"));
        assert_eq!(reg.pack_for("readme.md").map(|p| p.name()), None);
        assert_eq!(reg.pack_for("noext").map(|p| p.name()), None);
    }

    /// A pack that collides on an already-claimed extension for the duplicate test.
    struct DupPack;
    impl LanguagePack for DupPack {
        fn name(&self) -> &'static str {
            "ruby-legacy"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &[".rb"]
        }
        fn grammar(&self) -> Language {
            tree_sitter_ruby::LANGUAGE.into()
        }
        fn function_types(&self) -> &'static [&'static str] {
            &["method"]
        }
        fn class_types(&self) -> &'static [&'static str] {
            &["class"]
        }
        fn import_node_types(&self) -> &'static [&'static str] {
            &[]
        }
        fn extract_imports(&self, _root: Node, _src: &[u8]) -> Vec<String> {
            Vec::new()
        }
        fn sample(&self) -> &'static str {
            "class X\nend\n"
        }
        fn expected(&self) -> PackExpected {
            PackExpected { min_functions: 0, min_classes: 1, kinds: &["class"], imports: &[] }
        }
    }

    #[test]
    fn register_rejects_duplicate_extension_with_helpful_message() {
        let mut reg = default_registry();
        let err = reg.register(Box::new(DupPack)).expect_err("dup extension must be rejected");
        assert!(err.contains("[pack:ruby-legacy]"), "{err}");
        assert!(err.contains("already claimed by pack \"ruby\""), "{err}");
    }

    #[test]
    fn register_rejects_duplicate_name() {
        let mut reg = Registry::new();
        reg.register(Box::new(python::PythonPack)).unwrap();
        let err = reg.register(Box::new(python::PythonPack)).expect_err("dup name rejected");
        assert!(err.contains("name is already registered"), "{err}");
    }
}
