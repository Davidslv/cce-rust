//! # stats — deterministic paired-significance math for the relevance harness (issue #84)
//!
//! **Why this file exists:** `cce relevance --compare` is the mandated evidence
//! gate for ranking changes, but a raw mean delta over a handful of queries is
//! exactly the number most likely to mislead — at n=6 only huge effects are
//! trustworthy. This module adds the missing inferential layer: a paired
//! t-statistic, a two-sided p-value, and a 95% confidence interval on the mean
//! per-query delta, so a compare table states not just *how big* a delta looks
//! but *how much evidence* there is that it is real.
//!
//! **What it is / does:** A dependency-free, closed-form implementation of the
//! Student t distribution — log-gamma (Lanczos), the regularized incomplete
//! beta function (continued fraction, Lentz's method), the two-sided t-tail
//! probability, and the two-sided critical value (bisection on that tail).
//! `paired_t` composes them into the paired t-test over a delta vector.
//!
//! **Why closed-form t rather than a permutation test:** both are deterministic
//! and offline, but the closed form needs no seed, no resampling loop, and no
//! iteration-count knob to pin in a golden — the same deltas always produce the
//! same bytes on every platform, with nothing to tune. The paired t-test is
//! also the standard, well-behaved choice for per-topic IR metric deltas at
//! small n (Sakai; Urbano et al.), whereas an exact sign-flip permutation test
//! at n=6 has only 2⁶ = 64 distinct sign assignments, so its p-values are
//! quantized to 1/64 steps — strictly coarser evidence than the t
//! approximation it would be checking.
//!
//! **Responsibilities:**
//! - Own the t-distribution math and its numerical tolerances.
//! - It does NOT rank, score, or render: `relevance` owns metric deltas and
//!   report formatting; this module only turns a delta vector into statistics.

/// Natural log of the gamma function for `x > 0` (Lanczos approximation,
/// g = 5, 6 coefficients — the classic `gammln`). Accurate to well under
/// 1e-10 relative error over the range the t-test uses (half-integers ≥ 0.5).
pub fn ln_gamma(x: f64) -> f64 {
    const COF: [f64; 6] = [
        76.180_091_729_471_46,
        -86.505_320_329_416_77,
        24.014_098_240_830_91,
        -1.231_739_572_450_155,
        0.120_865_097_386_617_9e-2,
        -0.539_523_938_495_3e-5,
    ];
    let mut ser = 1.000_000_000_190_015_f64;
    for (j, c) in COF.iter().enumerate() {
        ser += c / (x + 1.0 + j as f64);
    }
    let tmp = x + 5.5;
    let tmp = tmp - (x + 0.5) * tmp.ln();
    -tmp + (2.506_628_274_631_000_5 * ser / x).ln()
}

/// The continued-fraction kernel of the incomplete beta function (modified
/// Lentz's method). Converges in a handful of iterations for
/// `x < (a + 1) / (a + b + 2)`, which `reg_inc_beta` guarantees.
fn betacf(a: f64, b: f64, x: f64) -> f64 {
    const MAX_IT: usize = 300;
    const EPS: f64 = 3e-14;
    const FPMIN: f64 = 1e-300;
    let qab = a + b;
    let qap = a + 1.0;
    let qam = a - 1.0;
    let mut c = 1.0;
    let mut d = 1.0 - qab * x / qap;
    if d.abs() < FPMIN {
        d = FPMIN;
    }
    d = 1.0 / d;
    let mut h = d;
    for m in 1..=MAX_IT {
        let m = m as f64;
        let m2 = 2.0 * m;
        // Even step.
        let aa = m * (b - m) * x / ((qam + m2) * (a + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        h *= d * c;
        // Odd step.
        let aa = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2));
        d = 1.0 + aa * d;
        if d.abs() < FPMIN {
            d = FPMIN;
        }
        c = 1.0 + aa / c;
        if c.abs() < FPMIN {
            c = FPMIN;
        }
        d = 1.0 / d;
        let del = d * c;
        h *= del;
        if (del - 1.0).abs() < EPS {
            break;
        }
    }
    h
}

/// The regularized incomplete beta function `I_x(a, b)` for `a, b > 0` and
/// `0 ≤ x ≤ 1` (Numerical-Recipes `betai`: the continued fraction, mirrored
/// through the symmetry `I_x(a, b) = 1 − I_{1−x}(b, a)` for fast convergence).
pub fn reg_inc_beta(a: f64, b: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let ln_bt = ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b) + a * x.ln() + b * (1.0 - x).ln();
    let bt = ln_bt.exp();
    if x < (a + 1.0) / (a + b + 2.0) {
        bt * betacf(a, b, x) / a
    } else {
        1.0 - bt * betacf(b, a, 1.0 - x) / b
    }
}

/// Two-sided tail probability of Student's t: `P(|T| ≥ |t|)` at `df` degrees
/// of freedom, via the exact identity `p = I_{df/(df+t²)}(df/2, 1/2)`.
pub fn t_two_sided_p(t: f64, df: f64) -> f64 {
    debug_assert!(df > 0.0);
    reg_inc_beta(df / 2.0, 0.5, df / (df + t * t))
}

/// The two-sided critical value: the `t ≥ 0` with `P(|T| ≥ t) = alpha` at `df`
/// degrees of freedom (e.g. `alpha = 0.05` gives the 97.5th percentile used by
/// a 95% CI). Deterministic bisection — `t_two_sided_p` is strictly decreasing
/// in `t`, and 200 fixed halvings of the bracket reach f64 resolution.
pub fn t_two_sided_critical(alpha: f64, df: f64) -> f64 {
    debug_assert!(alpha > 0.0 && alpha < 1.0 && df > 0.0);
    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;
    // Grow the bracket until the tail at `hi` is below alpha (df=1 at
    // alpha=0.05 needs t≈12.7; tiny alphas need more).
    while t_two_sided_p(hi, df) > alpha && hi < 1e300 {
        hi *= 2.0;
    }
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if t_two_sided_p(mid, df) > alpha {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The paired-t summary of one metric's per-query delta vector. Every field a
/// compare report renders, in one place. `None` marks "not defined at this
/// input", never a numerical failure:
/// - `n < 2`: no variance estimate exists — only `n` and `mean_delta` are set.
/// - zero-variance deltas: `t` is undefined (the limit is ±∞), but `p` and the
///   CI still have natural values — `p = 1` when every delta is 0 (no effect),
///   `p = 0` when every delta is the same non-zero value, CI = `[mean, mean]`.
#[derive(Debug, Clone, PartialEq)]
pub struct PairedStats {
    /// Number of paired observations (queries).
    pub n: usize,
    /// Mean of the deltas (b − a).
    pub mean_delta: f64,
    /// Paired t-statistic `mean / (sd / √n)`, when the variance is positive.
    pub t: Option<f64>,
    /// Two-sided p-value at `n − 1` degrees of freedom.
    pub p: Option<f64>,
    /// 95% confidence interval on the mean delta, `(low, high)`.
    pub ci95: Option<(f64, f64)>,
}

/// The paired t-test over a delta vector (95% CI, two-sided p). See
/// `PairedStats` for the degenerate-input conventions.
pub fn paired_t(deltas: &[f64]) -> PairedStats {
    let n = deltas.len();
    let mean = if n == 0 {
        0.0
    } else {
        deltas.iter().sum::<f64>() / n as f64
    };
    if n < 2 {
        return PairedStats { n, mean_delta: mean, t: None, p: None, ci95: None };
    }
    // Constancy check tolerant of rounding residue, not bit-identity. Per-query
    // deltas that are mathematically equal but computed from different bases
    // (0.6−0.4 = 0.199…996 vs 0.2−0.0 = 0.2) differ in the last bit(s); a raw
    // `all(== first)` misses them and their ~1e-17 variance then yields an
    // astronomical t (~1e16) that saturates the report's t column (#108). Treat
    // the deltas as constant when their spread is within a few ULPs of their
    // magnitude — the interval below which no honest per-query effect lives.
    let first = deltas[0];
    let (mut lo, mut hi) = (first, first);
    for &d in deltas {
        lo = lo.min(d);
        hi = hi.max(d);
    }
    let scale = lo.abs().max(hi.abs());
    let constancy_tol = 16.0 * f64::EPSILON * scale;
    if hi - lo <= constancy_tol {
        let p = if mean == 0.0 { 1.0 } else { 0.0 };
        return PairedStats {
            n,
            mean_delta: first,
            t: None,
            p: Some(p),
            ci95: Some((first, first)),
        };
    }
    let var = deltas.iter().map(|d| (d - mean) * (d - mean)).sum::<f64>() / (n - 1) as f64;
    let sd = var.sqrt();
    let df = (n - 1) as f64;
    let se = sd / (n as f64).sqrt();
    let t = mean / se;
    let p = t_two_sided_p(t, df);
    let half = t_two_sided_critical(0.05, df) * se;
    PairedStats {
        n,
        mean_delta: mean,
        t: Some(t),
        p: Some(p),
        ci95: Some((mean - half, mean + half)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ln_gamma against exact values ---

    #[test]
    fn ln_gamma_matches_exact_factorials_and_half_integers() {
        // Γ(1) = Γ(2) = 1; Γ(5) = 24; Γ(1/2) = √π.
        assert!(ln_gamma(1.0).abs() < 1e-10);
        assert!(ln_gamma(2.0).abs() < 1e-10);
        assert!((ln_gamma(5.0) - 24.0_f64.ln()).abs() < 1e-10);
        assert!((ln_gamma(0.5) - std::f64::consts::PI.sqrt().ln()).abs() < 1e-10);
        // Γ(2.5) = 3/4 · √π.
        let exact = (0.75 * std::f64::consts::PI.sqrt()).ln();
        assert!((ln_gamma(2.5) - exact).abs() < 1e-10);
    }

    // --- reg_inc_beta against closed forms ---

    #[test]
    fn reg_inc_beta_boundaries_and_symmetry() {
        assert_eq!(reg_inc_beta(2.0, 3.0, 0.0), 0.0);
        assert_eq!(reg_inc_beta(2.0, 3.0, 1.0), 1.0);
        // I_x(1, 1) = x (the uniform CDF).
        assert!((reg_inc_beta(1.0, 1.0, 0.3) - 0.3).abs() < 1e-12);
        // I_x(1, b) = 1 − (1 − x)^b.
        let x = 0.2;
        assert!((reg_inc_beta(1.0, 3.0, x) - (1.0 - (1.0 - x).powi(3))).abs() < 1e-12);
        // Symmetry: I_x(a, b) = 1 − I_{1−x}(b, a).
        let v = reg_inc_beta(2.5, 0.5, 0.4) + reg_inc_beta(0.5, 2.5, 0.6);
        assert!((v - 1.0).abs() < 1e-12);
    }

    // --- t tail probability against exact small-df forms and table values ---

    #[test]
    fn t_p_is_one_at_zero_and_symmetric() {
        assert!((t_two_sided_p(0.0, 5.0) - 1.0).abs() < 1e-12);
        for df in [1.0, 2.0, 5.0, 30.0] {
            let p_pos = t_two_sided_p(1.7, df);
            let p_neg = t_two_sided_p(-1.7, df);
            assert!((p_pos - p_neg).abs() < 1e-12, "asymmetric at df={df}");
        }
    }

    #[test]
    fn t_p_exact_at_df1_cauchy() {
        // df=1 is Cauchy: P(|T| ≥ t) = 1 − (2/π)·atan(t).
        // t=1 → 1 − 2·(π/4)/π = 1/2 exactly.
        assert!((t_two_sided_p(1.0, 1.0) - 0.5).abs() < 1e-10);
        // t=√3 → atan(√3) = π/3 → p = 1 − 2/3 = 1/3 exactly.
        assert!((t_two_sided_p(3.0_f64.sqrt(), 1.0) - 1.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn t_p_exact_at_df2() {
        // df=2 has the closed form P(|T| ≥ t) = 1 − t/√(2 + t²).
        // t=√2 → 1 − √2/2 = 0.292893218813…
        let expect = 1.0 - 2.0_f64.sqrt() / 2.0;
        assert!((t_two_sided_p(2.0_f64.sqrt(), 2.0) - expect).abs() < 1e-10);
    }

    #[test]
    fn t_p_matches_the_classic_005_table_row() {
        // Standard two-sided 5% critical values (Student t tables).
        for (df, t) in [(1.0, 12.7062), (2.0, 4.30265), (5.0, 2.570582), (10.0, 2.228139)] {
            let p = t_two_sided_p(t, df);
            assert!((p - 0.05).abs() < 1e-4, "df={df}: p={p}");
        }
    }

    #[test]
    fn t_critical_matches_the_table() {
        for (df, expect) in
            [(1.0, 12.706205), (2.0, 4.302653), (5.0, 2.570582), (10.0, 2.228139), (30.0, 2.042272)]
        {
            let t = t_two_sided_critical(0.05, df);
            assert!((t - expect).abs() < 1e-4, "df={df}: t={t}, expected {expect}");
        }
    }

    // --- paired_t: a fully hand-computed example ---

    #[test]
    fn paired_t_hand_computed_example() {
        // d = [0.2, 0.1, 0.0, 0.3, −0.1, 0.1]; n=6, df=5.
        // mean = 0.6/6 = 0.1
        // deviations: [0.1, 0, −0.1, 0.2, −0.2, 0] → Σd² = 0.10
        // var = 0.10/5 = 0.02, sd = 0.141421356, se = sd/√6 = 0.057735027
        // t = 0.1/0.057735027 = √3 = 1.732050808
        // CI = 0.1 ± 2.570582·0.057735027 = 0.1 ± 0.148413 → [−0.048413, 0.248413]
        let s = paired_t(&[0.2, 0.1, 0.0, 0.3, -0.1, 0.1]);
        assert_eq!(s.n, 6);
        assert!((s.mean_delta - 0.1).abs() < 1e-12);
        let t = s.t.unwrap();
        assert!((t - 3.0_f64.sqrt()).abs() < 1e-9, "t={t}");
        // t = 1.732 sits between the df=5 table points t₀.₁₀ = 2.015 and
        // t₀.₂₀ = 1.476 (two-sided), so p ∈ (0.10, 0.20).
        let p = s.p.unwrap();
        assert!(p > 0.10 && p < 0.20, "p={p}");
        let (lo, hi) = s.ci95.unwrap();
        assert!((lo - (-0.048413)).abs() < 1e-4, "lo={lo}");
        assert!((hi - 0.248413).abs() < 1e-4, "hi={hi}");
    }

    #[test]
    fn paired_t_all_zero_deltas_is_p_one() {
        let s = paired_t(&[0.0, 0.0, 0.0, 0.0]);
        assert_eq!(s.n, 4);
        assert_eq!(s.mean_delta, 0.0);
        assert_eq!(s.t, None);
        assert_eq!(s.p, Some(1.0));
        assert_eq!(s.ci95, Some((0.0, 0.0)));
    }

    #[test]
    fn paired_t_constant_nonzero_deltas_is_p_zero() {
        let s = paired_t(&[0.1, 0.1, 0.1]);
        assert_eq!(s.t, None);
        assert_eq!(s.p, Some(0.0));
        assert_eq!(s.ci95, Some((0.1, 0.1)));
        assert!((s.mean_delta - 0.1).abs() < 1e-12);
    }

    #[test]
    fn paired_t_mathematically_equal_deltas_from_different_bases_are_constant() {
        // #108: per-query deltas that are mathematically the same value but
        // computed from DIFFERENT bases differ in the last bit(s). The constancy
        // guard must treat them as constant (t = None, p = 0, CI = [mean, mean]),
        // not let ~1e-17 rounding residue produce a saturating t.
        let d = [0.6 - 0.4, 0.3 - 0.1, 0.5 - 0.3, 0.7 - 0.5];
        // Precondition: these really are bit-distinct, so a bit-identity guard
        // (the old code) would miss them — otherwise this test proves nothing.
        assert!(
            !(d[0] == d[1] && d[1] == d[2] && d[2] == d[3]),
            "inputs must be bit-distinct to exercise #108"
        );
        let s = paired_t(&d);
        assert_eq!(s.t, None, "rounding residue must not masquerade as a huge t");
        assert_eq!(s.p, Some(0.0));
        let (lo, hi) = s.ci95.unwrap();
        assert_eq!(lo, hi, "CI collapses to the constant delta, not a saturated bound");
        assert!((s.mean_delta - 0.2).abs() < 1e-9);
    }

    #[test]
    fn paired_t_underpowered_inputs_yield_none() {
        let one = paired_t(&[0.3]);
        assert_eq!(one.n, 1);
        assert!((one.mean_delta - 0.3).abs() < 1e-12);
        assert_eq!(one.t, None);
        assert_eq!(one.p, None);
        assert_eq!(one.ci95, None);
        let none = paired_t(&[]);
        assert_eq!(none.n, 0);
        assert_eq!(none.mean_delta, 0.0);
        assert_eq!(none.p, None);
    }

    #[test]
    fn paired_t_is_sign_symmetric() {
        let pos = paired_t(&[0.2, 0.1, 0.3, 0.05]);
        let neg = paired_t(&[-0.2, -0.1, -0.3, -0.05]);
        assert!((pos.t.unwrap() + neg.t.unwrap()).abs() < 1e-12);
        assert!((pos.p.unwrap() - neg.p.unwrap()).abs() < 1e-12);
        let (plo, phi) = pos.ci95.unwrap();
        let (nlo, nhi) = neg.ci95.unwrap();
        assert!((plo + nhi).abs() < 1e-12);
        assert!((phi + nlo).abs() < 1e-12);
    }
}
