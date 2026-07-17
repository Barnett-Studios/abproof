//! Run orchestration — seed generation, dry-run projection, and full experiment loop.

use crate::report::{MetricRow, ResultRecord};
use crate::stats::WilcoxonMethod;
use crate::{corpus, driver, experiment, judge, score, stats};

// ── Metric helpers ───────────────────────────────────────────────────────────

/// Port of `metrics_rollup.py::wellformed_pct`.
///
/// Denominator: `statuses` after dropping entries whose string starts with any
/// of `LOCAL_UNAVAILABLE`, `SKIPPED`, `FAILURE(commit_error`, `FAILURE(dirty_tree`,
/// or that equal `"Inconclusive"` exactly.  Of the remainder, `SUCCESS` and
/// `FAILURE(verify_error…` are well-formed; everything else is not.
/// Returns `0.0` when the remaining set is empty.
pub fn wellformed_pct(statuses: &[String]) -> f64 {
    const DROP: &[&str] = &[
        "LOCAL_UNAVAILABLE",
        "SKIPPED",
        "FAILURE(commit_error",
        "FAILURE(dirty_tree",
    ];
    let remaining: Vec<&str> = statuses
        .iter()
        .map(String::as_str)
        .filter(|s| !DROP.iter().any(|p| s.starts_with(p)) && *s != "Inconclusive")
        .collect();
    if remaining.is_empty() {
        return 0.0;
    }
    let wellformed = remaining
        .iter()
        .filter(|s| **s == "SUCCESS" || s.starts_with("FAILURE(verify_error"))
        .count();
    wellformed as f64 / remaining.len() as f64
}

/// Port of `metrics_rollup.py::pass_at_1_2`.
///
/// `attempts[i]` is the number of claude-cli invocations for run `i`
/// (from `RunOutput::claude_calls`).  `0` or `1` counts as a first-attempt
/// success (≤ 1); `2+` is a retry success.  Returns `(pass@1, pass@2)`.
/// Returns `(0.0, 0.0)` when `statuses` is empty.
pub fn pass_at_1_2(statuses: &[String], attempts: &[u64]) -> (f64, f64) {
    if statuses.is_empty() {
        return (0.0, 0.0);
    }
    let total = statuses.len() as f64;
    let mut success: u64 = 0;
    let mut success_1: u64 = 0;
    for (s, &a) in statuses.iter().zip(attempts.iter()) {
        if s == "SUCCESS" {
            success += 1;
            if a <= 1 {
                success_1 += 1;
            }
        }
    }
    (success_1 as f64 / total, success as f64 / total)
}

/// Produce the Python-oracle-compatible status string for a run output.
///
/// The driver emits an empty `stdout_tail` for a harness-side SIGKILL (the python
/// process was killed before producing a final line); the executor can also emit
/// its own `INCONCLUSIVE(<reason>)` terminal line. Both collapse to
/// `RunStatus::Inconclusive` (ADR-0042 timeout-collapse) — normalise both to the
/// literal string `"Inconclusive"` here so `wellformed_pct` drops them uniformly
/// from its denominator regardless of which producer fired.
fn effective_status_string(out: &driver::RunOutput) -> String {
    match out.status {
        driver::RunStatus::Inconclusive => "Inconclusive".to_string(),
        _ => out.stdout_tail.clone(),
    }
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Pinned seed for the bootstrap CI — guarantees reproducible intervals across runs.
const BOOTSTRAP_SEED: u64 = 0xAB12_3456_789A_BCDE;
const BOOTSTRAP_B: usize = 10_000;
const BOOTSTRAP_ALPHA: f64 = 0.05;

/// Conservative per-run time estimate used for the `--dry-run` envelope.
const EST_MINUTES_PER_RUN: f64 = 2.0;

/// Upper bound on the retry ladder for pre-flight call projections.
/// Matches `execute_node.py`'s default `LLM_MAX_ATTEMPTS = 2`.
const MAX_RETRY_LADDER: u64 = 2;

// ── Public types ─────────────────────────────────────────────────────────────

/// Dry-run projection: loop and judge call counts with a time envelope.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DryRun {
    /// Total driver invocations: battery_len × reps × 2 arms.
    pub loop_runs: u64,
    /// Total judge invocations: judged_tasks × judge_reps × 2 arms.
    pub judge_calls: u64,
    /// Conservative upper bound in minutes.
    pub est_minutes: f64,
    /// Human-readable baseline arm description.
    pub baseline: String,
    /// Human-readable treatment arm description.
    pub treatment: String,
    /// Upper-bound on live claude-cli calls: claude-cli arm count × battery_len × reps × MAX_RETRY_LADDER.
    /// Zero for fully-local experiments. The actual cost is measured live — this count is NOT a
    /// dollar estimate; cost is unknowable until the run completes (see I2 discipline).
    pub projected_claude_calls: u64,
}

/// Per-run safety options. Pass `RunOptions::default()` for a cap-free run.
#[derive(Debug, Default, Clone)]
pub struct RunOptions {
    /// Abort mid-battery when cumulative claude-cli cost exceeds this threshold (USD).
    pub max_cost: Option<f64>,
}

// ── seed_for ─────────────────────────────────────────────────────────────────

/// Compute a deterministic per-block seed from the experiment seed base,
/// the node id, and the rep index.
///
/// Both arms of the same `(node_id, rep)` receive this **same** seed,
/// making the pair the unit of comparison (the pairing dimension).
///
/// Implementation: FNV-1a-64 of `node_id` is folded with `seed_base` and
/// `rep` (scaled to break simple arithmetic sequences) into a single
/// `SplitMix64` step to produce the output. Pure, deterministic, no I/O.
pub fn seed_for(seed_base: u64, node_id: &str, rep: u32) -> u64 {
    // FNV-1a-64 of node_id: mixes node identity into the seed space.
    let mut fnv: u64 = 14_695_981_039_346_656_037;
    for byte in node_id.as_bytes() {
        fnv ^= *byte as u64;
        fnv = fnv.wrapping_mul(1_099_511_628_211);
    }
    // Combine dimensions; the rep multiplier breaks alignment between adjacent reps.
    let combined = seed_base
        .wrapping_add(fnv)
        .wrapping_add((rep as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    stats::SplitMix64::new(combined).next_u64()
}

// ── project ──────────────────────────────────────────────────────────────────

/// Compute a dry-run projection without constructing any driver.
///
/// - `loop_runs` = `battery_len × reps × 2` (two arms per pair)
/// - `judge_calls` = `judged_tasks × judge_reps × 2`
/// - `est_minutes` uses a fixed conservative constant of 2 min/run
pub fn project(manifest: &experiment::Manifest, judged_tasks: usize, judge_reps: u32) -> DryRun {
    let battery_len = manifest.battery.len() as u64;
    let reps = manifest.reps as u64;
    let loop_runs = battery_len * reps * 2;
    let judge_calls = judged_tasks as u64 * judge_reps as u64 * 2;
    let est_minutes = loop_runs as f64 * EST_MINUTES_PER_RUN;

    let claude_cli_arms = [&manifest.baseline, &manifest.treatment]
        .iter()
        .filter(|a| a.backend == experiment::Backend::ClaudeCli)
        .count() as u64;
    let projected_claude_calls = claude_cli_arms * battery_len * reps * MAX_RETRY_LADDER;

    DryRun {
        loop_runs,
        judge_calls,
        est_minutes,
        baseline: arm_label(&manifest.baseline),
        treatment: arm_label(&manifest.treatment),
        projected_claude_calls,
    }
}

// ── run_experiment ────────────────────────────────────────────────────────────

/// Spec §8 tools-off leak signal. `num_turns` and `claude_calls` are both
/// summed across the retry ladder, so a single-turn-per-call ladder yields
/// `num_turns == claude_calls`. A leak (a call that went multi-turn despite
/// tools being off) is the only way to get `num_turns > claude_calls`.
/// Comparing against `1` instead false-fires on every legitimate multi-attempt
/// run (the RED-baseline common case) and would render the signal meaningless.
fn turns_leaked(num_turns: u64, claude_calls: u64) -> bool {
    num_turns > claude_calls
}

/// Run a full A/B experiment over `nodes`, paired at the `(node, rep)` block seed.
///
/// Both arms receive the **same** seed per pair — that shared seed is the
/// pairing dimension.  Deltas (treatment_pass − baseline_pass) feed
/// `stats::wilcoxon_signed_rank`, `stats::cohens_dz`, and
/// `stats::bootstrap_median_ci` (pinned seed, B = 10 000, α = 0.05).
///
/// `judge` is called for both arms every rep to populate the `judge_quality`
/// tracked row.  Tracked rows carry values but never a verdict.
///
/// `seeds_honoured` is `true` iff at least one non-`Skipped` run occurred this
/// battery and every non-`Skipped` `RunOutput` reported `seeds_honoured == true`
/// (the driver's own signal — set from the arm's effective `LLM_TEMPERATURE`; a
/// greedy/temp-0 arm makes `LLM_SEED` a no-op). An aborted run (`abort_record`)
/// is always `false` — it is not a valid measurement.
///
/// `opts.max_cost` aborts mid-battery when cumulative cost exceeds the cap.
/// A `LocalUnavailable` arm output aborts with exit 3 and takes precedence
/// over any cost-cap abort (I3 semantics).
pub fn run_experiment(
    manifest: &experiment::Manifest,
    nodes: &[corpus::CorpusNode],
    driver: &dyn driver::SessionDriver,
    judge: &dyn judge::Judge,
    baseline: &score::Baseline,
    opts: &RunOptions,
) -> ResultRecord {
    let rubric = judge::Rubric {
        criteria: vec!["quality".to_string()],
        max_per_criterion: 4,
    };

    let mut pairs: Vec<score::PairedRep> = Vec::new();
    let mut baseline_judge: Vec<f64> = Vec::new();
    let mut treatment_judge: Vec<f64> = Vec::new();

    // Cost accumulators.
    let mut cum_cost: f64 = 0.0;
    let mut baseline_cost: f64 = 0.0;
    let mut treatment_cost: f64 = 0.0;
    let mut total_claude_calls: u64 = 0;
    let mut cost_unknown = false;

    // Validity warnings (spec §8: num_turns > 1 on a claude-cli arm).
    let mut validity_warnings: Vec<String> = vec![];

    // Inconclusive-fraction tracking (ADR-0041 fail-loud floor, A6).
    // `total_pairs` counts every (node, rep) iteration reached this run;
    // `inconclusive_pairs` counts those excluded because either arm was
    // Inconclusive (A5). The ratio is compared against
    // `INCONCLUSIVE_MAX_FRACTION` after the loop completes.
    let mut total_pairs: u64 = 0;
    let mut inconclusive_pairs: u64 = 0;
    // Counts pairs excluded by the Skipped gate (missing toolchain) — these
    // never reach the Inconclusive check and must not appear in the
    // fail-loud floor's denominator (BUG2): the floor measures how much of
    // the *attempted* battery was ungradable due to Inconclusive, not how
    // much of the raw total (which Skipped pairs would dilute).
    let mut skipped_pairs: u64 = 0;

    // Per-run status strings and attempt counts for wellformed_pct / pass@1 / pass@2.
    // Only populated for non-Skipped pairs (toolchain-absent nodes are excluded).
    let mut baseline_statuses: Vec<String> = Vec::new();
    let mut baseline_attempts: Vec<u64> = Vec::new();
    let mut treatment_statuses: Vec<String> = Vec::new();
    let mut treatment_attempts: Vec<u64> = Vec::new();

    // `seeds_honoured` tally (truthful replacement for the v1 hardcoded `false`):
    // true iff at least one non-Skipped run occurred AND every non-Skipped run
    // reported `RunOutput::seeds_honoured == true`. A Skipped run (missing
    // toolchain) never reached sampling at all, so it is excluded from the tally
    // rather than counted as an unhonoured seed.
    let mut any_non_skipped_run = false;
    let mut all_non_skipped_seeds_honoured = true;

    for node in nodes {
        for rep in 0..manifest.reps {
            let seed = seed_for(manifest.seed_base, &node.meta.id, rep);

            let baseline_json = corpus::bridge_node(node, &manifest.baseline);
            let treatment_json = corpus::bridge_node(node, &manifest.treatment);

            // Both arms at the same seed — this is the pairing dimension.
            let baseline_out = driver
                .run(&manifest.baseline, &baseline_json, seed)
                .unwrap_or_else(|_| failure_output());
            let treatment_out = driver
                .run(&manifest.treatment, &treatment_json, seed)
                .unwrap_or_else(|_| failure_output());

            for out in [&baseline_out, &treatment_out] {
                if out.status != driver::RunStatus::Skipped {
                    any_non_skipped_run = true;
                    all_non_skipped_seeds_honoured &= out.seeds_honoured;
                }
            }

            total_pairs += 1;

            // I3: LocalUnavailable MUST be detected and abort BEFORE cost
            // accumulation so a down runtime is never masked by a cost-cap abort.
            if baseline_out.status == driver::RunStatus::LocalUnavailable
                || treatment_out.status == driver::RunStatus::LocalUnavailable
            {
                return abort_record(
                    manifest,
                    &format!("local runtime unavailable at {}/{rep}", node.meta.id),
                    cum_cost,
                    baseline_cost,
                    treatment_cost,
                    total_claude_calls,
                    validity_warnings,
                    inconclusive_pairs,
                    inconclusive_fraction_of(inconclusive_pairs, total_pairs - skipped_pairs),
                );
            }

            // Toolchain gate: if either arm was skipped (missing required tool or
            // execute_node.py's own SKIPPED(already_satisfied)), exclude the entire
            // pair from pass/fail scoring and do not accumulate cost.
            if baseline_out.status == driver::RunStatus::Skipped
                || treatment_out.status == driver::RunStatus::Skipped
            {
                skipped_pairs += 1;
                continue;
            }

            // A5 / ADR-0041 §per-node soft-exclusion: an Inconclusive artifact in
            // either arm breaks the paired Wilcoxon delta for this (node, rep) —
            // exclude the whole pair, mirroring the Skipped gate above, and BEFORE
            // node_pass_score is ever called on either arm. Tracked separately for
            // the A6 fail-loud floor.
            if baseline_out.status == driver::RunStatus::Inconclusive
                || treatment_out.status == driver::RunStatus::Inconclusive
            {
                inconclusive_pairs += 1;
                continue;
            }

            // Accumulate status strings for wellformed_pct / pass@1 / pass@2.
            // Must happen after the Skipped gate so only completed runs are counted.
            baseline_statuses.push(effective_status_string(&baseline_out));
            baseline_attempts.push(baseline_out.claude_calls);
            treatment_statuses.push(effective_status_string(&treatment_out));
            treatment_attempts.push(treatment_out.claude_calls);

            // Cost accumulation — baseline arm.
            total_claude_calls += baseline_out.claude_calls;
            match baseline_out.cost_usd {
                Some(c) => {
                    baseline_cost += c;
                    cum_cost += c;
                }
                None if opts.max_cost.is_some() => {
                    return abort_record(
                        manifest,
                        "claude -p did not report cost; cannot enforce --max-cost",
                        cum_cost,
                        baseline_cost,
                        treatment_cost,
                        total_claude_calls,
                        validity_warnings,
                        inconclusive_pairs,
                        inconclusive_fraction_of(inconclusive_pairs, total_pairs),
                    );
                }
                None => {
                    cost_unknown = true;
                }
            }
            // Spec §8: a call going multi-turn (turns > calls) means the tools-off
            // constraint leaked. A legitimate retry ladder (turns == calls) is not a leak.
            if turns_leaked(baseline_out.num_turns, baseline_out.claude_calls) {
                validity_warnings.push(format!(
                    "claude-cli run at {}/{} took {} turns — tools-off constraint may have leaked; measurement suspect",
                    node.meta.id, rep, baseline_out.num_turns
                ));
            }

            // Cost accumulation — treatment arm.
            total_claude_calls += treatment_out.claude_calls;
            match treatment_out.cost_usd {
                Some(c) => {
                    treatment_cost += c;
                    cum_cost += c;
                }
                None if opts.max_cost.is_some() => {
                    return abort_record(
                        manifest,
                        "claude -p did not report cost; cannot enforce --max-cost",
                        cum_cost,
                        baseline_cost,
                        treatment_cost,
                        total_claude_calls,
                        validity_warnings,
                        inconclusive_pairs,
                        inconclusive_fraction_of(inconclusive_pairs, total_pairs),
                    );
                }
                None => {
                    cost_unknown = true;
                }
            }
            if turns_leaked(treatment_out.num_turns, treatment_out.claude_calls) {
                validity_warnings.push(format!(
                    "claude-cli run at {}/{} took {} turns — tools-off constraint may have leaked; measurement suspect",
                    node.meta.id, rep, treatment_out.num_turns
                ));
            }

            // Mid-battery cost-cap abort.
            if let Some(cap) = opts.max_cost {
                if cum_cost > cap {
                    return abort_record(
                        manifest,
                        &format!(
                            "cost cap ${cap:.4} exceeded at {}/{rep} (cum ${cum_cost:.4})",
                            node.meta.id
                        ),
                        cum_cost,
                        baseline_cost,
                        treatment_cost,
                        total_claude_calls,
                        validity_warnings,
                        inconclusive_pairs,
                        inconclusive_fraction_of(inconclusive_pairs, total_pairs),
                    );
                }
            }

            let baseline_pass = score::node_pass_score(&baseline_out);
            let treatment_pass = score::node_pass_score(&treatment_out);

            pairs.push(score::PairedRep {
                node_id: node.meta.id.clone(),
                rep,
                baseline_pass,
                treatment_pass,
            });

            // Judge tracking — errors are ignored; missing judge data means
            // the tracked row value is absent for that rep.
            if let Ok(js) = judge.score(&baseline_out, &rubric) {
                baseline_judge.push(js.total as f64);
            }
            if let Ok(js) = judge.score(&treatment_out, &rubric) {
                treatment_judge.push(js.total as f64);
            }
        }
    }

    // ── A6 / ADR-0041 fail-loud floor ────────────────────────────────────────
    // Per-node Inconclusive soft-excludes (A5) and does NOT abort on its own.
    // But a run that could not grade more than INCONCLUSIVE_MAX_FRACTION of its
    // pairs is not a valid measurement — raise a validity_warning and abort
    // exit 3, the same machinery as the systemic LocalUnavailable abort above.
    //
    // The denominator is `total_pairs - skipped_pairs` (attempted-gradable
    // pairs), NOT raw `total_pairs`: a Skipped pair (missing toolchain) never
    // reached the Inconclusive check, so counting it in the denominator would
    // dilute a genuinely high inconclusive-among-attempted rate below the
    // floor (BUG2).
    let attempted_pairs = total_pairs - skipped_pairs;
    let inconclusive_fraction = inconclusive_fraction_of(inconclusive_pairs, attempted_pairs);
    let max_fraction = inconclusive_max_fraction();
    if inconclusive_fraction > max_fraction {
        validity_warnings.push(format!(
            "inconclusive fraction {inconclusive_fraction:.3} ({inconclusive_pairs}/{attempted_pairs} attempted-gradable pairs) \
             exceeds INCONCLUSIVE_MAX_FRACTION {max_fraction:.3} — measurement validity floor breached"
        ));
        return abort_record(
            manifest,
            &format!(
                "inconclusive fraction {inconclusive_fraction:.3} exceeds INCONCLUSIVE_MAX_FRACTION {max_fraction:.3}"
            ),
            cum_cost,
            baseline_cost,
            treatment_cost,
            total_claude_calls,
            validity_warnings,
            inconclusive_pairs,
            inconclusive_fraction,
        );
    }

    // ── BUG1: empty pairs must not fabricate a gate verdict ──────────────────
    // If every (node, rep) pair was Skipped or Inconclusive-excluded, there is
    // no gradable data at all — this is an aborted experiment (exit 3), not a
    // measurement with an observed 0.0 pass rate (which would collide with a
    // genuine regression at gate time; see `score::aggregate_arm(&[], ..)`
    // returning `node_pass_rate: 0.0` for an empty slice).
    if pairs.is_empty() {
        return abort_record(
            manifest,
            "no gradable pairs (all skipped or excluded)",
            cum_cost,
            baseline_cost,
            treatment_cost,
            total_claude_calls,
            validity_warnings,
            inconclusive_pairs,
            inconclusive_fraction,
        );
    }

    // ── aggregate both arms ──────────────────────────────────────────────────
    let baseline_passes: Vec<f64> = pairs.iter().map(|p| p.baseline_pass).collect();
    let treatment_passes: Vec<f64> = pairs.iter().map(|p| p.treatment_pass).collect();

    let baseline_agg = score::aggregate_arm(&baseline_passes, &baseline_judge, &[]);
    let treatment_agg = score::aggregate_arm(&treatment_passes, &treatment_judge, &[]);

    // ── within-pair deltas for the gated metric (node_pass_rate) ────────────
    let deltas: Vec<f64> = pairs
        .iter()
        .map(|p| p.treatment_pass - p.baseline_pass)
        .collect();

    let wilcoxon = stats::wilcoxon_signed_rank(&deltas);
    let dz = stats::cohens_dz(&deltas);
    let ci = stats::bootstrap_median_ci(&deltas, BOOTSTRAP_B, BOOTSTRAP_SEED, BOOTSTRAP_ALPHA);

    // ── gate verdicts ────────────────────────────────────────────────────────
    let verdicts = score::gate(manifest, baseline, &treatment_agg);

    // Measurement-integrity guard: every manifest-declared gated metric MUST yield a
    // verdict. A gated metric absent from the baseline JSON (stale / typo'd / renamed
    // key) or otherwise unobservable is silently dropped by `score::gate`'s filter_map,
    // which would leave `exit_code(&verdicts) == 0` (PASS) for a comparison that never
    // happened — the exact false PASS the fail-loud contract forbids. Treat it as a
    // setup fault (exit 3) naming the missing metric, not a passing gate.
    let gated = manifest.gated_metrics();
    if verdicts.len() != gated.len() {
        let evaluated: std::collections::HashSet<&str> =
            verdicts.iter().map(|v| v.metric.as_str()).collect();
        let missing: Vec<&str> = gated
            .iter()
            .copied()
            .filter(|m| !evaluated.contains(m))
            .collect();
        return abort_record(
            manifest,
            &format!(
                "gated metric(s) {missing:?} produced no verdict \
                 (absent from baseline or not observable) — refusing to report a PASS"
            ),
            cum_cost,
            baseline_cost,
            treatment_cost,
            total_claude_calls,
            validity_warnings,
            inconclusive_pairs,
            inconclusive_fraction,
        );
    }
    let gate_exit = score::exit_code(&verdicts);

    // ── build result rows ────────────────────────────────────────────────────
    let mut rows: Vec<MetricRow> = Vec::new();

    // M3: carry the actual Wilcoxon method and n_nonzero into the record row.
    let wilcoxon_method_str = match wilcoxon.method {
        WilcoxonMethod::ExactPratt => "ExactPratt".to_string(),
        WilcoxonMethod::NormalApproxPratt => "NormalApproxPratt".to_string(),
    };

    // Gated row: node_pass_rate (v1 sole gated metric).
    let gate_verdict = verdicts.iter().find(|v| v.metric == "node_pass_rate");
    rows.push(MetricRow {
        metric: "node_pass_rate".to_string(),
        tag: "gated".to_string(),
        baseline: baseline_agg.node_pass_rate,
        treatment: treatment_agg.node_pass_rate,
        delta: treatment_agg.node_pass_rate - baseline_agg.node_pass_rate,
        w: Some(wilcoxon.w),
        p_two_sided: Some(wilcoxon.p_two_sided),
        d_z: Some(dz),
        ci_lower: Some(ci.lower),
        ci_upper: Some(ci.upper),
        verdict: gate_verdict.map(|v| !v.regressed),
        wilcoxon_method: Some(wilcoxon_method_str),
        n_nonzero: Some(wilcoxon.n_nonzero),
    });

    // Tracked rows — carry values, never a verdict.
    // Only emit a row when the manifest declares the metric AND a wired source
    // produced data; do not emit fabricated zeros for un-wired metrics (I2).
    for metric_name in manifest.tracked_metrics() {
        match metric_name {
            "judge_quality" => {
                // Only emit when the judge seam actually produced data for both arms.
                if let (Some(bval), Some(tval)) =
                    (baseline_agg.judge_quality, treatment_agg.judge_quality)
                {
                    rows.push(MetricRow {
                        metric: "judge_quality".to_string(),
                        tag: "tracked".to_string(),
                        baseline: bval,
                        treatment: tval,
                        delta: tval - bval,
                        w: None,
                        p_two_sided: None,
                        d_z: None,
                        ci_lower: None,
                        ci_upper: None,
                        verdict: None,
                        wilcoxon_method: None,
                        n_nonzero: None,
                    });
                }
                // If the judge produced no data, omit the row entirely — do not emit
                // fake 0.0 values (judge_quality with StubJudge zero-total is still
                // real data; missing judge data means the seam returned no scores).
            }
            // engine_broken_rate: no v1 source wired; will return in v2 with real
            // engine telemetry. Removed to avoid shipping fabricated zeros.
            _ => {
                // Unknown tracked metric — skip rather than panic or fabricate.
            }
        }
    }

    // ── wellformed% / pass@1 / pass@2 ───────────────────────────────────────
    // Always emitted when there is non-skipped run data; not manifest-driven.
    if !baseline_statuses.is_empty() {
        let bl_wf = wellformed_pct(&baseline_statuses);
        let tr_wf = wellformed_pct(&treatment_statuses);
        let (bl_p1, bl_p2) = pass_at_1_2(&baseline_statuses, &baseline_attempts);
        let (tr_p1, tr_p2) = pass_at_1_2(&treatment_statuses, &treatment_attempts);

        for (metric, baseline, treatment) in [
            ("wellformed_pct", bl_wf, tr_wf),
            ("pass_at_1", bl_p1, tr_p1),
            ("pass_at_2", bl_p2, tr_p2),
        ] {
            rows.push(MetricRow {
                metric: metric.to_string(),
                tag: "tracked".to_string(),
                baseline,
                treatment,
                delta: treatment - baseline,
                w: None,
                p_two_sided: None,
                d_z: None,
                ci_lower: None,
                ci_upper: None,
                verdict: None,
                wilcoxon_method: None,
                n_nonzero: None,
            });
        }
    }

    // Compute final cost fields: None when any call did not report cost.
    let total_cost_usd = if cost_unknown { None } else { Some(cum_cost) };
    let baseline_cost_usd = if cost_unknown {
        None
    } else {
        Some(baseline_cost)
    };
    let treatment_cost_usd = if cost_unknown {
        None
    } else {
        Some(treatment_cost)
    };

    ResultRecord {
        name: manifest.name.clone(),
        ts: now_iso8601().unwrap_or_else(|e| format!("CLOCK-ERR:{e}")),
        reps: manifest.reps,
        seeds_honoured: any_non_skipped_run && all_non_skipped_seeds_honoured,
        rows,
        gate_exit,
        aborted: false,
        abort_reason: None,
        total_cost_usd,
        baseline_cost_usd,
        treatment_cost_usd,
        total_claude_calls,
        validity_warnings,
        inconclusive_count: inconclusive_pairs,
        inconclusive_fraction,
    }
}

// ── private helpers ──────────────────────────────────────────────────────────

/// Default `INCONCLUSIVE_MAX_FRACTION` (ADR-0041): a run that could not grade
/// more than this fraction of its paired-rep battery is not a valid measurement.
const INCONCLUSIVE_MAX_FRACTION_DEFAULT: f64 = 0.20;

/// Reads `INCONCLUSIVE_MAX_FRACTION` from the environment, falling back to
/// [`INCONCLUSIVE_MAX_FRACTION_DEFAULT`] on an absent or unparseable value.
/// Fail-open: a malformed knob never panics and never silently widens the
/// floor either — it reverts to the documented default.
fn inconclusive_max_fraction() -> f64 {
    std::env::var("INCONCLUSIVE_MAX_FRACTION")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(INCONCLUSIVE_MAX_FRACTION_DEFAULT)
}

/// `inconclusive / total`, or `0.0` when `total == 0` (no pairs attempted yet).
fn inconclusive_fraction_of(inconclusive: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        inconclusive as f64 / total as f64
    }
}

/// Construct an aborted `ResultRecord` with exit code 3 and no gate rows.
///
/// Used for all abort paths (LocalUnavailable, cost-cap, unknown-cost-with-cap,
/// and the A6 inconclusive-fraction floor).
#[allow(clippy::too_many_arguments)]
fn abort_record(
    manifest: &experiment::Manifest,
    reason: &str,
    cum_cost: f64,
    baseline_cost: f64,
    treatment_cost: f64,
    total_claude_calls: u64,
    validity_warnings: Vec<String>,
    inconclusive_count: u64,
    inconclusive_fraction: f64,
) -> ResultRecord {
    ResultRecord {
        name: manifest.name.clone(),
        ts: now_iso8601().unwrap_or_else(|e| format!("CLOCK-ERR:{e}")),
        reps: manifest.reps,
        seeds_honoured: false,
        rows: vec![],
        gate_exit: 3,
        aborted: true,
        abort_reason: Some(reason.to_string()),
        total_cost_usd: Some(cum_cost),
        baseline_cost_usd: Some(baseline_cost),
        treatment_cost_usd: Some(treatment_cost),
        total_claude_calls,
        validity_warnings,
        inconclusive_count,
        inconclusive_fraction,
    }
}

fn arm_label(arm: &experiment::ArmConfig) -> String {
    let ctx = match arm.context {
        experiment::ContextStrategy::Cxpak => "cxpak",
        experiment::ContextStrategy::None => "none",
    };
    format!("{} context:{}", arm.model, ctx)
}

fn failure_output() -> driver::RunOutput {
    driver::RunOutput {
        status: driver::RunStatus::Failure,
        accept_passed: false,
        edited_files: vec![],
        stdout_tail: "FAILURE".to_string(),
        duration_ms: 0,
        cost_usd: None,
        input_tokens: 0,
        output_tokens: 0,
        claude_calls: 0,
        num_turns: 0,
        seeds_honoured: false,
    }
}

/// Current time as an ISO 8601 UTC string, computed from the Unix epoch
/// without external dependencies.
///
/// Returns `Err` (with the `SystemTimeError` message) if the system clock
/// reports a time before the Unix epoch — a host misconfiguration, not a
/// measurement fault.  Callers must handle the error explicitly; a silent
/// fallback to 1970-01-01 would produce a misleading timestamp in the record.
fn now_iso8601() -> Result<String, String> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| epoch_to_iso8601(d.as_secs()))
        .map_err(|e| e.to_string())
}

fn epoch_to_iso8601(secs: u64) -> String {
    let time_of_day = secs % 86_400;
    let days = secs / 86_400;
    let hh = time_of_day / 3_600;
    let mm = (time_of_day % 3_600) / 60;
    let ss = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Civil calendar (Gregorian) from a Unix day count.
///
/// Uses Howard Hinnant's algorithm:
/// <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Shift epoch to a March-origin proleptic Gregorian calendar.
    let z = days as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // day of era [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32, m as u32, d as u32)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experiment::{ArmConfig, Backend, ContextStrategy, Manifest, MetricTag};
    use indexmap::IndexMap;
    use std::collections::BTreeMap;

    // ── test helpers ──────────────────────────────────────────────────────────

    fn manifest_1node_30rep() -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        Manifest {
            name: "test-dry".to_string(),
            reps: 30,
            seed_base: 1,
            battery: vec!["py-add".to_string()],
            baseline: ArmConfig {
                loop_name: "execute-node".to_string(),
                model: "model-none".to_string(),
                context: ContextStrategy::None,
                env: BTreeMap::default(),
                backend: Backend::Local,
            },
            treatment: ArmConfig {
                loop_name: "execute-node".to_string(),
                model: "model-cxpak".to_string(),
                context: ContextStrategy::Cxpak,
                env: BTreeMap::default(),
                backend: Backend::Local,
            },
            metrics,
            tolerance: BTreeMap::default(),
        }
    }

    // ── Cost test helpers ─────────────────────────────────────────────────────

    /// Scripted per-node output for cost tests.
    #[derive(Clone)]
    struct ScriptedOutput {
        status: driver::RunStatus,
        cost_usd: Option<f64>,
        num_turns: u64,
    }

    /// A test-only driver that returns a scripted `RunOutput` keyed by `node.id`.
    /// Falls back to Failure / Some(0.0) / 0 turns for unrecognised nodes.
    struct ScriptedStub {
        scripted: std::collections::HashMap<String, ScriptedOutput>,
    }

    impl driver::SessionDriver for ScriptedStub {
        fn run(
            &self,
            _arm: &experiment::ArmConfig,
            node: &corpus::NodeJson,
            _seed: u64,
        ) -> Result<driver::RunOutput, driver::DriverError> {
            let ScriptedOutput {
                status,
                cost_usd,
                num_turns,
            } = self
                .scripted
                .get(&node.id)
                .cloned()
                .unwrap_or(ScriptedOutput {
                    status: driver::RunStatus::Failure,
                    cost_usd: Some(0.0),
                    num_turns: 0,
                });
            let accept_passed = status == driver::RunStatus::Success;
            let claude_calls = cost_usd.map(|_| 1u64).unwrap_or(0);
            // Produce a realistic stdout_tail matching what execute_node.py emits
            // so wellformed_pct / pass@1 / pass@2 helpers can parse it correctly.
            let stdout_tail = match &status {
                driver::RunStatus::Success => "SUCCESS".to_string(),
                driver::RunStatus::Failure => "FAILURE(no_edit:scripted)".to_string(),
                driver::RunStatus::Skipped => "SKIPPED(scripted)".to_string(),
                driver::RunStatus::LocalUnavailable => "LOCAL_UNAVAILABLE".to_string(),
                driver::RunStatus::Inconclusive => "INCONCLUSIVE(scripted)".to_string(),
            };
            Ok(driver::RunOutput {
                status,
                accept_passed,
                edited_files: vec![],
                stdout_tail,
                duration_ms: 0,
                cost_usd,
                input_tokens: 0,
                output_tokens: 0,
                claude_calls,
                num_turns,
                seeds_honoured: true,
            })
        }
    }

    fn scripted_stub(entries: &[(&str, driver::RunStatus, Option<f64>, u64)]) -> ScriptedStub {
        ScriptedStub {
            scripted: entries
                .iter()
                .map(|(id, s, c, t)| {
                    (
                        id.to_string(),
                        ScriptedOutput {
                            status: s.clone(),
                            cost_usd: *c,
                            num_turns: *t,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Build a minimal `CorpusNode` with the given id for unit tests that do not
    /// need real corpus content (the ScriptedStub ignores everything except `node.id`).
    fn make_node(id: &str) -> corpus::CorpusNode {
        corpus::CorpusNode {
            meta: corpus::NodeMeta {
                id: id.to_string(),
                language: "python".to_string(),
                files: vec!["stub.py".to_string()],
                accept: "true".to_string(),
                forbid: vec![],
                change: None,
                requires: vec![],
            },
            dir: std::path::PathBuf::from("/tmp"),
            seed: vec![
                ("stub.py".to_string(), String::new()),
                ("acceptance_test.py".to_string(), String::new()),
            ],
            context: None,
        }
    }

    /// Manifest with N nodes (by id), 1 rep, Local baseline / ClaudeCli treatment.
    fn manifest_n_nodes(ids: &[&str]) -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        Manifest {
            name: "cost-test".into(),
            reps: 1,
            seed_base: 1,
            battery: ids.iter().map(|s| s.to_string()).collect(),
            baseline: ArmConfig {
                loop_name: "execute-node".into(),
                model: "local".into(),
                context: ContextStrategy::None,
                backend: Backend::Local,
                env: BTreeMap::default(),
            },
            treatment: ArmConfig {
                loop_name: "execute-node".into(),
                model: "claude-haiku-4-5".into(),
                context: ContextStrategy::None,
                backend: Backend::ClaudeCli,
                env: BTreeMap::default(),
            },
            metrics,
            tolerance: BTreeMap::default(),
        }
    }

    /// Manifest with N nodes (by id), 1 rep, both arms Local (baseline
    /// context:None, treatment context:Cxpak) — the pairing dimension the
    /// [`AsymmetricStub`] below dispatches on.
    fn manifest_n_nodes_both_local(ids: &[&str]) -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        Manifest {
            name: "inconclusive-test".into(),
            reps: 1,
            seed_base: 1,
            battery: ids.iter().map(|s| s.to_string()).collect(),
            baseline: ArmConfig {
                loop_name: "execute-node".into(),
                model: "local".into(),
                context: ContextStrategy::None,
                backend: Backend::Local,
                env: BTreeMap::default(),
            },
            treatment: ArmConfig {
                loop_name: "execute-node".into(),
                model: "local".into(),
                context: ContextStrategy::Cxpak,
                backend: Backend::Local,
                env: BTreeMap::default(),
            },
            metrics,
            tolerance: BTreeMap::default(),
        }
    }

    /// A test-only driver that returns an independently-scripted status per
    /// arm (keyed by `node.id`), so a test can put one arm of a pair in
    /// `Inconclusive` while the other succeeds — the asymmetry A5's
    /// pair-level exclusion must catch (a per-arm drop would miss it).
    struct AsymmetricStub {
        // node id -> (baseline_status, treatment_status)
        scripted: std::collections::HashMap<String, (driver::RunStatus, driver::RunStatus)>,
    }

    impl driver::SessionDriver for AsymmetricStub {
        fn run(
            &self,
            arm: &ArmConfig,
            node: &corpus::NodeJson,
            _seed: u64,
        ) -> Result<driver::RunOutput, driver::DriverError> {
            let (baseline_status, treatment_status) = self
                .scripted
                .get(&node.id)
                .cloned()
                .unwrap_or((driver::RunStatus::Failure, driver::RunStatus::Failure));
            let status = match arm.context {
                ContextStrategy::None => baseline_status,
                ContextStrategy::Cxpak => treatment_status,
            };
            let accept_passed = status == driver::RunStatus::Success;
            let stdout_tail = match &status {
                driver::RunStatus::Success => "SUCCESS".to_string(),
                driver::RunStatus::Failure => "FAILURE(no_edit:scripted)".to_string(),
                driver::RunStatus::Skipped => "SKIPPED(scripted)".to_string(),
                driver::RunStatus::LocalUnavailable => "LOCAL_UNAVAILABLE".to_string(),
                driver::RunStatus::Inconclusive => "INCONCLUSIVE(scripted)".to_string(),
            };
            Ok(driver::RunOutput {
                status,
                accept_passed,
                edited_files: vec![],
                stdout_tail,
                duration_ms: 0,
                cost_usd: Some(0.0),
                input_tokens: 0,
                output_tokens: 0,
                claude_calls: 0,
                num_turns: 0,
                seeds_honoured: false,
            })
        }
    }

    /// A test-only driver whose `RunOutput::seeds_honoured` is fixed per arm
    /// (dispatched on `arm.context`, matching the `manifest_n_nodes_both_local`
    /// pairing dimension) — exercises `run_experiment`'s aggregate
    /// `seeds_honoured` field independently of `ScriptedStub`'s tuple-shaped
    /// scripting API.
    struct SeedsHonouredStub {
        baseline_honoured: bool,
        treatment_honoured: bool,
    }

    impl driver::SessionDriver for SeedsHonouredStub {
        fn run(
            &self,
            arm: &ArmConfig,
            _node: &corpus::NodeJson,
            _seed: u64,
        ) -> Result<driver::RunOutput, driver::DriverError> {
            let seeds_honoured = match arm.context {
                ContextStrategy::None => self.baseline_honoured,
                ContextStrategy::Cxpak => self.treatment_honoured,
            };
            Ok(driver::RunOutput {
                status: driver::RunStatus::Success,
                accept_passed: true,
                edited_files: vec![],
                stdout_tail: "SUCCESS".to_string(),
                duration_ms: 0,
                cost_usd: Some(0.0),
                input_tokens: 0,
                output_tokens: 0,
                claude_calls: 0,
                num_turns: 0,
                seeds_honoured,
            })
        }
    }

    fn zero_baseline() -> score::Baseline {
        let mut g = BTreeMap::new();
        g.insert("node_pass_rate".into(), 0.0);
        score::Baseline {
            name: "cost-test".into(),
            gated: g,
        }
    }

    /// Non-zero baseline (0.70) — a zero baseline masks the BUG1 fabricated-gate
    /// failure mode (0.0 observed never regresses against a 0.0 baseline), so
    /// tests that must distinguish "aborted" from "gate fabricated a pass/fail
    /// verdict off empty data" need this instead of [`zero_baseline`].
    fn baseline_070() -> score::Baseline {
        let mut g = BTreeMap::new();
        g.insert("node_pass_rate".into(), 0.70);
        score::Baseline {
            name: "cost-test".into(),
            gated: g,
        }
    }

    fn zero_judge() -> judge::StubJudge {
        judge::StubJudge {
            canned: judge::JudgeScore {
                per_criterion: Default::default(),
                total: 0,
            },
        }
    }

    // ── seed determinism ──────────────────────────────────────────────────────

    #[test]
    fn seed_is_deterministic_and_blocks_pairs() {
        // Same (base, node, rep) → same seed (both arms share it → the pairing dimension).
        assert_eq!(seed_for(1, "py-add", 0), seed_for(1, "py-add", 0));
        // Different rep / node / base → (almost surely) different seed.
        assert_ne!(seed_for(1, "py-add", 0), seed_for(1, "py-add", 1));
        assert_ne!(seed_for(1, "py-add", 0), seed_for(1, "py-sub", 0));
        assert_ne!(seed_for(1, "py-add", 0), seed_for(2, "py-add", 0));
    }

    // ── cross-loop test fixtures ──────────────────────────────────────────────

    fn cross_loop_manifest_1node_2rep() -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        Manifest {
            name: "cross-loop-test".into(),
            reps: 2,
            seed_base: 1,
            battery: vec!["py-add".to_string()],
            baseline: ArmConfig {
                loop_name: "execute-node".into(),
                model: "local".into(),
                context: ContextStrategy::None,
                backend: Backend::Local,
                env: BTreeMap::default(),
            },
            treatment: ArmConfig {
                loop_name: "execute-node".into(),
                model: "claude-haiku-4-5".into(),
                context: ContextStrategy::None,
                backend: Backend::ClaudeCli,
                env: BTreeMap::default(),
            },
            metrics,
            tolerance: BTreeMap::default(),
        }
    }

    #[test]
    fn dry_run_projects_claude_cli_calls_cross_loop() {
        // 1 claude-cli arm × 1 node × 2 reps × 2 retry-ladder = 4.
        let d = project(&cross_loop_manifest_1node_2rep(), 0, 0);
        assert_eq!(
            d.projected_claude_calls, 4,
            "1 ClaudeCli arm × 1 node × 2 reps × MAX_RETRY_LADDER(2)"
        );
    }

    #[test]
    fn dry_run_zero_projected_calls_for_all_local() {
        // Both arms Local → 0 projected claude-cli calls.
        let d = project(&manifest_1node_30rep(), 0, 0);
        assert_eq!(d.projected_claude_calls, 0);
    }

    #[test]
    fn dry_run_both_arms_claude_cli() {
        // Two claude-cli arms × 1 node × 1 rep × 2 = 4.
        let mut m = cross_loop_manifest_1node_2rep();
        m.reps = 1;
        m.baseline.backend = Backend::ClaudeCli;
        m.baseline.model = "claude-haiku-4-5".into();
        let d = project(&m, 0, 0);
        assert_eq!(
            d.projected_claude_calls, 4,
            "2 ClaudeCli arms × 1 node × 1 rep × MAX_RETRY_LADDER(2)"
        );
    }

    // ── dry-run projection ────────────────────────────────────────────────────

    #[test]
    fn dry_run_projects_call_counts() {
        // reps=30, 1 node, 2 arms → loop_runs = 1*30*2 = 60.
        // judged_tasks=1, judge_reps=3, 2 arms → judge_calls = 1*3*2 = 6.
        let d = project(&manifest_1node_30rep(), 1, 3);
        assert_eq!(d.loop_runs, 60);
        assert_eq!(d.judge_calls, 6);
        assert!(
            d.baseline.contains("none") && d.treatment.contains("cxpak"),
            "labels must contain context strategy names; baseline={:?} treatment={:?}",
            d.baseline,
            d.treatment
        );
    }

    #[test]
    fn dry_run_multi_node() {
        let mut m = manifest_1node_30rep();
        m.battery.push("py-sub".to_string());
        let d = project(&m, 0, 0);
        // 2 nodes, 30 reps, 2 arms → 120
        assert_eq!(d.loop_runs, 120);
        assert_eq!(d.judge_calls, 0);
    }

    // ── ISO 8601 timestamp ────────────────────────────────────────────────────

    #[test]
    fn epoch_to_iso8601_known_date() {
        // 1970-01-01T00:00:00Z = epoch 0
        assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00Z");
        // 1970-01-01T00:00:01Z = epoch 1
        assert_eq!(epoch_to_iso8601(1), "1970-01-01T00:00:01Z");
        // 2001-09-09T01:46:40Z = epoch 1_000_000_000
        assert_eq!(epoch_to_iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn now_iso8601_is_plausible() {
        let ts = now_iso8601().expect("system clock must be after Unix epoch");
        // Must start with a year ≥ 2025 and match basic ISO shape.
        assert!(ts.starts_with("20"), "ts={ts}");
        assert_eq!(ts.len(), 20, "ISO 8601 UTC must be 20 chars; ts={ts}");
        assert!(ts.ends_with('Z'), "must end in Z; ts={ts}");
    }

    // ── Cost accumulation ─────────────────────────────────────────────────────

    #[test]
    fn cost_accumulates_over_nodes_and_arms() {
        // 2 nodes, 1 rep, both arms return 0.01 USD per call.
        // Expected: baseline_cost = 0.02, treatment_cost = 0.02, total = 0.04,
        // total_claude_calls = 4 (2 nodes × 1 rep × 2 arms × 1 call each).
        let stub = scripted_stub(&[
            ("a", driver::RunStatus::Success, Some(0.01_f64), 0),
            ("b", driver::RunStatus::Success, Some(0.01_f64), 0),
        ]);
        let m = manifest_n_nodes(&["a", "b"]);
        let nodes = vec![make_node("a"), make_node("b")];
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            !rec.aborted,
            "must not abort; abort_reason={:?}",
            rec.abort_reason
        );

        let total = rec.total_cost_usd.expect("total_cost_usd must be Some");
        assert!(
            (total - 0.04).abs() < 1e-9,
            "expected total ≈ 0.04, got {total}"
        );

        let bl = rec
            .baseline_cost_usd
            .expect("baseline_cost_usd must be Some");
        assert!(
            (bl - 0.02).abs() < 1e-9,
            "expected baseline ≈ 0.02, got {bl}"
        );

        let tr = rec
            .treatment_cost_usd
            .expect("treatment_cost_usd must be Some");
        assert!(
            (tr - 0.02).abs() < 1e-9,
            "expected treatment ≈ 0.02, got {tr}"
        );

        assert_eq!(
            rec.total_claude_calls, 4,
            "2 nodes × 1 rep × 2 arms × 1 call = 4"
        );
        assert!(rec.validity_warnings.is_empty(), "no warnings expected");
    }

    // ── seeds_honoured aggregate (truthful replacement for the v1 hardcoded
    //    `false`) ───────────────────────────────────────────────────────────

    #[test]
    fn run_experiment_seeds_honoured_true_when_all_runs_honoured() {
        let m = manifest_n_nodes_both_local(&["a"]);
        let nodes = vec![make_node("a")];
        let stub = SeedsHonouredStub {
            baseline_honoured: true,
            treatment_honoured: true,
        };
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &baseline_070(), &opts);

        assert!(
            !rec.aborted,
            "must not abort; abort_reason={:?}",
            rec.abort_reason
        );
        assert!(
            rec.seeds_honoured,
            "every non-Skipped run honoured its seed — record must report true"
        );
    }

    #[test]
    fn run_experiment_seeds_honoured_false_when_an_arm_is_greedy() {
        let m = manifest_n_nodes_both_local(&["a"]);
        let nodes = vec![make_node("a")];
        let stub = SeedsHonouredStub {
            baseline_honoured: true,
            treatment_honoured: false,
        };
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &baseline_070(), &opts);

        assert!(
            !rec.aborted,
            "must not abort; abort_reason={:?}",
            rec.abort_reason
        );
        assert!(
            !rec.seeds_honoured,
            "treatment arm ran greedy (seed not honoured) — record must report false"
        );
    }

    #[test]
    fn missing_gated_metric_in_baseline_aborts_not_false_pass() {
        // A gated metric (node_pass_rate) absent from the baseline JSON must abort as a
        // setup fault (exit 3) naming the metric — NOT silently yield gate_exit 0, which
        // would report a PASS for a comparison that never ran (the false-PASS hole).
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, Some(0.0_f64), 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };
        let empty_baseline = score::Baseline {
            name: "missing-gated".into(),
            gated: BTreeMap::new(), // no node_pass_rate entry
        };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &empty_baseline, &opts);

        assert!(
            rec.aborted,
            "a gated metric missing from the baseline must abort, not fabricate a pass"
        );
        assert_eq!(rec.gate_exit, 3, "aborted → gate_exit must be 3, never 0 (false PASS)");
        assert!(rec.rows.is_empty(), "aborted record must have no gate rows");
        let reason = rec.abort_reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("node_pass_rate"),
            "abort reason must name the missing gated metric, got: {reason}"
        );
    }

    #[test]
    fn max_cost_abort_fires_when_exceeded() {
        // 1 node, each arm costs 0.1 → cum_cost after both arms = 0.2 > cap 0.05 → abort.
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, Some(0.1_f64), 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions {
            max_cost: Some(0.05),
        };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(rec.aborted, "must abort when cost cap exceeded");
        assert_eq!(rec.gate_exit, 3, "aborted → gate_exit must be 3");
        assert!(rec.rows.is_empty(), "aborted record must have no gate rows");

        let reason = rec
            .abort_reason
            .as_deref()
            .expect("abort_reason must be Some");
        assert!(
            reason.contains("cost cap"),
            "abort_reason must mention 'cost cap'; got: {reason}"
        );
        // Confirm the accumulated cost is reported (both arms ran: 0.1 + 0.1 = 0.2).
        let reported = rec
            .total_cost_usd
            .expect("total_cost_usd must be Some on cost abort");
        assert!(
            (reported - 0.2).abs() < 1e-9,
            "reported cost should be ≈ 0.2 (both arms ran), got {reported}"
        );
    }

    #[test]
    fn unknown_cost_with_max_cost_active_aborts() {
        // Driver returns cost_usd=None; --max-cost active → abort immediately.
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, None, 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions {
            max_cost: Some(100.0),
        };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            rec.aborted,
            "must abort when cost unknown and max_cost active"
        );
        assert_eq!(rec.gate_exit, 3, "aborted → gate_exit must be 3");
        assert!(rec.rows.is_empty(), "aborted record must have no gate rows");

        let reason = rec
            .abort_reason
            .as_deref()
            .expect("abort_reason must be Some");
        assert!(
            reason.contains("did not report cost"),
            "abort_reason must mention 'did not report cost'; got: {reason}"
        );
    }

    #[test]
    fn unknown_cost_without_max_cost_continues() {
        // Driver returns cost_usd=None; no --max-cost → run completes, total_cost_usd=None.
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, None, 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(!rec.aborted, "must not abort when max_cost not active");
        assert!(
            rec.total_cost_usd.is_none(),
            "total_cost_usd must be None when any cost was unknown"
        );
        assert!(
            rec.baseline_cost_usd.is_none(),
            "baseline_cost_usd must be None when cost unknown"
        );
        assert!(
            rec.treatment_cost_usd.is_none(),
            "treatment_cost_usd must be None when cost unknown"
        );
    }

    #[test]
    fn turns_leaked_predicate() {
        // No leak when turns match calls (single-turn-per-call ladder), including
        // the legitimate 2-attempt RED-baseline case that `> 1` used to false-fire on.
        assert!(!turns_leaked(0, 0), "local/mock arm (0 turns, 0 calls)");
        assert!(!turns_leaked(1, 1), "one single-turn call");
        assert!(!turns_leaked(2, 2), "two single-turn retries — NOT a leak");
        // Leak: a call went multi-turn (more turns than calls).
        assert!(turns_leaked(2, 1), "one call, two turns — leaked");
        assert!(turns_leaked(3, 2), "two calls, three turns — leaked");
    }

    #[test]
    fn num_turns_warning_recorded_when_turns_exceeds_one() {
        // Driver returns num_turns=2 with cost Some → claude_calls=1, so turns(2) >
        // calls(1): a leaked multi-turn call. validity_warnings must name the node.
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, Some(0.01_f64), 2)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(!rec.aborted, "must not abort due to num_turns alone");
        assert!(
            !rec.validity_warnings.is_empty(),
            "num_turns > 1 must produce at least one validity warning"
        );
        let names_node = rec
            .validity_warnings
            .iter()
            .any(|w| w.contains("a") && w.contains("turns"));
        assert!(
            names_node,
            "validity_warning must name the node and mention turns; warnings={:?}",
            rec.validity_warnings
        );
    }

    // ── wellformed_pct oracle cases ───────────────────────────────────────────

    #[test]
    fn wellformed_pct_half_wellformed() {
        // SUCCESS + FAILURE(no_edit:) → remaining=2, wellformed=1 → 0.5
        let statuses = vec!["SUCCESS".to_string(), "FAILURE(no_edit:)".to_string()];
        let pct = wellformed_pct(&statuses);
        assert!((pct - 0.5).abs() < 1e-12, "expected 0.5, got {pct}");
    }

    #[test]
    fn wellformed_pct_empty_returns_zero() {
        assert_eq!(wellformed_pct(&[]), 0.0);
    }

    #[test]
    fn wellformed_pct_drops_excluded_prefixes_and_inconclusive() {
        // All entries fall into the drop set; remaining is empty → 0.0.
        let statuses = vec![
            "SKIPPED(already_satisfied)".to_string(),
            "LOCAL_UNAVAILABLE".to_string(),
            "FAILURE(commit_error:x)".to_string(),
            "FAILURE(dirty_tree:)".to_string(),
            "Inconclusive".to_string(),
        ];
        assert_eq!(
            wellformed_pct(&statuses),
            0.0,
            "all-excluded statuses must yield 0.0"
        );
    }

    #[test]
    fn wellformed_pct_verify_error_counts_as_wellformed() {
        // SUCCESS and FAILURE(verify_error…) are both well-formed.
        let statuses = vec![
            "SUCCESS".to_string(),
            "FAILURE(verify_error:bad_output)".to_string(),
            "FAILURE(no_edit:)".to_string(),
            "SKIPPED(already_satisfied)".to_string(), // dropped from denominator
        ];
        // remaining: ["SUCCESS", "FAILURE(verify_error…)", "FAILURE(no_edit:)"] → 3
        // wellformed: 2 → 2/3
        let pct = wellformed_pct(&statuses);
        assert!((pct - 2.0 / 3.0).abs() < 1e-12, "expected 2/3, got {pct}");
    }

    // ── pass_at_1_2 oracle cases ──────────────────────────────────────────────

    #[test]
    fn pass_at_1_2_oracle_case() {
        // 3 runs: 2 SUCCESS (one first-attempt, one retry), 1 FAILURE.
        let statuses = vec![
            "SUCCESS".to_string(),
            "SUCCESS".to_string(),
            "FAILURE(no_edit:)".to_string(),
        ];
        let attempts = vec![1u64, 2u64, 0u64];
        let (p1, p2) = pass_at_1_2(&statuses, &attempts);
        assert!(
            (p1 - 1.0 / 3.0).abs() < 1e-12,
            "pass@1 expected 1/3, got {p1}"
        );
        assert!(
            (p2 - 2.0 / 3.0).abs() < 1e-12,
            "pass@2 expected 2/3, got {p2}"
        );
    }

    #[test]
    fn pass_at_1_2_empty_returns_zeros() {
        let (p1, p2) = pass_at_1_2(&[], &[]);
        assert_eq!(p1, 0.0);
        assert_eq!(p2, 0.0);
    }

    #[test]
    fn pass_at_1_2_zero_attempts_counts_as_first_attempt() {
        // Local runs have claude_calls=0; 0 <= 1 so they qualify for pass@1.
        let statuses = vec!["SUCCESS".to_string()];
        let attempts = vec![0u64];
        let (p1, p2) = pass_at_1_2(&statuses, &attempts);
        assert!(
            (p1 - 1.0).abs() < 1e-12,
            "zero attempts qualifies for pass@1"
        );
        assert!((p2 - 1.0).abs() < 1e-12);
    }

    // ── run_experiment tracked rows ───────────────────────────────────────────

    #[test]
    fn run_experiment_emits_wellformed_and_pass_tracked_rows() {
        let stub = scripted_stub(&[("a", driver::RunStatus::Success, Some(0.0), 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };
        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            !rec.aborted,
            "must not abort; reason={:?}",
            rec.abort_reason
        );
        let row_names: Vec<&str> = rec.rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(
            row_names.contains(&"wellformed_pct"),
            "wellformed_pct row must be present; rows={row_names:?}"
        );
        assert!(
            row_names.contains(&"pass_at_1"),
            "pass_at_1 row must be present; rows={row_names:?}"
        );
        assert!(
            row_names.contains(&"pass_at_2"),
            "pass_at_2 row must be present; rows={row_names:?}"
        );
        // Success with 0 claude_calls → wellformed=1.0, pass@1=1.0, pass@2=1.0
        let wf = rec
            .rows
            .iter()
            .find(|r| r.metric == "wellformed_pct")
            .unwrap();
        assert!(
            (wf.baseline - 1.0).abs() < 1e-12,
            "SUCCESS must be 100% wellformed; got baseline={}",
            wf.baseline
        );
        let p1 = rec.rows.iter().find(|r| r.metric == "pass_at_1").unwrap();
        assert!(
            (p1.baseline - 1.0).abs() < 1e-12,
            "0-attempt SUCCESS must be pass@1=1.0; got {}",
            p1.baseline
        );
    }

    #[test]
    fn skipped_pair_excluded_from_all_scoring() {
        // StubDriver returns Skipped for node "a" → the pair must be excluded
        // from scoring. This node is the ONLY node in the battery, so `pairs`
        // ends up empty — per the BUG1 fix, an experiment with zero gradable
        // pairs must abort (exit 3), never fabricate a gate row (previously
        // this returned a non-aborted record with a fabricated node_pass_rate
        // of 0.0 vs a 0.0 baseline, which happened not to read as a regression
        // only because the baseline here is zero — see
        // `all_skipped_battery_aborts_instead_of_fabricating_gate_verdict` for
        // the non-zero-baseline case this masked). Updated to assert the
        // corrected behaviour rather than the old fabrication.
        let mut scripted = std::collections::HashMap::new();
        scripted.insert("a".to_string(), driver::RunStatus::Skipped);
        let stub = driver::StubDriver { scripted };
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };
        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            rec.aborted,
            "an all-Skipped, zero-pair battery must abort rather than \
             fabricate a gate verdict; gate_exit={}",
            rec.gate_exit
        );
        // No gradable data at all → no gate rows, no tracked rows.
        assert!(
            rec.rows.is_empty(),
            "aborted record must carry no gate rows; rows={:?}",
            rec.rows
        );
    }

    #[test]
    fn local_unavailable_precedence_over_cost_abort() {
        // Driver returns LocalUnavailable; --max-cost is active with a large cap.
        // LocalUnavailable MUST abort with "unavailable" reason, not a cost reason.
        let stub = scripted_stub(&[("a", driver::RunStatus::LocalUnavailable, Some(0.0_f64), 0)]);
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        // Large cap so cost alone would not trigger abort.
        let opts = RunOptions {
            max_cost: Some(1_000.0),
        };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(rec.aborted, "LocalUnavailable must abort the experiment");
        assert_eq!(rec.gate_exit, 3, "aborted → gate_exit must be 3");
        assert!(rec.rows.is_empty(), "aborted record must have no gate rows");

        let reason = rec
            .abort_reason
            .as_deref()
            .expect("abort_reason must be Some");
        assert!(
            reason.contains("unavailable"),
            "abort_reason must mention 'unavailable'; got: {reason}"
        );
        assert!(
            !reason.contains("cost cap"),
            "abort must not be attributed to cost cap; got: {reason}"
        );
    }

    // ── A5: pair-level Inconclusive exclusion ─────────────────────────────────

    #[test]
    fn inconclusive_pair_excluded_symmetrically_not_per_arm() {
        // 10 nodes, 1 rep. n0: baseline Success, treatment Inconclusive — the pair
        // must be dropped entirely. n1..n9: baseline Failure, treatment Success.
        //
        // Correct pair-level exclusion (A5) drops n0 from BOTH arms, so baseline's
        // node_pass_rate is 0.0 (all 9 surviving baselines are Failure). A wrong
        // per-arm drop would instead let n0's baseline Success (1.0) survive on
        // the baseline side while dropping only the treatment side, reading 0.1.
        let mut scripted = std::collections::HashMap::new();
        scripted.insert(
            "n0".to_string(),
            (driver::RunStatus::Success, driver::RunStatus::Inconclusive),
        );
        for i in 1..10 {
            scripted.insert(
                format!("n{i}"),
                (driver::RunStatus::Failure, driver::RunStatus::Success),
            );
        }
        let stub = AsymmetricStub { scripted };
        let ids: Vec<String> = (0..10).map(|i| format!("n{i}")).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let m = manifest_n_nodes_both_local(&id_refs);
        let nodes: Vec<_> = ids.iter().map(|id| make_node(id)).collect();
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        // 1/10 = 10% is under the default 20% floor — this is also the A6
        // "below-threshold proceeds with exclusion" case.
        assert!(
            !rec.aborted,
            "10% inconclusive fraction must stay under the default 20% floor; reason={:?}",
            rec.abort_reason
        );
        assert_eq!(
            rec.inconclusive_count, 1,
            "exactly one pair (n0) is Inconclusive"
        );
        assert!(
            (rec.inconclusive_fraction - 0.1).abs() < 1e-9,
            "1/10 pairs excluded → fraction 0.1; got {}",
            rec.inconclusive_fraction
        );

        let row = rec
            .rows
            .iter()
            .find(|r| r.metric == "node_pass_rate")
            .expect("gated row must be present");
        assert!(
            (row.baseline - 0.0).abs() < 1e-9,
            "n0's baseline Success must be excluded pair-level, not survive a per-arm drop; got {}",
            row.baseline
        );
        assert!(
            (row.treatment - 1.0).abs() < 1e-9,
            "all 9 surviving treatment runs are Success; got {}",
            row.treatment
        );
    }

    // ── A6: INCONCLUSIVE_MAX_FRACTION fail-loud floor ─────────────────────────

    #[test]
    fn inconclusive_fraction_above_default_threshold_aborts_exit_3() {
        // 4 nodes, 1 rep; 2 Inconclusive out of 4 pairs = 50% > default 20% floor.
        let mut scripted = std::collections::HashMap::new();
        scripted.insert(
            "n0".to_string(),
            (driver::RunStatus::Inconclusive, driver::RunStatus::Success),
        );
        scripted.insert(
            "n1".to_string(),
            (driver::RunStatus::Inconclusive, driver::RunStatus::Success),
        );
        scripted.insert(
            "n2".to_string(),
            (driver::RunStatus::Success, driver::RunStatus::Success),
        );
        scripted.insert(
            "n3".to_string(),
            (driver::RunStatus::Success, driver::RunStatus::Success),
        );
        let stub = AsymmetricStub { scripted };
        let m = manifest_n_nodes_both_local(&["n0", "n1", "n2", "n3"]);
        let nodes = vec![
            make_node("n0"),
            make_node("n1"),
            make_node("n2"),
            make_node("n3"),
        ];
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            rec.aborted,
            "50% inconclusive fraction must breach the 20% floor and abort"
        );
        assert_eq!(
            rec.gate_exit, 3,
            "floor breach aborts exit 3, same machinery as systemic LocalUnavailable"
        );
        assert!(
            rec.rows.is_empty(),
            "aborted record must carry no gate rows"
        );
        assert_eq!(rec.inconclusive_count, 2);
        assert!(
            (rec.inconclusive_fraction - 0.5).abs() < 1e-9,
            "2/4 pairs excluded → fraction 0.5; got {}",
            rec.inconclusive_fraction
        );
        let reason = rec
            .abort_reason
            .as_deref()
            .expect("abort_reason must be Some");
        assert!(
            reason.contains("inconclusive fraction"),
            "abort_reason must mention the inconclusive floor; got: {reason}"
        );
        assert!(
            rec.validity_warnings
                .iter()
                .any(|w| w.contains("inconclusive fraction")),
            "validity_warnings must record the floor breach; got: {:?}",
            rec.validity_warnings
        );
    }

    // ── BUG1: all-Skipped battery must abort, not fabricate a gate verdict ────

    #[test]
    fn all_skipped_battery_aborts_instead_of_fabricating_gate_verdict() {
        // Every pair is Skipped (missing toolchain) → `pairs` ends up empty.
        // With a NON-ZERO baseline (0.70), a fabricated node_pass_rate of 0.0
        // would read as a regression (gate_exit=1) — indistinguishable from a
        // real regression. `skipped_pair_excluded_from_all_scoring` uses a
        // zero baseline, which masks this failure mode (0.0 vs 0.0 baseline
        // never regresses) — this test is deliberately distinct.
        let mut scripted = std::collections::HashMap::new();
        scripted.insert("a".to_string(), driver::RunStatus::Skipped);
        let stub = driver::StubDriver { scripted };
        let m = manifest_n_nodes(&["a"]);
        let nodes = vec![make_node("a")];
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &baseline_070(), &opts);

        assert!(
            rec.aborted,
            "an all-Skipped battery has no gradable data and must abort, not \
             fabricate a gate row; gate_exit={}, rows={:?}",
            rec.gate_exit, rec.rows
        );
        assert_eq!(rec.gate_exit, 3, "abort path must report exit 3");
        assert!(
            rec.rows.is_empty(),
            "aborted record must carry no gate rows; rows={:?}",
            rec.rows
        );
        let reason = rec
            .abort_reason
            .as_deref()
            .expect("abort_reason must be Some");
        assert!(
            reason.contains("no gradable pairs"),
            "abort_reason must explain no gradable pairs; got: {reason}"
        );
    }

    // ── BUG2: inconclusive fraction must be computed over attempted-gradable
    // pairs, not diluted by Skipped pairs in the denominator ──────────────────

    #[test]
    fn inconclusive_fraction_computed_over_attempted_not_total_with_dilutive_skips() {
        // 20 Skipped + 2 Inconclusive + 2 gradable (Success/Success) = 24 total
        // pairs. Attempted-gradable = 24 - 20(skipped) = 4; inconclusive/attempted
        // = 2/4 = 0.5, which breaches the default 20% floor and must abort.
        // The buggy denominator (raw total_pairs) computes 2/24 ≈ 0.083 < 0.20
        // and would NOT abort — this test pins the correct (attempted-only)
        // denominator.
        let mut scripted = std::collections::HashMap::new();
        for i in 0..20 {
            scripted.insert(
                format!("skip{i}"),
                (driver::RunStatus::Skipped, driver::RunStatus::Skipped),
            );
        }
        scripted.insert(
            "inc0".to_string(),
            (
                driver::RunStatus::Inconclusive,
                driver::RunStatus::Inconclusive,
            ),
        );
        scripted.insert(
            "inc1".to_string(),
            (
                driver::RunStatus::Inconclusive,
                driver::RunStatus::Inconclusive,
            ),
        );
        scripted.insert(
            "ok0".to_string(),
            (driver::RunStatus::Success, driver::RunStatus::Success),
        );
        scripted.insert(
            "ok1".to_string(),
            (driver::RunStatus::Success, driver::RunStatus::Success),
        );

        let mut ids: Vec<String> = (0..20).map(|i| format!("skip{i}")).collect();
        ids.push("inc0".into());
        ids.push("inc1".into());
        ids.push("ok0".into());
        ids.push("ok1".into());

        let stub = AsymmetricStub { scripted };
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let m = manifest_n_nodes_both_local(&id_refs);
        let nodes: Vec<_> = ids.iter().map(|id| make_node(id)).collect();
        let opts = RunOptions { max_cost: None };

        let rec = run_experiment(&m, &nodes, &stub, &zero_judge(), &zero_baseline(), &opts);

        assert!(
            rec.aborted,
            "2/4 attempted-gradable inconclusive (50%) must breach the 20% \
             floor even though 20 skips dilute the naive total; reason={:?}",
            rec.abort_reason
        );
        assert_eq!(rec.inconclusive_count, 2);
        assert!(
            (rec.inconclusive_fraction - 0.5).abs() < 1e-9,
            "fraction must be computed over attempted-gradable pairs \
             (2/4=0.5), not raw total (2/24≈0.083); got {}",
            rec.inconclusive_fraction
        );
    }

    // ── INCONCLUSIVE_MAX_FRACTION env parsing (fail-open) ─────────────────────
    //
    // Serialised via a Mutex: these are the only tests in this module that
    // mutate the process-wide `INCONCLUSIVE_MAX_FRACTION` env var, and every
    // other test in this file relies on it being absent (default 0.20).

    static INCONCLUSIVE_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn inconclusive_max_fraction_defaults_to_0_20_when_absent() {
        let _guard = INCONCLUSIVE_ENV_GUARD.lock().unwrap();
        std::env::remove_var("INCONCLUSIVE_MAX_FRACTION");
        assert!((inconclusive_max_fraction() - 0.20).abs() < 1e-12);
    }

    #[test]
    fn inconclusive_max_fraction_falls_open_on_unparseable_value() {
        let _guard = INCONCLUSIVE_ENV_GUARD.lock().unwrap();
        std::env::set_var("INCONCLUSIVE_MAX_FRACTION", "not-a-number");
        let got = inconclusive_max_fraction();
        std::env::remove_var("INCONCLUSIVE_MAX_FRACTION");
        assert!(
            (got - 0.20).abs() < 1e-12,
            "unparseable value must fall back to default 0.20; got {got}"
        );
    }

    #[test]
    fn inconclusive_max_fraction_reads_valid_override() {
        let _guard = INCONCLUSIVE_ENV_GUARD.lock().unwrap();
        std::env::set_var("INCONCLUSIVE_MAX_FRACTION", "0.5");
        let got = inconclusive_max_fraction();
        std::env::remove_var("INCONCLUSIVE_MAX_FRACTION");
        assert!(
            (got - 0.5).abs() < 1e-12,
            "valid override must be honoured; got {got}"
        );
    }
}
