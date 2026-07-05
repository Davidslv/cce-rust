//! # pricing — the embedded, offline token price table (SPEC-V2.5 §3)
//!
//! **Why this file exists:** `cce savings` turns saved tokens into a dollar
//! estimate. That conversion needs a price, and the invariant is offline-first:
//! the table is CHECKED IN and compiled into the binary (`include_str!`), never
//! fetched at runtime. Editing `pricing.json` and rebuilding is the only update
//! path.
//!
//! **What it is / does:** Parses the embedded JSON price table once, exposes the
//! default model's input price, and turns a saved-token count into a USD figure
//! (saved context is INPUT tokens, so the input rate applies). Rounded to cents,
//! round-half-away-from-zero, for a deterministic string.
//!
//! **Responsibilities:**
//! - Own the parsed price table and the `default_input_price_per_million` lookup.
//! - Own `dollars_saved(saved_tokens)` — the only place tokens become dollars.
//! - It deliberately makes NO network call and states its figures are estimates.

use serde::Deserialize;
use std::collections::BTreeMap;

/// The raw embedded table (bytes are fixed at compile time; offline by construction).
const PRICING_JSON: &str = include_str!("pricing.json");

/// One model's per-million-token USD rates.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelPrice {
    pub input_per_million_usd: f64,
    pub output_per_million_usd: f64,
}

/// The parsed price table. `models` is a `BTreeMap` so any listing is sorted and
/// deterministic (no hash-iteration order leaks into output).
#[derive(Debug, Clone, Deserialize)]
pub struct PriceTable {
    pub id: String,
    pub default_model: String,
    pub models: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// Parse the embedded table. Panics only on a corrupt checked-in file, which a
    /// unit test guards against — so at runtime this is effectively infallible.
    pub fn builtin() -> PriceTable {
        serde_json::from_str(PRICING_JSON).expect("embedded pricing.json must be valid")
    }

    /// The default model's input price per 1M tokens, or `None` if the named
    /// default is missing from the table.
    pub fn default_input_price_per_million(&self) -> Option<f64> {
        self.models.get(&self.default_model).map(|m| m.input_per_million_usd)
    }

    /// USD saved for `saved_tokens` input tokens, priced at the default model's
    /// input rate, rounded to cents (round-half-away-from-zero). `0.0` if the
    /// default model is absent.
    pub fn dollars_saved(&self, saved_tokens: u64) -> f64 {
        let rate = self.default_input_price_per_million().unwrap_or(0.0);
        round2(saved_tokens as f64 / 1_000_000.0 * rate)
    }
}

/// Round to 2 decimals, round-half-away-from-zero (matches the aggregator's cost
/// rounding so `cce savings` and the dashboard agree to the cent).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_table_parses_and_has_a_valid_default() {
        let t = PriceTable::builtin();
        assert_eq!(t.id, "cce.pricing/builtin-v1");
        // The named default model must exist in the table.
        assert!(t.models.contains_key(&t.default_model));
        assert_eq!(t.default_input_price_per_million(), Some(3.0));
    }

    #[test]
    fn dollars_saved_uses_input_rate_and_rounds_to_cents() {
        let t = PriceTable::builtin();
        // 1,000,000 tokens at $3/Mtok = $3.00 exactly.
        assert_eq!(t.dollars_saved(1_000_000), 3.0);
        // 53,000 tokens at $3/Mtok = $0.159 -> $0.16.
        assert_eq!(t.dollars_saved(53_000), 0.16);
        assert_eq!(t.dollars_saved(0), 0.0);
    }

    #[test]
    fn models_iterate_in_sorted_order() {
        let t = PriceTable::builtin();
        let keys: Vec<&String> = t.models.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }
}
