//! # packs/typescript — the TypeScript language pack
//!
//! **Why this file exists:** TypeScript is one of the four languages added in v2
//! (SPEC-V2 §2). Its extensions, grammar, node-type mapping, and import rule live
//! here and nowhere else.
//!
//! **What it is / does:** Claims `.ts`/`.tsx`, binds the tree-sitter TypeScript
//! grammar, maps function declarations, methods, arrow and function expressions
//! to `function` chunks and class/interface/enum declarations to `class` chunks,
//! and extracts imports from the module specifier of an `import … from "x"`
//! statement (first path segment, mirroring the JavaScript rule).
//!
//! **Responsibilities:**
//! - Own TypeScript's node-type mapping and import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that.

use super::{first_specifier_segment, import_source_string, push_unique, visit_pre};
use super::{LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The TypeScript pack (`.ts`, `.tsx`).
pub struct TypeScriptPack;

impl LanguagePack for TypeScriptPack {
    fn name(&self) -> &'static str {
        "typescript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".ts", ".tsx"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["function_declaration", "method_definition", "arrow_function", "function_expression"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["class_declaration", "interface_declaration", "enum_declaration"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["import_statement"]
    }

    fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        visit_pre(root, &mut |node| {
            if node.kind() == "import_statement" {
                if let Some(spec) = import_source_string(node, src) {
                    push_unique(&mut out, &mut seen, &first_specifier_segment(&spec));
                }
            }
        });
        out
    }

    fn sample(&self) -> &'static str {
        include_str!("../../test/fixture/samples/typescript.ts")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 2,
            min_classes: 2,
            kinds: &["interface_declaration", "class_declaration"],
            imports: &["fs"],
        }
    }
}
