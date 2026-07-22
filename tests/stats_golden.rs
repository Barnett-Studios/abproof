//! Scipy-oracle golden tests for the paired Wilcoxon signed-rank p-value (issue #7).
//!
//! Each `expected` p-value is produced by an INDEPENDENT oracle —
//! `scipy.stats.wilcoxon(zero_method='pratt', correction=True, mode='approx')` for the
//! normal-approximation path, and exact 2ⁿ sign-flip enumeration for the exact path
//! (scipy 1.18.0; generator committed at `tests/golden/oracle.py`). They are NOT derived
//! from abproof's own output, so they test *correctness*, not self-consistency.
//!
//! The `// RED` cases FAIL against the pre-fix code (which tested Pratt-ranked W+ against
//! zero-free null moments) and pass only once the moments are corrected to the sign-flip
//! randomization moments μ = Σr/2, σ² = Σr²/4. The trigger for the bug is zeros present
//! (Pratt rank-shift) AND ties among the non-zero |δ| (which forces the broken approximation
//! path) — the ubiquitous real-data case for per-node pass-rate deltas.

use abproof::stats::wilcoxon_signed_rank;

/// `(label, deltas, scipy_expected_p, is_red_against_buggy_code)`.
const GOLDEN: &[(&str, &[f64], f64, bool)] = &[
    // ── RED: zeros + ties → broken approximation path; buggy code is wrong ──────────
    (
        "audit_counterexample",
        &[0., 0., 1., -1., 2., 2., -3.],
        0.795_511,
        true,
    ),
    (
        "thirty_zeros_ten_nonzero",
        &[
            0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0., 0.,
            0., 0., 0., 0., 0., 0., 0., 0., 1., -1., 2., 2., -1., 3., 1., -2., 2., 1.,
        ],
        0.191_567,
        true,
    ),
    (
        "n6_zeros_ties_all_pos",
        &[0., 0., 1., 1., 2., 2., 3., 3.],
        0.022_785,
        true,
    ),
    (
        "n6_zeros_ties_mixed",
        &[0., 0., -1., 1., 2., 2., 3., -3.],
        0.476_732,
        true,
    ),
    // The bug flips the verdict the OTHER way here: correct p=0.048 (< 0.05, significant),
    // buggy p=0.146 (not significant) — the buggy code MISSES a real regression.
    (
        "zeros_ties_gate_flip",
        &[0., 0., 0., -1., -1., -2., -2., -3., 1., -3.],
        0.047_604,
        true,
    ),
    // ── GREEN guards: exact path & no-zero tie path are already correct; keep them so ──
    (
        "exact_all_positive_n5",
        &[1., 2., 3., 4., 5.],
        0.062_500,
        false,
    ),
    ("exact_mixed_n4", &[1., 2., 3., -4.], 0.875_000, false),
    // Zeros but DISTINCT non-zero magnitudes → exact path, which handles Pratt correctly.
    (
        "exact_with_zeros_distinct",
        &[0., 0., 1., 2., 3., 4., 5., 6.],
        0.031_250,
        false,
    ),
    // Ties but NO zeros → the pre-existing tie correction was already exact here.
    ("no_zero_all_ties_n4", &[2., 2., 2., 2.], 0.071_861, false),
    // ── Boundaries ────────────────────────────────────────────────────────────────
    ("single_nonzero", &[5.], 1.000_000, false),
    ("all_zero", &[0., 0., 0.], 1.000_000, false),
    ("empty", &[], 1.000_000, false),
];

#[test]
fn matches_scipy_oracle() {
    // Tolerance covers the Abramowitz–Stegun erf approximation (|err| < 1.5e-7) used by
    // the crate vs. scipy's exact erf; every RED case differs from the buggy value by
    // > 0.02, far outside this band.
    const TOL: f64 = 1.5e-3;
    let mut failures = Vec::new();
    for (label, deltas, expected, _red) in GOLDEN {
        let got = wilcoxon_signed_rank(deltas).p_two_sided;
        if (got - expected).abs() > TOL {
            failures.push(format!("{label}: got {got:.6}, want {expected:.6} (scipy)"));
        }
    }
    assert!(
        failures.is_empty(),
        "scipy-oracle mismatches:\n{}",
        failures.join("\n")
    );
}

// ── Property / fuzz invariants (deterministic; no external oracle) ──────────────────

/// Independent brute-force exact two-sided signed-rank p (sign-flip randomization over the
/// observed Pratt ranks). Valid with ties; O(2ⁿ) — callers keep n small.
fn brute_exact_p(deltas: &[f64]) -> f64 {
    // Pratt average ranks over |δ| including zeros, then keep non-zero.
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
    }
}

#[test]
fn exact_path_equals_independent_enumeration() {
    // On the exact path (no ties among non-zero |δ|, small n), the crate must equal an
    // independent brute-force enumeration — a scipy-free correctness anchor.
    let cases: &[&[f64]] = &[
        &[1., 2., 3., 4., 5.],
        &[1., 2., 3., -4.],
        &[0., 0., 1., 2., 3., 4., 5., 6.],
        &[-1., 2., -3., 4., -5.],
        &[0., 1., -2., 3.],
    ];
    for d in cases {
        let got = wilcoxon_signed_rank(d).p_two_sided;
        let want = brute_exact_p(d);
        assert!(
            (got - want).abs() < 1e-9,
            "exact path {d:?}: got {got}, enum {want}"
        );
    }
}
