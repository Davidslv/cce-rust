//! # embedder — deterministic hashing embedder, cosine, and rounding
//!
//! **Why this file exists:** Retrieval needs a vector for every chunk and the
//! query. SPEC §5 mandates an exact, model-free hashing embedder so the two
//! language implementations produce bit-comparable vectors, plus the cosine and
//! the round-half-away-from-zero rules (SPEC §5.3) that make rankings reproduce
//! across languages.
//!
//! **What it is / does:** Implements the FNV-1a-64 hash, the 256-dim hashing
//! `Embed` (SPEC §5.1), cosine similarity (SPEC §5.2), an `Embedder` trait so the
//! Ollama backend is interchangeable, and the deterministic rounding/formatting
//! helpers used everywhere scores are compared or emitted.
//!
//! **Responsibilities:**
//! - Own `fnv1a64`, `HashEmbedder`, `cosine`, `round6`, `score_key`, `format6`.
//! - Define the `Embedder` trait; the network Ollama impl lives in `store`/here.
//! - It does NOT rank or persist; it only turns text into vectors and rounds.

use crate::config::EMBED_DIM;
use crate::tokenizer::tokenize;

/// FNV-1a 64-bit hash over the given bytes (SPEC §5.1).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let prime: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(prime);
    }
    hash
}

/// The embedding backend interface. Backends must be interchangeable (SPEC §11).
pub trait Embedder {
    /// Embed a single text — the infallible query-time path. The deterministic
    /// hash backend cannot fail; a fallible backend (Ollama) must make any
    /// failure **visible** (it warns on stderr and returns an empty vector,
    /// which contributes zero vector signal). The indexing path must never use
    /// this method: it calls [`Embedder::try_embed`] and aborts on error, so a
    /// store can never silently persist empty embeddings (issue #30).
    fn embed(&self, text: &str) -> Vec<f64>;

    /// Fallible single embed — the indexing path. Default wraps `embed`
    /// (the hash backend cannot fail); fallible backends override it to
    /// propagate the real error instead of degrading.
    fn try_embed(&self, text: &str) -> Result<Vec<f64>, String> {
        Ok(self.embed(text))
    }

    /// Fallible batch embed; default maps `try_embed` over the inputs. There is
    /// deliberately NO infallible batch API: a batch failure must surface as an
    /// `Err`, never as silently empty vectors (issue #30).
    fn try_embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        texts.iter().map(|t| self.try_embed(t)).collect()
    }

    /// Human-readable backend name (for stats / reporting).
    fn name(&self) -> &'static str;
}

/// Deterministic hashing embedder (SPEC §5.1) — the default backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct HashEmbedder;

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Vec<f64> {
        let mut v = vec![0.0f64; EMBED_DIM];
        for tok in tokenize(text) {
            let h = fnv1a64(tok.as_bytes());
            let bucket = (h % EMBED_DIM as u64) as usize;
            let sign = if ((h >> 63) & 1) == 1 { -1.0 } else { 1.0 };
            v[bucket] += sign;
        }
        let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        v
    }

    fn name(&self) -> &'static str {
        "hash"
    }
}

/// Optional Ollama HTTP embedder (SPEC §11). Model-dependent vectors, so it is
/// NOT covered by conformance. Kept behind the same `Embedder` trait so the two
/// backends are interchangeable.
#[derive(Debug, Clone)]
pub struct OllamaEmbedder {
    pub base_url: String,
    pub model: String,
}

impl Default for OllamaEmbedder {
    /// Defaults to `http://localhost:11434` / `nomic-embed-text`; both are
    /// overridable via the `CCE_OLLAMA_URL` and `CCE_OLLAMA_MODEL` environment
    /// variables (also what keeps the Ollama failure tests hermetic).
    fn default() -> Self {
        OllamaEmbedder {
            base_url: std::env::var("CCE_OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string()),
            model: std::env::var("CCE_OLLAMA_MODEL")
                .unwrap_or_else(|_| "nomic-embed-text".to_string()),
        }
    }
}

impl OllamaEmbedder {
    /// Health check: attempt to embed a trivial input.
    pub fn healthy(&self) -> bool {
        self.try_embed_batch(&["ping".to_string()]).is_ok()
    }
}

impl Embedder for OllamaEmbedder {
    /// Query-time embed. A failure here is made visible (a stderr warning) and
    /// yields an empty vector — zero vector signal for this one query. The
    /// indexing path never uses this: it calls `try_embed` and aborts (#30).
    fn embed(&self, text: &str) -> Vec<f64> {
        match self.try_embed(text) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: {e}; vector recall disabled for this query");
                Vec::new()
            }
        }
    }

    /// Fallible single embed via the batch endpoint. Empty text embeds to an
    /// empty vector without a request (SPEC §11: skip empty inputs).
    fn try_embed(&self, text: &str) -> Result<Vec<f64>, String> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = self.try_embed_batch(&[text.to_string()])?;
        out.pop().ok_or_else(|| "ollama returned no embedding".to_string())
    }

    /// Embed a batch of texts via `POST /api/embed`. Returns `Err` on any
    /// network/protocol error, on a count mismatch, or if the server returns an
    /// empty embedding for a non-empty input — it NEVER degrades to empty
    /// vectors (issue #30); callers decide loudly what an error means.
    fn try_embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f64>>, String> {
        // Truncate to ~2000 chars; keep empties as empty strings (SPEC §11).
        let inputs: Vec<String> =
            texts.iter().map(|t| t.chars().take(2000).collect::<String>()).collect();
        let body = serde_json::json!({ "model": self.model, "input": inputs });
        let url = format!("{}/api/embed", self.base_url);
        let mut resp =
            ureq::post(&url).send_json(&body).map_err(|e| format!("ollama request failed: {e}"))?;
        let val: serde_json::Value =
            resp.body_mut().read_json().map_err(|e| format!("ollama bad response: {e}"))?;
        let embs = val
            .get("embeddings")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "ollama response missing .embeddings".to_string())?;
        if embs.len() != texts.len() {
            return Err(format!(
                "ollama returned {} embedding(s) for {} input(s)",
                embs.len(),
                texts.len()
            ));
        }
        let mut out = Vec::with_capacity(embs.len());
        for (e, text) in embs.iter().zip(texts) {
            let vec: Vec<f64> = e
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                .unwrap_or_default();
            if vec.is_empty() && !text.is_empty() {
                return Err("ollama returned an empty embedding for a non-empty input".to_string());
            }
            out.push(vec);
        }
        Ok(out)
    }

    fn name(&self) -> &'static str {
        "ollama"
    }
}

/// Cosine similarity of two vectors (SPEC §5.2). Since both are L2-normalized,
/// cosine == dot product, summed over indices 0..EMBED_DIM in order.
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let mut s = 0.0;
    for i in 0..n {
        s += a[i] * b[i];
    }
    s
}

/// Integer key for a score at 6-decimal precision, round-half-away-from-zero.
/// Used to compare/sort scores deterministically (SPEC §5.3).
#[inline]
pub fn score_key(x: f64) -> i64 {
    (x * 1_000_000.0).round() as i64
}

/// Round a value to 6 decimals, round-half-away-from-zero (SPEC §5.3).
#[inline]
pub fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// Format a score as a fixed 6-decimal string, round-half-away-from-zero.
/// Built from the integer scaled value so it is exact and deterministic.
pub fn format6(x: f64) -> String {
    let scaled = (x.abs() * 1_000_000.0).round() as i64;
    let int_part = scaled / 1_000_000;
    let frac = scaled % 1_000_000;
    let neg = x < 0.0 && scaled != 0;
    format!("{}{}.{:06}", if neg { "-" } else { "" }, int_part, frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_anchor_empty() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv_anchor_a() {
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn fnv_anchor_foobar() {
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn cosine_anchor() {
        let mut a = vec![0.0; EMBED_DIM];
        a[0] = 0.6;
        a[1] = 0.8;
        let mut b = vec![0.0; EMBED_DIM];
        b[0] = 1.0;
        assert!((cosine(&a, &b) - 0.6).abs() < 1e-12);
    }

    #[test]
    fn embed_is_l2_normalized() {
        let e = HashEmbedder;
        let v = e.embed("hash password user login");
        let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-12);
    }

    #[test]
    fn embed_empty_is_all_zeros() {
        let e = HashEmbedder;
        let v = e.embed("");
        assert_eq!(v.len(), EMBED_DIM);
        assert!(v.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn embed_deterministic() {
        let e = HashEmbedder;
        assert_eq!(e.embed("def hash_password"), e.embed("def hash_password"));
    }

    #[test]
    fn format6_rounds_and_signs() {
        assert_eq!(format6(0.1234567), "0.123457"); // 0.7 rounds up
        assert_eq!(format6(0.1234564), "0.123456"); // 0.4 rounds down
        assert_eq!(format6(-0.1234567), "-0.123457");
        assert_eq!(format6(1.0), "1.000000");
        assert_eq!(format6(0.0), "0.000000");
        assert_eq!(format6(2.5), "2.500000");
    }

    #[test]
    fn round_half_away_from_zero() {
        // The determinism rule (SPEC §5.3) relies on std's round(): ties away
        // from zero. 0.5 / 1.5 / 2.5 are exactly representable in f64.
        assert_eq!((0.5f64).round(), 1.0);
        assert_eq!((2.5f64).round(), 3.0);
        assert_eq!((-2.5f64).round(), -3.0);
    }

    #[test]
    fn score_key_matches_format6() {
        // Two scores that round to the same 6-decimal value compare equal.
        assert_eq!(score_key(0.1234564), score_key(0.1234561));
    }

    #[test]
    fn round6_rounds_half_away_from_zero() {
        assert_eq!(round6(0.1234567), 0.123457);
        assert_eq!(round6(0.1234564), 0.123456);
        assert_eq!(round6(-0.1234567), -0.123457);
        assert_eq!(round6(1.0), 1.0);
    }

    #[test]
    fn hash_try_embed_batch_uses_default_trait_impl() {
        // HashEmbedder does not override try_embed/try_embed_batch, so this
        // exercises the default trait methods that map `embed` over the inputs
        // (infallible for the hash backend).
        let e = HashEmbedder;
        let out = e.try_embed_batch(&["one".to_string(), "two".to_string()]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], e.embed("one"));
        assert_eq!(out[1], e.embed("two"));
        assert_eq!(e.try_embed("one").unwrap(), e.embed("one"));
    }

    // --- Ollama loud-failure path (hermetic: no server, connection refused) ---

    /// An OllamaEmbedder pointed at a closed local port. Port 1 is never open,
    /// so every request is refused immediately — no real server is contacted.
    fn unreachable_ollama() -> OllamaEmbedder {
        OllamaEmbedder {
            base_url: "http://127.0.0.1:1".to_string(),
            model: "nomic-embed-text".to_string(),
        }
    }

    #[test]
    fn ollama_default_has_expected_url_and_model() {
        let oll = OllamaEmbedder::default();
        assert_eq!(oll.base_url, "http://localhost:11434");
        assert_eq!(oll.model, "nomic-embed-text");
        assert_eq!(oll.name(), "ollama");
    }

    #[test]
    fn ollama_try_embed_batch_errors_when_unreachable() {
        let oll = unreachable_ollama();
        let err = oll.try_embed_batch(&["hello".to_string()]).expect_err("closed port must fail");
        assert!(err.contains("ollama request failed"), "unexpected error: {err}");
    }

    #[test]
    fn ollama_healthy_is_false_when_unreachable() {
        assert!(!unreachable_ollama().healthy());
    }

    #[test]
    fn ollama_embed_empty_text_is_empty_without_request() {
        // Empty text returns early, so no request is attempted at all.
        assert!(unreachable_ollama().embed("").is_empty());
        assert!(unreachable_ollama().try_embed("").unwrap().is_empty());
    }

    #[test]
    fn ollama_try_embed_propagates_the_error_not_an_empty_vector() {
        // Issue #30: the indexing path uses try_embed, which must surface the
        // failure as an Err — never a silently-empty embedding.
        let err = unreachable_ollama().try_embed("nonempty").expect_err("closed port must fail");
        assert!(err.contains("ollama request failed"), "unexpected error: {err}");
    }

    #[test]
    fn ollama_batch_failure_is_an_error_never_empty_vectors() {
        // Issue #30: there is no infallible batch API left — a batch failure is
        // an Err, so no caller can receive per-text empty vectors.
        let oll = unreachable_ollama();
        let err = oll
            .try_embed_batch(&["a".to_string(), "b".to_string()])
            .expect_err("closed port must fail");
        assert!(err.contains("ollama request failed"), "unexpected error: {err}");
    }

    #[test]
    fn ollama_query_time_embed_is_empty_but_warned_on_failure() {
        // The one remaining degradation point: the infallible query-time embed
        // returns an empty vector (zero vector signal) and warns on stderr. It
        // is unreachable from the indexing path, which aborts via try_embed.
        assert!(unreachable_ollama().embed("nonempty").is_empty());
    }
}
