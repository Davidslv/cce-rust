//! # packs/javascript — the JavaScript language pack
//!
//! **Why this file exists:** JavaScript support is one pluggable pack (SPEC-V2
//! §2). Its extensions, grammar, node-type mapping, and import rule live here.
//!
//! **What it is / does:** Claims `.js`/`.jsx`/`.mjs`/`.cjs`, binds the tree-sitter
//! JavaScript grammar, maps function declarations, methods, arrow and function
//! expressions to `function` chunks and class declarations to `class` chunks, and
//! extracts imports from the module specifier of an `import … from "x"` statement
//! (first path segment, e.g. `"./auth"` -> `auth`, `"react"` -> `react`).
//!
//! **Responsibilities:**
//! - Own JavaScript's node-type mapping and import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that.

use super::{first_specifier_segment, import_source_string, push_unique, visit_pre};
use super::{LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The JavaScript pack (`.js`, `.jsx`, `.mjs`, `.cjs`).
pub struct JavaScriptPack;

impl LanguagePack for JavaScriptPack {
    fn name(&self) -> &'static str {
        "javascript"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".js", ".jsx", ".mjs", ".cjs"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["function_declaration", "method_definition", "arrow_function", "function_expression"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["class_declaration"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["import_statement"]
    }

    fn body_node_types(&self) -> &'static [&'static str] {
        // Functions/methods/arrows → `statement_block`; `class_declaration` →
        // `class_body`.
        &["statement_block", "class_body"]
    }

    fn doc_node_types(&self) -> &'static [&'static str] {
        &["comment"]
    }

    fn member_node_types(&self) -> &'static [&'static str] {
        // Class fields (`class_body` → `field_definition`). Methods
        // (`method_definition`) are kept via `function_types` (SPEC-V2.5-TUNING §A).
        &["field_definition"]
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
        include_str!("../../test/fixture/samples/javascript.js")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 2,
            min_classes: 1,
            kinds: &["function_declaration", "method_definition", "class_declaration"],
            imports: &["fs"],
        }
    }
}
