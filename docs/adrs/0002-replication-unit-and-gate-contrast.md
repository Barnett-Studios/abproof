---
title: Replication unit and gate contrast
issue: Barnett-Studios/abproof#7
status: Proposed
created: 2026-07-22T07:19:41Z
updated: 2026-07-22T07:19:41Z
---

# ADR 0002 — Replication unit and gate contrast

## Context

Two coupled defects in how the paired delta series is built and compared (Spec 0001, D2/D3).

## Decision D2 — the node is the unit of replication

Aggregate within node **before** the paired test. For each node, the arm value is the mean pass
score across that node's `reps`; the paired delta is `mean(treatment) − mean(baseline)` — one
delta per node. The `reps` lever still buys power (a node's rate is measured more precisely with
more reps) but no longer fabricates independent observations. The delta vector length is the
**node count**, not `node_count × reps`.

Order is fixed (nodes visited in battery order) so the bootstrap CI's pinned seed stays
reproducible.

The reported **point estimates** (`node_pass_rate` per arm) stay the rep-weighted grand mean over
all `(node,rep)` cells — a descriptive statistic. When no pair is excluded (every node runs the
same `reps`) this exactly equals the node-weighted mean of per-node means, so the point estimate
and the per-node significance test are the identical contrast. **Caveat (honest):** per-pair
`Inconclusive`/`Skipped` exclusion (`run.rs:306–323`) can leave nodes with *unequal* gradable rep
counts; then the reported grand mean is rep-weighted while the p-value / d_z / bootstrap are
node-weighted. The two are no longer strictly identical, but the divergence is bounded by the
fail-loud inconclusive floor (`INCONCLUSIVE_MAX_FRACTION`, default 0.20) that aborts any run
excluding more than that fraction — and is zero in the common no-exclusion case. Both halves still describe the **same series** — in-run treatment vs. in-run baseline arm (the D3
alignment); the residual is only a rep- vs. node-weighting nuance on that one series, bounded by
the floor, not the two-different-references defect D3 fixes. We accept the reported metric as
rep-weighted (matching what abproof has always displayed) rather than re-weight the descriptive
statistic and make the gate compare numbers that differ from what the R-table shows.

## Decision D3 — both halves of the gate use the in-run baseline arm

The point-estimate `worse` test and the significance test must reference the **same** series.
The p-value is intrinsically a paired, in-run contrast (treatment arm vs. baseline arm at the
same seeds) — it *cannot* be computed against a committed scalar. Therefore the point estimate
moves to the same contrast:

```
worse     = treatment_rate < baseline_ARM_rate − tolerance     // both in-run, this experiment
regressed = worse && p_two_sided < alpha
```

`baseline_ARM_rate` is the in-run baseline arm's aggregate `node_pass_rate` (already computed as
`baseline_agg`), not the committed `baseline.json` value.

### What happens to `baseline.json`?

It stays a **required input** (no CLI/exit-code change: missing baseline is still a setup error)
and is still reported, but its role narrows from *the gate anchor* to a **drift reference**: the
run reports committed-vs-observed-baseline-arm so a stale baseline is visible, without letting
that staleness silently decide the gate against a different series than the p-value tested.

## Alternatives considered

- **Keep committed `baseline.json` as the point-estimate anchor (status quo).** Rejected: it is
  exactly the D3 incoherence — `worse` and `p` then describe different comparisons whenever the
  committed value ≠ the in-run baseline arm.
- **Make the p-value test treatment vs. the committed baseline.** Impossible: the committed
  baseline is a scalar; there is no per-node committed series to pair against.
- **Drop `baseline.json` entirely.** Rejected as out-of-scope churn: it would change the
  CLI/exit-code contract (`baseline_json` required) and remove a useful drift signal.

## Contract impact & HITL

This **is** a CONTRACT.md change: the gate's `baseline_value` is redefined from "committed
`baseline.json`" to "in-run baseline arm rate". CONTRACT.md's A/B section and the `worse`
formula are updated in the same commit (docs-with-code). `dotclaude detect-hitl` is expected to
fire on the CONTRACT.md edit; that firing is a genuine human sign-off point, batched as an
`AskUserQuestion`, suppressed in the commit body only with an `HITL-ACK:` line citing this ADR.

## Falsifier

D2 is wrong if an experiment over `k` nodes yields a delta vector whose length ≠ `k`
(asserted with a scripted multi-node/multi-rep driver). D3 is wrong if, holding the in-run arms
fixed, swapping the committed `baseline.json` value changes the `regressed` verdict — after the
fix it must not, because the gate no longer reads that value for the contrast.
