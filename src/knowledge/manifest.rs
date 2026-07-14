//! # knowledge::manifest — the neutral feed-manifest check (`cce.feed-manifest/v1`, U6.2)
//!
//! **Why this file exists:** `cce knowledge index` trusts its input blindly — a
//! truncated or misdirected feed indexes silently (gap G16). This module adds an
//! **optional, opt-in** integrity gate: a producer emits a sidecar manifest stating
//! how many records the feed carries and the SHA-256 of its bytes, and cce refuses
//! to index a feed that does not match. The failure is loud (a non-zero error), never
//! a silently-wrong store.
//!
//! **What it is / does:** Declares [`FeedManifest`] — cce's OWN neutral contract
//! (`cce.feed-manifest/v1`), two required fields: `records` (count) and `sha256` (hex
//! of the raw feed bytes). It is deliberately **tolerant**: parsing ignores unknown
//! keys and does not require a specific `schema` value, so a producer's own manifest
//! (e.g. thresh's `thresh.cce_stage/v1` MANIFEST) *maps onto* this format simply by
//! carrying the two fields — cce imports no producer-named schema across the seam
//! (C8). [`verify`](FeedManifest::verify) compares a manifest against a feed's actual
//! bytes and record count, returning a loud, human-readable error on any mismatch.
//!
//! **Responsibilities:**
//! - Own the `cce.feed-manifest/v1` field set, its schema id, and JSON parsing.
//! - Own [`feed_sha256`] (the full 64-hex digest of the raw feed bytes) and the
//!   record-count + checksum verification.
//! - It does NOT read, parse, or ingest the feed itself (that is `contract`/`store`);
//!   it is handed the raw bytes and the already-parsed record count.

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The pinned schema id cce stamps on a feed-manifest it emits. A bump is a
/// compatibility event (C21). The *reader* does not require this exact value — see
/// [`FeedManifest`] — so a producer may stamp its own schema and still map on.
pub const FEED_MANIFEST_SCHEMA_ID: &str = "cce.feed-manifest/v1";

/// A neutral sidecar manifest for a `cce.knowledge/v1` feed (`cce.feed-manifest/v1`).
///
/// Only `records` and `sha256` are required; every other key (including `schema`) is
/// ignored, so a producer's richer manifest maps onto this format without cce knowing
/// the producer's schema (C8). `sha256` is the lowercase-hex SHA-256 of the feed's
/// **raw bytes** — the same bytes the M3 snapshot id is keyed on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FeedManifest {
    /// The number of `cce.knowledge/v1` records the feed must contain.
    pub records: usize,
    /// Lowercase-hex SHA-256 (64 chars) over the raw feed bytes.
    pub sha256: String,
    /// The producer's schema id, if it stamped one. Informational only — the reader
    /// never requires a specific value, which is what lets a foreign manifest map on.
    #[serde(default)]
    pub schema: Option<String>,
}

/// The full lowercase-hex SHA-256 (64 chars) of `bytes` — the feed integrity digest a
/// manifest declares. (The M3 snapshot id keeps its own 16-hex short form; this is the
/// complete digest a `sha256sum` of the feed file reproduces.)
pub fn feed_sha256(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in Sha256::digest(bytes) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

impl FeedManifest {
    /// Parse a `cce.feed-manifest/v1` JSON document. Missing a required field
    /// (`records`/`sha256`) or malformed JSON is a loud `Err` — the whole point is to
    /// fail rather than index blindly. Unknown keys are ignored (a foreign manifest
    /// carrying extra fields is valid).
    pub fn parse(text: &str) -> Result<FeedManifest, String> {
        serde_json::from_str::<FeedManifest>(text)
            .map_err(|e| format!("invalid cce.feed-manifest/v1 manifest: {e}"))
    }

    /// Verify a feed against this manifest. `feed_bytes` are the raw bytes read from
    /// the feed file; `actual_records` is the count the contract parser returned.
    ///
    /// Checks completeness first (record count — the friendliest truncation message),
    /// then byte integrity (SHA-256 — catches corruption or a wholly wrong/misdirected
    /// feed). Any mismatch returns a loud, specific error; `Ok(())` means the feed is
    /// exactly what the producer declared.
    pub fn verify(&self, feed_bytes: &[u8], actual_records: usize) -> Result<(), String> {
        let declared = self.sha256.trim().to_ascii_lowercase();
        if declared.len() != 64 || !declared.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "feed manifest sha256 is not a 64-char hex digest: {:?}",
                self.sha256
            ));
        }
        if actual_records != self.records {
            return Err(format!(
                "feed does not match its manifest: manifest declares {} record(s), feed has {} \
                 — the feed is truncated or incomplete",
                self.records, actual_records
            ));
        }
        let actual = feed_sha256(feed_bytes);
        if actual != declared {
            return Err(format!(
                "feed does not match its manifest checksum (truncated, corrupt, or misdirected \
                 feed): manifest sha256 {declared}, feed sha256 {actual}"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tiny well-formed feed and its true digest/count, reused across cases.
    fn feed() -> &'static str {
        "{\"id\":\"a\",\"title\":\"A\",\"body\":\"x\",\"source\":\"s\"}\n\
         {\"id\":\"b\",\"title\":\"B\",\"body\":\"y\",\"source\":\"s\"}\n"
    }

    fn good_manifest() -> FeedManifest {
        FeedManifest { records: 2, sha256: feed_sha256(feed().as_bytes()), schema: None }
    }

    #[test]
    fn schema_id_is_pinned() {
        assert_eq!(FEED_MANIFEST_SCHEMA_ID, "cce.feed-manifest/v1");
    }

    #[test]
    fn feed_sha256_is_full_64_lowercase_hex_and_deterministic() {
        let a = feed_sha256(b"hello");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(a, feed_sha256(b"hello"));
        assert_ne!(a, feed_sha256(b"world"));
        // Matches a plain `sha256sum` of the same bytes.
        assert_eq!(a, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[test]
    fn parses_required_fields_and_ignores_unknown_keys() {
        // A foreign producer manifest (thresh's shape) maps on: extra keys ignored,
        // its own `schema` tolerated, only `records`+`sha256` are read.
        let doc = r#"{"schema":"thresh.cce_stage/v1","records":2,"sha256":"AB",
                      "instance_path":"/x","staged":["a.cce.ndjson"],"created_at":"t"}"#;
        let m = FeedManifest::parse(doc).unwrap();
        assert_eq!(m.records, 2);
        assert_eq!(m.sha256, "AB");
        assert_eq!(m.schema.as_deref(), Some("thresh.cce_stage/v1"));
    }

    #[test]
    fn missing_required_field_is_a_loud_error() {
        let err = FeedManifest::parse(r#"{"records":2}"#).unwrap_err();
        assert!(err.contains("cce.feed-manifest/v1"), "{err}");
        let err = FeedManifest::parse("not json").unwrap_err();
        assert!(err.contains("cce.feed-manifest/v1"), "{err}");
    }

    #[test]
    fn a_matching_feed_verifies() {
        assert!(good_manifest().verify(feed().as_bytes(), 2).is_ok());
    }

    #[test]
    fn a_truncated_feed_fails_loudly_on_record_count() {
        // A feed with the last record dropped: fewer records than the manifest declares.
        let truncated = "{\"id\":\"a\",\"title\":\"A\",\"body\":\"x\",\"source\":\"s\"}\n";
        let err = good_manifest().verify(truncated.as_bytes(), 1).unwrap_err();
        assert!(err.contains("truncated or incomplete"), "{err}");
        assert!(err.contains("declares 2"), "{err}");
        assert!(err.contains("feed has 1"), "{err}");
    }

    #[test]
    fn a_misdirected_feed_of_the_same_length_fails_loudly_on_checksum() {
        // Same record count, different bytes (a wholly different but same-sized feed):
        // the count passes, the checksum catches it.
        let other = "{\"id\":\"c\",\"title\":\"C\",\"body\":\"z\",\"source\":\"s\"}\n\
                     {\"id\":\"d\",\"title\":\"D\",\"body\":\"w\",\"source\":\"s\"}\n";
        let err = good_manifest().verify(other.as_bytes(), 2).unwrap_err();
        assert!(err.contains("checksum"), "{err}");
        assert!(err.contains("misdirected"), "{err}");
    }

    #[test]
    fn a_manifest_with_a_malformed_digest_fails_loudly() {
        let m = FeedManifest { records: 2, sha256: "not-a-digest".into(), schema: None };
        let err = m.verify(feed().as_bytes(), 2).unwrap_err();
        assert!(err.contains("64-char hex"), "{err}");
    }

    #[test]
    fn digest_comparison_is_case_insensitive() {
        // A producer that emits UPPERCASE hex still verifies.
        let m = FeedManifest {
            records: 2,
            sha256: feed_sha256(feed().as_bytes()).to_ascii_uppercase(),
            schema: Some(FEED_MANIFEST_SCHEMA_ID.into()),
        };
        assert!(m.verify(feed().as_bytes(), 2).is_ok());
    }
}
