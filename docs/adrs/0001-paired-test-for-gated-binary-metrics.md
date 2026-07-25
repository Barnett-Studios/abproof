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

**Keep the paired Wilcoxon signed-rank test. Compute it exactly for small batteries, and fix
the large-battery approximation's null moments.**

1. **Exact path for `n_nonzero ≤ 25` — including ties (Option C, adopted).** The exact 2ⁿ
   sign-flip enumeration is a valid randomization test *with* ties (it sums the observed Pratt
   ranks), so it is routed for every battery with ≤ 25 gradable nodes — which is the regime
   abproof runs in practice (the clean-kata corpus is 24 nodes). This yields the *true*
   conditional p-value, not an approximation.

2. **Corrected normal approximation for `n_nonzero > 25`.** Replace the zero-free
   normal-approximation moments with the **sign-flip randomization moments**, exact for Pratt
   ranks *including ties*:

   ```
   W+ = Σ rᵢ · Bᵢ ,  Bᵢ ~ Bernoulli(½) i.i.d.
   ⇒  μ = E[W+]   = Σ rᵢ / 2
      σ² = Var[W+] = Σ rᵢ² / 4
   ```

   (`rᵢ` = the Pratt rank of the `i`-th non-zero delta.) Two-sided p with continuity
   correction: `z = (|W+ − μ| − ½)/σ`, `p = 2(1 − Φ(z))`. The separate tie-correction term is
   **deleted** — `Σrᵢ²/4` already accounts for ties and the zero-induced rank shift.

**Why route small/tied batteries to exact, not the corrected approx?** The normal
approximation's −½ continuity correction is calibrated for a *unit-step* statistic. Under heavy
ties (the pass/fail-delta regime) W+ steps by `(n+1)/2`, so the correction under-corrects and
the approximation is **anti-conservative near α**. Concretely, 5 unanimous ±1 nodes give
corrected-approx `p ≈ 0.037` ("significant") versus exact `p = 0.0625` ("not significant") —
a false gate failure at exactly the honesty floor CONTRACT.md promises (`n = 6` is the true
minimum node count for a unanimous regression to clear α = 0.05). The exact path removes this
error entirely for `n ≤ 25`; for `n > 25` the sample is large enough that the residual
continuity bias no longer straddles α for realistic effects.

## Alternatives considered

### (A) Switch to an exact McNemar / sign test on discordant pairs — REJECTED

The issue text recommends this ("the textbook test for paired binary outcomes"). We **rejected
it on evidence**:

1. **It is the wrong test for the post-D2 data.** McNemar/sign discards magnitude and needs a
   single binary outcome per unit. After D2 the per-node unit is a *rate*, not a bit — a node
   that goes 0.9→0.5 and one that goes 0.9→0.85 are both "one negative discordant pair" to a
   sign test, yet they are very different regressions. Signed-rank uses that magnitude. On the
   audit vector `[0,0,1,-1,2,2,-3]` the sign test gives `p = 1.0` where the correct answer
   (exact signed-rank) is `0.8125`.
2. **A rigorous clustered/GEE McNemar collapses into D2.** The only statistically defensible way
   to apply McNemar to `reps`-replicated binary data (avoiding the pseudo-replication of a
   per-rep 2×2 table, D2) is a cluster-level test on each node's net discordance rate
   `dᵢ = (cᵢ − bᵢ)/repsᵢ`. But for binary pass/fail data `dᵢ ≡ treatment_rateᵢ − baseline_rateᵢ`
   *exactly* (verified to 1e-16) — i.e. the node-rate delta this ADR already tests. A correct
   McNemar therefore re-derives the node as the unit of replication and lands on a
   magnitude-bearing statistic; it does not rescue anything distinctively sign-based.
3. **It is a larger contract change:** CONTRACT.md already names "paired Wilcoxon signed-rank".
   Keeping it (correcting the implementation) is a bug fix; McNemar replaces the named test.

(Note: an earlier draft cited "does not match the issue's golden targets" as a separate reason;
that is circular — the targets were authored assuming Wilcoxon, so it merely restates reason 1
and is folded into it.)

### (B) Correct the Pratt moments + (C) exact-for-ties — BOTH ADOPTED (see Decision).

Option C (route small/tied batteries to the exact enumeration) was initially deferred on the
mistaken grounds that the corrected approximation was "within ~0.02 of enumeration, far from any
α boundary". That is false: 5 unanimous ±1 nodes give approx 0.037 vs exact 0.0625 — a
decision-flipping divergence straddling α = 0.05. Adversarial review surfaced the
counterexample, so C is adopted, not deferred.

## Assumptions

The signed-rank null requires the per-node deltas to be **symmetric about 0 under H₀**. abproof's
design satisfies this *by construction*: both arms run the same node at the same seeds
(exchangeable arms), so under a true null (identical arm behaviour) the per-node rate delta is
symmetric about 0 regardless of each arm's marginal skew — verified empirically (skew ≈ 0.002
even at a ceiling rate p = 0.98). **This rests on arm exchangeability conditional on node.** If a
future infra change breaks it — shared quota, ordering effects, or caching between the baseline
and treatment runs of a pair — the calibration degrades (a sign test would be more robust there).
Preserve arm exchangeability, or revisit the test choice if it is lost.

## Known limitation (monitored, not blocking)

Signed-rank weights by magnitude, so a small number of high-binomial-variance nodes (pass rate
near 0.5, `Var = p(1−p)/reps`) can, by chance, produce large non-informative rate swings that
outrank and mask a real but small, consistent regression across many low-variance nodes. In
simulation this is a *wash* versus a sign test (Wilcoxon wins ≈11.5% / loses ≈7.9% of such
batteries), not a reason to switch — but it is a real trade-off. Surfacing per-node binomial-CI
width alongside the p-value is a sensible future diagnostic.

## Falsifier

The decision is **wrong** if abproof's p-value diverges from its independent oracle: the exact
2ⁿ enumeration for `n ≤ 25` (cross-checked against `scipy.stats.wilcoxon(mode='exact')` on
tie-free inputs), and `scipy.stats.wilcoxon(zero_method='pratt', correction=True, mode='approx')`
for `n > 25`. Pre-registered checks (run): exact path matches independent enumeration to 1e-9
including ties; approx moments match scipy to **4.44e-16** over 20 000 tie/zero cases; and the
exact path removes the n=5 α-boundary false-positive. Any golden reproducing a divergence
falsifies the decision.

## Consequences

- CONTRACT.md keeps "Wilcoxon signed-rank"; correctness is now oracle-backed, and the small-n
  path is *exact* (the honesty floor "n = 6 minimum for a unanimous regression" holds precisely).
- p-values rise vs the buggy code (de-anti-conservative): some runs previously flagged as
  significant regressions no longer are — the intended direction (fewer false regressions).
- No change to the public API of `stats` (the `approx_wilcoxon_p` signature is private); the
  `WilcoxonMethod` reported on the result row now reads `ExactPratt` for `n ≤ 25`.
