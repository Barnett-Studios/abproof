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

The **statistics** twins are shared the same way, and for a sharper reason.
`tests/power-guard/vectors.json` is a byte-identical copy of the canonical
`(n_discordant, alpha) → verdict` corpus; both this crate's minimum-power guard and the
consumer's in-tree twin are judged against it. `dotclaude measure run` invokes abproof as a
container and **fails open to that in-tree twin** (ADR-0055), so two implementations that
disagree mean the fallback silently applies different verdict semantics from the container
— on the guard whose whole job is refusing to call an unfailable battery a PASS. That path
is not hypothetical: the framework's only experiment ran on it.

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

`node_pass_rate` is the **sole gated metric**, and the consequence is worth stating
plainly: **a treatment that holds solve-rate while regressing anything else still exits
0.** Double the token cost, halved `wellformed_pct`, a quality drop — all PASS.

**Why not gate a panel.** Not because of the multiple-comparisons trap: a Holm correction
over a small pre-declared panel handles that cheaply, and saying otherwise would be
restating the problem the correction exists to solve. The reason is that gating a metric
requires a **pre-registered tolerance** — how much cost regression is a failure? measured
against which reference, the in-run baseline arm or the committed baseline? — and those
are measurement-design decisions that need data to set and a decision to record. Inventing
them inside a reporting change would put numbers into a gate that nobody chose. The panel
is a live option; it is a *pre-registration* task, not a formatting one.

Until then a PASS must not read as "nothing regressed", so every report names both sides —
the scope, always:

```
Gate covers: node_pass_rate.
UNGATED (measured, never gated — a regression in these does NOT fail the run):
  judge_quality, wellformed_pct, pass_at_1, pass_at_2, cost_usd
```

**and the alarm, only when something actually moved the wrong way:**

```
UNGATED REGRESSION — moved the wrong way and did not fail the run
  (>= 5% materiality, NOT a significance test): cost_usd +112.0%, wellformed_pct -50.0%
```

The distinction is the whole point. The scope line states policy and prints identically
whether cost doubled or held, so on its own a 2x cost regression produced byte-identical
output to a flat run and the reader had to find it unaided in the deltas — which is what
"silently PASS" means. The alarm fires only on movement, so it stays an alarm rather than
becoming a second banner that trains readers to skip it.

Two honesty constraints on that line. It is a **materiality** threshold, never a
significance claim: no ungated dimension has a paired-delta series in the record, so there
is no test to run, and the threshold is printed so a reader knows what was filtered. And
direction is per-metric — higher is worse for `cost_usd` and `engine_broken_rate`, lower
for the rates and scores — with `every_tracked_metric_has_a_known_direction` failing the
build if a new metric arrives without one, since an unknown direction means *never
alarmed*, which is the original defect one level down.

A **zero baseline** is reported, not exempted:

```
UNGATED REGRESSION — moved the wrong way and did not fail the run
  (>= 5% materiality, NOT a significance test): engine_broken_rate from zero (unbounded), …
```

Materiality is a *relative* threshold and zero has no relative change to divide by, so the
first cut of this alarm returned "no regression" for a zero baseline.

**In v1 the live case is cost.** A free-local-baseline vs paid-treatment run has
`baseline_cost_usd = 0` by construction — the local rung reports `cost_usd=0.0`, not
`unknown` — so a $0 -> $1.06 regression had nothing to divide by. The two row metrics v1
emits, `node_pass_rate` and `judge_quality`, are both lower-is-worse, so a zero baseline on
either can only be an improvement and is correctly silent either way.

**`engine_broken_rate` is the forward case, not a current one.** Its healthy baseline is
exactly zero, which makes it the load-bearing case for this guard — but it is unwired in v1
and reports ABSENT, so 0% -> 40% broken cannot occur as a row today. The guard is correct
before its motivating metric exists rather than after.

Direction still decides — a metric climbing off zero the *right* way stays silent — and an
unbounded move sorts ahead of every finite one on the line.

`gated` and `ungated` are a **partition** over what *this run actually produced*. `gated`
is read from the emitted rows; `ungated` is every other emitted row, plus `cost_usd` when a
paid call ran **and the run could price it**, minus anything gated. Gating a dimension
removes it from the ungated list in the same step, so no dimension can appear in both.

Both cost conditions are load-bearing, and they fail differently. No paid call at all →
the footer is omitted entirely, so naming `cost_usd` would point at a number the report
never produced. Paid calls that could not be priced → any call reporting `cost_usd=unknown`
blanks all three cost fields run-wide while the call count stays positive, degrading the
footer to `Cost: unreported`; naming `cost_usd` as *measured* there contradicts that footer
two lines up. This is the same invariant `absent_metrics` enforces for row metrics — ABSENT
must not also be reported as measured — which cost slipped through by having no row to be
absent from.

The line says **measured**, never gated. Naming a dimension there asserts this run measured
it, and three ways of getting that wrong were each shipped before being caught:

- **A hand-written tail.** `["cost_usd", "duration"]` appended to a row-derived list. Gating
  either would have produced a report claiming the gate both covered and did not cover it.
- **A static registry of every known dimension.** This listed declared-but-unmeasured
  metrics as measured — two lines above the ABSENT line calling them unmeasured.
- **Naming a conditionally-measured dimension unconditionally.** The cost footer is omitted
  on local-only runs by design, since a misleading `$0.0000` is worse than silence. Listing
  `cost_usd` anyway told the reader the gate does not cover a number the report never
  produced.

Hence: everything comes from the run's own output, and the single exception — cost, which
has no row — is conditioned on the same test the cost footer uses. `duration` is **not**
listed: the driver times each run, but that never reaches the result record or any output,
and naming a dimension the report does not surface points a reader at nothing. It belongs
on the line the day it is reported.

A declared dimension therefore lands in exactly one of three buckets — **gated**,
**ungated**, or **ABSENT** — and they do not overlap.

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
