# abproof — Contract

abproof turns a *change to your agent setup* into a stat-gated A/B verdict over a corpus, reusing
an executor as the measured arm. Two front doors (CLI + library crate) wrap one core.

## The measurement-integrity guarantee (fail-loud, by design)

> abproof never presents an invalid measurement as a result. An aborted run (local runtime down,
> cost cap hit mid-battery, unknown per-call cost) exits **3** with an explicit `EXPERIMENT
> ABORTED` message — not a green gate line. A setup fault (bad manifest, missing baseline) exits
> **1**. This is the deliberate inverse of a live-loop component's fail-open: an *offline* oracle
> that hid a broken run behind a PASS would defeat its own purpose. It still honours the
> constitution — abproof is offline and never feeds the live agent loop; its absence just means the
> harness goes unmeasured.

## Front door 1 — CLI

```
abproof run <manifest.yaml> [--dry-run | --confirm] [--out <path>] [--max-cost <usd>] [--max-calls <n>]
```

- Without `--confirm`: prints the dry-run projection (loop-runs, judge-calls, minutes, projected
  claude-cli calls) and exits 0 — nothing is spent.
- `--dry-run`: projection only, exit 0.
- `--confirm`: runs the seed-blocked A/B; `--max-calls` pre-flight-refuses (exit 64) if the
  projection exceeds the cap; `--max-cost` aborts mid-battery (exit 3) rather than overspending.
- Exit: `0` pass · `1` setup error · `3` aborted · `64` usage · otherwise the gate's own code.

Run-time inputs are resolved by env (`ABPROOF_CORPUS`, `ABPROOF_EXECUTE_NODE`, `ABPROOF_RESULTS`),
each falling back to a walk-up from the CWD so it works inside a checkout without configuration.

## Front door 2 — Library crate

```rust
pub mod experiment; // load_manifest, Manifest::{validate, is_cross_loop, tracked_metrics, ...}
pub mod corpus;     // red_baseline_root, load_battery, load_node
pub mod run;        // project, run_experiment, RunOptions, DryRun, ExperimentRecord
pub mod driver;     // NodeDriver trait, LocalNodeDriver, ClaudeCliDriver
pub mod judge;      // Judge trait, StubJudge, JudgeScore
pub mod score;      // load_baseline, task-typed scoring
pub mod stats;      // hand-rolled non-verbatim statistics (Pratt zeros, average-rank ties)
pub mod report;     // write_result_json, render_r_table
pub mod worktree;   // seed-project work-tree provisioner
pub mod env_filter; // child-process env allowlist (inlined; no framework dependency)
```

The library is **fully standalone** — it inlines what it needs (`env_filter`, the `ABPROOF_CORPUS`
resolver) and depends on no engine crate. It drives an executor (the reference is the
`execute_node.py` loop) and `claude -p` over **subprocess** boundaries only.

## The A/B model (what the gate means)

Two pipeline configurations (baseline vs. treatment), **seed-blocked** so the same seeds run both
arms, `reps` per seed. Deterministic acceptance (the RED test) is **gated**; judge + engine quality
are **tracked**. Statistics are hand-rolled and non-verbatim (Pratt treatment of zeros, average-rank
ties, gate-vs-track separation). A cross-loop manifest (local vs claude-cli) compares runtimes over
the shared loop. Remote/infra failure maps to *abort*, never a measured 0.0.

**The node is the unit of replication.** Each node's `reps` are aggregated into ONE paired
observation — the mean pass score per arm — before the paired test; the delta series has one
entry per node. `reps` correlated runs of the same node are not independent observations. The
paired test is **Wilcoxon signed-rank**, computed **exactly** (2ⁿ sign-flip enumeration, valid
with ties) for batteries of ≤ 25 gradable nodes — the true conditional p-value — and by a normal
approximation with the sign-flip randomization moments `μ = Σr/2`, `σ² = Σr²/4` for larger
batteries (matching `scipy.stats.wilcoxon(zero_method='pratt')` to machine precision). The exact
path is required because the normal approximation is anti-conservative near α under heavy ties
(the pass/fail-delta regime).

**The gate is significance-based, not a bare point estimate.** A worse observed value only fails
the run when it also clears statistical significance on the paired test over the gated metric's
**per-node** deltas:

```
worse     = treatment_arm_value < baseline_arm_value - tolerance   // both in-run, this experiment
regressed = worse && p_two_sided < alpha                           // alpha defaults to 0.05
```

Both halves reference the **in-run baseline arm** — the same series the p-value is computed
against. The committed `<stem>.baseline.json` is **not** the gate anchor (using it for the point
estimate while the p-value tested the in-run arm mixed two reference series in one verdict); it is
retained as a **drift reference** — a large gap between the committed value and the freshly
measured baseline arm is surfaced as a validity warning, and a required-but-absent gated value
warns rather than aborting.

`alpha` is `Manifest.gate_alpha` when set (validated to `(0.0, 1.0)`), else `0.05`. A metric
with no paired-delta series to test (`p_two_sided: None`) falls back to the bare point-estimate
rule. **Small-n consequence, stated honestly:** the significance test's `n` is the **node
count**, so the lever for statistical power is the **battery size**, not `reps` (which only
sharpens each node's rate). A run over too few nodes — even at high `reps` — cannot reach
`p < alpha` for a real effect and honestly reports "not a confirmed regression", exiting 0
rather than failing on a point estimate it cannot statistically back up.

## Compatibility

Semver on the crate. The CLI (`run` + flags), the exit-code contract, and the manifest +
baseline-JSON schema are the stable public surface.
