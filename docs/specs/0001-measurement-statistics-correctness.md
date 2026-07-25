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
with `n = n_nonzero`. **The precise defect is the zeros, not the ties:** with no zeros the Pratt
ranks *are* the ordinary `1..n` average ranks, `Σr = n(n+1)/2`, and the existing tie-corrected
variance already equals `Σr²/4` — so the shipped moments are exactly correct. It is the
**zero-induced Pratt rank shift** (`Σr_nonzero ≠ n(n+1)/2` once zeros consume low ranks) that
breaks both `mu` and `sigma²`. Per-node pass-rate deltas are full of exact zeros (nodes where
both arms tie), so the bug bites on essentially all real data. The exact path
(`exact_wilcoxon_p`) uses the correct mean `sum(nonzero_ranks)/2` and *is* reached when the
non-zero magnitudes are distinct — but `use_exact` requires `!has_ties` (`stats.rs:190`), and
±1/±2 pass/fail deltas tie in magnitude, so the broken approximation runs whenever zeros **and**
tied magnitudes coincide (the common case). The error can go **either** way — anti-conservative
(falsely significant) *or* conservative (missing a real regression), depending on the rank shape.

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

- **Scipy-golden matrix** (oracle = `scipy.stats.wilcoxon(zero_method='pratt', correction=True,
  mode='approx')` for the approximation path; exact 2ⁿ sign-flip enumeration for the exact path)
  over ties, zeros, `n∈{0,1}`, all-same-sign, and the audit counterexamples — each committed
  **RED-first** (fails the pre-fix code). `mcnemar`/`binomtest` appear only as *counterexample
  oracles* in the ADR (showing the rejected sign-test gives the wrong answer), never as the gate
  oracle. Verified values include `[0,0,1,-1,2,2,-3] → 0.7955` (was 0.0769),
  `[0,0,1,1,2,2,3,3] → 0.0228` (was ≈0.0), and `[0,0,0,-1,-1,-2,-2,-3,1,-3] → 0.0476` (was 0.146
  — the buggy code missed this significant regression).
- **Property/fuzz invariants** (deterministic, scipy-free): `p ∈ [0,1]` and finite; **sign
  symmetry** `p(δ) = p(−δ)`; **permutation invariance**; and, on the exact path, equality to an
  independent brute-force enumeration. (We do *not* assert "never below the exact reference" — a
  normal approximation of a discrete statistic is legitimately anti-conservative in the tails
  (e.g. `0.7955 < 0.8125` exact); correctness is pinned by equality-to-scipy on the golden matrix,
  not by a bound against enumeration.)
- **Pseudo-replication:** `score::node_pass_deltas` over `k` nodes × `r` reps yields a `k`-length
  delta vector whose values are per-node *means* (unit-tested with a mixed node → 0.5 delta), and
  a multi-node `run_experiment` reports `n_nonzero` equal to the node count, not `k·r`.
- **Contrast alignment:** `worse` and the p-value both reference the in-run baseline arm; the
  committed `baseline.json` no longer decides the gate (unit + e2e falsifiers).
- **e2e:** `run_experiment` asserts the gate decision matches the corrected statistics — a 6-node
  consistent regression confirms (exit 1), a single-node regression is honestly underpowered
  (exit 0), and mixed-direction noise does not regress (exit 0).
