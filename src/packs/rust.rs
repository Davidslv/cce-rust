//! # packs/rust — the Rust language pack
//!
//! **Why this file exists:** Rust is one of the four languages added in v2
//! (SPEC-V2 §2). Its extensions, grammar, node-type mapping, and `use` rule live
//! here and nowhere else.
//!
//! **What it is / does:** Claims `.rs`, binds the tree-sitter Rust grammar, maps
//! `function_item` to `function` chunks and `struct_item`/`enum_item`/
//! `trait_item`/`impl_item`/`union_item` to `class` chunks, and extracts imports
//! from the first segment of a `use` path (`use std::collections::HashMap` ->
//! `std`, `use crate::store::Index` -> `crate`).
//!
//! **Responsibilities:**
//! - Own Rust's node-type mapping and the `use`-path import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that.

use super::{node_text, push_unique, visit_pre, LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The Rust pack (`.rs`).
pub struct RustPack;

/// First path segment of a `use` declaration's text (`use a::b::C;` -> `a`).
fn first_use_segment(decl_text: &str) -> &str {
    let rest = decl_text.trim().trim_start_matches("use").trim_start();
    let first = rest.split("::").next().unwrap_or("");
    // Trim at the first non-identifier character (whitespace, `;`, `{`, `*`).
    let end = first.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(first.len());
    &first[..end]
}

impl LanguagePack for RustPack {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".rs"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["function_item"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["struct_item", "enum_item", "trait_item", "impl_item", "union_item"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["use_declaration"]
    }

    fn body_node_types(&self) -> &'static [&'static str] {
        // `function_item` → `block`; `impl_item`/`trait_item` → `declaration_list`;
        // `struct_item`/`union_item` → `field_declaration_list`; `enum_item` →
        // `enum_variant_list`. The signature is the header before any of these.
        &["block", "declaration_list", "field_declaration_list", "enum_variant_list"]
    }

    fn doc_node_types(&self) -> &'static [&'static str] {
        // A `///`/`/** */` doc comment leading the body (outer docs sit above the
        // item, outside the chunk span; inner `//!` docs can lead a body).
        &["line_comment", "block_comment"]
    }

    fn member_node_types(&self) -> &'static [&'static str] {
        // Struct/union fields (`field_declaration_list` → `field_declaration`),
        // enum variants (`enum_variant_list` → `enum_variant`), and impl/trait
        // constants (`declaration_list` → `const_item`). Methods (`function_item`)
        // are kept via `function_types` (SPEC-V2.5-TUNING §A).
        &["field_declaration", "enum_variant", "const_item"]
    }

    fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        visit_pre(root, &mut |node| {
            if node.kind() == "use_declaration" {
                push_unique(&mut out, &mut seen, first_use_segment(node_text(node, src)));
            }
        });
        out
    }

    fn sample(&self) -> &'static str {
        include_str!("../../test/fixture/samples/rust.rs")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 2,
            min_classes: 2,
            kinds: &["function_item", "struct_item", "impl_item"],
            imports: &["std"],
        }
    }
}
