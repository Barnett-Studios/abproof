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

**The gate is significance-based, not a bare point estimate.** A worse observed value
(`observed < baseline - tolerance`) only fails the run when it also clears statistical
significance on the paired Wilcoxon signed-rank test over the gated metric's within-pair
deltas:

```
worse     = observed_value < baseline_value - tolerance
regressed = worse && p_two_sided < alpha        // alpha defaults to 0.05
```

`alpha` is `Manifest.gate_alpha` when set (validated to `(0.0, 1.0)`), else `0.05`. A metric
with no paired-delta series to test (`p_two_sided: None`) falls back to the bare point-estimate
rule. **Small-n consequence, stated honestly:** an underpowered run — too few paired reps to
reach `p < alpha` even for a real effect — reports "not a confirmed regression" and exits 0.
This is by design: the gate refuses to fail a run on a point estimate it cannot statistically
back up. `reps` (default 30) is the lever for statistical power, not the gate rule.

## Compatibility

Semver on the crate. The CLI (`run` + flags), the exit-code contract, and the manifest +
baseline-JSON schema are the stable public surface.
