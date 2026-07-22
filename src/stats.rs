//! Statistical tests for A/B measurement: Wilcoxon signed-rank, Mann-Whitney U,
//! Cohen's d_z, bootstrap median CI, and supporting helpers.

// ── normal_cdf / erf ────────────────────────────────────────────────────────

/// Abramowitz & Stegun 7.1.26 rational approximation; |error| < 1.5e-7.
fn erf_approx(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs());
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736
                + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    sign * (1.0 - poly * (-x * x).exp())
}

fn erfc_approx(x: f64) -> f64 {
    1.0 - erf_approx(x)
}

/// Standard-normal CDF: Φ(z) = 0.5 · erfc(−z / √2).
pub fn normal_cdf(z: f64) -> f64 {
    0.5 * erfc_approx(-z / std::f64::consts::SQRT_2)
}

// ── SplitMix64 PRNG ─────────────────────────────────────────────────────────

pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Unbiased index in `[0, n)` via rejection. `n > 0` assumed by callers.
    pub(crate) fn next_below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }
}

// ── average_ranks ────────────────────────────────────────────────────────────

/// Given a sorted slice of absolute values, return average ranks (1-based).
/// Ties receive the mean of the positions they span.
pub(crate) fn average_ranks(sorted_abs: &[f64]) -> Vec<f64> {
    let n = sorted_abs.len();
    let mut ranks = vec![0.0_f64; n];
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n
            && (sorted_abs[j] - sorted_abs[i]).abs() < f64::EPSILON * sorted_abs[i].abs().max(1.0)
        {
            j += 1;
        }
        // positions i..j (0-based) → 1-based: (i+1)..(j+1)
        let avg = ((i + 1) as f64 + j as f64) / 2.0;
        for r in &mut ranks[i..j] {
            *r = avg;
        }
        i = j;
    }
    ranks
}

// ── median helper ────────────────────────────────────────────────────────────

fn median_sorted(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

fn median_of(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    median_sorted(&sorted)
}

// ── Wilcoxon signed-rank ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WilcoxonMethod {
    ExactPratt,
    NormalApproxPratt,
}

#[derive(Debug, Clone)]
pub struct WilcoxonResult {
    pub w_plus: f64,
    pub w_minus: f64,
    pub w: f64,
    pub n_nonzero: usize,
    pub p_two_sided: f64,
    pub method: WilcoxonMethod,
}

/// Largest non-zero count for which the exact 2ⁿ sign-flip enumeration is used. Above this
/// the normal approximation takes over (2^25 ≈ 34M sign assignments is the tractability ceiling).
const EXACT_MAX_N: usize = 25;

pub fn wilcoxon_signed_rank(deltas: &[f64]) -> WilcoxonResult {
    // Sort all |delta| including zeros (Pratt method).
    let mut abs_all: Vec<f64> = deltas.iter().map(|d| d.abs()).collect();
    abs_all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let ranks_all = average_ranks(&abs_all);

    // Build a rank per original delta (match by index after sorting).
    // We need to assign each original delta its Pratt rank.
    // Strategy: for each delta, find its position in the sorted order.
    // Since there can be ties, we do a stable assignment.
    let mut indexed: Vec<(usize, f64)> = deltas
        .iter()
        .enumerate()
        .map(|(i, &d)| (i, d.abs()))
        .collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut rank_by_original_idx = vec![0.0_f64; deltas.len()];
    for (sorted_pos, &(orig_idx, _)) in indexed.iter().enumerate() {
        rank_by_original_idx[orig_idx] = ranks_all[sorted_pos];
    }

    // Compute W+ and W- from non-zero deltas only.
    let mut w_plus = 0.0_f64;
    let mut w_minus = 0.0_f64;
    let mut nonzero_ranks: Vec<f64> = Vec::new();

    for (i, &d) in deltas.iter().enumerate() {
        if d == 0.0 {
            continue;
        }
        let r = rank_by_original_idx[i];
        if d > 0.0 {
            w_plus += r;
        } else {
            w_minus += r;
        }
        nonzero_ranks.push(r);
    }

    let n_nonzero = nonzero_ranks.len();
    let w = w_plus.min(w_minus);

    if n_nonzero == 0 {
        return WilcoxonResult {
            w_plus: 0.0,
            w_minus: 0.0,
            w: 0.0,
            n_nonzero: 0,
            p_two_sided: 1.0,
            method: WilcoxonMethod::NormalApproxPratt,
        };
    }

    // Route to the EXACT sign-flip enumeration whenever it is tractable (n ≤ 25),
    // **including tied |delta|** (issue #7). The enumeration sums the actual observed Pratt
    // ranks, so it is a valid exact randomization test with ties — and it is the *right*
    // answer for the small batteries abproof runs. The earlier `!has_ties` guard sent every
    // tied input (all real pass/fail data ties) to the normal approximation, whose −½
    // continuity correction is calibrated for a unit-step statistic; under heavy ties W+
    // moves in steps of (n+1)/2, so the approximation under-corrects and is anti-conservative
    // near α (e.g. 5 unanimous ±1 nodes: approx 0.037 < 0.05 "significant" vs exact 0.0625
    // "not" — a false gate failure at the honesty floor). The normal approximation is retained
    // only for `n > 25`, where enumeration is intractable and the large-sample fit is close.
    // ponytail: 2^25 enumeration ceiling; if batteries routinely exceed ~25 gradable nodes,
    // swap the enumeration for the O(n·Σr) DP convolution rather than raising this bound.
    let use_exact = n_nonzero <= EXACT_MAX_N;

    if use_exact {
        let p = exact_wilcoxon_p(w_plus, &nonzero_ranks);
        WilcoxonResult {
            w_plus,
            w_minus,
            w,
            n_nonzero,
            p_two_sided: p,
            method: WilcoxonMethod::ExactPratt,
        }
    } else {
        let p = approx_wilcoxon_p(w_plus, &nonzero_ranks);
        WilcoxonResult {
            w_plus,
            w_minus,
            w,
            n_nonzero,
            p_two_sided: p,
            method: WilcoxonMethod::NormalApproxPratt,
        }
    }
}

/// Exact two-sided p-value by enumerating all 2^n sign assignments.
fn exact_wilcoxon_p(observed_w_plus: f64, nonzero_ranks: &[f64]) -> f64 {
    let n = nonzero_ranks.len();
    let total: u64 = 1u64 << n;
    let sum_ranks: f64 = nonzero_ranks.iter().sum();
    let mean = sum_ranks / 2.0;
    let observed_dev = (observed_w_plus - mean).abs();

    let mut count = 0u64;
    for mask in 0..total {
        let mut wp = 0.0_f64;
        for (bit, &r) in nonzero_ranks.iter().enumerate() {
            if (mask >> bit) & 1 == 1 {
                wp += r;
            }
        }
        if (wp - mean).abs() >= observed_dev - 1e-12 {
            count += 1;
        }
    }

    (count as f64 / total as f64).min(1.0)
}

/// Normal approximation to the two-sided p-value, using the **sign-flip randomization
/// moments** of W+.
///
/// Under the null "each non-zero delta is equally likely +/−", `W+ = Σ rᵢ·Bᵢ` with
/// `Bᵢ ~ Bernoulli(½)` i.i.d. over the observed non-zero Pratt ranks `rᵢ`, so
///
/// ```text
/// μ  = E[W+]   = Σ rᵢ / 2
/// σ² = Var[W+] = Σ rᵢ² / 4
/// ```
///
/// These are **exact for Pratt ranks with ties**: `Σrᵢ²/4` subsumes the separate
/// tie-correction term, and using the actual `Σrᵢ` accounts for the zeros having consumed
/// the low ranks. The prior `μ = n(n+1)/4`, `σ² = n(n+1)(2n+1)/24` are the *zero-free*
/// moments — correct only when the ranks are exactly `1..n` (no zeros), which is precisely
/// the case pass/fail deltas never satisfy. Matches
/// `scipy.stats.wilcoxon(zero_method='pratt', correction=True, mode='approx')` to machine
/// precision (issue #7; `tests/stats_golden.rs`).
fn approx_wilcoxon_p(w_plus: f64, nonzero_ranks: &[f64]) -> f64 {
    let mu = nonzero_ranks.iter().sum::<f64>() / 2.0;
    let sigma2 = nonzero_ranks.iter().map(|r| r * r).sum::<f64>() / 4.0;
    let sigma = sigma2.sqrt();

    if sigma == 0.0 {
        return 1.0;
    }

    let diff = (w_plus - mu).abs() - 0.5;
    let z = diff.max(0.0) / sigma;
    (2.0 * (1.0 - normal_cdf(z))).clamp(0.0, 1.0)
}

// ── Mann-Whitney U ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MannWhitneyResult {
    pub u: f64,
    pub u1: f64,
    pub u2: f64,
    pub p_two_sided: f64,
}

pub fn mann_whitney_u(a: &[f64], b: &[f64]) -> MannWhitneyResult {
    let na = a.len();
    let nb = b.len();
    let n_total = na + nb;

    // Pool and sort with group labels.
    let mut pool: Vec<(f64, u8)> = a
        .iter()
        .map(|&v| (v, 0u8))
        .chain(b.iter().map(|&v| (v, 1u8)))
        .collect();
    pool.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(std::cmp::Ordering::Equal));

    let vals: Vec<f64> = pool.iter().map(|p| p.0).collect();
    let ranks = average_ranks(&vals);

    let mut r_a = 0.0_f64;
    for (i, &(_, grp)) in pool.iter().enumerate() {
        if grp == 0 {
            r_a += ranks[i];
        }
    }

    let na_f = na as f64;
    let nb_f = nb as f64;
    let u1 = na_f * nb_f + na_f * (na_f + 1.0) / 2.0 - r_a;
    let u2 = na_f * nb_f - u1;
    let u = u1.min(u2);

    // Normal approximation with tie correction.
    let n_f = n_total as f64;
    let mu_u = na_f * nb_f / 2.0;

    let mut tie_sum = 0.0_f64;
    let mut i = 0;
    while i < vals.len() {
        let mut j = i + 1;
        while j < vals.len() && (vals[j] - vals[i]).abs() < f64::EPSILON * vals[i].abs().max(1.0) {
            j += 1;
        }
        let t = (j - i) as f64;
        tie_sum += t * t * t - t;
        i = j;
    }

    let p_two_sided = if n_total <= 1 {
        1.0
    } else {
        let sigma2_u = (na_f * nb_f / 12.0) * ((n_f + 1.0) - tie_sum / (n_f * (n_f - 1.0)));
        let sigma_u = sigma2_u.max(0.0).sqrt();
        if sigma_u == 0.0 {
            1.0
        } else {
            let diff = (u - mu_u).abs() - 0.5;
            let z = diff.max(0.0) / sigma_u;
            (2.0 * (1.0 - normal_cdf(z))).clamp(0.0, 1.0)
        }
    };

    MannWhitneyResult {
        u,
        u1,
        u2,
        p_two_sided,
    }
}

// ── Cohen's d_z ──────────────────────────────────────────────────────────────

pub fn cohens_dz(deltas: &[f64]) -> f64 {
    let n = deltas.len();
    if n < 2 {
        return 0.0;
    }
    let mean = deltas.iter().sum::<f64>() / n as f64;
    let variance = deltas.iter().map(|&d| (d - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    let sd = variance.sqrt();
    if sd == 0.0 {
        0.0
    } else {
        mean / sd
    }
}

// ── Bootstrap median CI ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BootstrapCi {
    pub point: f64,
    pub lower: f64,
    pub upper: f64,
    pub b: usize,
    pub seed: u64,
}

pub fn bootstrap_median_ci(deltas: &[f64], b: usize, seed: u64, alpha: f64) -> BootstrapCi {
    if deltas.is_empty() {
        return BootstrapCi {
            point: 0.0,
            lower: 0.0,
            upper: 0.0,
            b,
            seed,
        };
    }

    let point = median_of(deltas);

    // No resamples requested → the interval collapses to the point estimate.
    // Guards the `b - 1` index math below against usize underflow at b == 0
    // and keeps the lower <= point <= upper invariant.
    if b == 0 {
        return BootstrapCi {
            point,
            lower: point,
            upper: point,
            b,
            seed,
        };
    }

    let n = deltas.len() as u64;
    let mut rng = SplitMix64::new(seed);

    let mut boot_medians: Vec<f64> = (0..b)
        .map(|_| {
            let mut resample: Vec<f64> =
                (0..n).map(|_| deltas[rng.next_below(n) as usize]).collect();
            resample.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));
            median_sorted(&resample)
        })
        .collect();

    boot_medians.sort_by(|a, c| a.partial_cmp(c).unwrap_or(std::cmp::Ordering::Equal));

    // Nearest-rank percentile (1-based clamped).
    let lower_idx = ((alpha / 2.0 * b as f64).ceil() as usize)
        .saturating_sub(1)
        .min(b - 1);
    let upper_idx = (((1.0 - alpha / 2.0) * b as f64).ceil() as usize)
        .saturating_sub(1)
        .min(b - 1);

    BootstrapCi {
        point,
        lower: boot_medians[lower_idx],
        upper: boot_medians[upper_idx],
        b,
        seed,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Step 1/2: normal_cdf + average_ranks + PRNG

    #[test]
    fn normal_cdf_known_points() {
        let approx = |z, want: f64| {
            assert!(
                (normal_cdf(z) - want).abs() < 1e-4,
                "Φ({z})={} want {want}",
                normal_cdf(z)
            )
        };
        approx(0.0, 0.5);
        approx(1.0, 0.841_345);
        approx(1.96, 0.975_002);
        approx(-1.96, 0.024_998);
        approx(2.575_829, 0.995);
    }

    #[test]
    fn average_ranks_handles_ties() {
        assert_eq!(
            average_ranks(&[1.0, 1.0, 2.0, 2.0, 3.0]),
            vec![1.5, 1.5, 3.5, 3.5, 5.0]
        );
        assert_eq!(average_ranks(&[10.0]), vec![1.0]);
    }

    #[test]
    fn splitmix64_is_deterministic() {
        let mut a = SplitMix64::new(0xDEAD_BEEF);
        let mut b = SplitMix64::new(0xDEAD_BEEF);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(seq_a, seq_b, "same seed → same stream");
        assert!(
            seq_a.windows(2).any(|w| w[0] != w[1]),
            "stream is not constant"
        );
        let mut r = SplitMix64::new(1);
        for _ in 0..1000 {
            assert!(r.next_below(7) < 7);
        }
    }

    // Step 3/4: Wilcoxon exact

    #[test]
    fn wilcoxon_exact_all_positive_n5() {
        let r = wilcoxon_signed_rank(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r.n_nonzero, 5);
        assert_eq!(r.w_plus, 15.0);
        assert_eq!(r.w_minus, 0.0);
        assert_eq!(r.w, 0.0);
        assert_eq!(r.method, WilcoxonMethod::ExactPratt);
        assert!(
            (r.p_two_sided - 0.0625).abs() < 1e-12,
            "p={}",
            r.p_two_sided
        );
    }

    #[test]
    fn wilcoxon_exact_mixed_n4() {
        let r = wilcoxon_signed_rank(&[1.0, 2.0, 3.0, -4.0]);
        assert_eq!(r.w_plus, 6.0);
        assert_eq!(r.w_minus, 4.0);
        assert_eq!(r.w, 4.0);
        assert!((r.p_two_sided - 0.875).abs() < 1e-12, "p={}", r.p_two_sided);
    }

    // Step 5/6: Pratt zeros + ties + degenerate + large-n approx

    #[test]
    fn wilcoxon_pratt_zeros_and_ties() {
        let r = wilcoxon_signed_rank(&[0.0, 0.0, 1.0, -1.0, 2.0, 2.0, -3.0]);
        assert_eq!(r.n_nonzero, 5);
        assert!((r.w_plus - 14.5).abs() < 1e-12, "w+={}", r.w_plus);
        assert!((r.w_minus - 10.5).abs() < 1e-12, "w-={}", r.w_minus);
        assert!((r.w - 10.5).abs() < 1e-12);
        // Issue #7 / Option C: tie+zero data with n_nonzero ≤ 25 now takes the EXACT path
        // (valid with ties), not the anti-conservative normal approximation.
        assert_eq!(r.method, WilcoxonMethod::ExactPratt);
        assert!(
            (r.p_two_sided - 0.8125).abs() < 1e-12,
            "exact randomization p={}",
            r.p_two_sided
        );
    }

    #[test]
    fn wilcoxon_all_zero_is_degenerate() {
        let r = wilcoxon_signed_rank(&[0.0, 0.0, 0.0]);
        assert_eq!(r.n_nonzero, 0);
        assert_eq!(r.w, 0.0);
        assert!((r.p_two_sided - 1.0).abs() < 1e-12, "no signal → p=1");
    }

    #[test]
    fn wilcoxon_normal_approx_large_n_all_ties() {
        let deltas = vec![1.0_f64; 30];
        let r = wilcoxon_signed_rank(&deltas);
        assert_eq!(r.method, WilcoxonMethod::NormalApproxPratt);
        assert!((r.w_plus - 465.0).abs() < 1e-9);
        let z = {
            let mut lo = 0.0;
            let mut hi = 10.0;
            for _ in 0..200 {
                let m = (lo + hi) / 2.0;
                if 2.0 * (1.0 - normal_cdf(m)) > r.p_two_sided {
                    lo = m;
                } else {
                    hi = m;
                }
            }
            (lo + hi) / 2.0
        };
        assert!((z - 5.4655).abs() < 0.01, "z≈{z}");
    }

    // Step 7: Mann-Whitney U

    #[test]
    fn mann_whitney_complete_separation() {
        let r = mann_whitney_u(&[1., 2., 3., 4.], &[5., 6., 7., 8.]);
        assert_eq!(r.u, 0.0);
        assert_eq!(r.u1, 16.0);
        assert_eq!(r.u2, 0.0);
    }

    #[test]
    fn mann_whitney_interleaved() {
        let r = mann_whitney_u(&[1., 3., 5.], &[2., 4., 6.]);
        assert_eq!(r.u, 3.0);
    }

    #[test]
    fn mann_whitney_with_ties() {
        let r = mann_whitney_u(&[1., 2.], &[2., 3.]);
        assert!((r.u - 0.5).abs() < 1e-12, "U={}", r.u);
    }

    // Step 8: Cohen's d_z

    #[test]
    fn cohens_dz_textbook() {
        assert!((cohens_dz(&[1., 2., 3., 4., 5.]) - 1.897_367).abs() < 1e-5);
    }

    #[test]
    fn cohens_dz_zero_variance_is_finite() {
        assert_eq!(cohens_dz(&[4., 4., 4., 4.]), 0.0);
        assert_eq!(cohens_dz(&[]), 0.0);
    }

    // Step 9: Bootstrap median CI

    #[test]
    fn bootstrap_ci_all_equal_is_point() {
        let c = bootstrap_median_ci(&[5., 5., 5., 5.], 2000, 42, 0.05);
        assert_eq!((c.lower, c.point, c.upper), (5.0, 5.0, 5.0));
    }

    #[test]
    fn bootstrap_ci_is_deterministic_and_bounded() {
        let d = [1., 2., 3., 4., 5., 6., 7., 8., 9., 10.];
        let a = bootstrap_median_ci(&d, 10_000, 7, 0.05);
        let b = bootstrap_median_ci(&d, 10_000, 7, 0.05);
        assert_eq!(
            (a.lower, a.point, a.upper),
            (b.lower, b.point, b.upper),
            "pinned seed → reproducible"
        );
        assert!(a.lower <= a.point && a.point <= a.upper);
        assert!(
            a.lower >= 1.0 && a.upper <= 10.0,
            "resampled medians stay within data range"
        );
    }

    #[test]
    fn bootstrap_ci_empty_is_zero() {
        let c = bootstrap_median_ci(&[], 100, 1, 0.05);
        assert_eq!((c.lower, c.point, c.upper), (0.0, 0.0, 0.0));
    }

    #[test]
    fn bootstrap_ci_zero_reps_collapses_to_point() {
        // b == 0 must not underflow `b - 1`; the interval collapses to the
        // point estimate (median) with lower == point == upper.
        let c = bootstrap_median_ci(&[1., 2., 3., 4.], 0, 9, 0.05);
        assert_eq!((c.lower, c.point, c.upper), (2.5, 2.5, 2.5));
    }
}
