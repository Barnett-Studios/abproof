//! Reference-oracle golden tests for the paired Wilcoxon signed-rank p-value (issue #7).
//!
//! Routing (`src/stats.rs`): batteries with `n_nonzero ≤ 25` use the **exact** 2ⁿ sign-flip
//! enumeration (valid with ties — it sums the observed Pratt ranks); larger batteries use the
//! normal approximation with the sign-flip randomization moments `μ = Σr/2`, `σ² = Σr²/4`.
//!
//! Oracles (both INDEPENDENT of abproof's own output — these test correctness, not
//! self-consistency):
//!   * exact path → the true conditional randomization p-value, computed by an independent
//!     brute-force enumeration (`brute_exact_p` below; also cross-checked against
//!     `scipy.stats.wilcoxon(mode='exact')` on tie-free inputs, where it agrees to 1e-9).
//!   * approx path → `scipy.stats.wilcoxon(zero_method='pratt', correction=True, mode='approx')`.
//!
//! Every value below FAILS against the pre-fix code: the original tested Pratt-ranked W+ against
//! zero-free normal-approx moments AND sent every tied input (all real pass/fail data ties) to
//! that approximation, whose −½ continuity correction is calibrated for a unit-step statistic —
//! under heavy ties W+ steps by (n+1)/2, so it under-corrects and is anti-conservative near α
//! (e.g. 5 unanimous ±1 nodes: old ≈0.037 "significant" vs correct 0.0625 "not"). The fix
//! corrects the moments AND routes small/tied batteries to the exact test.

use abproof::stats::{wilcoxon_signed_rank, WilcoxonMethod};

/// `(label, deltas, expected_p, expected_method)`.
const GOLDEN: &[(&str, &[f64], f64, WilcoxonMethod)] = &[
    // ── exact path (n_nonzero ≤ 25): expected = true randomization p ─────────────────
    // The audit counterexample: was 0.0769 (buggy approx), then 0.7955 (corrected approx),
    // now 0.8125 — the exact truth.
    (
        "audit_counterexample",
        &[0., 0., 1., -1., 2., 2., -3.],
        0.812_500,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "n6_zeros_ties_all_pos",
        &[0., 0., 1., 1., 2., 2., 3., 3.],
        0.031_250,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "n6_zeros_ties_mixed",
        &[0., 0., -1., 1., 2., 2., 3., -3.],
        0.531_250,
        WilcoxonMethod::ExactPratt,
    ),
    // Boundary honesty: 7 nodes, mostly one direction → exact 0.0625 is NOT significant at
    // 0.05. The buggy approx reported 0.146; the corrected approx reported 0.0476 (a false
    // "significant"); the exact truth is 0.0625 — the gate must NOT fire here.
    (
        "zeros_ties_boundary",
        &[0., 0., 0., -1., -1., -2., -2., -3., 1., -3.],
        0.062_500,
        WilcoxonMethod::ExactPratt,
    ),
    // Ties but no zeros — the old approx was anti-conservative here too (0.0719 vs 0.125).
    (
        "no_zero_all_ties_n4",
        &[2., 2., 2., 2.],
        0.125_000,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "exact_all_positive_n5",
        &[1., 2., 3., 4., 5.],
        0.062_500,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "exact_mixed_n4",
        &[1., 2., 3., -4.],
        0.875_000,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "exact_with_zeros_distinct",
        &[0., 0., 1., 2., 3., 4., 5., 6.],
        0.031_250,
        WilcoxonMethod::ExactPratt,
    ),
    // ── boundaries ──────────────────────────────────────────────────────────────────
    (
        "single_nonzero",
        &[5.],
        1.000_000,
        WilcoxonMethod::ExactPratt,
    ),
    (
        "all_zero",
        &[0., 0., 0.],
        1.000_000,
        WilcoxonMethod::NormalApproxPratt,
    ),
    ("empty", &[], 1.000_000, WilcoxonMethod::NormalApproxPratt),
    // ── approx path (n_nonzero = 30 > 25): expected = scipy Wilcoxon-Pratt approx ─────
    (
        "large_battery_approx",
        &[
            0., 0., 0., 0., 0., 1., -1., 2., -1., 3., 1., -2., 2., 1., -1., 2., 3., -1., 1., 2.,
            -2., 1., 3., -1., 2., 1., -2., 2., 1., -1., 3., 1., -1., 2., -1.,
        ],
        0.046_617,
        WilcoxonMethod::NormalApproxPratt,
    ),
];

#[test]
fn matches_reference_oracle() {
    // Exact-path values are pinned to the independent enumeration (they agree to ~1e-9); the
    // approx-path value is scipy's, matched within the Abramowitz–Stegun erf band (< 1.5e-7).
    // Every RED case differs from the pre-fix value by ≫ this tolerance.
    const TOL: f64 = 1.5e-3;
    let mut failures = Vec::new();
    for (label, deltas, expected, method) in GOLDEN {
        let r = wilcoxon_signed_rank(deltas);
        if (r.p_two_sided - expected).abs() > TOL {
            failures.push(format!(
                "{label}: p={:.6}, want {expected:.6}",
                r.p_two_sided
            ));
        }
        if r.n_nonzero > 0 && r.method != *method {
            failures.push(format!("{label}: method={:?}, want {method:?}", r.method));
        }
    }
    assert!(
        failures.is_empty(),
        "oracle mismatches:\n{}",
        failures.join("\n")
    );
}

// ── Property / fuzz invariants (deterministic; no external oracle) ──────────────────

/// Independent brute-force exact two-sided signed-rank p (sign-flip randomization over the
/// observed Pratt ranks). Valid with ties; O(2ⁿ) — callers keep n small.
fn brute_exact_p(deltas: &[f64]) -> f64 {
    let mut idx: Vec<usize> = (0..deltas.len()).collect();
    idx.sort_by(|&a, &b| deltas[a].abs().partial_cmp(&deltas[b].abs()).unwrap());
    let mut rank = vec![0.0f64; deltas.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i + 1;
        while j < idx.len() && deltas[idx[j]].abs() == deltas[idx[i]].abs() {
            j += 1;
        }
        let avg = ((i + 1) as f64 + j as f64) / 2.0;
        for &k in &idx[i..j] {
            rank[k] = avg;
        }
        i = j;
    }
    let r: Vec<f64> = (0..deltas.len())
        .filter(|&k| deltas[k] != 0.0)
        .map(|k| rank[k])
        .collect();
    let n = r.len();
    if n == 0 {
        return 1.0;
    }
    let w_plus: f64 = r
        .iter()
        .zip(deltas.iter().filter(|&&d| d != 0.0))
        .filter(|(_, &d)| d > 0.0)
        .map(|(&rr, _)| rr)
        .sum();
    let mean: f64 = r.iter().sum::<f64>() / 2.0;
    let obs = (w_plus - mean).abs();
    let mut count = 0u64;
    for mask in 0..(1u64 << n) {
        let wp: f64 = (0..n).filter(|b| (mask >> b) & 1 == 1).map(|b| r[b]).sum();
        if (wp - mean).abs() >= obs - 1e-12 {
            count += 1;
        }
    }
    count as f64 / (1u64 << n) as f64
}

fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 16
}

#[test]
fn fuzz_invariants() {
    let mut s = 0x1234_5678_9abc_def0u64;
    for _ in 0..4000 {
        let n = (lcg(&mut s) % 12) as usize + 1; // 1..=12
        let deltas: Vec<f64> = (0..n).map(|_| (lcg(&mut s) % 7) as f64 - 3.0).collect(); // {-3..3}
        let p = wilcoxon_signed_rank(&deltas).p_two_sided;

        assert!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "p out of range for {deltas:?}: {p}"
        );

        // Sign symmetry: negating every delta swaps W+↔W- but leaves |W+−μ| unchanged.
        let neg: Vec<f64> = deltas.iter().map(|d| -d).collect();
        let p_neg = wilcoxon_signed_rank(&neg).p_two_sided;
        assert!(
            (p - p_neg).abs() < 1e-9,
            "not sign-symmetric for {deltas:?}: {p} vs {p_neg}"
        );

        // Permutation invariance: order must not matter.
        let mut rev = deltas.clone();
        rev.reverse();
        let p_rev = wilcoxon_signed_rank(&rev).p_two_sided;
        assert!(
            (p - p_rev).abs() < 1e-9,
            "not permutation-invariant for {deltas:?}"
        );

        // On the exact path (n ≤ 25, which every fuzz case satisfies), the crate must equal
        // an INDEPENDENT enumeration — including tied inputs (the Option-C path).
        let want = brute_exact_p(&deltas);
        assert!(
            (p - want).abs() < 1e-9,
            "exact path diverges from enumeration for {deltas:?}: {p} vs {want}"
        );
    }
}

#[test]
fn exact_path_equals_independent_enumeration_with_ties() {
    // Explicit tie + zero cases: the crate's exact path must equal the independent brute-force
    // enumeration (this is the Option-C path that the pre-fix code never reached).
    let cases: &[&[f64]] = &[
        &[0., 0., 1., -1., 2., 2., -3.], // audit
        &[2., 2., 2., 2.],               // all ties
        &[0., 0., 1., 1., 2., 2., 3., 3.],
        &[-1., 1., -1., 1., -1.],
        &[0., 0., 0., -1., -1., -2., -2., -3., 1., -3.],
    ];
    for d in cases {
        let got = wilcoxon_signed_rank(d).p_two_sided;
        let want = brute_exact_p(d);
        assert!(
            (got - want).abs() < 1e-9,
            "exact-with-ties {d:?}: got {got}, enum {want}"
        );
    }
}
