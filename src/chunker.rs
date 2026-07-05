//! # chunker — generic AST chunking, chunk IDs, and import extraction
//!
//! **Why this file exists:** Searching whole files is wasteful; the engine splits
//! each file into the functions/classes it defines so retrieval returns precise
//! snippets. In v2 this module holds **zero** language-specific knowledge: it asks
//! a `LanguagePack` (resolved from the registry) which node types are functions
//! and classes, and how to extract imports. Adding a language never touches this
//! file (SPEC-V2 §1).
//!
//! **What it is / does:** Resolves a file to its pack via the registry, parses
//! with the pack's grammar, walks the tree depth-first emitting a chunk for every
//! node whose type is in the pack's function/class sets (nested included), records
//! each chunk's exact node-type `kind` (SPEC-V2 §3), falls back to a whole-file
//! `module` chunk otherwise, and computes the cross-language `chunk_id` and
//! `token_count`. Import extraction is delegated to the pack.
//!
//! **Responsibilities:**
//! - Own `Chunk`, the generic tree walk, and the fallback rule (SPEC-V2 §4).
//! - Own `chunk_id` and `token_count` (byte-exact) and the `kind` field.
//! - It does NOT know any language by name — the packs do.

use crate::config::CHARS_PER_TOKEN;
use crate::packs::{LanguagePack, Registry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

/// The language string used for a fallback chunk that no pack claimed.
const PLAINTEXT: &str = "plaintext";
/// The `chunk_type`/`kind` of the whole-file fallback chunk.
const MODULE: &str = "module";

/// A single indexed unit: a function, class, or whole-file `module` fallback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Chunk {
    pub chunk_id: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Coarse taxonomy: `"function"`, `"class"`, or `"module"` (SPEC-V2 §3).
    pub chunk_type: String,
    /// Exact tree-sitter node type that produced this chunk; `"module"` for the
    /// fallback (SPEC-V2 §3). Not part of `chunk_id`.
    #[serde(default)]
    pub kind: String,
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
    /// Deduplicated, first-seen-order import names (delegated to the pack).
    pub imports: Vec<String>,
}

/// token_count(content) = max(1, floor(byte_length / CHARS_PER_TOKEN)).
///
/// Delegates to `tokenizer::estimate_tokens` — the ONE canonical `cce.tokens/v1`
/// estimator (SPEC-V2.5 §4) — so the chunk-size heuristic, `conformance.json`, the
/// Sync artifact, and the savings ledger all share a single byte-pinned rule.
/// `CHARS_PER_TOKEN` remains this rule's documented divisor.
pub fn token_count(content: &str) -> usize {
    debug_assert_eq!(CHARS_PER_TOKEN, 4);
    crate::tokenizer::estimate_tokens(content) as usize
}

/// Compute the exact, cross-language chunk id (base SPEC §4.3). Unchanged in v2:
/// the `kind` field is deliberately NOT part of the id.
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

/// Holds the pack registry and a reusable tree-sitter parser per pack so indexing
/// does not re-create them per file. Constructing a `Chunker` loads every pack's
/// grammar, which is the cheap fail-fast startup check (SPEC-V2 §5): an unloadable
/// grammar panics here with a clear message rather than silently mis-chunking.
pub struct Chunker {
    registry: Registry,
    parsers: HashMap<&'static str, Parser>,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        Self::with_registry(Registry::default())
    }

    /// Build a chunker over a specific registry (used by tests and validators).
    pub fn with_registry(registry: Registry) -> Self {
        let mut parsers = HashMap::new();
        for pack in registry.all() {
            let mut parser = Parser::new();
            parser
                .set_language(&pack.grammar())
                .unwrap_or_else(|e| panic!("[pack:{}] grammar failed to load: {e}", pack.name()));
            parsers.insert(pack.name(), parser);
        }
        Chunker { registry, parsers }
    }

    /// Chunk one file's content. `file_path` must already be root-relative with
    /// `/` separators. Never panics on parse failure — returns a fallback chunk.
    pub fn chunk_file(&mut self, file_path: &str, content: &str) -> FileChunks {
        // Resolve the pack (immutable borrow of the registry) and its parser
        // (mutable borrow of the disjoint parsers map).
        let Some(pack) = self.registry.pack_for(file_path) else {
            return FileChunks {
                chunks: vec![fallback_chunk(file_path, content, PLAINTEXT)],
                imports: Vec::new(),
            };
        };
        let parser = self.parsers.get_mut(pack.name()).expect("parser for every registered pack");
        parse_and_collect(parser, pack, file_path, content)
    }
}

/// Chunk `content` with an explicit pack, creating a throwaway parser. Used by the
/// pack validators' behavioural self-test (SPEC-V2 §5 Layer 3).
pub fn chunk_with_pack(pack: &dyn LanguagePack, file_path: &str, content: &str) -> FileChunks {
    let mut parser = Parser::new();
    parser
        .set_language(&pack.grammar())
        .unwrap_or_else(|e| panic!("[pack:{}] grammar failed to load: {e}", pack.name()));
    parse_and_collect(&mut parser, pack, file_path, content)
}

/// Parse with `parser`, collect chunks against `pack`'s node-type sets, and ask
/// the pack for imports. Falls back to a whole-file `module` chunk when parsing
/// fails or yields no function/class chunks.
fn parse_and_collect(
    parser: &mut Parser,
    pack: &dyn LanguagePack,
    file_path: &str,
    content: &str,
) -> FileChunks {
    let src = content.as_bytes();
    match parser.parse(content, None) {
        Some(tree) => {
            let root = tree.root_node();
            let mut chunks = Vec::new();
            collect_chunks(root, src, file_path, pack, &mut chunks);
            let imports = pack.extract_imports(root, src);
            if chunks.is_empty() {
                FileChunks {
                    chunks: vec![fallback_chunk(file_path, content, pack.name())],
                    imports,
                }
            } else {
                FileChunks { chunks, imports }
            }
        }
        None => FileChunks {
            chunks: vec![fallback_chunk(file_path, content, pack.name())],
            imports: Vec::new(),
        },
    }
}

/// Build a whole-file fallback `module` chunk (SPEC-V2 §4). The line-count rule is
/// normative: `end_line = (number of "\n" bytes in the content) + 1`.
fn fallback_chunk(file_path: &str, content: &str, language: &str) -> Chunk {
    let end_line = content.bytes().filter(|&b| b == b'\n').count() + 1;
    let bytes = content.as_bytes();
    Chunk {
        chunk_id: chunk_id(file_path, 1, end_line, bytes),
        file_path: file_path.to_string(),
        start_line: 1,
        end_line,
        chunk_type: MODULE.to_string(),
        kind: MODULE.to_string(),
        language: language.to_string(),
        content: content.to_string(),
        token_count: token_count(content),
        embedding: Vec::new(),
    }
}

/// Depth-first pre-order walk: emit a chunk for every node whose type is in the
/// pack's function/class sets. `kind` is the exact node type (SPEC-V2 §3).
fn collect_chunks(
    node: Node,
    src: &[u8],
    file_path: &str,
    pack: &dyn LanguagePack,
    out: &mut Vec<Chunk>,
) {
    let kind = node.kind();
    // Only named AST nodes are chunk candidates. Some grammars name a definition
    // node the same string as its keyword token (e.g. Ruby's `class` node vs the
    // anonymous `class` keyword); the `is_named` guard excludes the keyword token
    // so a class/method is not double-counted.
    let named = node.is_named();
    let is_fn = named && pack.function_types().contains(&kind);
    let is_cls = named && pack.class_types().contains(&kind);
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
            kind: kind.to_string(),
            language: pack.name().to_string(),
            content,
            token_count: token_count(&String::from_utf8_lossy(content_bytes)),
            embedding: Vec::new(),
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_chunks(child, src, file_path, pack, out);
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
    fn fallback_line_count_counts_trailing_newline() {
        // SPEC-V2 §4: end_line = number of "\n" bytes + 1; a trailing newline
        // still counts its line. "a\nb\n" has two newlines -> end_line 3.
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("notes.md", "a\nb\n");
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].kind, "module");
        assert_eq!(fc.chunks[0].start_line, 1);
        assert_eq!(fc.chunks[0].end_line, 3);
        assert_eq!(fc.chunks[0].language, "plaintext");
    }

    #[test]
    fn no_pack_file_is_plaintext_fallback_without_imports() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("data.txt", "just some text\nmore text\n");
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].language, "plaintext");
        assert!(fc.imports.is_empty());
    }

    #[test]
    fn parsed_chunk_carries_exact_node_kind() {
        // A Python function/class file: kinds are the exact tree-sitter node types.
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("m.py", "def f():\n    pass\n\nclass C:\n    pass\n");
        let kinds: Vec<&str> = fc.chunks.iter().map(|c| c.kind.as_str()).collect();
        assert!(kinds.contains(&"function_definition"));
        assert!(kinds.contains(&"class_definition"));
        // chunk_type stays coarse.
        assert!(fc.chunks.iter().any(|c| c.chunk_type == "function"));
        assert!(fc.chunks.iter().any(|c| c.chunk_type == "class"));
    }

    #[test]
    fn parsed_file_with_no_symbols_falls_back_to_pack_language() {
        let mut ck = Chunker::new();
        let fc = ck.chunk_file("s.py", "x = 1\ny = 2\n");
        assert_eq!(fc.chunks.len(), 1);
        assert_eq!(fc.chunks[0].chunk_type, "module");
        assert_eq!(fc.chunks[0].language, "python");
    }
}
