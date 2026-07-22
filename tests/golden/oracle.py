#!/usr/bin/env python3
"""Independent oracle for the paired Wilcoxon signed-rank goldens in `stats_golden.rs`.

Produces the expected two-sided p-values with scipy (the approximation path) and exact
2^n sign-flip enumeration (the exact path). Run to regenerate/verify the literals in the
Rust test; CI does not depend on Python (the values are pinned as literals).

    python3 -m pip install scipy && python3 tests/golden/oracle.py
"""
import numpy as np
from scipy.stats import wilcoxon, rankdata
from itertools import product


def scipy_pratt(deltas):
    """Normal-approximation path oracle: scipy Wilcoxon, Pratt zeros, continuity-corrected."""
    if all(d == 0 for d in deltas):
        return 1.0
    return float(wilcoxon(deltas, zero_method="pratt", correction=True, mode="approx").pvalue)


def exact_enum(deltas):
    """Exact path oracle: sign-flip randomization over the observed Pratt ranks (ties ok)."""
    d = np.array(deltas, float)
    r = rankdata(np.abs(d))[d != 0]
    n = len(r)
    if n == 0:
        return 1.0
    w_plus = r[d[d != 0] > 0].sum()
    mean = r.sum() / 2.0
    obs = abs(w_plus - mean)
    count = sum(
        1
        for signs in product([0, 1], repeat=n)
        if abs(sum(rr for rr, b in zip(r, signs) if b) - mean) >= obs - 1e-12
    )
    return count / 2 ** n


def has_ties_nonzero(deltas):
    a = sorted(abs(x) for x in deltas if x != 0)
    return len(a) != len(set(a))


def expected(deltas):
    """What `wilcoxon_signed_rank` should return: exact when reachable, else corrected approx."""
    nz = [x for x in deltas if x != 0]
    use_exact = len(nz) <= 25 and not has_ties_nonzero(deltas)
    return exact_enum(deltas) if use_exact else scipy_pratt(deltas)


GOLDEN = {
    "audit_counterexample": [0, 0, 1, -1, 2, 2, -3],
    "thirty_zeros_ten_nonzero": [0] * 30 + [1, -1, 2, 2, -1, 3, 1, -2, 2, 1],
    "n6_zeros_ties_all_pos": [0, 0, 1, 1, 2, 2, 3, 3],
    "n6_zeros_ties_mixed": [0, 0, -1, 1, 2, 2, 3, -3],
    "zeros_ties_gate_flip": [0, 0, 0, -1, -1, -2, -2, -3, 1, -3],
    "exact_all_positive_n5": [1, 2, 3, 4, 5],
    "exact_mixed_n4": [1, 2, 3, -4],
    "exact_with_zeros_distinct": [0, 0, 1, 2, 3, 4, 5, 6],
    "no_zero_all_ties_n4": [2, 2, 2, 2],
    "single_nonzero": [5],
    "all_zero": [0, 0, 0],
    "empty": [],
}

if __name__ == "__main__":
    for name, d in GOLDEN.items():
        print(f"{name:28} {expected(d):.6f}")
