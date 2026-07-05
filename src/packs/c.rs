//! # packs/c — the C language pack
//!
//! **Why this file exists:** C is one of the four languages added in v2
//! (SPEC-V2 §2). Its extensions, grammar, node-type mapping, and `#include` rule
//! live here and nowhere else.
//!
//! **What it is / does:** Claims `.c`/`.h`, binds the tree-sitter C grammar, maps
//! `function_definition` to `function` chunks and `struct_specifier`/
//! `union_specifier`/`enum_specifier` to `class` chunks, and extracts imports
//! from `#include` directives — stripping `<>`/quotes and taking the basename
//! without extension (`<stdlib.h>` -> `stdlib`, `"store.h"` -> `store`).
//!
//! **Responsibilities:**
//! - Own C's node-type mapping and the `#include` import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that.

use super::{node_text, push_unique, visit_pre, LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The C pack (`.c`, `.h`).
pub struct CPack;

/// Basename-without-extension of an include target, with `<>`/quotes stripped
/// (`<sys/types.h>` -> `types`, `"store.h"` -> `store`).
fn include_stem(raw: &str) -> &str {
    let inner = raw.trim().trim_matches(|c| c == '<' || c == '>' || c == '"');
    let base = inner.rsplit('/').next().unwrap_or(inner);
    match base.rfind('.') {
        Some(dot) if dot > 0 => &base[..dot],
        _ => base,
    }
}

impl LanguagePack for CPack {
    fn name(&self) -> &'static str {
        "c"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".c", ".h"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_c::LANGUAGE.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["function_definition"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["struct_specifier", "union_specifier", "enum_specifier"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["preproc_include"]
    }

    fn body_node_types(&self) -> &'static [&'static str] {
        // `function_definition` → `compound_statement`; `struct_specifier`/
        // `union_specifier` → `field_declaration_list`; `enum_specifier` →
        // `enumerator_list`.
        &["compound_statement", "field_declaration_list", "enumerator_list"]
    }

    fn doc_node_types(&self) -> &'static [&'static str] {
        &["comment"]
    }

    fn member_node_types(&self) -> &'static [&'static str] {
        // Struct/union fields (`field_declaration_list` → `field_declaration`) and
        // enum constants (`enumerator_list` → `enumerator`). C has no methods inside
        // these aggregates (SPEC-V2.5-TUNING §A).
        &["field_declaration", "enumerator"]
    }

    fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        visit_pre(root, &mut |node| {
            if node.kind() == "preproc_include" {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "system_lib_string" | "string_literal" => {
                            push_unique(&mut out, &mut seen, include_stem(node_text(child, src)));
                        }
                        _ => {}
                    }
                }
            }
        });
        out
    }

    fn sample(&self) -> &'static str {
        include_str!("../../test/fixture/samples/c.c")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 1,
            min_classes: 1,
            kinds: &["function_definition", "struct_specifier"],
            imports: &["stdlib"],
        }
    }
}
