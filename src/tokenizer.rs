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

/// True for the ASCII bytes that make up a token: letters, digits, underscore.
#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
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
}
