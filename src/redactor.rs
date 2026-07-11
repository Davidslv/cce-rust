//! # redactor — Layer-2 secret redaction (SPEC-V2.1 §1/§2)
//!
//! **Why this file exists:** A file can pass Layer 1 (its name looks innocuous)
//! yet still contain a hard-coded credential in its body — an AWS key in a config
//! module, a token in a fixture. SPEC-V2.1 Layer 2 scrubs those high-confidence
//! secrets *before* chunking, so the value never reaches the embedder or the
//! on-disk store: the redacted text is what gets chunked, embedded, and persisted.
//!
//! **What it is / does:** [`redact`] runs the SPEC-V2.1 §1 pattern table over a
//! string. Nine specific patterns (private keys, provider API keys, JWTs, …) run
//! first, each replacing the matched value with `[REDACTED:<LABEL>]`; then one
//! generic `key = value` assignment pattern runs, guarded so it never scrubs
//! documentation placeholders (`API_KEY="your-api-key-here"`) or a value an
//! earlier pattern already redacted. Matching is deterministic: identical input
//! always yields identical output, which the cross-language equivalence gate relies on.
//!
//! **Responsibilities:**
//! - Own the §1 regex table, its ordering, and the `[REDACTED:LABEL]` form.
//! - Own the generic-assignment placeholder guard (SPEC-V2.1 §1).
//! - It does NOT decide which files to read (Layer 1) or perform chunking.

use regex::{Captures, Regex};
use std::sync::LazyLock;

/// The nine specific, high-confidence patterns (SPEC-V2.1 §1, rows 1–9), applied
/// in order. Each entry is `(regex, label)`; the whole match is replaced with
/// `[REDACTED:<label>]`.
static SPECIFIC: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let table: [(&str, &str); 9] = [
        (
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            "PRIVATE_KEY",
        ),
        (r"sk-ant-[A-Za-z0-9_-]{20,}", "ANTHROPIC_KEY"),
        (r"sk-[A-Za-z0-9]{32,}", "OPENAI_KEY"),
        (r"sk_live_[A-Za-z0-9]{16,}", "STRIPE_KEY"),
        (r"gh[pousr]_[A-Za-z0-9]{36,}", "GITHUB_TOKEN"),
        (r"xox[baprs]-[A-Za-z0-9-]{10,}", "SLACK_TOKEN"),
        (r"AKIA[0-9A-Z]{16}", "AWS_ACCESS_KEY"),
        (r"AIza[0-9A-Za-z_-]{35}", "GOOGLE_API_KEY"),
        (r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "JWT"),
    ];
    table
        .iter()
        .map(|(re, label)| (Regex::new(re).expect("valid redaction regex"), *label))
        .collect()
});

/// The generic assignment pattern (SPEC-V2.1 §1, row 10). Captures:
/// 1 = secret-ish key, 2 = operator (`=`/`:`) with surrounding spaces, then one
/// of three value branches (leftmost-first): `dq` = a double-quoted value (runs
/// to the matching `"`, so it may contain `'` and spaces), `sq` = a
/// single-quoted value (runs to the matching `'`, so it may contain `"` and
/// spaces), `uq` = an unquoted value (runs to whitespace/line end, so it may
/// contain an apostrophe — #104). A quoted value never crosses a line, and the
/// closing quote is deliberately left outside the match so it survives.
static GENERIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|secret[_-]?key|secret|api[_-]?key|access[_-]?key|auth[_-]?token|private[_-]?key|token)\b(\s*[:=]\s*)(?:"(?P<dq>[^"\n]+)|'(?P<sq>[^'\n]+)|(?P<uq>\S+))"#,
    )
    .expect("valid generic-assignment regex")
});

/// Redact every high-confidence secret in `content` per SPEC-V2.1 §1.
///
/// Specific patterns (rows 1–9) run first in table order, then the generic
/// assignment (row 10). Returns the scrubbed string; deterministic.
pub fn redact(content: &str) -> String {
    let mut text = content.to_string();

    for (re, label) in SPECIFIC.iter() {
        let replacement = format!("[REDACTED:{label}]");
        // NoExpand: treat the replacement literally (labels contain no `$`).
        text = re.replace_all(&text, regex::NoExpand(&replacement)).into_owned();
    }

    GENERIC
        .replace_all(&text, |caps: &Captures| {
            // Exactly one value branch participates; recover the opening quote
            // (if any) from which branch it was, so it can be re-emitted.
            let (quote, value) = if let Some(m) = caps.name("dq") {
                ("\"", m.as_str())
            } else if let Some(m) = caps.name("sq") {
                ("'", m.as_str())
            } else {
                ("", caps.name("uq").expect("a value branch always matches").as_str())
            };
            if value.len() < 8 || is_placeholder(value) {
                caps[0].to_string() // leave the whole match untouched
            } else {
                format!("{}{}{quote}[REDACTED:SECRET]", &caps[1], &caps[2])
            }
        })
        .into_owned()
}

/// Should the generic assignment (row 10) leave this value alone? True for
/// documentation placeholders, interpolations, literals, and a value an earlier
/// specific pattern already turned into `[REDACTED:…]` (SPEC-V2.1 §1 guard).
fn is_placeholder(value: &str) -> bool {
    // A value a specific pattern already redacted must not be re-redacted; this
    // keeps output idempotent and preserves the specific label (e.g. a
    // `token = "[REDACTED:GITHUB_TOKEN]"` stays GITHUB_TOKEN, not SECRET).
    if value.starts_with("[REDACTED:") {
        return true;
    }

    let v = value.to_ascii_lowercase();

    const PLACEHOLDER_PREFIXES: [&str; 10] = [
        "your",
        "my-",
        "the-",
        "example",
        "changeme",
        "placeholder",
        "dummy",
        "test",
        "sample",
        "xxx",
    ];
    if PLACEHOLDER_PREFIXES.iter().any(|p| v.starts_with(p)) {
        return true;
    }

    // Interpolation markers: `<...>`, `${...}`, `{{...}}`.
    if (v.starts_with('<') && v.ends_with('>'))
        || (v.starts_with("${") && v.ends_with('}'))
        || (v.starts_with("{{") && v.ends_with("}}"))
    {
        return true;
    }

    // Literal non-secrets.
    if ["null", "nil", "none", "true", "false"].contains(&v.as_str()) {
        return true;
    }

    // A single repeated character (e.g. `--------`, `00000000`).
    is_single_repeated_char(value)
}

/// True if `value` is one character repeated (length ≥ 1).
fn is_single_repeated_char(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => chars.all(|c| c == first),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Secret-shaped test inputs are assembled from split fragments via `concat!`
    // so no committed source file contains a contiguous secret literal (GitHub
    // push protection would block it). `concat!` joins at compile time, so the
    // redactor still sees a real, full-format secret at runtime.
    const AWS_KEY: &str = concat!("AKIA", "IOSFODNN7EXAMPLE");
    const STRIPE_KEY: &str = concat!("sk", "_live_", "4eC39HqLyjWDarjtT1zdp7dc");
    const GH_TOKEN: &str = concat!("ghp", "_", "0123456789abcdefghijklmnopqrstuvwx01");
    const OPENAI_KEY: &str = concat!("sk", "-", "abcdefghijklmnopqrstuvwxyz012345ABCD");
    const ANTHROPIC_KEY: &str = concat!("sk-", "ant-", "api03_abcdefghijklmnopqrstuvwxyz0123");
    const SLACK_TOKEN: &str = concat!("xox", "b-", "1234567890-abcdefABCDEF");
    const GOOGLE_KEY: &str = concat!("AIza", "SyA1234567890abcdefghijklmnopqrstuv");

    #[test]
    fn redacts_aws_access_key() {
        assert_eq!(
            redact(&format!(r#"AWS = "{AWS_KEY}""#)),
            r#"AWS = "[REDACTED:AWS_ACCESS_KEY]""#
        );
    }

    #[test]
    fn redacts_stripe_live_key() {
        assert_eq!(redact(&format!(r#"key = "{STRIPE_KEY}""#)), r#"key = "[REDACTED:STRIPE_KEY]""#);
    }

    #[test]
    fn redacts_github_token_and_keeps_specific_label() {
        // SPEC-V2.1 §3: the key `token` is secret-ish, but the value has already
        // been redacted by the specific GITHUB_TOKEN pattern, so the generic
        // pattern must NOT re-label it as SECRET.
        assert_eq!(
            redact(&format!(r#"token = "{GH_TOKEN}""#)),
            r#"token = "[REDACTED:GITHUB_TOKEN]""#
        );
    }

    #[test]
    fn redacts_openai_key() {
        assert_eq!(redact(&format!("OPENAI={OPENAI_KEY}")), "OPENAI=[REDACTED:OPENAI_KEY]");
    }

    #[test]
    fn redacts_anthropic_key_before_openai() {
        assert_eq!(redact(&format!("A={ANTHROPIC_KEY}")), "A=[REDACTED:ANTHROPIC_KEY]");
    }

    #[test]
    fn redacts_slack_token() {
        assert_eq!(redact(&format!("SLACK={SLACK_TOKEN}")), "SLACK=[REDACTED:SLACK_TOKEN]");
    }

    #[test]
    fn redacts_google_api_key() {
        assert_eq!(redact(&format!("G={GOOGLE_KEY}")), "G=[REDACTED:GOOGLE_API_KEY]");
    }

    #[test]
    fn redacts_jwt() {
        let jwt = concat!(
            "eyJ",
            "hbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            ".eyJ",
            "zdWIiOiIxMjM0NTY3ODkwIn0",
            ".abcDEF123456"
        );
        assert_eq!(redact(jwt), "[REDACTED:JWT]");
    }

    #[test]
    fn redacts_private_key_block() {
        // Markers are split around "KEY" so the doc/source carries no contiguous
        // "PRIVATE KEY" literal; `concat!` restores it at runtime.
        let block = concat!(
            "-----BEGIN RSA PRIVATE ",
            "KEY-----\nMIIBOgIBAAJBAK\n-----END RSA PRIVATE ",
            "KEY-----"
        );
        assert_eq!(redact(block), "[REDACTED:PRIVATE_KEY]");
    }

    #[test]
    fn redacts_generic_secret_assignment() {
        assert_eq!(redact(r#"password = "hunter2secret""#), r#"password = "[REDACTED:SECRET]""#);
        assert_eq!(redact("api_key: s3cr3tvalue"), "api_key: [REDACTED:SECRET]");
    }

    #[test]
    fn placeholder_guard_leaves_examples_unchanged() {
        // SPEC-V2.1 §3 negative: a documentation placeholder is not a secret.
        assert_eq!(redact(r#"key = "your-api-key""#), r#"key = "your-api-key""#);
        for value in [
            "your-token-here",
            "changeme",
            "placeholder",
            "dummy-value",
            "test-secret",
            "sample-key",
            "xxxxxxxx",
            "example-value",
        ] {
            let input = format!("secret = \"{value}\"");
            assert_eq!(redact(&input), input, "value {value} must be guarded");
        }
    }

    #[test]
    fn placeholder_guard_covers_interpolation_and_literals() {
        for value in [
            "${DB_PASSWORD}",
            "<your-secret>",
            "{{token}}",
            "null",
            "nil",
            "none",
            "true",
            "false",
            "--------",
        ] {
            let input = format!("password = {value}");
            assert_eq!(redact(&input), input, "value {value} must be guarded");
        }
    }

    #[test]
    fn short_values_below_eight_chars_are_not_redacted() {
        // Length < 8 is below the generic threshold (SPEC-V2.1 §1).
        assert_eq!(redact("password = short12"), "password = short12");
        assert_eq!(redact(r#"token = "abc""#), r#"token = "abc""#);
    }

    #[test]
    fn redacts_unquoted_value_containing_apostrophe() {
        // #104 mode (a): an apostrophe inside an unquoted value must not
        // truncate the match to a short prefix that the len<8 guard then skips.
        let out = redact("password = don't-tell-anyone-secretvalue");
        assert_eq!(out, "password = [REDACTED:SECRET]");
        assert!(!out.contains("tell-anyone"), "secret tail leaked: {out}");
    }

    #[test]
    fn redacts_double_quoted_value_containing_apostrophe() {
        // #104 mode (b): an apostrophe inside a double-quoted value must not
        // end the value early and leave the tail unredacted. The closing `"`
        // stays outside the match and survives.
        let out = redact(r#"password = "abcdefghij'tail-super-secret""#);
        assert_eq!(out, r#"password = "[REDACTED:SECRET]""#);
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
    }

    #[test]
    fn redacts_single_quoted_value_and_preserves_closing_quote() {
        // #104 use case (c), pinned: a single-quoted value redacts up to (not
        // including) its closing quote.
        assert_eq!(redact("api_key='qwertyuiop-secret'"), "api_key='[REDACTED:SECRET]'");
    }

    #[test]
    fn redacts_double_quoted_value_containing_single_quote_of_other_kind() {
        // Symmetric to mode (b): a double quote inside a single-quoted value.
        let out = redact(r#"password = 'abcdefghij"tail-super-secret'"#);
        assert_eq!(out, "password = '[REDACTED:SECRET]'");
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
    }

    #[test]
    fn redacts_quoted_value_containing_space() {
        // A quoted value extends to its matching closing quote, so an inner
        // space must not split it into a short (guard-skipped) prefix.
        assert_eq!(
            redact(r#"password = "correct horse battery staple""#),
            r#"password = "[REDACTED:SECRET]""#
        );
    }

    #[test]
    fn adjacent_assignments_do_not_over_capture_across_closing_quotes() {
        // The quoted-value fix must stop at the FIRST matching closing quote,
        // never spanning into a neighbouring assignment on the same line.
        assert_eq!(
            redact(r#"password = "a1b2c3d4e5" token = "f6g7h8i9j0""#),
            r#"password = "[REDACTED:SECRET]" token = "[REDACTED:SECRET]""#
        );
    }

    #[test]
    fn quoted_values_do_not_span_lines() {
        // An unterminated quote must not swallow the next line.
        let out = redact("password = \"abcdefghij\ntoken = \"klmnopqrst\"");
        assert_eq!(out, "password = \"[REDACTED:SECRET]\ntoken = \"[REDACTED:SECRET]\"");
    }

    #[test]
    fn empty_or_quote_only_values_are_left_untouched() {
        for input in [r#"password = """#, "password = ''", r#"password = ""#, "password ="] {
            assert_eq!(redact(input), input, "input {input:?} must be untouched");
        }
    }

    #[test]
    fn non_secret_key_names_are_ignored_by_generic() {
        // `AWS` / `DATABASE_URL` are not in the secret-ish key list.
        let input = "DATABASE_URL=postgres://user:password@localhost/app";
        assert_eq!(redact(input), input);
    }

    #[test]
    fn redaction_is_idempotent() {
        let input = format!(r#"AWS = "{AWS_KEY}""#);
        let once = redact(&input);
        assert_eq!(redact(&once), once);
    }
}
