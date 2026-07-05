//! # chunker — tree-sitter AST chunking, chunk IDs, and import extraction
//!
//! **Why this file exists:** Searching whole files is wasteful; SPEC §4.2 wants
//! each file split into the functions/classes it defines so retrieval returns
//! precise snippets. This module turns a file's bytes into `Chunk`s and pulls
//! the import edges used by the graph.
//!
//! **What it is / does:** Resolves a file's language by extension, parses Python
//! and JavaScript with tree-sitter, walks the tree depth-first emitting a chunk
//! for every function/class node (nested included), falls back to a whole-file
//! `module` chunk otherwise, computes the exact cross-language `chunk_id`
//! (SPEC §4.3) and `token_count` (SPEC §4.4), and extracts import module names.
//!
//! **Responsibilities:**
//! - Own `Chunk`, language resolution, the tree walk, the fallback rule.
//! - Own `chunk_id` and `token_count` (byte-exact).
//! - Own import extraction; failures return `[]` and never crash indexing.
//! - It does NOT embed, rank, or persist.

use crate::config::CHARS_PER_TOKEN;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tree_sitter::{Node, Parser};

/// A single indexed unit: a function, class, or whole-file `module` fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub chunk_id: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_type: String,
    pub language: String,
    pub content: String,
    pub token_count: usize,
    /// The hashing/Ollama embedding for this chunk (persisted).
    #[serde(default)]
    pub embedding: Vec<f64>,
}

/// Result of chunking a single file.
#[derive(Debug, Clone)]
pub struct FileChunks {
    pub chunks: Vec<Chunk>,
    /// Deduplicated, first-seen-order import module names (first dotted component).
    pub imports: Vec<String>,
}

/// The languages we parse with tree-sitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Python,
    JavaScript,
}

impl Lang {
    fn as_str(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::JavaScript => "javascript",
        }
    }
}

/// Resolve language from a file path's extension (SPEC §4.2).
fn resolve_language(path: &str) -> Option<Lang> {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "py" => Some(Lang::Python),
        "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
        _ => None,
    }
}

/// The language string used for a fallback chunk: resolved language or plaintext.
fn fallback_language(path: &str) -> String {
    match resolve_language(path) {
        Some(l) => l.as_str().to_string(),
        None => "plaintext".to_string(),
    }
}

/// token_count(content) = max(1, floor(byte_length / CHARS_PER_TOKEN)) (SPEC §4.4).
pub fn token_count(content: &str) -> usize {
    (content.len() / CHARS_PER_TOKEN).max(1)
}

/// Compute the exact, cross-language chunk id (SPEC §4.3).
pub fn chunk_id(
    file_path: &str,
    start_line: usize,
    end_line: usize,
    content_bytes: &[u8],
) -> String {
    let prefix = &content_bytes[..content_bytes.len().min(100)];
    let mut input = format!("{}:{}:{}:", file_path, start_line, end_line).into_bytes();
    input.extend_from_slice(prefix);
    let digest = Sha256::digest(&input);
    let hex = hex_lower(&digest);
    hex[..16].to_string()
}

/// Lowercase hex of a byte slice.
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Node types that are functions / classes, per language (SPEC §4.2).
fn is_function_type(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Python => kind == "function_definition",
        Lang::JavaScript => matches!(
            kind,
            "function_declaration" | "method_definition" | "arrow_function" | "function_expression"
        ),
    }
}

fn is_class_type(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Python => kind == "class_definition",
        Lang::JavaScript => kind == "class_declaration",
    }
}

/// Holds reusable tree-sitter parsers so indexing does not re-create them per file.
pub struct Chunker {
    py: Parser,
    js: Parser,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        let mut py = Parser::new();
        py.set_language(&tree_sitter_python::LANGUAGE.into()).expect("load python grammar");
        let mut js = Parser::new();
        js.set_language(&tree_sitter_javascript::LANGUAGE.into()).expect("load javascript grammar");
        Chunker { py, js }
    }

    /// Chunk one file's content. `file_path` must already be root-relative with
    /// `/` separators. Never panics on parse failure — returns a fallback chunk.
    pub fn chunk_file(&mut self, file_path: &str, content: &str) -> FileChunks {
        let lang = resolve_language(file_path);
        let src = content.as_bytes();

        let parsed = match lang {
            Some(Lang::Python) => self.py.parse(content, None).map(|t| (t, Lang::Python)),
            Some(Lang::JavaScript) => self.js.parse(content, None).map(|t| (t, Lang::JavaScript)),
            None => None,
        };

        if let Some((tree, l)) = parsed {
            let root = tree.root_node();
            let mut chunks = Vec::new();
            collect_chunks(root, src, file_path, l, &mut chunks);
            let imports = extract_imports(root, src, l);
            if chunks.is_empty() {
                return FileChunks { chunks: vec![fallback_chunk(file_path, content)], imports };
            }
            return FileChunks { chunks, imports };
        }

        // Unparsed / other / parse-failure: single fallback chunk, no imports.
        FileChunks { chunks: vec![fallback_chunk(file_path, content)], imports: Vec::new() }
    }
}

/// Build a whole-file fallback `module` chunk (SPEC §4.2).
fn fallback_chunk(file_path: &str, content: &str) -> Chunk {
    let end_line = content.lines().count().max(1);
    let bytes = content.as_bytes();
    Chunk {
        chunk_id: chunk_id(file_path, 1, end_line, bytes),
        file_path: file_path.to_string(),
        start_line: 1,
        end_line,
        chunk_type: "module".to_string(),
        language: fallback_language(file_path),
        content: content.to_string(),
        token_count: token_count(content),
        embedding: Vec::new(),
    }
}

/// Depth-first pre-order walk: emit a chunk for every function/class node.
fn collect_chunks(node: Node, src: &[u8], file_path: &str, lang: Lang, out: &mut Vec<Chunk>) {
    let kind = node.kind();
    let is_fn = is_function_type(lang, kind);
    let is_cls = is_class_type(lang, kind);
    if is_fn || is_cls {
        let start = node.start_byte();
        let end = node.end_byte();
        let content_bytes = &src[start..end];
        let content = String::from_utf8_lossy(content_bytes).to_string();
        let start_line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;
        let chunk_type = if is_cls { "class" } else { "function" };
        out.push(Chunk {
            chunk_id: chunk_id(file_path, start_line, end_line, content_bytes),
            file_path: file_path.to_string(),
            start_line,
            end_line,
            chunk_type: chunk_type.to_string(),
            language: lang.as_str().to_string(),
            content,
            token_count: token_count(&String::from_utf8_lossy(content_bytes)),
            embedding: Vec::new(),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_chunks(child, src, file_path, lang, out);
    }
}

/// Node text as a string slice from source bytes.
fn node_text<'a>(node: Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.start_byte()..node.end_byte()]).unwrap_or("")
}

/// First non-empty dotted component of a module path (e.g. `os.path` -> `os`).
fn first_component(module: &str) -> Option<String> {
    module.split('.').find(|s| !s.is_empty()).map(|s| s.to_string())
}

/// First path segment of a JS specifier, ignoring `.`/`..` (e.g. `./auth` -> `auth`).
fn first_js_segment(spec: &str) -> Option<String> {
    spec.split('/').find(|s| !s.is_empty() && *s != "." && *s != "..").map(|s| s.to_string())
}

/// Extract import module names (SPEC §4.2). Never panics; on trouble returns [].
/// Preserves first-seen order and deduplicates.
fn extract_imports(root: Node, src: &[u8], lang: Lang) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_imports(root, src, lang, &mut out, &mut seen);
    out
}

fn push_import(
    name: Option<String>,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    if let Some(n) = name {
        if !n.is_empty() && seen.insert(n.clone()) {
            out.push(n);
        }
    }
}

/// Recursive DFS pre-order collecting import module names in document order.
fn walk_imports(
    node: Node,
    src: &[u8],
    lang: Lang,
    out: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match lang {
        Lang::Python => {
            if node.kind() == "import_statement" {
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    match child.kind() {
                        "dotted_name" => {
                            push_import(first_component(node_text(child, src)), out, seen)
                        }
                        "aliased_import" => {
                            if let Some(name) = child.child(0) {
                                push_import(first_component(node_text(name, src)), out, seen)
                            }
                        }
                        _ => {}
                    }
                }
            } else if node.kind() == "import_from_statement" {
                if let Some(mn) = node.child_by_field_name("module_name") {
                    push_import(first_component(node_text(mn, src)), out, seen);
                }
            }
        }
        Lang::JavaScript => {
            if node.kind() == "import_statement" {
                if let Some(source) = node.child_by_field_name("source") {
                    let mut frag: Option<String> = None;
                    let mut sc = source.walk();
                    for ch in source.children(&mut sc) {
                        if ch.kind() == "string_fragment" {
                            frag = Some(node_text(ch, src).to_string());
                        }
                    }
                    let spec = frag.unwrap_or_else(|| {
                        node_text(source, src)
                            .trim_matches(|c| c == '\'' || c == '"' || c == '`')
                            .to_string()
                    });
                    push_import(first_js_segment(&spec), out, seen);
                }
            }
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        walk_imports(child, src, lang, out, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_rule() {
        assert_eq!(token_count(""), 1); // max(1, 0)
        assert_eq!(token_count("abcd"), 1); // 4/4
        assert_eq!(token_count("abcdefgh"), 2); // 8/4
        assert_eq!(token_count("abcde"), 1); // floor(5/4)=1
    }

    #[test]
    fn chunk_id_is_deterministic_and_16_hex() {
        let id1 = chunk_id("a.py", 1, 2, b"def f(): pass");
        let id2 = chunk_id("a.py", 1, 2, b"def f(): pass");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn chunk_id_changes_with_path_or_lines() {
        let a = chunk_id("a.py", 1, 2, b"x");
        let b = chunk_id("b.py", 1, 2, b"x");
        let c = chunk_id("a.py", 1, 3, b"x");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn python_fixture_chunks() {
        let mut ck = Chunker::new();
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/auth.py"))
                .unwrap();
        let fc = ck.chunk_file("auth.py", &src);
        // hash_password (fn), verify_password (fn), SessionManager (class), create_session (fn)
        assert_eq!(fc.chunks.len(), 4);
        let types: Vec<&str> = fc.chunks.iter().map(|c| c.chunk_type.as_str()).collect();
        assert_eq!(types, vec!["function", "function", "class", "function"]);
        assert!(fc.chunks.iter().all(|c| c.language == "python"));
    }

    #[test]
    fn payments_fixture_chunks_and_import() {
        let mut ck = Chunker::new();
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test/fixture/payments.py"
        ))
        .unwrap();
        let fc = ck.chunk_file("payments.py", &src);
        assert_eq!(fc.chunks.len(), 2);
        assert_eq!(fc.imports, vec!["auth"]);
    }

    #[test]
    fn readme_fallback_module_chunk() {
        let mut ck = Chunker::new();
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/README.md"))
                .unwrap();
        let fc = ck.chunk_file("README.md", &src);
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].language, "plaintext");
        assert_eq!(fc.chunks[0].start_line, 1);
        assert_eq!(fc.chunks[0].end_line, 2);
    }

    #[test]
    fn python_import_first_component() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("m.py", "import os.path\nfrom pkg.sub import x\nimport hashlib\n");
        assert_eq!(fc.imports, vec!["os", "pkg", "hashlib"]);
    }

    #[test]
    fn js_imports_segments() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file(
            "m.js",
            "import a from 'react';\nimport b from './auth';\nfunction q(){}\n",
        );
        assert_eq!(fc.imports, vec!["react", "auth"]);
        // function_declaration chunk present
        assert!(fc.chunks.iter().any(|c| c.chunk_type == "function"));
    }

    #[test]
    fn js_class_and_method_and_arrow() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("m.js", "class Foo { bar() { return 1; } }\nconst g = () => 2;\n");
        let types: Vec<&str> = fc.chunks.iter().map(|c| c.chunk_type.as_str()).collect();
        assert!(types.contains(&"class"));
        assert!(types.iter().filter(|t| **t == "function").count() >= 2);
    }

    #[test]
    fn parse_failure_or_other_ext_is_fallback() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("data.txt", "just some text\nmore text\n");
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].language, "plaintext");
        assert!(fc.imports.is_empty());
    }

    #[test]
    fn python_no_symbols_is_fallback_with_python_language() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("s.py", "x = 1\ny = 2\n");
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].language, "python");
    }
}
