//! # sensitive — Layer-1 sensitive-file policy (SPEC-V2.1 §1/§2)
//!
//! **Why this file exists:** Some files should never be read into the index at
//! all — private keys, credential dumps, `.env` files with live secrets. Reading
//! them, even to redact, would place their bytes in memory and risk leaking them
//! into the store. SPEC-V2.1 Layer 1 makes indexing *secret-safe by default* by
//! deciding, from a file's **basename alone**, whether it must be skipped before
//! any read happens.
//!
//! **What it is / does:** A single pure predicate, [`is_sensitive`], that tests a
//! basename (case-insensitively) against three rules from SPEC-V2.1 §1: a
//! sensitive-extension list, an exact-basename list, and the dotenv rule
//! (`.env` / `.env.*` are sensitive **unless** they carry a safe-template suffix
//! like `.example`).
//!
//! **Responsibilities:**
//! - Own the §1 constant tables (extensions, exact basenames, dotenv suffixes).
//! - Classify a basename as sensitive-or-not, deterministically, from the name.
//! - It does NOT touch the filesystem, read contents, or count skips — the walker
//!   applies this policy and keeps the tally.

use std::path::Path;

/// Sensitive final extensions (compared lower-case, without the dot). SPEC-V2.1 §1.
const SENSITIVE_EXTENSIONS: [&str; 9] =
    ["pem", "key", "p12", "pfx", "keystore", "jks", "ppk", "der", "asc"];

/// Sensitive exact basenames (whole file name, compared lower-case). SPEC-V2.1 §1.
const SENSITIVE_BASENAMES: [&str; 15] = [
    "credentials.json",
    "credentials.yml",
    "credentials.yaml",
    "secrets.json",
    "secrets.yml",
    "secrets.yaml",
    ".netrc",
    ".pgpass",
    ".htpasswd",
    ".dockercfg",
    "kubeconfig",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Suffixes that make a `.env`-family file a safe, indexable template. SPEC-V2.1 §1.
const DOTENV_SAFE_SUFFIXES: [&str; 4] = [".example", ".sample", ".template", ".dist"];

/// Is a file with this `basename` sensitive and therefore never to be read?
///
/// Applies, in order, the three SPEC-V2.1 §1 rules — exact basename, sensitive
/// extension, and the dotenv rule. Comparison is case-insensitive throughout.
pub fn is_sensitive(basename: &str) -> bool {
    let lower = basename.to_ascii_lowercase();

    // Rule 1: exact basename match.
    if SENSITIVE_BASENAMES.contains(&lower.as_str()) {
        return true;
    }

    // Rule 2: sensitive final extension. `Path::extension` treats a leading-dot
    // name (`.env`, `.key`) as having *no* extension, matching OS conventions;
    // such files are handled by the exact-basename and dotenv rules instead.
    if let Some(ext) = Path::new(&lower).extension().and_then(|e| e.to_str()) {
        if SENSITIVE_EXTENSIONS.contains(&ext) {
            return true;
        }
    }

    // Rule 3: dotenv. `.env` or `.env.*` is sensitive, EXCEPT a safe-template
    // suffix (`.env.example`, `.env.local.sample`, …) which is indexed normally.
    if lower == ".env" || lower.starts_with(".env.") {
        if DOTENV_SAFE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
            return false;
        }
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_extensions_are_flagged_case_insensitively() {
        for name in [
            "server.pem",
            "private.KEY",
            "store.p12",
            "cert.PFX",
            "a.keystore",
            "a.jks",
            "putty.ppk",
            "cert.der",
            "key.asc",
        ] {
            assert!(is_sensitive(name), "{name} should be sensitive");
        }
    }

    #[test]
    fn exact_basenames_are_flagged_case_insensitively() {
        for name in [
            "credentials.json",
            "CREDENTIALS.YML",
            "credentials.yaml",
            "secrets.json",
            "secrets.yml",
            "secrets.yaml",
            ".netrc",
            ".pgpass",
            ".htpasswd",
            ".dockercfg",
            "kubeconfig",
            "id_rsa",
            "ID_DSA",
            "id_ecdsa",
            "id_ed25519",
        ] {
            assert!(is_sensitive(name), "{name} should be sensitive");
        }
    }

    #[test]
    fn dotenv_files_are_sensitive() {
        assert!(is_sensitive(".env"));
        assert!(is_sensitive(".ENV"));
        assert!(is_sensitive(".env.local"));
        assert!(is_sensitive(".env.production"));
        assert!(is_sensitive(".env.local.secret"));
    }

    #[test]
    fn dotenv_safe_templates_are_indexable() {
        for name in [
            ".env.example",
            ".env.sample",
            ".env.template",
            ".env.dist",
            ".env.local.example",
            ".env.EXAMPLE",
        ] {
            assert!(!is_sensitive(name), "{name} should be a safe template");
        }
    }

    #[test]
    fn ordinary_source_files_are_not_sensitive() {
        for name in [
            "config.rb",
            "main.rs",
            "auth.py",
            "README.md",
            "notes.txt",
            "foo.env",
            "envfile",
            "keys.go",
            "monkey.js",
        ] {
            assert!(!is_sensitive(name), "{name} should NOT be sensitive");
        }
    }

    #[test]
    fn leading_dot_only_name_has_no_extension() {
        // `.key` is a hidden file with no extension (OS convention), so it is not
        // caught by the extension rule — and it is not an exact/dotenv match.
        assert!(!is_sensitive(".key"));
        assert!(!is_sensitive(".pem"));
    }
}
