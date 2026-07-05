//! # packs/validators — the three-layer pack safety rail
//!
//! **Why this file exists:** Adding a language must be safe and self-diagnosing
//! (SPEC-V2 §5). A pack that is structurally valid but wired to a misspelled node
//! type would silently mis-chunk. These validators turn every such mistake into a
//! precise diagnostic that names the pack, the offending member, the problem, and
//! — where possible — a fix.
//!
//! **What it is / does:** Runs three layers over a pack: (1) a structural lint
//! (name, extensions), (2) a grammar-binding lint that checks every declared node
//! type is a real kind in the grammar and suggests the nearest valid kind on a
//! miss, and (3) a behavioural self-test that chunks the pack's sample and checks
//! it satisfies `expected` (min counts, kinds present, and `extract_imports`
//! exactly). Surfaced by `cce packs --validate`, the CI test gate, and fail-fast
//! startup (Layer 1 only).
//!
//! **Responsibilities:**
//! - Own the three validation layers and their diagnostic wording.
//! - Own the "did you mean" nearest-kind suggestion (edit distance).
//! - It does NOT register packs or chunk corpora — it validates one pack in
//!   isolation using the shared chunker.

use super::{LanguagePack, Registry};
use crate::chunker::chunk_with_pack;
use tree_sitter::Language;

/// The outcome of validating one pack: its name and any diagnostics (empty = ok).
#[derive(Debug, Clone)]
pub struct PackReport {
    pub name: String,
    pub diagnostics: Vec<String>,
}

impl PackReport {
    /// True when the pack passed every layer.
    pub fn ok(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Validate a single pack through all three layers, collecting every diagnostic.
pub fn validate_pack(pack: &dyn LanguagePack) -> PackReport {
    let mut diagnostics = Vec::new();
    layer1_structural(pack, &mut diagnostics);
    let grammar_ok = layer2_grammar_binding(pack, &mut diagnostics);
    // The behavioural self-test needs a loadable, correctly-bound grammar; skip it
    // when Layer 2 already failed so the message stays focused on the root cause.
    if grammar_ok {
        layer3_behavioural(pack, &mut diagnostics);
    }
    PackReport { name: pack.name().to_string(), diagnostics }
}

/// Validate every pack in a registry (the CI test gate, SPEC-V2 §5).
pub fn validate_all(reg: &Registry) -> Vec<PackReport> {
    reg.all().iter().map(|p| validate_pack(p.as_ref())).collect()
}

/// The cheap Layer-1 startup checks (SPEC-V2 §5): duplicate extensions across
/// packs and unloadable grammars. Returns a clear error rather than letting the
/// engine silently mis-chunk. Called on engine construction.
pub fn startup_check(reg: &Registry) -> Result<(), String> {
    let mut claimed: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for pack in reg.all() {
        for ext in pack.extensions() {
            if let Some(prev) = claimed.insert(ext, pack.name()) {
                return Err(format!(
                    "[pack:{}] extension \"{}\" already claimed by pack \"{}\"; each extension \
                     maps to exactly one pack.",
                    pack.name(),
                    ext,
                    prev
                ));
            }
        }
        // Grammar must load (cheap: build a parser and bind the language).
        let mut parser = tree_sitter::Parser::new();
        if let Err(e) = parser.set_language(&pack.grammar()) {
            return Err(format!("[pack:{}] grammar failed to load: {e}", pack.name()));
        }
    }
    Ok(())
}

// --- Layer 1: structural lint ---

fn layer1_structural(pack: &dyn LanguagePack, out: &mut Vec<String>) {
    let name = pack.name();
    if name.is_empty() {
        out.push("[pack:?] name is empty; a pack needs a unique lowercase id.".to_string());
    }
    if pack.extensions().is_empty() {
        out.push(format!("[pack:{name}] declares no extensions; a pack needs at least one."));
    }
    for ext in pack.extensions() {
        if !ext.starts_with('.') {
            out.push(format!(
                "[pack:{name}] extension \"{ext}\" must start with a leading dot, e.g. \".{ext}\"."
            ));
        }
        if ext != &ext.to_ascii_lowercase() {
            out.push(format!(
                "[pack:{name}] extension \"{ext}\" must be lowercase, e.g. \"{}\".",
                ext.to_ascii_lowercase()
            ));
        }
    }
}

// --- Layer 2: grammar-binding lint ---

/// Returns true when every declared node type binds to a real grammar kind.
fn layer2_grammar_binding(pack: &dyn LanguagePack, out: &mut Vec<String>) -> bool {
    let name = pack.name();
    let lang = pack.grammar();
    let kinds = named_kinds(&lang);
    if kinds.is_empty() {
        out.push(format!(
            "[pack:{name}] grammar failed to load or exposes no node kinds — add/verify the \
             grammar crate for this pack."
        ));
        return false;
    }
    let mut ok = true;
    for (member, types) in [
        ("function_types", pack.function_types()),
        ("class_types", pack.class_types()),
        ("import node types", pack.import_node_types()),
    ] {
        for &ty in types {
            if !kinds.iter().any(|k| k == ty) {
                ok = false;
                let suggestion = suggest(ty, &kinds);
                out.push(format!(
                    "[pack:{name}] {member}: \"{ty}\" is not a node kind in the {name} grammar.{}",
                    suggestion
                ));
            }
        }
    }
    ok
}

/// All named node kinds of a grammar.
fn named_kinds(lang: &Language) -> Vec<String> {
    (0..lang.node_kind_count() as u16)
        .filter(|&id| lang.node_kind_is_named(id))
        .filter_map(|id| lang.node_kind_for_id(id).map(|s| s.to_string()))
        .collect()
}

/// A " Did you mean: …" clause naming the nearest valid kinds by edit distance,
/// or an empty string when nothing is close.
fn suggest(target: &str, kinds: &[String]) -> String {
    let mut scored: Vec<(usize, &String)> =
        kinds.iter().map(|k| (levenshtein(target, k), k)).collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)));
    // Only suggest genuinely close kinds (within a third of the target length + 2).
    let budget = target.len() / 3 + 2;
    let picks: Vec<String> = scored
        .iter()
        .filter(|(d, _)| *d <= budget)
        .take(3)
        .map(|(_, k)| format!("\"{k}\""))
        .collect();
    if picks.is_empty() {
        String::new()
    } else {
        format!(" Did you mean: {}?", picks.join(", "))
    }
}

/// Classic Levenshtein edit distance (bytes; node kinds are ASCII).
fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

// --- Layer 3: behavioural self-test ---

fn layer3_behavioural(pack: &dyn LanguagePack, out: &mut Vec<String>) {
    let name = pack.name();
    let exp = pack.expected();
    let sample_path = format!("{name}{}", pack.extensions().first().copied().unwrap_or(""));
    let fc = chunk_with_pack(pack, &sample_path, pack.sample());

    let functions = fc.chunks.iter().filter(|c| c.chunk_type == "function").count();
    let classes = fc.chunks.iter().filter(|c| c.chunk_type == "class").count();

    if functions < exp.min_functions {
        out.push(format!(
            "[pack:{name}] produced {functions} function chunks from its sample; expected at \
             least {}. Check function_types against the grammar.",
            exp.min_functions
        ));
    }
    if classes < exp.min_classes {
        out.push(format!(
            "[pack:{name}] produced {classes} class chunks from its sample; expected at least \
             {}. Check class_types against the grammar.",
            exp.min_classes
        ));
    }

    let present: std::collections::HashSet<&str> =
        fc.chunks.iter().map(|c| c.kind.as_str()).collect();
    for kind in exp.kinds {
        if !present.contains(kind) {
            out.push(format!(
                "[pack:{name}] expected a chunk of kind \"{kind}\" from its sample but none was \
                 produced; add \"{kind}\" to function_types/class_types."
            ));
        }
    }

    let expected_imports: Vec<String> = exp.imports.iter().map(|s| s.to_string()).collect();
    if fc.imports != expected_imports {
        out.push(format!(
            "[pack:{name}] imports mismatch: extracted {:?} but expected {:?} — fix \
             extract_imports (order and de-duplication matter).",
            fc.imports, expected_imports
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{default_registry, PackExpected};
    use tree_sitter::Node;

    #[test]
    fn all_default_packs_validate() {
        let reg = default_registry();
        for report in validate_all(&reg) {
            assert!(report.ok(), "pack {} failed: {:?}", report.name, report.diagnostics);
        }
    }

    #[test]
    fn startup_check_passes_for_default_registry() {
        assert!(startup_check(&default_registry()).is_ok());
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("struct_specifer", "struct_specifier"), 1);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    /// A deliberately broken C-like pack: a misspelled class node type.
    struct BrokenPack;
    impl LanguagePack for BrokenPack {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &[".broken"]
        }
        fn grammar(&self) -> Language {
            tree_sitter_c::LANGUAGE.into()
        }
        fn function_types(&self) -> &'static [&'static str] {
            &["function_definition"]
        }
        fn class_types(&self) -> &'static [&'static str] {
            &["struct_specifer"] // deliberate typo
        }
        fn import_node_types(&self) -> &'static [&'static str] {
            &["preproc_include"]
        }
        fn extract_imports(&self, _root: Node, _src: &[u8]) -> Vec<String> {
            Vec::new()
        }
        fn sample(&self) -> &'static str {
            "struct Node { int v; };\nint f(void) { return 0; }\n"
        }
        fn expected(&self) -> PackExpected {
            PackExpected {
                min_functions: 1,
                min_classes: 1,
                kinds: &["struct_specifier"],
                imports: &[],
            }
        }
    }

    #[test]
    fn broken_pack_produces_a_helpful_diagnostic() {
        let report = validate_pack(&BrokenPack);
        assert!(!report.ok());
        let joined = report.diagnostics.join("\n");
        // Names the pack, the offending member, the bad value, and a fix.
        assert!(joined.contains("[pack:broken]"), "{joined}");
        assert!(joined.contains("class_types"), "{joined}");
        assert!(joined.contains("\"struct_specifer\""), "{joined}");
        assert!(joined.contains("Did you mean"), "{joined}");
        assert!(joined.contains("\"struct_specifier\""), "{joined}");
    }

    /// A structurally-broken pack: bad extension casing / missing dot.
    struct BadExtPack;
    impl LanguagePack for BadExtPack {
        fn name(&self) -> &'static str {
            "badext"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["RB"]
        }
        fn grammar(&self) -> Language {
            tree_sitter_ruby::LANGUAGE.into()
        }
        fn function_types(&self) -> &'static [&'static str] {
            &["method"]
        }
        fn class_types(&self) -> &'static [&'static str] {
            &["class"]
        }
        fn import_node_types(&self) -> &'static [&'static str] {
            &[]
        }
        fn extract_imports(&self, _root: Node, _src: &[u8]) -> Vec<String> {
            Vec::new()
        }
        fn sample(&self) -> &'static str {
            "class X\n  def y\n  end\nend\n"
        }
        fn expected(&self) -> PackExpected {
            PackExpected { min_functions: 1, min_classes: 1, kinds: &["class"], imports: &[] }
        }
    }

    #[test]
    fn structural_lint_flags_bad_extension() {
        let report = validate_pack(&BadExtPack);
        assert!(!report.ok());
        let joined = report.diagnostics.join("\n");
        assert!(joined.contains("leading dot"), "{joined}");
        assert!(joined.contains("lowercase"), "{joined}");
    }

    /// A grammar-valid pack that is wired to the wrong node types and expects
    /// imports it never extracts — exercises the Layer-3 behavioural diagnostics.
    struct MiswiredPack;
    impl LanguagePack for MiswiredPack {
        fn name(&self) -> &'static str {
            "miswired"
        }
        fn extensions(&self) -> &'static [&'static str] {
            &[".mw"]
        }
        fn grammar(&self) -> Language {
            tree_sitter_c::LANGUAGE.into()
        }
        fn function_types(&self) -> &'static [&'static str] {
            &["enum_specifier"] // real kind, but the sample has no enum
        }
        fn class_types(&self) -> &'static [&'static str] {
            &["union_specifier"] // real kind, but the sample defines a struct
        }
        fn import_node_types(&self) -> &'static [&'static str] {
            &["preproc_include"]
        }
        fn extract_imports(&self, _root: Node, _src: &[u8]) -> Vec<String> {
            Vec::new()
        }
        fn sample(&self) -> &'static str {
            "struct Node { int v; };\nint f(void) { return 0; }\n"
        }
        fn expected(&self) -> PackExpected {
            PackExpected {
                min_functions: 1,
                min_classes: 1,
                kinds: &["struct_specifier"],
                imports: &["stdlib"],
            }
        }
    }

    #[test]
    fn behavioural_layer_reports_counts_kinds_and_imports() {
        let report = validate_pack(&MiswiredPack);
        assert!(!report.ok());
        let joined = report.diagnostics.join("\n");
        // Under-count of functions and classes, missing kind, and imports mismatch.
        assert!(joined.contains("0 function chunks"), "{joined}");
        assert!(joined.contains("0 class chunks"), "{joined}");
        assert!(joined.contains("kind \"struct_specifier\""), "{joined}");
        assert!(joined.contains("imports mismatch"), "{joined}");
    }
}
