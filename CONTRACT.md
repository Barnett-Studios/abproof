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

### Corpus input is untrusted, and malformed input is refused rather than repaired

The corpus is authored, and abproof treats what it authors as untrusted at every filesystem sink:
`worktree.rs` refuses an absolute or `..`-bearing `meta.files` entry, and `driver.rs` refuses a
`node.id` that is not a slug (`[A-Za-z0-9][A-Za-z0-9._-]*`, at most 100 characters) before it
reaches a temp path — `DriverError::InvalidNode`.

That rule is shared with the consumer harness, and the sharing is mechanical rather than
declared: `tests/id-guard/vectors.json` is a byte-identical copy of the family's canonical
adversarial corpus, this guard is judged against it in CI, and the upstream twin's CI diffs the
copy. A guard that is stricter or laxer than its sibling fails a test rather than a review.

**Refused, never rewritten.** Sanitizing a bad id into a legal one (`a/b` → `a_b`) would be safe
and dishonest: the run, its temp artifact, and every report derived from them would describe a node
identity that is not in the corpus. That is the same fail-loud rule as above, applied to input — a
malformed corpus entry is a curation defect to surface, not to paper over.

## Front door 1 — CLI

```
abproof run <manifest.yaml> [--dry-run | --confirm] [--out <path>] [--max-cost <usd>] [--max-calls <n>]
```

- Without `--confirm`: prints the dry-run projection (loop-runs, judge-calls, minutes, projected
  claude-cli calls) and exits 0 — nothing is spent.
- `--dry-run`: projection only, exit 0.
- `--confirm`: runs the seed-blocked A/B; `--max-calls` pre-flight-refuses (exit 64) if the
  projection exceeds the cap; `--max-cost` aborts mid-battery (exit 3) rather than overspending.
- Exit: `0` pass · `1` setup error · `3` aborted · `4` underpowered (alpha unreachable — **not** a
  pass) · `64` usage · otherwise the gate's own code.

Run-time inputs are resolved by env (`ABPROOF_CORPUS`, `ABPROOF_EXECUTE_NODE`, `ABPROOF_RESULTS`),
each falling back to a walk-up from the CWD so it works inside a checkout without configuration.

## Front door 2 — Library crate

```rust
pub mod experiment; // load_manifest, Manifest::{validate, is_cross_loop, tracked_metrics, ...}
pub mod corpus;     // red_baseline_root, load_battery, load_node
pub mod run;        // project, run_experiment, RunOptions, DryRun, ExperimentRecord
pub mod driver;     // NodeDriver trait, LocalNodeDriver, ClaudeCliDriver
pub mod judge;      // Judge trait, AbsentJudge (the shipped default), StubJudge, JudgeScore
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
are **tracked** — and, today, **not measured**: no judge is wired (`AbsentJudge` is the shipped
default) and `engine_broken_rate` has no source, so both are reported **ABSENT**, never `0.0`. Statistics are hand-rolled and non-verbatim (Pratt treatment of zeros, average-rank
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
worse        = treatment_arm_value < baseline_arm_value - tolerance   // both in-run, this experiment
underpowered = min_attainable_p(n_discordant) > alpha                 // 2/2^n; alpha defaults to 0.05
regressed    = worse && !underpowered && p_two_sided < alpha
outcome      = UNDERPOWERED if underpowered else (FAIL if regressed else PASS)
```

Both halves reference the **in-run baseline arm** — the same series the p-value is computed
against. The committed `<stem>.baseline.json` is **not** the gate anchor (using it for the point
estimate while the p-value tested the in-run arm mixed two reference series in one verdict); it is
retained as a **drift reference** — a large gap between the committed value and the freshly
measured baseline arm is surfaced as a validity warning, and a required-but-absent gated value
warns rather than aborting.

`alpha` is `Manifest.gate_alpha` when set (validated to `(0.0, 1.0)`), else `0.05`. A metric
with no paired-delta series to test (`p_two_sided: None`) falls back to the bare point-estimate
rule.

**The minimum-power guard: an underpowered battery is not a PASS.** The significance test's `n`
is the **node count**, so the lever for statistical power is the **battery size**, not `reps`
(which only sharpens each node's rate). The exact two-sided sign-flip test has a hard floor of
`2/2ⁿ` at `n` discordant pairs — only the all-positive and all-negative assignments reach the
extreme deviation, out of `2ⁿ` — so `α = 0.05` is unreachable at `n ≤ 5` and reachable from
`n = 6`.

A run below that threshold could not have failed its own gate at any effect size. It reports the
distinct `UNDERPOWERED` verdict and exits **4**, never a PASS/0: "we couldn't have found a
regression" is a different claim from "we looked and found none", and conflating them is how an
underpowered null gets read as evidence of no effect. The guard is **direction-blind** — a
battery with no power to detect a regression did not establish its absence just because the point
estimate improved.

`regressed` and `UNDERPOWERED` are mutually exclusive by construction, so the confirmed-regression
path is unchanged on any powered battery. Every result carries `n_discordant` (the power
denominator) and `min_attainable_p` alongside the realised `p`.

## Compatibility

Semver on the crate. The CLI (`run` + flags), the exit-code contract, and the manifest +
baseline-JSON schema are the stable public surface.

`DriverError` is `#[non_exhaustive]`: match it with a wildcard arm. Adding a variant is then a
minor change rather than a breaking one — which it was not when `InvalidNode` was added.

## What a PASS does and does not cover

`node_pass_rate` is the **sole gated metric**. One pre-registered metric is a deliberate
choice — it avoids the multiple-comparisons trap that a panel of gated dimensions would
introduce — but it has a consequence worth stating plainly: **a treatment that holds
solve-rate while regressing anything else still exits 0.** Double the token cost, halved
`wellformed_pct`, a large latency increase, a quality drop: all PASS.

So every report names both sides:

```
Gate covers: node_pass_rate.
UNGATED (measured, never gated — a regression in these does NOT fail the run):
  wellformed_pct, pass_at_1, pass_at_2, judge_quality, cost_usd, duration
```

`gated` and `ungated` are a **partition**. `gated` is read from the emitted rows;
`ungated` is every other emitted row plus `UNROWED_DIMENSIONS` (`src/report.rs` —
`cost_usd` and `duration`, which are measured and reported in the footer but never get a
row), minus anything gated. Gating a dimension therefore removes it from the ungated list
in the same step, and no dimension can appear in both.

Two failure modes are ruled out by construction, and both were shipped before being caught:

- Appending `cost_usd`/`duration` to a row-derived list by hand. Gating either would then
  have produced a report claiming the gate both covered and did not cover it.
- Building `ungated` from a static registry of every dimension the harness knows about.
  This line says **measured**, never gated — so naming a dimension here asserts *this run
  measured it*. A declared-but-unmeasured metric is reported ABSENT (below), and a static
  registry listed those as measured two lines above the line calling them unmeasured.

Hence the narrow scope of `UNROWED_DIMENSIONS`: it answers only "what did this run measure
that has no row", which is a fixed, short list. Everything else is evidence from the run
itself. The three buckets a declared dimension can land in are **gated**, **ungated**, and
**ABSENT** — and they do not overlap.

A PASS from abproof means "solve-rate did not regress", not "nothing regressed". Read the
tracked deltas before concluding a change is safe.

## Unmeasured metrics are ABSENT, never `0.0`

A metric the manifest declares but nothing measures produces **no row**, and is named in
`absent_metrics` on the record and in an `ABSENT (declared but not measured — no value, NOT
0.0)` line in the rendered table. Silence alone is not enough: a reader who finds no
`judge_quality` row must be able to tell "declared and unmeasured" from "never in scope".

This matters because the failure it replaces was directional. `judge_quality` was reported as
a fabricated `0.0` — a *number*, which a consumer reads as "quality was measured, and it was
the worst possible". Absence is the honest statement; a zero is a false claim about output
quality.

`absent_metrics` is derived from the rows actually emitted, not from a hand-kept list, so it
cannot drift out of step with the emitters.

**On wiring a real judge.** Until [attestr#9] establishes judge ↔ human-label agreement, any
LLM judge is an *uncalibrated instrument*: report it as uncalibrated rather than promoting it
to ground truth. An uncalibrated judge's score is a measurement of the judge as much as of the
work.

[attestr#9]: https://github.com/Barnett-Studios/attestr/issues/9
