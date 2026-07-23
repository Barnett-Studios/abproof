#!/usr/bin/env python3
"""Proof-grade verification for abproof#7 (paired Wilcoxon signed-rank correctness).

Models the ACTUAL shipped dispatch of `src/stats.rs::wilcoxon_signed_rank` in both the pre-fix
and post-fix code, so the quantitative claims represent deployed behaviour (per adversarial
review). Run: `python3 docs/plans/proof-abproof-7.py` (needs scipy).

Dispatch:
  OLD shipped: use_exact = n_nonzero <= 25 AND no ties among |delta|  -> exact; else BUGGY approx
               (mu=n(n+1)/4, tie-corrected zero-free variance).
  NEW shipped (Option C): use_exact = n_nonzero <= 25 (ties allowed)  -> exact;
               else CORRECTED approx (mu=Sum r/2, sigma^2=Sum r^2/4).
The exact path is identical in both (byte-for-byte in Rust) and is the ground-truth randomization
p-value. Oracles: scipy (approx path) and independent 2^n enumeration (exact path).
"""
import numpy as np
from scipy.stats import wilcoxon, rankdata, norm, binomtest
from itertools import product
rng = np.random.default_rng(2026)

def ranks_pratt(d):
    d = np.array(d, float); return rankdata(np.abs(d))[d != 0], d[d != 0]

def has_ties(d):
    a = sorted(abs(x) for x in d if x != 0); return len(a) != len(set(a))

def corrected(d):  # NEW approx (large-n path)
    r, nz = ranks_pratt(d); n = len(r)
    if n == 0: return 1.0
    wp = r[nz > 0].sum(); mu = r.sum()/2; s = ((r**2).sum()/4)**0.5
    if s == 0: return 1.0
    z = max(abs(wp-mu)-0.5, 0)/s; return float(2*(1-norm.cdf(z)))

def old_approx(d):  # OLD buggy approx
    r, nz = ranks_pratt(d); n = len(r)
    if n == 0: return 1.0
    wp = r[nz > 0].sum(); mu = n*(n+1)/4
    a = np.sort(np.abs(nz)); tie = 0; i = 0
    while i < len(a):
        j = i+1
        while j < len(a) and a[j] == a[i]: j += 1
        t = j-i; tie += t**3-t; i = j
    s = max(n*(n+1)*(2*n+1)/24 - tie/48, 0)**0.5
    if s == 0: return 1.0
    z = max(abs(wp-mu)-0.5, 0)/s; return float(2*(1-norm.cdf(z)))

def exact(d):  # ground-truth randomization p (independent of scipy)
    r, nz = ranks_pratt(d); n = len(r)
    if n == 0: return 1.0
    wp = r[nz > 0].sum(); m = r.sum()/2; o = abs(wp-m); c = 0
    for s in product([0, 1], repeat=n):
        if abs(sum(rr for rr, b in zip(r, s) if b)-m) >= o-1e-12: c += 1
    return c/2**n

def old_shipped(d):
    nz = [x for x in d if x != 0]
    return exact(d) if (len(nz) <= 25 and not has_ties(d)) else old_approx(d)

def new_shipped(d):  # Option C
    nz = [x for x in d if x != 0]
    return exact(d) if len(nz) <= 25 else corrected(d)

def sign(d):
    d = np.array(d, float); d = d[d != 0]; n = len(d)
    return 1.0 if n == 0 else binomtest(int((d > 0).sum()), n, 0.5).pvalue

# PROOF 1 -- corrected moments == scipy Pratt (a PORT-FIDELITY check, not independent proof).
worst = 0.0
for _ in range(20000):
    n = rng.integers(2, 40); d = rng.integers(-4, 5, size=n)
    if not np.any(d != 0): continue
    try: sp = wilcoxon(d, zero_method='pratt', correction=True, mode='approx').pvalue
    except Exception: continue
    worst = max(worst, abs(sp-corrected(d)))
print(f"PROOF 1 (port fidelity)  max|corrected - scipy_pratt|, 20k cases = {worst:.2e}")

# PROOF 2 -- deployed dispatch vs exact ground truth (independent enumeration).
no = oo = 0.0; ncase = 0
for _ in range(6000):
    n = rng.integers(3, 14); d = rng.integers(-3, 4, size=n)
    if not np.any(d != 0): continue
    ex = exact(d); no += abs(new_shipped(d)-ex); oo += abs(old_shipped(d)-ex); ncase += 1
print(f"PROOF 2 (deployed dispatch vs exact, {ncase} cases):")
print(f"        mean|NEW_shipped - exact| = {no/ncase:.4f}  (Option C is exact for n<=25 -> 0)")
print(f"        mean|OLD_shipped - exact| = {oo/ncase:.4f}")

# PROOF 3 -- signed-rank degrades to the sign test on binary (all-magnitude-equal) data.
w3 = 0.0
for _ in range(5000):
    n = rng.integers(1, 20); d = rng.choice([-1, 0, 1], size=n)
    if not np.any(d != 0): continue
    w3 = max(w3, abs(exact(d)-sign(d)))   # NEW ships exact for n<=25, so this IS shipped behaviour
print(f"PROOF 3  max|shipped exact signed-rank - exact sign test| on all-pm1 = {w3:.2e}")

# PROOF 4 -- gate-decision agreement with exact truth @alpha=0.05, deployed dispatch.
of = nf = tot = 0; anti = cons = 0
for _ in range(20000):
    k = rng.integers(4, 12); reps = rng.choice([5, 10, 30])
    d = np.round(rng.integers(-reps, reps+1, size=k)/reps, 6)
    if not np.any(d != 0): continue
    pe = exact(d); tot += 1
    if (old_shipped(d) < 0.05) != (pe < 0.05):
        of += 1
        if old_shipped(d) < 0.05: anti += 1      # old says sig, truth says not (false regression)
        else: cons += 1                          # old says not, truth says sig (missed regression)
    if (new_shipped(d) < 0.05) != (pe < 0.05): nf += 1
print(f"PROOF 4  gate-decision disagreements vs exact truth @a=0.05, {tot} realistic cases:")
print(f"        OLD shipped: {of} ({100*of/tot:.1f}%)  [false-regression={anti}, missed-regression={cons}]")
print(f"        NEW shipped: {nf} ({100*nf/tot:.1f}%)  (0 expected: n<=25 -> exact truth)")
