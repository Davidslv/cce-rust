# Adding a language pack

Language support in cce is a set of **pluggable packs** (SPEC-V2). The core
chunker/importer knows no language by name; it only ever talks to a pack through
the `LanguagePack` trait. Adding a language is therefore a **one-file change**:
write a pack, register it, and make it pass validation. No core edits.

This guide walks the whole loop with a worked example (a hypothetical Go pack),
then shows how the validators guide you when something is wrong.

## The five steps

1. **Add the grammar crate.** Pin a tree-sitter grammar crate that is
   ABI-compatible with the pinned `tree-sitter` core (see `Cargo.toml` — all
   grammar crates share the same `tree-sitter-language` version).

   ```toml
   # Cargo.toml
   tree-sitter-go = "=0.23.0"
   ```

2. **Write the pack file** under `src/packs/`, one struct implementing
   `LanguagePack`, with a why/what/responsibilities header describing *that*
   language. Implement every member:

   ```rust
   //! # packs/go — the Go language pack … (why / what / responsibilities)
   use super::{node_text, push_unique, visit_pre, LanguagePack, PackExpected};
   use std::collections::HashSet;
   use tree_sitter::{Language, Node};

   pub struct GoPack;

   impl LanguagePack for GoPack {
       fn name(&self) -> &'static str { "go" }
       fn extensions(&self) -> &'static [&'static str] { &[".go"] }
       fn grammar(&self) -> Language { tree_sitter_go::LANGUAGE.into() }
       fn function_types(&self) -> &'static [&'static str] {
           &["function_declaration", "method_declaration"]
       }
       fn class_types(&self) -> &'static [&'static str] {
           &["type_declaration"]
       }
       fn import_node_types(&self) -> &'static [&'static str] { &["import_spec"] }
       fn extract_imports(&self, root: Node, src: &[u8]) -> Vec<String> {
           let (mut out, mut seen) = (Vec::new(), HashSet::new());
           visit_pre(root, &mut |n| {
               if n.kind() == "import_spec" {
                   // …take the string path, last segment…
                   push_unique(&mut out, &mut seen, /* name */ "");
               }
           });
           out
       }
       fn sample(&self) -> &'static str {
           include_str!("../../test/fixture/samples/go.go")
       }
       fn expected(&self) -> PackExpected {
           PackExpected {
               min_functions: 1,
               min_classes: 1,
               kinds: &["function_declaration", "type_declaration"],
               imports: &["fmt"],
           }
       }
   }
   ```

   **Pick the node types from the grammar, not from memory.** The exact spellings
   (`function_declaration` vs `func_declaration`, `type_declaration` vs
   `type_spec`, …) come from the grammar; the validators check them for you (see
   below). A quick way to discover them is a throwaway `examples/probe.rs` that
   parses a snippet and prints `node.kind()` for every node.

3. **Write the sample + expected.** Add a small, self-contained source file under
   `test/fixture/samples/` (this is both the pack's self-test fixture *and* part
   of the cross-language conformance corpus, so keep it minimal). State its
   `expected` in the pack: the minimum function/class counts, the set of `kind`s
   that must appear, and the **exact** ordered, de-duplicated `imports` list.

4. **Register the pack** in `src/packs/mod.rs` `default_registry()` (and declare
   the module):

   ```rust
   mod go;
   // …
   Box::new(go::GoPack),
   ```

5. **Validate.** Run the three validator layers and read the diagnostics:

   ```bash
   cargo run -- packs --validate
   ```

## What the validators check (and how they help)

A pack is *compatible* iff it passes three layers (SPEC-V2 §5). Every diagnostic
names the pack, the offending member, the problem, and — where possible — a fix.

- **Layer 1 — structural.** `name` non-empty and unique; ≥1 extension, each a
  lowercased leading-dot string; no extension already claimed by another pack.

  ```
  [pack:go] extension "GO" must start with a leading dot, e.g. ".GO".
  ```

- **Layer 2 — grammar-binding.** The grammar loads, and **every** string in
  `function_types`, `class_types`, and `import_node_types` is a real node kind in
  that grammar. On a miss it suggests the nearest valid kind by edit distance:

  ```
  [pack:go] function_types: "func_declaration" is not a node kind in the go
  grammar. Did you mean: "function_declaration", "method_declaration"?
  ```

- **Layer 3 — behavioural self-test.** The pack is run over its own `sample` and
  must satisfy `expected`: at least the declared function/class counts, all the
  declared kinds present, **and `extract_imports(sample) == expected.imports`
  exactly**. This catches a pack that is structurally valid but wired to the wrong
  node type, and it validates import extraction:

  ```
  [pack:go] produced 0 class chunks from its sample; expected at least 1.
  Check class_types against the grammar.
  [pack:go] imports mismatch: extracted ["fmt","fmt"] but expected ["fmt"] —
  fix extract_imports (order and de-duplication matter).
  ```

These same three layers run as a CI test gate over every pack, and the cheap
Layer-1 checks (duplicate extension, unloadable grammar) run fail-fast when the
engine is constructed — so a broken pack never silently mis-chunks.

## Tips

- Only **named** AST nodes become chunks. Some grammars name a definition node
  the same string as its keyword token (e.g. Ruby's `class`); the chunker's
  `is_named` guard already excludes the keyword token, so you list `class` in
  `class_types` and get exactly one chunk.
- The registry resolves files **by extension only** — one extension maps to
  exactly one pack. Two languages that share an extension cannot both be served,
  and there is no per-file dialect sniffing.
- Keep all language-specific comments **inside** the pack. The core carries no
  language-specific comments by design (a guard test enforces it).
- `extract_imports` must never panic; on trouble, return what you have so far.
