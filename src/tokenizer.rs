//! # tokenizer — the one shared, byte-exact tokenizer
//!
//! **Why this file exists:** The embedder, BM25, and keyword matching must all
//! split text identically or the pipeline is inconsistent and cross-language
//! equivalence (SPEC §8) is impossible. One tokenizer, used everywhere.
//!
//! **What it is / does:** Implements SPEC §4.1 exactly: over the RAW UTF-8 bytes,
//! a token is a maximal run of `[A-Za-z0-9_]`; every other byte is a separator;
//! ASCII `A`–`Z` are lowercased by adding 0x20; tokens are emitted left-to-right
//! with no de-duplication.
//!
//! **Responsibilities:**
//! - Own `tokenize`, the single definition of a "token" for the whole engine.
//! - It deliberately does NOT stem, stopword-filter, or split camelCase.
//! - Own `estimate_tokens` (SPEC-V2.5 §4): the ONE cross-language token *count*
//!   estimator every savings computation uses. It is an ESTIMATOR, not a model
//!   tokenizer; it is byte/char-pinned so cce-ruby can reconcile to it exactly.
//!   (Distinct from `chunker::token_count`, the legacy chunk-size heuristic that
//!   feeds `conformance.json` and MUST stay byte-identical — see that module.)

/// True for the ASCII bytes that make up a token: letters, digits, underscore.
#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

// --- SPEC-V2.5 §4: the deterministic savings token estimator (`cce.tokens/v1`) ---

/// Provenance tag for the savings token estimator. Stamped where a consumer needs
/// to record WHICH counter produced a number, so Ruby's later catch-up can pin to
/// the same rule. Bump the suffix only on a (breaking) rule change.
pub const TOKEN_ESTIMATOR_ID: &str = "cce.tokens/v1";

/// The divisor of the byte-pinned estimator: `floor(bytes / 4)`.
const ESTIMATOR_BYTES_PER_TOKEN: usize = 4;

/// Estimate the token count of `text` per SPEC-V2.5 §4 (`cce.tokens/v1`).
///
/// **This is an estimator, not a model tokenizer** — it is labelled so on every
/// surface that shows a count. The exact, byte-pinned rule is:
///
/// > `estimate_tokens(text) = max(1, floor(byte_length(text) / 4))`
///
/// where `byte_length` is the raw UTF-8 byte count. Bytes are counted uniformly:
/// ASCII, whitespace, and multi-byte (CJK/emoji) bytes all contribute equally, so
/// the rule is trivially reproducible in any language from the byte length alone —
/// no Unicode tables, no dependency, fully deterministic. The empty string is 1
/// (a non-zero floor), matching the engine's long-standing chunk-size rule.
///
/// This is the ONE counter every savings computation uses (SPEC-V2.5 §4). It is
/// deliberately identical to the rule behind `chunker::token_count` — that function
/// delegates here, so the persisted index, `conformance.json`, the Sync artifact,
/// and the savings ledger all agree to the byte. A golden corpus checksum pins it.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() / ESTIMATOR_BYTES_PER_TOKEN).max(1) as u64
}

/// Lowercase hex of a byte slice (used by the golden checksum test and any caller
/// that needs a stable digest string).
pub fn hex_of(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Tokenize text per SPEC §4.1, operating on raw UTF-8 bytes.
///
/// Returns tokens in left-to-right order, lowercased (ASCII only), not deduped.
pub fn tokenize(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for &b in bytes {
        if is_word_byte(b) {
            // Lowercase ASCII A-Z (0x41..=0x5A) by mapping to a-z; leave others.
            let lb = if (0x41..=0x5A).contains(&b) { b + 0x20 } else { b };
            cur.push(lb);
        } else if !cur.is_empty() {
            // A token boundary: flush. `cur` is ASCII word bytes, always valid UTF-8.
            out.push(String::from_utf8(std::mem::take(&mut cur)).unwrap());
        }
    }
    if !cur.is_empty() {
        out.push(String::from_utf8(cur).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_hash_password() {
        assert_eq!(tokenize("hashPassword(user_id)"), vec!["hashpassword", "user_id"]);
    }

    #[test]
    fn anchor_select() {
        assert_eq!(tokenize("SELECT * FROM users;"), vec!["select", "from", "users"]);
    }

    #[test]
    fn anchor_empty() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn anchor_camelcase_not_split() {
        assert_eq!(tokenize("getUserById"), vec!["getuserbyid"]);
    }

    #[test]
    fn non_ascii_is_separator() {
        // The é (multi-byte UTF-8) bytes are all separators.
        assert_eq!(tokenize("cafébar"), vec!["caf", "bar"]);
    }

    #[test]
    fn underscore_and_digits_kept() {
        assert_eq!(tokenize("_a1_b2"), vec!["_a1_b2"]);
    }

    #[test]
    fn no_dedup_and_order() {
        assert_eq!(tokenize("user login user"), vec!["user", "login", "user"]);
    }

    // --- SPEC-V2.5 §4 token estimator (`cce.tokens/v1`) ---

    #[test]
    fn estimator_anchor_rule() {
        // The rule: max(1, floor(byte_length / 4)).
        assert_eq!(estimate_tokens(""), 1); // max(1, 0)
        assert_eq!(estimate_tokens("a"), 1); // max(1, floor(1/4))
        assert_eq!(estimate_tokens("abcd"), 1); // 4/4
        assert_eq!(estimate_tokens("abcde"), 1); // floor(5/4)
        assert_eq!(estimate_tokens("abcdefgh"), 2); // 8/4
    }

    #[test]
    fn estimator_counts_raw_utf8_bytes_uniformly() {
        // Multi-byte scalars count by their UTF-8 byte length, not by scalar count:
        // four Han ideographs are 12 bytes -> floor(12/4) = 3.
        assert_eq!(estimate_tokens("你好世界"), 3);
        // U+00A0 (no-break space) is just its 2 bytes: "a\u{00A0}b" = 4 bytes -> 1.
        assert_eq!(estimate_tokens("a\u{00A0}b"), 1);
    }

    #[test]
    fn estimator_matches_chunker_token_count_rule() {
        // The savings estimator is the single source of truth for the chunk-size
        // rule too: `chunker::token_count` delegates here, so they never diverge.
        for s in ["", "a", "abcd", "def hash_password(u):", "你好 world"] {
            assert_eq!(estimate_tokens(s), crate::chunker::token_count(s) as u64, "{s:?}");
        }
    }

    #[test]
    fn estimator_golden_corpus_checksum() {
        // Cross-language reconciliation gate (SPEC-V2.5 §4): a checked-in corpus
        // maps to a pinned SHA-256 over a canonical `index<TAB>count\n` serialization.
        // cce-ruby must reproduce this exact checksum from the same corpus.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixture/savings/token_corpus.json");
        let text = std::fs::read_to_string(path).unwrap();
        let corpus: Vec<String> = serde_json::from_str(&text).unwrap();
        let mut canonical = String::new();
        for (i, s) in corpus.iter().enumerate() {
            canonical.push_str(&format!("{i}\t{}\n", estimate_tokens(s)));
        }
        use sha2::Digest;
        let digest = sha2::Sha256::digest(canonical.as_bytes());
        let hex = super::hex_of(&digest);
        assert_eq!(hex, "d7716035af7021fdc122a7783ffcd4ddadcd38d200cb06f7fd77731d4b503052");
    }
}
