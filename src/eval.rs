//! # eval — the real-world A/B benchmark harness (SPEC-V2.5 §7)
//!
//! **Why this file exists:** `cce savings` reports the internal "vs full-file"
//! ledger, which is honest but is NOT the real end-to-end agent cost. §7 requires
//! a shipped harness that measures the REAL delta: run the same question through
//! an agent with cce `off` vs `on`, headless, and compare. This module owns the
//! deterministic half of that harness — parsing, correctness-gating, punt
//! detection, and cost-primary aggregation — so it is unit-testable on canned run
//! outputs without a live model (the live runs are produced out-of-band; see
//! `eval/README.md`).
//!
//! **What it is / does:** Parses a pinned question set (with ground truth) and a
//! set of recorded run outputs, grades each answer (Punt / Incorrect / Correct),
//! and produces a **cost-primary, correctness-gated, paired** A/B report. Cost
//! includes sub-agents (raw token totals undercount them, so we take reported
//! cost). Cheap non-answers (punts) never count as a win.
//!
//! **Responsibilities:**
//! - Own the `Question` / `RunRecord` shapes and their JSONL parsers.
//! - Own `is_punt`, `grade`, and `evaluate` (the paired A/B roll-up).
//! - It deliberately does NOT call a model or read a clock — pure and deterministic.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One benchmark question with its pinned ground truth. `must_include` is the set
/// of substrings a correct answer must contain (case-insensitive) — a simple,
/// deterministic, cross-language correctness oracle.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub must_include: Vec<String>,
}

/// Which arm of the A/B a run belongs to: cce disabled (`off`) or enabled (`on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Condition {
    Off,
    On,
}

impl Condition {
    /// Parse `"off"`/`"on"` (case-insensitive). Unknown ⇒ `None`.
    pub fn parse(s: &str) -> Option<Condition> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Condition::Off),
            "on" => Some(Condition::On),
            _ => None,
        }
    }
}

/// One recorded agent run for a (question, condition). `cost_usd` is the primary
/// answer cost; `subagent_cost_usd` is the cost of any sub-agents the run spawned
/// (raw token totals undercount these, so the harness sums real cost). `punted`
/// lets the runner mark a non-answer explicitly, in addition to text detection.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RunRecord {
    pub question_id: String,
    #[serde(deserialize_with = "de_condition")]
    pub condition: Condition,
    #[serde(default)]
    pub answer: String,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub subagent_cost_usd: f64,
    #[serde(default)]
    pub punted: Option<bool>,
}

impl RunRecord {
    /// Total cost of the run: the answer cost plus sub-agent cost (SPEC-V2.5 §7 —
    /// cost is primary and includes sub-agents).
    pub fn total_cost(&self) -> f64 {
        self.cost_usd + self.subagent_cost_usd
    }
}

fn de_condition<'de, D>(d: D) -> Result<Condition, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Condition::parse(&s).ok_or_else(|| serde::de::Error::custom("condition must be 'off' or 'on'"))
}

/// The grade of a single answer against its ground truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    /// A cheap non-answer ("I couldn't find…", empty, punted). Never a win.
    Punt,
    /// A real attempt that misses required ground-truth content.
    Incorrect,
    /// A real attempt containing every required substring.
    Correct,
}

/// The fixed set of punt phrases (lowercased). An answer containing any of these,
/// or shorter than 3 non-space chars, is a cheap non-answer.
const PUNT_PHRASES: [&str; 12] = [
    "i don't know",
    "i do not know",
    "i cannot find",
    "i can't find",
    "couldn't find",
    "could not find",
    "unable to",
    "no information",
    "not sure",
    "cannot determine",
    "n/a",
    "no answer",
];

/// True if `answer` reads as a cheap non-answer (SPEC-V2.5 §7 punt-detection).
/// Deterministic: case-insensitive substring match plus a minimum-length floor.
pub fn is_punt(answer: &str) -> bool {
    let trimmed = answer.trim();
    if trimmed.chars().filter(|c| !c.is_whitespace()).count() < 3 {
        return true;
    }
    let lower = trimmed.to_lowercase();
    PUNT_PHRASES.iter().any(|p| lower.contains(p))
}

/// Grade one run against its question. Punt-detection first (a punt is never
/// correct even if it happens to contain a keyword), then the correctness oracle.
pub fn grade(question: &Question, run: &RunRecord) -> Grade {
    if run.punted == Some(true) || is_punt(&run.answer) {
        return Grade::Punt;
    }
    let lower = run.answer.to_lowercase();
    let all_present =
        question.must_include.iter().all(|needle| lower.contains(&needle.to_lowercase()));
    if all_present {
        Grade::Correct
    } else {
        Grade::Incorrect
    }
}

/// Per-condition tallies (correctness-gated). `correct_cost` sums `total_cost`
/// over the CORRECT runs only.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ArmSummary {
    pub runs: u64,
    pub correct: u64,
    pub incorrect: u64,
    pub punts: u64,
    pub correct_cost_usd: f64,
    pub mean_correct_cost_usd: f64,
}

/// The paired, cost-primary A/B report. The headline (`cost_saved_ratio`) is
/// computed ONLY over questions correct in BOTH arms, so a regression that turns a
/// correct answer into a punt cannot masquerade as a saving.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AbReport {
    pub questions: u64,
    pub off: ArmSummary,
    pub on: ArmSummary,
    /// Questions graded `Correct` under BOTH conditions — the paired, gated set.
    pub paired_correct: u64,
    pub paired_off_cost_usd: f64,
    pub paired_on_cost_usd: f64,
    pub cost_delta_usd: f64,
    pub cost_saved_ratio: f64,
    /// Malformed run lines skipped while parsing (never a crash).
    pub skipped_runs: u64,
    pub note: String,
}

/// The honesty counterpart to the ledger note: this IS the real end-to-end number.
pub const EVAL_NOTE: &str = "real end-to-end A/B (cost-primary, correctness-gated, paired)";

/// Parse a JSONL question set, skipping malformed/blank lines. Returns the
/// questions in file order plus a skipped count.
pub fn parse_questions(text: &str) -> (Vec<Question>, usize) {
    parse_jsonl(text)
}

/// Parse a JSONL run set, skipping malformed/blank lines.
pub fn parse_runs(text: &str) -> (Vec<RunRecord>, usize) {
    parse_jsonl(text)
}

fn parse_jsonl<T: for<'de> Deserialize<'de>>(text: &str) -> (Vec<T>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        if line.trim().is_empty() {
            skipped += 1;
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(v) => out.push(v),
            Err(_) => skipped += 1,
        }
    }
    (out, skipped)
}

/// Evaluate a question set against recorded runs into the A/B report. Deterministic:
/// questions are processed in sorted-id order, and a duplicate (question, condition)
/// run is resolved last-wins. `skipped_runs` reflects lines the caller already
/// dropped as malformed (pass it through from `parse_runs`).
pub fn evaluate(questions: &[Question], runs: &[RunRecord], skipped_runs: usize) -> AbReport {
    // Index runs by (question_id, condition); last occurrence wins.
    let mut by_key: BTreeMap<(String, Condition), &RunRecord> = BTreeMap::new();
    for r in runs {
        by_key.insert((r.question_id.clone(), r.condition), r);
    }
    let questions_by_id: BTreeMap<&str, &Question> =
        questions.iter().map(|q| (q.id.as_str(), q)).collect();

    let mut off = ArmAcc::default();
    let mut on = ArmAcc::default();
    let mut paired_correct = 0u64;
    let mut paired_off_cost = 0.0f64;
    let mut paired_on_cost = 0.0f64;

    for (id, q) in &questions_by_id {
        let off_run = by_key.get(&(id.to_string(), Condition::Off)).copied();
        let on_run = by_key.get(&(id.to_string(), Condition::On)).copied();
        let off_grade = off_run.map(|r| (grade(q, r), r.total_cost()));
        let on_grade = on_run.map(|r| (grade(q, r), r.total_cost()));
        off.record(off_grade);
        on.record(on_grade);
        if let (Some((Grade::Correct, oc)), Some((Grade::Correct, nc))) = (off_grade, on_grade) {
            paired_correct += 1;
            paired_off_cost += oc;
            paired_on_cost += nc;
        }
    }

    let paired_off = round2(paired_off_cost);
    let paired_on = round2(paired_on_cost);
    let delta = round2(paired_off_cost - paired_on_cost);
    let ratio = if paired_off_cost > 0.0 {
        round6((paired_off_cost - paired_on_cost) / paired_off_cost)
    } else {
        0.0
    };

    AbReport {
        questions: questions.len() as u64,
        off: off.finish(),
        on: on.finish(),
        paired_correct,
        paired_off_cost_usd: paired_off,
        paired_on_cost_usd: paired_on,
        cost_delta_usd: delta,
        cost_saved_ratio: ratio,
        skipped_runs: skipped_runs as u64,
        note: EVAL_NOTE.to_string(),
    }
}

/// Mutable per-arm accumulator.
#[derive(Default)]
struct ArmAcc {
    runs: u64,
    correct: u64,
    incorrect: u64,
    punts: u64,
    correct_cost: f64,
}

impl ArmAcc {
    /// Fold one question's outcome for this arm (`None` = no run recorded).
    fn record(&mut self, outcome: Option<(Grade, f64)>) {
        if let Some((g, cost)) = outcome {
            self.runs += 1;
            match g {
                Grade::Correct => {
                    self.correct += 1;
                    self.correct_cost += cost;
                }
                Grade::Incorrect => self.incorrect += 1,
                Grade::Punt => self.punts += 1,
            }
        }
    }

    fn finish(self) -> ArmSummary {
        let mean = if self.correct > 0 {
            round2(self.correct_cost / self.correct as f64)
        } else {
            0.0
        };
        ArmSummary {
            runs: self.runs,
            correct: self.correct,
            incorrect: self.incorrect,
            punts: self.punts,
            correct_cost_usd: round2(self.correct_cost),
            mean_correct_cost_usd: mean,
        }
    }
}

/// Round to 2 decimals, round-half-away-from-zero (money).
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Round to 6 decimals, round-half-away-from-zero (ratios).
fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

/// Load, parse, and evaluate a question file and a runs file from disk. A thin,
/// deterministic wrapper the CLI calls; the pure pieces above are what tests drive.
pub fn evaluate_files(questions_text: &str, runs_text: &str) -> AbReport {
    let (questions, _) = parse_questions(questions_text);
    let (runs, skipped) = parse_runs(runs_text);
    evaluate(&questions, &runs, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: &str, needles: &[&str]) -> Question {
        Question {
            id: id.to_string(),
            question: format!("Q {id}"),
            must_include: needles.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn run(id: &str, cond: Condition, answer: &str, cost: f64, sub: f64) -> RunRecord {
        RunRecord {
            question_id: id.to_string(),
            condition: cond,
            answer: answer.to_string(),
            cost_usd: cost,
            subagent_cost_usd: sub,
            punted: None,
        }
    }

    #[test]
    fn punt_detection_catches_non_answers() {
        assert!(is_punt(""));
        assert!(is_punt("  "));
        assert!(is_punt("no"));
        assert!(is_punt("I couldn't find the function anywhere."));
        assert!(is_punt("N/A"));
        assert!(!is_punt("It is defined in auth.py at hash_password."));
    }

    #[test]
    fn grade_gates_on_ground_truth_and_punts() {
        let question = q("q1", &["auth.py", "hash_password"]);
        // Correct: both substrings present (case-insensitive).
        assert_eq!(
            grade(&question, &run("q1", Condition::On, "See Auth.py -> hash_password()", 0.1, 0.0)),
            Grade::Correct
        );
        // Incorrect: missing one required substring.
        assert_eq!(
            grade(&question, &run("q1", Condition::On, "It's in auth.py somewhere", 0.1, 0.0)),
            Grade::Incorrect
        );
        // Punt beats a keyword hit: explicit non-answer even though it names auth.py.
        assert_eq!(
            grade(
                &question,
                &run("q1", Condition::On, "I couldn't find auth.py hash_password", 0.1, 0.0)
            ),
            Grade::Punt
        );
    }

    #[test]
    fn explicit_punted_flag_is_respected() {
        let question = q("q1", &["x"]);
        let mut r = run("q1", Condition::On, "x is here, fully correct looking", 0.2, 0.0);
        r.punted = Some(true);
        assert_eq!(grade(&question, &r), Grade::Punt);
    }

    #[test]
    fn cost_includes_subagents() {
        let r = run("q1", Condition::On, "answer", 0.10, 0.25);
        assert_eq!(r.total_cost(), 0.35);
    }

    #[test]
    fn evaluate_pairs_and_is_cost_primary() {
        let questions = vec![q("q1", &["alpha"]), q("q2", &["beta"]), q("q3", &["gamma"])];
        let runs = vec![
            // q1 correct in both; off costs more than on -> a real saving.
            run("q1", Condition::Off, "alpha found", 1.00, 0.00),
            run("q1", Condition::On, "alpha found", 0.40, 0.00),
            // q2 correct off, punt on -> excluded from the paired set (no fake win).
            run("q2", Condition::Off, "beta found", 0.80, 0.00),
            run("q2", Condition::On, "I don't know", 0.05, 0.00),
            // q3 correct in both; on includes a sub-agent cost.
            run("q3", Condition::Off, "gamma found", 0.50, 0.00),
            run("q3", Condition::On, "gamma found", 0.20, 0.20),
        ];
        let report = evaluate(&questions, &runs, 0);
        assert_eq!(report.questions, 3);
        assert_eq!(report.off.correct, 3);
        assert_eq!(report.on.correct, 2);
        assert_eq!(report.on.punts, 1);
        // Paired = q1 and q3 (both correct in both arms).
        assert_eq!(report.paired_correct, 2);
        // off: 1.00 + 0.50 = 1.50 ; on: 0.40 + (0.20+0.20)=0.40 -> 0.80.
        assert_eq!(report.paired_off_cost_usd, 1.50);
        assert_eq!(report.paired_on_cost_usd, 0.80);
        assert_eq!(report.cost_delta_usd, 0.70);
        // 0.70 / 1.50 = 0.466667.
        assert_eq!(report.cost_saved_ratio, 0.466667);
        assert_eq!(report.note, EVAL_NOTE);
    }

    #[test]
    fn missing_arm_and_empty_paired_set_is_safe() {
        let questions = vec![q("q1", &["alpha"])];
        // Only an off run exists; nothing to pair.
        let runs = vec![run("q1", Condition::Off, "alpha found", 1.0, 0.0)];
        let report = evaluate(&questions, &runs, 0);
        assert_eq!(report.paired_correct, 0);
        assert_eq!(report.cost_saved_ratio, 0.0);
        assert_eq!(report.on.runs, 0);
    }

    #[test]
    fn parsers_skip_malformed_lines() {
        let qtext = concat!(
            "{\"id\":\"q1\",\"question\":\"where\",\"must_include\":[\"auth.py\"]}\n",
            "\n",
            "not json\n",
            "{\"id\":\"q2\",\"question\":\"what\"}\n"
        );
        let (qs, skipped) = parse_questions(qtext);
        assert_eq!(qs.len(), 2);
        assert_eq!(skipped, 2);
        // q2 has no must_include (defaults to empty).
        assert!(qs[1].must_include.is_empty());

        let rtext = concat!(
            "{\"question_id\":\"q1\",\"condition\":\"on\",\"answer\":\"a\",\"cost_usd\":0.1}\n",
            "{\"question_id\":\"q1\",\"condition\":\"sideways\"}\n"
        );
        let (rs, rskip) = parse_runs(rtext);
        assert_eq!(rs.len(), 1);
        assert_eq!(rskip, 1); // bad condition line dropped
    }

    #[test]
    fn evaluate_files_end_to_end() {
        let qtext = "{\"id\":\"q1\",\"question\":\"where\",\"must_include\":[\"auth.py\"]}\n";
        let rtext = concat!(
            "{\"question_id\":\"q1\",\"condition\":\"off\",\"answer\":\"auth.py\",\"cost_usd\":1.0}\n",
            "{\"question_id\":\"q1\",\"condition\":\"on\",\"answer\":\"auth.py\",\"cost_usd\":0.5}\n"
        );
        let report = evaluate_files(qtext, rtext);
        assert_eq!(report.paired_correct, 1);
        assert_eq!(report.cost_saved_ratio, 0.5);
    }
}
