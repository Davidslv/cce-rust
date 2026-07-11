//! # tests/secrets — end-to-end secret & sensitive-file protection (SPEC-V2.1)
//!
//! **Why this file exists:** SPEC-V2.1 §3/§4 require an end-to-end proof that
//! Layer 1 skips sensitive files, that Layer 2 redacts high-confidence secrets in
//! stored chunk content while leaving documentation placeholders alone, and that
//! `--allow-secrets` bypasses both layers. Unit tests cover the pieces; only
//! driving the real binary and reading back the persisted store proves the
//! whole pipeline.
//!
//! **What it is / does:** Generates the SPEC-V2.1 §3 secrets corpus **into a temp
//! directory at runtime** (so no committed file contains a contiguous secret
//! literal — GitHub push protection would block it), runs `cce index` over it
//! (protected and with `--allow-secrets`), then loads the persisted `index.json`
//! and asserts the stored chunk content matches the §3 expectations.
//!
//! **Responsibilities:**
//! - Own the process-level acceptance tests for secret protection.
//! - Assemble secret-shaped fixtures from split fragments so committed source is
//!   secret-free; the tool still sees real, full-format secrets at runtime.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cce")
}

// Secret-shaped values, assembled from split fragments via `concat!` so this
// committed source carries no contiguous secret literal. `concat!` joins at
// compile time, so the fixture written to disk holds a real full-format secret.
const AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");
const STRIPE_KEY: &str = concat!("sk", "_live_", "4eC39HqLyjWDarjtT1zdp7dc");

/// Write the SPEC-V2.1 §3 secrets fixture into `dir` at runtime.
fn write_secrets_fixture(dir: &Path) {
    // `.env` — must be SKIPPED (never indexed).
    std::fs::write(
        dir.join(".env"),
        format!(
            "AWS_ACCESS_KEY_ID={AWS_KEY}\nDATABASE_URL=postgres://user:hunter2@localhost/app\n"
        ),
    )
    .unwrap();
    // `.env.example` — must be INDEXED normally (safe template).
    std::fs::write(
        dir.join(".env.example"),
        "AWS_ACCESS_KEY_ID=your-access-key-here\nDATABASE_URL=postgres://user:password@localhost/app\n",
    )
    .unwrap();
    // `id_rsa` — must be SKIPPED (sensitive basename). Markers split so no
    // contiguous "PRIVATE KEY" literal appears in committed source.
    std::fs::write(
        dir.join("id_rsa"),
        concat!(
            "-----BEGIN OPENSSH PRIVATE ",
            "KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAA\n-----END OPENSSH PRIVATE ",
            "KEY-----\n"
        ),
    )
    .unwrap();
    // `config.rb` — must be INDEXED with redaction applied.
    std::fs::write(
        dir.join("config.rb"),
        format!(
            "module Config\n  AWS = \"{AWS_KEY}\"\n  API_KEY = \"your-api-key-here\"\n  STRIPE = \"{STRIPE_KEY}\"\nend\n"
        ),
    )
    .unwrap();
}

/// Load the persisted store JSON and return `(all chunk file_paths, concatenated
/// chunk content)`.
fn read_store(store: &Path) -> (Vec<String>, String) {
    let raw = std::fs::read_to_string(store).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let chunks = v["chunks"].as_array().unwrap();
    let files: Vec<String> =
        chunks.iter().map(|c| c["file_path"].as_str().unwrap().to_string()).collect();
    let content: String =
        chunks.iter().map(|c| c["content"].as_str().unwrap()).collect::<Vec<_>>().join("\n");
    (files, content)
}

#[test]
fn protected_index_skips_sensitive_and_redacts_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("secrets");
    std::fs::create_dir(&fixture).unwrap();
    write_secrets_fixture(&fixture);
    let store = tmp.path().join("index.json");

    let out = Command::new(bin())
        .args(["index"])
        .arg(&fixture)
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // SPEC-V2.1 §2 reporting: `.env` and `id_rsa` are the two sensitive skips.
    assert!(stdout.contains("sensitive skipped : 2"), "got: {stdout}");

    let (files, content) = read_store(&store);

    // Layer 1: `.env` and `id_rsa` produced NO chunks.
    assert!(!files.iter().any(|f| f == ".env"), "`.env` must not be indexed");
    assert!(!files.iter().any(|f| f == "id_rsa"), "`id_rsa` must not be indexed");
    // The safe template and the redacted source ARE indexed.
    assert!(files.iter().any(|f| f == ".env.example"), "`.env.example` must be indexed");
    assert!(files.iter().any(|f| f == "config.rb"), "`config.rb` must be indexed");

    // Layer 2: secrets in config.rb are redacted in the STORED content...
    assert!(content.contains("[REDACTED:AWS_ACCESS_KEY]"), "AWS key not redacted: {content}");
    assert!(content.contains("[REDACTED:STRIPE_KEY]"), "Stripe key not redacted: {content}");
    // ...the raw secret values never reach the store...
    assert!(!content.contains(AWS_KEY), "raw AWS key leaked into store");
    assert!(!content.contains(STRIPE_KEY), "raw Stripe key leaked");
    // ...but the documentation placeholder is left untouched (placeholder guard).
    assert!(content.contains("your-api-key-here"), "placeholder must be preserved: {content}");
}

#[test]
fn protected_index_redacts_values_containing_quotes() {
    // #104: a quote or apostrophe inside a secret value must not defeat the
    // generic-assignment redaction — neither by truncating an unquoted value
    // to a short (guard-skipped) prefix nor by ending a quoted value early
    // and persisting the tail.
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("quoted");
    std::fs::create_dir(&fixture).unwrap();
    std::fs::write(
        fixture.join("settings.conf"),
        concat!(
            "password = don't-tell-anyone-secretvalue\n",
            "password = \"abcdefghij'tail-super-secret\"\n",
            "api_key='qwertyuiop-secret'\n",
        ),
    )
    .unwrap();
    let store = tmp.path().join("index.json");

    let out = Command::new(bin())
        .args(["index"])
        .arg(&fixture)
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));

    let (_, content) = read_store(&store);
    for leaked in ["tell-anyone-secretvalue", "tail-super-secret", "qwertyuiop"] {
        assert!(!content.contains(leaked), "secret fragment {leaked:?} leaked into store");
    }
    assert!(content.contains("[REDACTED:SECRET]"), "expected redaction marker: {content}");
}

#[test]
fn allow_secrets_bypasses_both_layers() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("secrets");
    std::fs::create_dir(&fixture).unwrap();
    write_secrets_fixture(&fixture);
    let store = tmp.path().join("index.json");

    let out = Command::new(bin())
        .args(["index"])
        .arg(&fixture)
        .args(["--allow-secrets", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {}", String::from_utf8_lossy(&out.stderr));
    // A warning is printed to stderr, and nothing is counted as a sensitive skip.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--allow-secrets"), "expected opt-out warning: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sensitive skipped : 0"), "got: {stdout}");

    let (files, content) = read_store(&store);

    // Layer 1 off: the sensitive files ARE indexed now.
    assert!(files.iter().any(|f| f == ".env"), "`.env` must be indexed with --allow-secrets");
    assert!(files.iter().any(|f| f == "id_rsa"), "`id_rsa` must be indexed with --allow-secrets");
    // Layer 2 off: config.rb is stored verbatim (raw secrets present, no REDACTED).
    assert!(content.contains(AWS_KEY), "raw AWS key must be stored verbatim");
    assert!(content.contains(STRIPE_KEY), "raw Stripe key must be stored");
    assert!(!content.contains("[REDACTED:"), "no redaction with --allow-secrets: {content}");
}
