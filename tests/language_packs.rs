//! # tests/language_packs — pack architecture acceptance tests (SPEC-V2 §10)
//!
//! **Why this file exists:** SPEC-V2 requires guarding that the core chunker
//! names no language, that every pack's sample meets its declared `expected`
//! (counts, kinds, imports), that the module-fallback line count is fixed, and
//! that `kind` flows end-to-end index→persist→search.
//!
//! **What it is / does:** Reads the production portion of `src/chunker.rs` and
//! asserts it contains no language name or extension literal; iterates the six
//! packs and checks each sample against its `expected`; and drives the public
//! library API to confirm `kind` survives persistence and reaches search results.
//!
//! **Responsibilities:**
//! - Own the "no language names in core" grep-style guard.
//! - Own the per-sample structural assertions and the kind end-to-end check.

use cce::chunker::{chunk_with_pack, Chunker};
use cce::embedder::HashEmbedder;
use cce::packs::default_registry;
use cce::retriever::search;
use cce::store::Index;
use std::path::PathBuf;

fn samples_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/samples"))
}

#[test]
fn core_chunker_names_no_language_or_extension() {
    // The generic chunker/importer must reference no language by name and no file
    // extension (SPEC-V2 §1). Scan only the production code, above the test module.
    let src = include_str!("../src/chunker.rs");
    let production = src.split("#[cfg(test)]").next().unwrap();
    let lower = production.to_lowercase();

    for name in ["\"python\"", "\"javascript\"", "\"ruby\"", "\"rust\"", "\"typescript\""] {
        assert!(!lower.contains(name), "core chunker names a language: {name}");
    }
    // Match extension *literals* (quoted), not method calls like `.children`.
    for ext in [".py", ".js", ".jsx", ".mjs", ".cjs", ".rb", ".rs", ".ts", ".tsx", ".c", ".h"] {
        let literal = format!("\"{ext}\"");
        assert!(
            !production.contains(&literal),
            "core chunker contains an extension literal: {literal}"
        );
    }
}

#[test]
fn every_pack_sample_meets_its_expected() {
    // SPEC-V2 §6/§7: each pack's sample yields at least the declared function and
    // class counts, contains all declared kinds, and extracts exactly the declared
    // imports. These are hand-derivable and pin the chunking without sha256 ids.
    let reg = default_registry();
    for pack in reg.all() {
        let exp = pack.expected();
        let path = format!("{}{}", pack.name(), pack.extensions()[0]);
        let fc = chunk_with_pack(pack.as_ref(), &path, pack.sample());

        let functions = fc.chunks.iter().filter(|c| c.chunk_type == "function").count();
        let classes = fc.chunks.iter().filter(|c| c.chunk_type == "class").count();
        assert!(
            functions >= exp.min_functions,
            "[{}] {functions} functions < {}",
            pack.name(),
            exp.min_functions
        );
        assert!(
            classes >= exp.min_classes,
            "[{}] {classes} classes < {}",
            pack.name(),
            exp.min_classes
        );

        let kinds: std::collections::HashSet<&str> =
            fc.chunks.iter().map(|c| c.kind.as_str()).collect();
        for k in exp.kinds {
            assert!(kinds.contains(k), "[{}] missing kind {k}", pack.name());
        }

        let expected_imports: Vec<String> = exp.imports.iter().map(|s| s.to_string()).collect();
        assert_eq!(fc.imports, expected_imports, "[{}] imports", pack.name());
    }
}

#[test]
fn notes_md_falls_back_to_one_module_chunk_ending_at_line_three() {
    // SPEC-V2 §4/§6: notes.md is claimed by no pack; it yields exactly one module
    // chunk whose end_line is (two "\n" bytes) + 1 == 3.
    let path = samples_dir().join("notes.md");
    let content = std::fs::read_to_string(&path).unwrap();
    let mut ck = Chunker::new();
    let fc = ck.chunk_file("notes.md", &content);
    assert_eq!(fc.chunks.len(), 1);
    let chunk = &fc.chunks[0];
    assert_eq!(chunk.chunk_type, "module");
    assert_eq!(chunk.kind, "module");
    assert_eq!(chunk.start_line, 1);
    assert_eq!(chunk.end_line, 3);
}

#[test]
fn kind_survives_persistence_and_reaches_search() {
    // index → persist → load (fresh Index) → search, and the kind is carried the
    // whole way (SPEC-V2 §3).
    let e = HashEmbedder;
    let (idx, _) = Index::build_from_dir(&samples_dir(), &e);
    assert!(idx.chunks.iter().all(|c| !c.kind.is_empty()));

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("index.json");
    idx.save(&path).unwrap();
    let loaded = Index::load(&path).unwrap();
    assert!(loaded.chunks.iter().all(|c| !c.kind.is_empty()));

    // A Rust struct chunk persists its exact node type.
    assert!(loaded.chunks.iter().any(|c| c.kind == "struct_item"));

    let results = search(&loaded, &e, "build index hashmap store", 5, false);
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| !r.kind.is_empty()));
}

/// Chunk `src` with the pack that claims `path`, returning imports.
fn imports_for(path: &str, src: &str) -> Vec<String> {
    let reg = default_registry();
    let pack = reg.pack_for(path).unwrap_or_else(|| panic!("no pack for {path}"));
    chunk_with_pack(pack, path, src).imports
}

#[test]
fn import_extraction_edge_cases_per_pack() {
    // Python: first dotted component, aliased and from-imports, first-seen order.
    assert_eq!(
        imports_for(
            "m.py",
            "import os.path\nfrom pkg.sub import x\nimport hashlib\nimport numpy as np\n"
        ),
        vec!["os", "pkg", "hashlib", "numpy"]
    );
    // JavaScript: first path segment, relative specifiers stripped.
    assert_eq!(
        imports_for("m.js", "import a from 'react';\nimport b from './auth';\n"),
        vec!["react", "auth"]
    );
    // Ruby: require + require_relative, last path segment stem, de-duplicated.
    assert_eq!(
        imports_for("m.rb", "require \"json\"\nrequire_relative \"lib/foo\"\nrequire \"json\"\n"),
        vec!["json", "foo"]
    );
    // Rust: first use-path segment, de-duplicated (std twice -> once).
    assert_eq!(
        imports_for("m.rs", "use std::x;\nuse crate::y::Z;\nuse std::w;\n"),
        vec!["std", "crate"]
    );
    // TypeScript: scoped package keeps its scope; relative specifier stripped.
    assert_eq!(
        imports_for("m.ts", "import {a} from \"@scope/pkg\";\nimport b from \"./store\";\n"),
        vec!["@scope/pkg", "store"]
    );
    // C: basename without extension, angle and quoted includes.
    assert_eq!(
        imports_for("m.c", "#include <sys/types.h>\n#include \"store.h\"\n"),
        vec!["types", "store"]
    );
}

#[test]
fn javascript_class_method_and_arrow_all_chunk() {
    // The JS pack maps methods and arrow functions to function chunks and class
    // declarations to class chunks (nested included).
    let reg = default_registry();
    let pack = reg.pack_for("m.js").unwrap();
    let fc =
        chunk_with_pack(pack, "m.js", "class Foo { bar() { return 1; } }\nconst g = () => 2;\n");
    let types: Vec<&str> = fc.chunks.iter().map(|c| c.chunk_type.as_str()).collect();
    assert!(types.contains(&"class"));
    assert!(types.iter().filter(|t| **t == "function").count() >= 2);
    // kinds carry the exact node types.
    let kinds: std::collections::HashSet<&str> =
        fc.chunks.iter().map(|c| c.kind.as_str()).collect();
    assert!(kinds.contains("class_declaration"));
    assert!(kinds.contains("method_definition") || kinds.contains("arrow_function"));
}
