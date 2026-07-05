//! # packs/ruby — the Ruby language pack
//!
//! **Why this file exists:** Ruby is one of the four languages added in v2
//! (SPEC-V2 §2). Its extensions, grammar, node-type mapping, and require rule
//! live here and nowhere else.
//!
//! **What it is / does:** Claims `.rb`, binds the tree-sitter Ruby grammar, maps
//! `method`/`singleton_method` to `function` chunks and `class`/`module` to
//! `class` chunks, and extracts imports from `require`/`require_relative` calls —
//! taking the last path segment's stem of the string argument (`require "a/b"`
//! -> `b`, `require "json"` -> `json`).
//!
//! **Responsibilities:**
//! - Own Ruby's node-type mapping and the require/require_relative import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that.

use super::{node_text, push_unique, visit_pre, LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The Ruby pack (`.rb`).
pub struct RubyPack;

/// Last path segment's stem of a require target (`"a/b.rb"` -> `b`).
fn require_stem(arg: &str) -> &str {
    let last = arg.rsplit('/').next().unwrap_or(arg);
    match last.rfind('.') {
        Some(dot) if dot > 0 => &last[..dot],
        _ => last,
    }
}

/// If `call` is a bare `require`/`require_relative "…"`, return the string
/// argument's text. A `require` call has an `identifier` first child (the method)
/// and an `argument_list` — unlike `Foo.bar(...)`, whose first child is a receiver.
fn require_argument<'a>(call: Node, src: &'a [u8]) -> Option<&'a str> {
    let mut cursor = call.walk();
    let children: Vec<Node> = call.children(&mut cursor).collect();
    let first = children.first()?;
    if first.kind() != "identifier" {
        return None; // has a receiver (e.g. JSON.parse) — not a bare require.
    }
    let method = node_text(*first, src);
    if method != "require" && method != "require_relative" {
        return None;
    }
    // Find the string_content within the argument_list.
    let args = children.iter().find(|c| c.kind() == "argument_list")?;
    let mut found: Option<&str> = None;
    visit_pre(*args, &mut |n| {
        if found.is_none() && n.kind() == "string_content" {
            found = Some(node_text(n, src));
        }
    });
    found
}

impl LanguagePack for RubyPack {
    fn name(&self) -> &'static str {
        "ruby"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".rb"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_ruby::LANGUAGE.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["method", "singleton_method"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["class", "module"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["call"]
    }

    fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        visit_pre(root, &mut |node| {
            if node.kind() == "call" {
                if let Some(arg) = require_argument(node, src) {
                    push_unique(&mut out, &mut seen, require_stem(arg));
                }
            }
        });
        out
    }

    fn sample(&self) -> &'static str {
        include_str!("../../test/fixture/samples/ruby.rb")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 2,
            min_classes: 1,
            kinds: &["method", "class"],
            imports: &["json"],
        }
    }
}
