//! # savings — the seven-bucket savings ledger (SPEC-V2.5 §3)
//!
//! **Why this file exists:** v2.5 measures token savings across seven layers, not
//! one. This module owns the ledger's shape: the per-event `savings` object each
//! search records, the read-side parse that stays backward-compatible with pre-2.5
//! logs, and the roll-up the `/api/metrics` panel and `cce savings` both print.
//!
//! **What it is / does:** Defines `Bucket` (`saved_tokens`/`baseline_tokens`), the
//! fixed seven-bucket `SavingsBuckets` carried on a search event, and the
//! `SavingsByLayer` aggregate. Only the `retrieval` bucket is populated in Stage ①
//! (Layer 1); the other six are present-and-zero, ready for later stages to fill.
//! Every field is a named struct member serialized in declaration order, so the
//! JSON is byte-deterministic (no hash-iteration order) and cce-ruby can reconcile.
//!
//! **Responsibilities:**
//! - Own the bucket names, the honesty label, and the ledger types.
//! - Own `SavingsBuckets::from_event` (additive parse) and `sum_by_layer` (roll-up).
//! - It does NOT read logs or price tokens — callers wire those in.

use serde::Serialize;
use serde_json::Value;

/// The mandatory honesty label on every surface that shows the ledger
/// (SPEC-V2.5 §3, §5). Kept as one constant so the CLI and the API cannot drift.
pub const SAVINGS_NOTE: &str = "vs full-file baseline — not your real end-to-end agent cost";

/// The seven layer bucket names, in their fixed canonical order. Used for the
/// `cce savings` listing and as the authoritative key list for cross-language
/// reconciliation.
pub const BUCKET_NAMES: [&str; 7] = [
    "retrieval",
    "chunk_compression",
    "grammar",
    "output",
    "memory",
    "turn_summarization",
    "progressive_disclosure",
];

/// One layer's savings: tokens NOT sent (`saved_tokens`) against the counterfactual
/// full-file cost (`baseline_tokens`). `saved = baseline − served`, so
/// `saved_tokens ≤ baseline_tokens` for a well-formed bucket.
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct Bucket {
    pub saved_tokens: u64,
    pub baseline_tokens: u64,
}

impl Bucket {
    /// Accumulate another bucket into this one (saturating; totals never overflow).
    fn add_assign(&mut self, other: &Bucket) {
        self.saved_tokens = self.saved_tokens.saturating_add(other.saved_tokens);
        self.baseline_tokens = self.baseline_tokens.saturating_add(other.baseline_tokens);
    }
}

/// The seven-bucket `savings` object carried on a `search` metrics event. Fixed
/// field order = byte-deterministic serialization.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SavingsBuckets {
    pub retrieval: Bucket,
    pub chunk_compression: Bucket,
    pub grammar: Bucket,
    pub output: Bucket,
    pub memory: Bucket,
    pub turn_summarization: Bucket,
    pub progressive_disclosure: Bucket,
}

impl SavingsBuckets {
    /// A ledger with only the `retrieval` bucket populated (Stage ① / Layer 1).
    pub fn retrieval_only(saved: u64, baseline: u64) -> SavingsBuckets {
        SavingsBuckets {
            retrieval: Bucket { saved_tokens: saved, baseline_tokens: baseline },
            ..Default::default()
        }
    }

    /// A ledger with the `retrieval` (Layer 1) AND `chunk_compression` (Layer 2)
    /// buckets populated (Stage ②); the other five present-and-zero. A search event
    /// records this so `cce savings` and the dashboard attribute each layer's saving
    /// separately — retrieval saves vs whole files, chunk compression vs full chunks.
    pub fn layers_1_2(
        retrieval_saved: u64,
        retrieval_baseline: u64,
        chunk_saved: u64,
        chunk_baseline: u64,
    ) -> SavingsBuckets {
        SavingsBuckets {
            retrieval: Bucket {
                saved_tokens: retrieval_saved,
                baseline_tokens: retrieval_baseline,
            },
            chunk_compression: Bucket {
                saved_tokens: chunk_saved,
                baseline_tokens: chunk_baseline,
            },
            ..Default::default()
        }
    }

    /// Parse the `savings` object from a raw event value (read side, additive).
    ///
    /// If the event carries a `savings` object, each of the seven buckets is read
    /// from it (missing bucket ⇒ zero). If it does NOT (a pre-2.5 log), the
    /// `retrieval` bucket is reconstructed from the legacy top-level
    /// `tokens_saved` / `baseline_tokens` fields, so old logs still contribute to
    /// the ledger unchanged.
    pub fn from_event(v: &Value) -> SavingsBuckets {
        match v.get("savings") {
            Some(obj) => SavingsBuckets {
                retrieval: bucket_at(obj, "retrieval"),
                chunk_compression: bucket_at(obj, "chunk_compression"),
                grammar: bucket_at(obj, "grammar"),
                output: bucket_at(obj, "output"),
                memory: bucket_at(obj, "memory"),
                turn_summarization: bucket_at(obj, "turn_summarization"),
                progressive_disclosure: bucket_at(obj, "progressive_disclosure"),
            },
            None => SavingsBuckets::retrieval_only(
                u64_at(v, "tokens_saved"),
                u64_at(v, "baseline_tokens"),
            ),
        }
    }

    /// Serialize to a `serde_json::Value` for embedding in the emitted event.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The roll-up shown by `/api/metrics.savings_by_layer` and `cce savings`: each of
/// the seven buckets summed over the log, a grand `total`, and the honesty `note`.
#[derive(Debug, Clone, Serialize)]
pub struct SavingsByLayer {
    pub retrieval: Bucket,
    pub chunk_compression: Bucket,
    pub grammar: Bucket,
    pub output: Bucket,
    pub memory: Bucket,
    pub turn_summarization: Bucket,
    pub progressive_disclosure: Bucket,
    pub total: Bucket,
    pub note: String,
}

/// Sum every event's buckets into the `SavingsByLayer` roll-up. Pure and
/// order-independent (integer addition), so both engines produce identical numbers.
pub fn sum_by_layer<'a>(events: impl Iterator<Item = &'a SavingsBuckets>) -> SavingsByLayer {
    let mut acc = SavingsBuckets::default();
    for b in events {
        acc.retrieval.add_assign(&b.retrieval);
        acc.chunk_compression.add_assign(&b.chunk_compression);
        acc.grammar.add_assign(&b.grammar);
        acc.output.add_assign(&b.output);
        acc.memory.add_assign(&b.memory);
        acc.turn_summarization.add_assign(&b.turn_summarization);
        acc.progressive_disclosure.add_assign(&b.progressive_disclosure);
    }
    let mut total = Bucket::default();
    for b in [
        &acc.retrieval,
        &acc.chunk_compression,
        &acc.grammar,
        &acc.output,
        &acc.memory,
        &acc.turn_summarization,
        &acc.progressive_disclosure,
    ] {
        total.add_assign(b);
    }
    SavingsByLayer {
        retrieval: acc.retrieval,
        chunk_compression: acc.chunk_compression,
        grammar: acc.grammar,
        output: acc.output,
        memory: acc.memory,
        turn_summarization: acc.turn_summarization,
        progressive_disclosure: acc.progressive_disclosure,
        total,
        note: SAVINGS_NOTE.to_string(),
    }
}

impl SavingsByLayer {
    /// The seven buckets paired with their canonical names, in fixed order — for a
    /// deterministic per-bucket listing without hash iteration.
    pub fn ordered(&self) -> [(&'static str, Bucket); 7] {
        [
            ("retrieval", self.retrieval),
            ("chunk_compression", self.chunk_compression),
            ("grammar", self.grammar),
            ("output", self.output),
            ("memory", self.memory),
            ("turn_summarization", self.turn_summarization),
            ("progressive_disclosure", self.progressive_disclosure),
        ]
    }
}

/// Read a `Bucket` from `obj[key]` (missing/malformed ⇒ zeros).
fn bucket_at(obj: &Value, key: &str) -> Bucket {
    match obj.get(key) {
        Some(b) => Bucket {
            saved_tokens: u64_at(b, "saved_tokens"),
            baseline_tokens: u64_at(b, "baseline_tokens"),
        },
        None => Bucket::default(),
    }
}

/// Read a `u64` from `v[key]` (missing/malformed ⇒ 0).
fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_event_reads_the_savings_object() {
        let v = serde_json::json!({
            "event": "search",
            "tokens_saved": 1,
            "baseline_tokens": 2,
            "savings": {
                "retrieval": { "saved_tokens": 100, "baseline_tokens": 400 },
                "chunk_compression": { "saved_tokens": 0, "baseline_tokens": 0 }
            }
        });
        let s = SavingsBuckets::from_event(&v);
        // The object wins over the legacy top-level fields.
        assert_eq!(s.retrieval, Bucket { saved_tokens: 100, baseline_tokens: 400 });
        assert_eq!(s.grammar, Bucket::default());
    }

    #[test]
    fn from_event_falls_back_to_legacy_top_level_fields() {
        // A pre-2.5 search event with no `savings` object.
        let v = serde_json::json!({
            "event": "search",
            "tokens_saved": 32000,
            "baseline_tokens": 40000
        });
        let s = SavingsBuckets::from_event(&v);
        assert_eq!(s.retrieval, Bucket { saved_tokens: 32000, baseline_tokens: 40000 });
        // Every other bucket is present-and-zero.
        assert_eq!(s.chunk_compression, Bucket::default());
        assert_eq!(s.progressive_disclosure, Bucket::default());
    }

    #[test]
    fn sum_by_layer_totals_across_events() {
        let a = SavingsBuckets::retrieval_only(100, 400);
        let b = SavingsBuckets::retrieval_only(50, 90);
        let roll = sum_by_layer([&a, &b].into_iter());
        assert_eq!(roll.retrieval, Bucket { saved_tokens: 150, baseline_tokens: 490 });
        assert_eq!(roll.total, Bucket { saved_tokens: 150, baseline_tokens: 490 });
        assert_eq!(roll.note, SAVINGS_NOTE);
        // The six unfilled buckets stay zero.
        assert_eq!(roll.output, Bucket::default());
    }

    #[test]
    fn empty_log_is_a_clean_zero_ledger() {
        let roll = sum_by_layer(std::iter::empty());
        assert_eq!(roll.total, Bucket::default());
        for (_, b) in roll.ordered() {
            assert_eq!(b, Bucket::default());
        }
    }

    #[test]
    fn bucket_names_match_struct_order() {
        let roll = sum_by_layer(std::iter::empty());
        let names: Vec<&str> = roll.ordered().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, BUCKET_NAMES);
    }
}
