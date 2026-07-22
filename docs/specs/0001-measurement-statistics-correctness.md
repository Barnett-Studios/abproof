---
title: Measurement statistics correctness
issue: Barnett-Studios/abproof#7
status: Draft
created: 2026-07-22T07:19:41Z
updated: 2026-07-22T07:19:41Z
---

# Spec 0001 — Measurement statistics correctness

## Problem

abproof's gate claims to be *significance-based*: a worse observed value only fails a run
when a paired test on the gated metric's within-pair deltas also clears `alpha` (CONTRACT.md).
The paired test is currently invalid, so **every p-value abproof has produced on real
pass/fail data is wrong**, and the "stat-gated" guarantee is unfounded.

Three independent defects, verified against `src/stats.rs`, `src/run.rs`, `src/score.rs`
on the published crate (commit at branch point):

### D1 — Wilcoxon normal-approx uses the wrong null moments

`wilcoxon_signed_rank` (`stats.rs`) assigns **Pratt ranks** (zeros ranked, then dropped, so the
non-zero ranks are shifted up). The normal approximation `approx_wilcoxon_p` (`stats.rs:240`)
tests `W+` against the **zero-free** moments `mu = n(n+1)/4`, `sigma² = n(n+1)(2n+1)/24 − ties`,
with `n = n_nonzero`. Those moments are only correct when the ranks are exactly `1..n`
(no zeros, no ties) — not for Pratt ranks. The exact path (`exact_wilcoxon_p`) uses the correct
mean `sum(nonzero_ranks)/2` but is **unreachable on real data**: `use_exact` requires
`!has_ties` (`stats.rs:190`), and ±1/±2 pass/fail deltas always tie, so the broken
approximation always runs. The error is **anti-conservative** (falsely significant).

Verified numerically (scipy 1.18.0 is the independent oracle):

| deltas | shipped p | correct p (scipy Wilcoxon-Pratt) |
|---|---|---|
| `[0,0,1,-1,2,2,-3]` | 0.0769 | **0.7955** |

### D2 — Pseudo-replication

One `PairedRep` is pushed per `(node, rep)` (`run.rs:419`) and all reps feed **flat** into the
delta vector (`run.rs:499`). `reps` correlated observations of one node are treated as `reps`
independent observations, inflating effective `n` and shrinking the p-value. The experimental
unit is the **node**, not the (node, rep) cell.

### D3 — Gate contrast ≠ p-value contrast

The point-estimate `worse` test compares the committed `baseline.json` value
(`score.rs:138`) against the in-run treatment arm, while the p-value tests the in-run
**baseline arm** against the in-run treatment arm (`run.rs:499`). Two different reference
series decide one gate. When the committed baseline drifts from the in-run baseline arm,
`worse` and the significance test describe different comparisons.

## Goals

1. The gated metric's p-value equals the value an independent oracle (scipy) computes for the
   same paired data, across ties, zeros, tiny `n`, and all-same-sign inputs.
2. The paired test's unit of replication is the node.
3. The point estimate and the significance test describe the **same** contrast.
4. Every new golden **fails against the current code** before the fix (proves correctness,
   not self-consistency) and passes after.

## Non-goals

- The in-tree `dotclaude-measure` twin (dotclaude#25 makes it a re-export) — untouched here.
- The minimum-power floor (abproof#3), the roadmap "measure the swap" work (abproof#5).
- Replacing the bootstrap CI or Cohen's d_z machinery (correct as-is).

## Acceptance

- Scipy-golden tests (`mcnemar`/`binomtest`/`wilcoxon` as appropriate) over a matrix: ties,
  zeros, `n∈{0,1}`, all-same-sign, and the audit counterexamples, each committed **RED-first**.
- `[0,0,1,-1,2,2,-3] → ≈0.7955` (was 0.0769); a mixed-sign battery and the on-disk
  `n_nonzero=6` case match scipy.
- Property/fuzz: random deltas never produce a p-value below the exact reference
  (never anti-conservative); boundaries `n=0,1`, all-zeros, all-ties handled.
- Pseudo-replication: an experiment over `k` nodes × `r` reps feeds a **`k`-length** delta
  vector to the test (asserted end-to-end via a scripted driver).
- Contrast alignment: `worse` and the p-value reference the same baseline series.
- An e2e `run_experiment` over a fixture manifest asserts the gate decision matches the
  corrected statistics.
