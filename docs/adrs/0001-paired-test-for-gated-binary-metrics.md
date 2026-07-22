---
title: The paired test for the gated metric
issue: Barnett-Studios/abproof#7
status: Proposed
created: 2026-07-22T07:19:41Z
updated: 2026-07-22T07:19:41Z
---

# ADR 0001 — The paired test for the gated metric

## Context

The gate is significance-based: a worse point estimate fails only when a paired test on the
gated metric's within-pair deltas clears `alpha` (CONTRACT.md). The shipped test
(`stats::wilcoxon_signed_rank` + `approx_wilcoxon_p`) is wrong (Spec 0001, D1). We must decide
**which test the gate uses**, because the choice touches abproof's public CONTRACT (the
statistical method is part of the stable surface).

After D2 (pseudo-replication) is fixed, the per-node observation is a **pass rate** — the mean
of `reps` binary outcomes, a value in `[0,1]` (multiples of `1/reps`), *not* a single binary
outcome. The per-node delta `treatment_rate − baseline_rate ∈ [−1,1]` therefore carries
**magnitude**, not just sign.

## Decision

**Keep the paired Wilcoxon signed-rank test; fix its null moments.** Replace the zero-free
normal-approximation moments with the **sign-flip randomization moments**, which are exact for
Pratt ranks *including ties*:

```
W+ = Σ rᵢ · Bᵢ ,  Bᵢ ~ Bernoulli(½) i.i.d.
⇒  μ = E[W+]   = Σ rᵢ / 2
   σ² = Var[W+] = Σ rᵢ² / 4
```

(`rᵢ` = the Pratt rank of the `i`-th non-zero delta.) Two-sided p with continuity correction:
`z = (|W+ − μ| − ½)/σ`, `p = 2(1 − Φ(z))`. The separate tie-correction term is **deleted** —
`Σrᵢ²/4` already accounts for ties and the zero-induced rank shift. The exact enumeration path
(already correct) is retained for small `n` with no ties.

## Alternatives considered

### (A) Switch to an exact McNemar / sign test on discordant pairs — REJECTED

The issue text recommends this ("the textbook test for paired binary outcomes"). We **rejected
it on evidence**:

1. **It is the wrong test for the post-D2 data.** McNemar/sign discards magnitude and needs a
   single binary outcome per unit. After D2 the per-node unit is a *rate*, not a bit — a node
   that goes 0.9→0.5 and one that goes 0.9→0.85 are both "one negative discordant pair" to a
   sign test, yet they are very different regressions. Signed-rank uses that magnitude.
2. **It does not match the required golden targets.** The issue's own acceptance numbers are
   Wilcoxon values, not sign-test values. Measured with scipy 1.18.0:

   | deltas | corrected Wilcoxon (scipy pratt) | McNemar / sign test |
   |---|---|---|
   | `[0,0,1,-1,2,2,-3]` | **0.7955** ✓ (issue: "≈0.80") | 1.0000 |
   | `[1,2,3,4,5,-6]` | 0.4017 | 0.2188 |

   The issue asks `[0,0,1,-1,2,2,-3] → ≈0.80`; only the Wilcoxon branch produces it. A sign
   test gives 1.0. Adopting (A) would make abproof fail its own stated acceptance.
3. **It is a larger contract change** than (B): CONTRACT.md already names "paired Wilcoxon
   signed-rank". (B) is a bug fix to the named test; (A) replaces the named test.

### (B, chosen) Correct the Pratt moments — see Decision.

### (C) Make the exact path reachable with ties too — DEFERRED

The exact enumeration is a valid randomization test *with* ties (it sums the observed ranks),
so dropping the `!has_ties` guard would give exact p-values on small batteries
(e.g. `[0,0,1,-1,2,2,-3] → 0.8125`). Rejected for now because it *diverges from the scipy
oracle* the acceptance is written against (scipy uses the normal approx for ties → 0.7955), and
the corrected approximation is already exact-in-moments and within ~0.02 of the enumeration far
from any `alpha` boundary. Left as a `ponytail:` note; revisit if a borderline case near
`alpha` is ever observed.

## Falsifier

The decision is **wrong** if, on a matrix of paired inputs with ties and zeros, abproof's
corrected p-value diverges from `scipy.stats.wilcoxon(zero_method='pratt', correction=True,
mode='approx')` by more than floating-point tolerance. Pre-registered check (already run,
3000 random tie/zero-heavy cases): **max |abproof − scipy| = 0.0** to machine precision. Any
golden that reproduces a divergence falsifies (B) and reopens (A)/(C).

## Consequences

- CONTRACT.md keeps "Wilcoxon signed-rank" but its correctness is now oracle-backed.
- p-values rise (the fix is de-anti-conservative): some runs previously flagged as significant
  regressions no longer are. This is the intended direction — fewer false regressions.
- No change to the public API of `stats` (the `approx_wilcoxon_p` signature is private).
