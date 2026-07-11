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

use regex::Regex;
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

/// The generic assignment PREFIX (SPEC-V2.1 §1, row 10): a secret-ish key
/// (group 1) and its `=`/`:` operator with surrounding spaces (group 2). The
/// VALUE that follows is deliberately NOT part of this regex — it is delimited by
/// [`scan_value`], a small explicit quote/escape-aware scanner.
///
/// **Why a scanner, not a regex value branch (#142).** A quoted value's true
/// extent depends on TWO escape conventions (backslash `\"` and doubled `''` /
/// `""` / backtick-pair) and on telling a structural close from an inner quote.
/// Encoding that in the crate's regex (no look-around, no back-references) forced
/// a "continue-unless-whitespace" heuristic that both leaked (doubled quotes) and
/// over-captured (merging `"a", "b"` and swallowing `.freeze` / a JSON sibling).
/// A single explicit pass expresses the real grammar without that fragility.
static GENERIC_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|passwd|secret[_-]?key|secret|api[_-]?key|access[_-]?key|auth[_-]?token|private[_-]?key|token)\b(\s*[:=]\s*)"#,
    )
    .expect("valid generic-assignment prefix regex")
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

    redact_generic(&text)
}

/// Run the generic assignment redaction (SPEC-V2.1 §1, row 10) over `text`.
///
/// For each secret-ish `key = ` / `key: ` prefix, the value that follows is
/// delimited by [`scan_value`] and — unless it is shorter than 8 bytes or a
/// documentation placeholder — replaced with `[REDACTED:SECRET]` (any surrounding
/// quotes preserved). Assignments are handled left to right; a prefix that lies
/// INSIDE a value already consumed by an earlier assignment is skipped, so a
/// `key=` that is itself part of a secret value never fires a second, overlapping
/// redaction. Because each value is delimited to a single, structurally-bounded
/// span, the placeholder guard can never short-circuit more than one value.
fn redact_generic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;

    for caps in GENERIC_PREFIX.captures_iter(text) {
        let whole = caps.get(0).expect("group 0 always present");
        // Skip a prefix that falls within a value we already consumed/redacted.
        if whole.start() < cursor {
            continue;
        }
        let value_start = whole.end();
        let Some(scanned) = scan_value(&text[value_start..]) else {
            // No value after the operator: leave the prefix in place (it will be
            // emitted verbatim as part of a later gap or the final tail).
            continue;
        };
        let value_end = value_start + scanned.full.len();

        // Emit the untouched text between the cursor and this assignment.
        out.push_str(&text[cursor..whole.start()]);

        if scanned.inner.len() < 8 || is_placeholder(scanned.inner) {
            // Byte-identical: re-emit the whole key/op/value verbatim.
            out.push_str(&text[whole.start()..value_end]);
        } else {
            out.push_str(caps.get(1).expect("key group").as_str());
            out.push_str(caps.get(2).expect("operator group").as_str());
            match scanned.quote {
                Some(q) => {
                    out.push(q);
                    out.push_str("[REDACTED:SECRET]");
                    // Re-emit the closing quote only when the value was actually
                    // terminated (an unterminated line-end value has none).
                    if scanned.closed {
                        out.push(q);
                    }
                }
                None => out.push_str("[REDACTED:SECRET]"),
            }
        }
        cursor = value_end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// The delimited extent of an assignment value, as parsed by [`scan_value`].
struct Scanned<'a> {
    /// The full byte-slice the value occupies (including any surrounding quotes):
    /// what a redaction replaces, or what an untouched value re-emits verbatim.
    full: &'a str,
    /// The content used for the length and placeholder checks — the text between
    /// the quotes (quoted), or the whole run (unquoted).
    inner: &'a str,
    /// The opening quote character, if the value was quoted.
    quote: Option<char>,
    /// Whether a matching closing quote was consumed at the value's end (false for
    /// an unquoted value or a quoted value left unterminated at the line end).
    closed: bool,
}

/// True when byte `b` glues an inner quote to a longer token: an ASCII word char
/// (`[A-Za-z0-9_-]`). A candidate closing quote *immediately followed by* such a
/// char is treated as an INNER quote (the SPEC-V2.1 §1 / #142 same-delimiter
/// secret-tail shape, `'abc'tail`), so the scan keeps going and no tail can
/// survive. A quote followed by whitespace, the line end, or ANY other char
/// (`,` `)` `}` `.` `;` …) is the structural close — so a sibling value or
/// trailing code (`", host: …`, `".freeze`) is left intact, not over-captured.
fn is_value_glue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Delimit the assignment value at the start of `s` (the text immediately after
/// the operator). Returns `None` when no value is present (empty, or the operator
/// is followed by whitespace or the line end).
///
/// **Quoted values (#142).** A value opened with `"`, `'`, or `` ` `` runs to its
/// TRUE structural close, handling BOTH escape conventions so no secret tail can
/// survive past a mid-value quote:
///   - a backslash escapes the next char (`\"` does not close the value), but it
///     never escapes a newline — a value never crosses a line;
///   - a DOUBLED quote (`''`, `""`, ` `` ``) is a literal escaped quote and does
///     not close the value;
///   - a lone quote is the close UNLESS immediately followed by an ASCII word
///     char (see [`is_value_glue`]), in which case it is an inner quote and the
///     scan continues.
///
/// A value unterminated at the line end closes at the newline with `closed = false`.
///
/// **Unquoted values (#104).** Run to the next ASCII whitespace or the line end,
/// so an apostrophe or an inner structural char (a connection string's `,`) stays
/// in the value rather than leaking as a truncated tail.
fn scan_value(s: &str) -> Option<Scanned<'_>> {
    let bytes = s.as_bytes();
    let &first = bytes.first()?;
    if first.is_ascii_whitespace() {
        return None;
    }

    if first == b'"' || first == b'\'' || first == b'`' {
        let quote = first;
        let mut i = 1;
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'\n' {
                break; // unterminated at the line end
            }
            if c == b'\\' {
                // Escape the next char, but never a newline (no line crossing).
                i += 1;
                if i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if c == quote {
                // A doubled quote (`''`) is a literal escaped quote — consume both.
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                // A quote glued to a word char is an inner quote; keep scanning.
                if bytes.get(i + 1).is_some_and(|&b| is_value_glue(b)) {
                    i += 1;
                    continue;
                }
                // Structural close: include the closing quote in `full`.
                return Some(Scanned {
                    full: &s[..=i],
                    inner: &s[1..i],
                    quote: Some(quote as char),
                    closed: true,
                });
            }
            i += 1;
        }
        // Unterminated: the value runs to the newline / end of input.
        return Some(Scanned {
            full: &s[..i],
            inner: &s[1..i],
            quote: Some(quote as char),
            closed: false,
        });
    }

    // Unquoted: run to the next ASCII whitespace (matches the old `\S+`).
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    Some(Scanned { full: &s[..i], inner: &s[..i], quote: None, closed: false })
}

/// Should the generic assignment (row 10) leave this value alone? True for
/// documentation placeholders, interpolations, literals, and a value an earlier
/// specific pattern already turned into `[REDACTED:…]` (SPEC-V2.1 §1 guard).
fn is_placeholder(value: &str) -> bool {
    // A value that is EXACTLY a `[REDACTED:LABEL]` token must not be re-redacted;
    // this keeps output idempotent and preserves the specific label (e.g. a
    // `token = "[REDACTED:GITHUB_TOKEN]"` stays GITHUB_TOKEN, not SECRET).
    //
    // #142: the guard must be EXACT. A value that merely *begins* with a
    // placeholder and continues with more content (e.g. a specific pattern
    // redacted only a prefix — `[REDACTED:AWS_ACCESS_KEY]suffix-secret`) still
    // hides a real secret tail, so it must fall through and be scrubbed.
    if is_exact_redacted_token(value) {
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

/// True iff `value` is EXACTLY a single `[REDACTED:LABEL]` token — i.e. it opens
/// with `[REDACTED:` and the first `]` is the final character, with nothing after
/// it. A value that begins with such a token but carries a trailing tail (#142)
/// is deliberately NOT matched, so its remainder is still scrubbed.
fn is_exact_redacted_token(value: &str) -> bool {
    match value.strip_prefix("[REDACTED:") {
        Some(inner) => matches!(inner.find(']'), Some(i) if i == inner.len() - 1),
        None => false,
    }
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

    // ------------------------------------------------------------------
    // #142 — the two residual tail-leaks (RED-first leak matrix).
    // Each of these leaked a real secret tail into the persisted store on
    // pre-#142 code; after the fix, NO secret fragment may survive.
    // ------------------------------------------------------------------

    #[test]
    fn leak142_same_single_quote_prefix_ge8_no_tail() {
        // Residual 1a: a same-style quote INSIDE a single-quoted value. The
        // old lazy scan stopped at the inner `'`, redacted the ≥8 prefix, and
        // left `tail-super-secret` behind. Quote-aware extent now consumes the
        // whole value up to the true closing quote.
        let out = redact("password = 'abcdefghij'tail-super-secret'");
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
        assert_eq!(out, "password = '[REDACTED:SECRET]'");
    }

    #[test]
    fn leak142_same_double_quote_escaped_inner_no_tail() {
        // Residual 1b: a JSON-escaped inner double quote (`\"`). The old scan
        // stopped at the backslash-quote and leaked the tail.
        let out = redact(r#"password = "abcdefghij\"tail-super-secret""#);
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
        assert_eq!(out, r#"password = "[REDACTED:SECRET]""#);
    }

    #[test]
    fn leak142_specific_prefix_then_secret_no_tail() {
        // Residual 2: a specific pattern (AWS) redacts a PREFIX of a longer
        // concatenated value; the over-broad idempotency guard then skipped the
        // whole value, leaking `suffix-secret`. Now only an EXACT `[REDACTED:…]`
        // value is skipped; a value that merely begins with one is re-scrubbed.
        let out = redact(&format!(r#"password = "{AWS_KEY}suffix-secret""#));
        assert!(!out.contains("suffix-secret"), "secret tail leaked: {out}");
        assert_eq!(out, r#"password = "[REDACTED:SECRET]""#);
    }

    #[test]
    fn leak142_specific_prefix_then_secret_unquoted_no_tail() {
        let out = redact(&format!("password = {AWS_KEY}suffix-secret"));
        assert!(!out.contains("suffix-secret"), "secret tail leaked: {out}");
        assert_eq!(out, "password = [REDACTED:SECRET]");
    }

    #[test]
    fn backtick_quoted_value_is_redacted_as_a_new_feature() {
        // NOT a pre-existing leak (an inner-quote backtick value has no spaces,
        // so the old `\S+` unquoted branch already caught it whole). This pins
        // the new first-class backtick handling: the value redacts to a single
        // token with its backticks preserved, and no fragment survives.
        let out = redact("password = `abcdefghij`tail-super-secret`");
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
        assert_eq!(out, "password = `[REDACTED:SECRET]`");
    }

    #[test]
    fn leak142_multiple_inner_quotes_no_tail() {
        let out = redact("password = 'abcdefgh'mid'tail-super-secret'");
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
        assert!(!out.contains("mid"), "inner fragment leaked: {out}");
        assert_eq!(out, "password = '[REDACTED:SECRET]'");
    }

    // ------------------------------------------------------------------
    // #142 — controls: the fix must not over-capture or regress.
    // ------------------------------------------------------------------

    #[test]
    fn ctrl142_adjacent_same_delimiter_assignments_not_merged() {
        // Two genuinely separate same-line assignments (whitespace-delimited)
        // must each redact independently — the quote-aware scan must NOT merge
        // them into one over-captured span.
        assert_eq!(
            redact("password = 'a1b2c3d4e5' token = 'f6g7h8i9j0'"),
            "password = '[REDACTED:SECRET]' token = '[REDACTED:SECRET]'"
        );
    }

    #[test]
    fn ctrl142_value_beginning_with_placeholder_but_continuing_is_scrubbed() {
        // The idempotency guard skips a value that is EXACTLY a placeholder,
        // but NOT one that merely begins with one and continues with content.
        assert!(is_placeholder("[REDACTED:AWS_ACCESS_KEY]"), "exact placeholder must be skipped");
        assert!(
            !is_placeholder("[REDACTED:AWS_ACCESS_KEY]suffix-secret"),
            "value continuing past a placeholder must be re-scrubbed"
        );
    }

    #[test]
    fn ctrl142_clean_non_secret_assignment_is_byte_identical() {
        // A recognised-key assignment whose value is a documentation placeholder
        // is returned byte-for-byte unchanged.
        for input in [
            r#"password = "changeme""#,
            r#"api_key = "<your-api-key>""#,
            r#"secret = "${VAULT_SECRET}""#,
        ] {
            assert_eq!(redact(input), input, "input {input:?} must be byte-identical");
        }
    }

    // ------------------------------------------------------------------
    // Round-2 findings (#142): the "continue-unless-whitespace" heuristic is
    // replaced by an explicit escape/close-aware scanner. Each RED case below
    // either leaked or over-captured under the heuristic.
    // ------------------------------------------------------------------

    #[test]
    fn r2_doubled_single_quote_no_tail() {
        // Finding 1: SQL/CSV-style doubled-quote escaping (`''`). The heuristic
        // stopped at the first `'`, redacted the prefix, and left the tail.
        let out = redact("password = 'abcdefghij''tail-super-secret'");
        assert!(!out.contains("tail-super-secret"), "secret tail leaked: {out}");
        assert_eq!(out, "password = '[REDACTED:SECRET]'");
    }

    #[test]
    fn r2_doubled_single_quote_short_prefix_fully_redacted() {
        // Finding 1 amplifier: with a <8 prefix the whole value used to pass the
        // len guard and persist UNREDACTED. The doubled-quote-aware extent makes
        // the value the full `abc''realsecret-here`, so it redacts.
        let out = redact("password = 'abc''realsecret-here'");
        assert!(!out.contains("realsecret"), "secret leaked: {out}");
        assert_eq!(out, "password = '[REDACTED:SECRET]'");
    }

    #[test]
    fn r2_doubled_double_quote_and_backtick_no_tail() {
        let dq = redact(r#"password = "abcdefghij""tail-super-secret""#);
        assert!(!dq.contains("tail-super-secret"), "secret tail leaked: {dq}");
        assert_eq!(dq, r#"password = "[REDACTED:SECRET]""#);

        let bq = redact("password = `abcdefghij``tail-super-secret`");
        assert!(!bq.contains("tail-super-secret"), "secret tail leaked: {bq}");
        assert_eq!(bq, "password = `[REDACTED:SECRET]`");
    }

    #[test]
    fn r2_comma_adjacent_not_merged_and_placeholder_scoped() {
        // Finding 2 (regression the heuristic introduced): the close quote of
        // value 1 is followed by `,` (non-whitespace), so the heuristic MERGED
        // the two assignments, then the placeholder guard saw the merged span
        // start with `changeme` and skipped BOTH — leaking `token`'s secret.
        // The scanner closes value 1 at the structural `,`, so `token` is scanned
        // and redacted independently.
        let out = redact(r#"password = "changeme", token = "realsecrettail123""#);
        assert!(!out.contains("realsecrettail123"), "secret tail leaked: {out}");
        assert_eq!(out, r#"password = "changeme", token = "[REDACTED:SECRET]""#);
    }

    #[test]
    fn r2_two_adjacent_real_secrets_each_redacted() {
        let out = redact(r#"password = "realsecrettail123", token = "anotherrealsecret999""#);
        assert!(!out.contains("realsecrettail123"), "first secret leaked: {out}");
        assert!(!out.contains("anotherrealsecret999"), "second secret leaked: {out}");
        assert_eq!(out, r#"password = "[REDACTED:SECRET]", token = "[REDACTED:SECRET]""#);
    }

    #[test]
    fn r2_clean_json_sibling_is_preserved() {
        // Finding 3: the heuristic over-consumed across `,` and deleted the clean
        // sibling `host: "publicdb"` and the closing brace. The structural close
        // at `,` preserves everything after the secret value.
        let out = redact(r#"{password: "abcdefghij", host: "publicdb"}"#);
        assert!(out.contains(r#"host: "publicdb""#), "clean sibling was deleted: {out}");
        assert!(out.ends_with('}'), "closing brace was deleted: {out}");
        assert_eq!(out, r#"{password: "[REDACTED:SECRET]", host: "publicdb"}"#);
    }

    #[test]
    fn r2_method_chain_after_value_is_preserved() {
        // Finding 3: `"…".freeze` — the `.` after the close quote is a structural
        // boundary, so the method call survives.
        assert_eq!(
            redact(r#"password = "abcdefghij".freeze"#),
            r#"password = "[REDACTED:SECRET]".freeze"#
        );
        // A short value keeps its trailing code untouched too.
        assert_eq!(redact(r#"password = "x".freeze"#), r#"password = "x".freeze"#);
    }

    #[test]
    fn r2_connection_string_with_comma_inside_quotes_not_cut_short() {
        // The structural-close check only applies AFTER a candidate close quote,
        // never to a `,` INSIDE the quotes — so a value that legitimately holds a
        // comma redacts whole and never leaks its tail.
        let out = redact(r#"password = "p@ss,realsecret,longenough""#);
        assert!(!out.contains("realsecret"), "value tail leaked: {out}");
        assert_eq!(out, r#"password = "[REDACTED:SECRET]""#);
    }
}
