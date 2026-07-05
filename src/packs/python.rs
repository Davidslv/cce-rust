//! # packs/python — the Python language pack
//!
//! **Why this file exists:** Python support is one pluggable pack (SPEC-V2 §2).
//! All Python-specific knowledge — its extensions, grammar, function/class node
//! types, and import rule — lives here and nowhere else in the engine.
//!
//! **What it is / does:** Declares `.py`, binds the tree-sitter Python grammar,
//! maps `function_definition`/`class_definition` to `function`/`class` chunks,
//! and extracts imports from `import`/`from … import` statements (first dotted
//! component of the module, e.g. `import os.path` -> `os`).
//!
//! **Responsibilities:**
//! - Own Python's node-type mapping and import rule.
//! - It does NOT walk the tree or emit chunks — the generic chunker does that
//!   using the node-type sets this pack declares.

use super::{node_text, push_unique, visit_pre, LanguagePack, PackExpected};
use std::collections::HashSet;
use tree_sitter::{Language, Node};

/// The Python pack (`.py`).
pub struct PythonPack;

/// First non-empty dotted component of a module path (`os.path` -> `os`).
fn first_component(module: &str) -> &str {
    module.split('.').find(|s| !s.is_empty()).unwrap_or("")
}

impl LanguagePack for PythonPack {
    fn name(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &[".py"]
    }

    fn grammar(&self) -> Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn function_types(&self) -> &'static [&'static str] {
        &["function_definition"]
    }

    fn class_types(&self) -> &'static [&'static str] {
        &["class_definition"]
    }

    fn import_node_types(&self) -> &'static [&'static str] {
        &["import_statement", "import_from_statement"]
    }

    fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        visit_pre(root, &mut |node| match node.kind() {
            "import_statement" => {
                let mut c = node.walk();
                for child in node.children(&mut c) {
                    match child.kind() {
                        "dotted_name" => {
                            push_unique(&mut out, &mut seen, first_component(node_text(child, src)))
                        }
                        "aliased_import" => {
                            if let Some(name) = child.child(0) {
                                push_unique(
                                    &mut out,
                                    &mut seen,
                                    first_component(node_text(name, src)),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                if let Some(mn) = node.child_by_field_name("module_name") {
                    push_unique(&mut out, &mut seen, first_component(node_text(mn, src)));
                }
            }
            _ => {}
        });
        out
    }

    fn sample(&self) -> &'static str {
        include_str!("../../test/fixture/samples/python.py")
    }

    fn expected(&self) -> PackExpected {
        PackExpected {
            min_functions: 2,
            min_classes: 1,
            kinds: &["function_definition", "class_definition"],
            imports: &["os"],
        }
    }
}
