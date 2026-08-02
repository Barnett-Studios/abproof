//! Score — numeric metrics derived from driver outcomes, arm aggregation,
//! and gate/track verdict against a committed baseline.

use crate::driver::{RunOutput, RunStatus};
use crate::experiment::Manifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One rep's paired outcome for one node (both arms at the same seed/block).
#[derive(Debug, Clone)]
pub struct PairedRep {
    pub node_id: String,
    pub rep: u32,
    pub baseline_pass: f64,
    pub treatment_pass: f64,
}

/// Per-node pass-rate deltas for the paired test (issue #7, D2).
///
/// The node is the unit of replication: each node's `reps` are aggregated into ONE paired
/// observation — the mean pass score per arm — and the delta is `mean(treatment) −
/// mean(baseline) ∈ [−1, 1]`. `reps` correlated observations of one node are not `reps`
/// independent observations; feeding them flat inflated effective `n` and shrank the
/// p-value. `reps` still buys power (a node's rate is measured more precisely) but no
/// longer fabricates observations.
///
/// Nodes appear in first-seen (battery) order so the pinned-seed bootstrap CI stays
/// reproducible. Each node's delta is a *node-weighted* mean of its gradable reps; when
/// per-pair `Inconclusive`/`Skipped` exclusion leaves nodes with unequal gradable rep
/// counts, this per-node mean is the node-weighted contrast, which the point-estimate
/// grand mean (rep-weighted) approximates — exactly equal when no pair is excluded, and
/// bounded otherwise by the inconclusive-fraction floor (≤ `INCONCLUSIVE_MAX_FRACTION`).
pub fn node_pass_deltas(pairs: &[PairedRep]) -> Vec<f64> {
    let mut order: Vec<&str> = Vec::new();
    let mut acc: std::collections::HashMap<&str, (f64, f64, u32)> =
        std::collections::HashMap::new();
    for p in pairs {
        let entry = acc.entry(p.node_id.as_str()).or_insert_with(|| {
            order.push(p.node_id.as_str());
            (0.0, 0.0, 0)
        });
        entry.0 += p.baseline_pass;
        entry.1 += p.treatment_pass;
        entry.2 += 1;
    }
    order
        .iter()
        .map(|id| {
            let (b_sum, t_sum, n) = acc[id];
            (t_sum - b_sum) / n as f64
        })
        .collect()
}

/// Maps a single RunOutput to a binary pass score: Success → 1.0, everything else → 0.0.
///
/// `LocalUnavailable` maps to 0.0; abort detection in `run_experiment` catches it
/// separately before the gate is evaluated (I3). `Inconclusive` also maps to 0.0
/// here, but `run_experiment`'s pair-level exclusion (per-node
/// soft-exclusion) drops any pair touching it before this function is ever called
/// on that pair, so 0.0 is never actually observed in the aggregate for it.
/// `Skipped` is also 0.0 for now — a Skipped RED-baseline node is a corpus curation
/// issue and is out of v1 abort scope; v2 should surface it as a separate error path.
pub fn node_pass_score(out: &RunOutput) -> f64 {
    match out.status {
        RunStatus::Success => 1.0,
        _ => 0.0,
    }
}

/// Aggregated metrics for one arm across all reps and nodes.
#[derive(Debug, Clone)]
pub struct ArmAggregate {
    pub node_pass_rate: f64,
    pub judge_quality: Option<f64>,
    pub engine_broken_rate: Option<f64>,
}

/// Returns arithmetic means; `None` for empty judge/engine slices.
pub fn aggregate_arm(pass: &[f64], judge: &[f64], engine: &[f64]) -> ArmAggregate {
    let node_pass_rate = if pass.is_empty() {
        0.0
    } else {
        pass.iter().sum::<f64>() / pass.len() as f64
    };

    let judge_quality = if judge.is_empty() {
        None
    } else {
        Some(judge.iter().sum::<f64>() / judge.len() as f64)
    };

    let engine_broken_rate = if engine.is_empty() {
        None
    } else {
        Some(engine.iter().sum::<f64>() / engine.len() as f64)
    };

    ArmAggregate {
        node_pass_rate,
        judge_quality,
        engine_broken_rate,
    }
}

/// The committed prior gated-metric value(s):
/// `measurement/experiments/<name>.baseline.json`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Baseline {
    pub name: String,
    pub gated: BTreeMap<String, f64>,
}

/// Loads and deserialises a baseline JSON file.
pub fn load_baseline(path: &Path) -> Result<Baseline, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

/// Default significance level for the gated metric's paired Wilcoxon test
/// (CONTRACT.md amendment), used when a manifest does not set `gate_alpha`.
pub const DEFAULT_GATE_ALPHA: f64 = 0.05;

/// Exit code for a battery that could not have reached `alpha` (issue #3). Distinct from
/// `0` (measured, no regression), `1` (confirmed regression) and `3` (aborted): the
/// measurement completed and is internally valid, it simply had no power to decide.
pub const EXIT_UNDERPOWERED: i32 = 4;

/// Three-state gate outcome. `Pass` means "we looked and found no regression";
/// `Underpowered` means "this battery could not have found one" — collapsing the two
/// into a boolean is what let underpowered nulls read as evidence of no effect (#3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum GateOutcome {
    Pass,
    Fail,
    Underpowered,
}

/// Gate verdict for a single gated metric.
#[derive(Debug, Clone, Serialize)]
pub struct GateVerdict {
    pub metric: String,
    pub baseline_value: f64,
    pub observed_value: f64,
    pub tolerance: f64,
    /// Significance level the verdict was evaluated against.
    pub alpha: f64,
    /// Two-sided p-value of the paired test on this metric's deltas, if one
    /// was supplied. `None` when no paired-delta series exists for this
    /// metric (v1: always `Some` for `node_pass_rate`, the sole gated metric).
    pub p_two_sided: Option<f64>,
    /// Discordant (non-zero) paired-delta count — the power denominator.
    /// `None` when no paired series was supplied.
    pub n_nonzero: Option<usize>,
    /// Smallest two-sided p this battery's `n_nonzero` could have produced.
    /// `None` when no paired series was supplied.
    pub min_attainable_p: Option<f64>,
    /// True only for a *confirmed* regression (worse **and** significant). Never true
    /// when the outcome is `Underpowered` — see `gate`.
    pub regressed: bool,
    pub outcome: GateOutcome,
}

/// Returns the observed value for the named gated metric from an ArmAggregate.
/// Only `node_pass_rate` is a gated metric in v1.
fn observed_for(metric: &str, treatment: &ArmAggregate) -> Option<f64> {
    match metric {
        "node_pass_rate" => Some(treatment.node_pass_rate),
        _ => None,
    }
}

/// Gate-what-you-lead: for each GATED metric, a point-estimate regression
/// (`observed < baseline_value - tolerance`) is confirmed only when it is
/// also statistically significant — `p_two_sided < alpha` — before it fails
/// the run. Tracked metrics are excluded by construction (they never appear
/// in `gated_metrics()`).
///
/// `baseline_arm` is the **in-run baseline arm** aggregate — the same series the
/// paired p-value was computed against (issue #7, D3). Both halves of the gate
/// therefore describe one contrast: in-run treatment vs. in-run baseline arm. The
/// committed `baseline.json` is *not* read here — using it for the point estimate
/// while the p-value tested the in-run arm mixed two reference series in one verdict.
/// It is retained by the caller as a drift reference (a stale committed baseline is
/// surfaced as a validity warning, not a silent gate against a different series).
///
/// `p_two_sided` is the two-sided p-value of the paired test (Wilcoxon
/// signed-rank) on the gated metric's per-node deltas. `None` falls back
/// to the bare point estimate — for any future gated metric that has no
/// paired-delta series to test. A point estimate alone is never sufficient to fail
/// the gate when significance data is available; a noisy run that fails to clear
/// `alpha` is honestly reported as "not a confirmed regression".
///
/// **Minimum-power guard (issue #3).** `n_nonzero` is the realised discordant-pair
/// count. When `stats::min_attainable_p(n_nonzero) > alpha`, no arrangement of the
/// observed data could have cleared `alpha` — the battery was mathematically incapable
/// of failing its own gate — and the outcome is `Underpowered`, never `Pass`. The guard
/// is direction-blind on purpose: a battery that could not have detected a regression
/// did not establish its absence just because the point estimate happened to improve.
///
/// `regressed` keeps its prior meaning (worse **and** significant) on every powered
/// battery, so the confirmed-regression path is untouched. It additionally requires
/// `!underpowered`. On self-consistent inputs that conjunct is redundant — a real
/// `p >= min_attainable_p > alpha` already makes `p < alpha` unsatisfiable — but relying
/// on that leaves the invariant resting on callers passing a `(p, n_nonzero)` pair drawn
/// from the same test. Enforcing it here makes "a battery cannot be both underpowered and
/// a confirmed regression" structural rather than assumed, so a future caller that
/// computes the two from different series cannot resurrect a false FAIL. Pinned by
/// `gate_underpowered_and_regressed_are_mutually_exclusive`.
pub fn gate(
    manifest: &Manifest,
    baseline_arm: &ArmAggregate,
    treatment: &ArmAggregate,
    p_two_sided: Option<f64>,
    n_nonzero: Option<usize>,
) -> Vec<GateVerdict> {
    let alpha = manifest.gate_alpha.unwrap_or(DEFAULT_GATE_ALPHA);
    let min_attainable_p = n_nonzero.map(crate::stats::min_attainable_p);
    let underpowered = min_attainable_p.is_some_and(|floor| floor > alpha);
    manifest
        .gated_metrics()
        .into_iter()
        .filter_map(|metric| {
            let baseline_value = observed_for(metric, baseline_arm)?;
            let observed_value = observed_for(metric, treatment)?;
            let tolerance = manifest.tolerance.get(metric).copied().unwrap_or(0.0);
            let worse = observed_value < baseline_value - tolerance;
            let regressed = worse && !underpowered && p_two_sided.is_none_or(|p| p < alpha);
            let outcome = if underpowered {
                GateOutcome::Underpowered
            } else if regressed {
                GateOutcome::Fail
            } else {
                GateOutcome::Pass
            };
            Some(GateVerdict {
                metric: metric.to_string(),
                baseline_value,
                observed_value,
                tolerance,
                alpha,
                p_two_sided,
                n_nonzero,
                min_attainable_p,
                regressed,
                outcome,
            })
        })
        .collect()
}

/// `1` for a confirmed regression, [`EXIT_UNDERPOWERED`] when no verdict regressed but
/// at least one battery could not have reached `alpha`, else `0`.
///
/// A confirmed regression outranks an underpowered sibling: a metric that *did* detect
/// a regression is the more actionable signal, and the underpowered one is still named
/// in the report.
pub fn exit_code(verdicts: &[GateVerdict]) -> i32 {
    if verdicts.iter().any(|v| v.outcome == GateOutcome::Fail) {
        1
    } else if verdicts
        .iter()
        .any(|v| v.outcome == GateOutcome::Underpowered)
    {
        EXIT_UNDERPOWERED
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{RunOutput, RunStatus};
    use crate::experiment::{ArmConfig, Backend, ContextStrategy, Manifest, MetricTag};
    use indexmap::IndexMap;
    use std::collections::BTreeMap;

    // ── test helpers ──────────────────────────────────────────────────────────

    fn any_arm() -> ArmConfig {
        ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::None,
            env: BTreeMap::default(),
            backend: Backend::Local,
        }
    }

    /// Manifest: node_pass_rate gated, tolerance 0.0.
    fn manifest_070_tol0() -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        let mut tolerance = BTreeMap::new();
        tolerance.insert("node_pass_rate".to_string(), 0.0_f64);
        Manifest {
            name: "test-exp".into(),
            reps: 1,
            seed_base: 1,
            battery: vec!["suite-a".into()],
            baseline: any_arm(),
            treatment: any_arm(),
            metrics,
            tolerance,
            gate_alpha: None,
        }
    }

    /// Manifest: node_pass_rate gated, tolerance 0.05.
    fn manifest_070_tol005() -> Manifest {
        let mut m = manifest_070_tol0();
        m.tolerance.insert("node_pass_rate".to_string(), 0.05_f64);
        m
    }

    /// Manifest: node_pass_rate gated, tolerance 0.0, `gate_alpha` overridden.
    fn manifest_070_tol0_alpha(alpha: f64) -> Manifest {
        let mut m = manifest_070_tol0();
        m.gate_alpha = Some(alpha);
        m
    }

    /// Manifest with all three metrics: node_pass_rate gated, others tracked.
    fn manifest_valid() -> Manifest {
        let mut metrics = IndexMap::new();
        metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
        metrics.insert("judge_quality".to_string(), MetricTag::Tracked);
        metrics.insert("engine_broken_rate".to_string(), MetricTag::Tracked);
        let mut tolerance = BTreeMap::new();
        tolerance.insert("node_pass_rate".to_string(), 0.0_f64);
        Manifest {
            name: "test-full".into(),
            reps: 1,
            seed_base: 1,
            battery: vec!["suite-a".into()],
            baseline: any_arm(),
            treatment: any_arm(),
            metrics,
            tolerance,
            gate_alpha: None,
        }
    }

    /// ArmAggregate with the given pass rate; judge/engine absent.
    fn agg(rate: f64) -> ArmAggregate {
        ArmAggregate {
            node_pass_rate: rate,
            judge_quality: None,
            engine_broken_rate: None,
        }
    }

    /// ArmAggregate with all three fields populated.
    fn agg_full(rate: f64, judge: f64, engine: f64) -> ArmAggregate {
        ArmAggregate {
            node_pass_rate: rate,
            judge_quality: Some(judge),
            engine_broken_rate: Some(engine),
        }
    }

    // ── scoring ───────────────────────────────────────────────────────────────

    #[test]
    fn pass_score_maps_status() {
        let ok = RunOutput {
            status: RunStatus::Success,
            accept_passed: true,
            edited_files: vec![],
            stdout_tail: "SUCCESS".into(),
            duration_ms: 1,
            cost_usd: Some(0.0),
            input_tokens: 0,
            output_tokens: 0,
            claude_calls: 0,
            num_turns: 0,
            seeds_honoured: false,
        };
        let no = RunOutput {
            status: RunStatus::Failure,
            accept_passed: false,
            edited_files: vec![],
            stdout_tail: "FAILURE".into(),
            duration_ms: 1,
            cost_usd: None,
            input_tokens: 0,
            output_tokens: 0,
            claude_calls: 0,
            num_turns: 0,
            seeds_honoured: false,
        };
        assert_eq!(node_pass_score(&ok), 1.0);
        assert_eq!(node_pass_score(&no), 0.0);
    }

    #[test]
    fn pass_score_non_success_variants_are_zero() {
        for status in [
            RunStatus::Skipped,
            RunStatus::LocalUnavailable,
            RunStatus::Inconclusive,
        ] {
            let out = RunOutput {
                status,
                accept_passed: false,
                edited_files: vec![],
                stdout_tail: String::new(),
                duration_ms: 0,
                cost_usd: None,
                input_tokens: 0,
                output_tokens: 0,
                claude_calls: 0,
                num_turns: 0,
                seeds_honoured: false,
            };
            assert_eq!(node_pass_score(&out), 0.0);
        }
    }

    // ── D2: per-node aggregation (unit of replication) ────────────────────────

    fn pair(node: &str, rep: u32, b: f64, t: f64) -> PairedRep {
        PairedRep {
            node_id: node.into(),
            rep,
            baseline_pass: b,
            treatment_pass: t,
        }
    }

    #[test]
    fn node_pass_deltas_aggregates_reps_into_one_per_node() {
        // 3 nodes × 4 reps = 12 pairs, but the delta vector has length 3 (one per node),
        // not 12 — the pseudo-replication fix. Node "b" has a MIXED treatment (2 pass /
        // 2 fail) so its per-node delta is a mean (0.5), proving mean-aggregation, not a
        // sum (which would give 2.0) and not a length-only check.
        let pairs = vec![
            // node a: treatment beats baseline every rep → delta +1.0
            pair("a", 0, 0.0, 1.0),
            pair("a", 1, 0.0, 1.0),
            pair("a", 2, 0.0, 1.0),
            pair("a", 3, 0.0, 1.0),
            // node b: baseline always passes; treatment passes 2/4 → delta (0.5 - 1.0) = -0.5
            pair("b", 0, 1.0, 1.0),
            pair("b", 1, 1.0, 0.0),
            pair("b", 2, 1.0, 1.0),
            pair("b", 3, 1.0, 0.0),
            // node c: both always pass → delta 0.0
            pair("c", 0, 1.0, 1.0),
            pair("c", 1, 1.0, 1.0),
            pair("c", 2, 1.0, 1.0),
            pair("c", 3, 1.0, 1.0),
        ];
        let deltas = node_pass_deltas(&pairs);
        assert_eq!(deltas.len(), 3, "one delta per node, not per (node,rep)");
        // First-seen (battery) order: a, b, c.
        assert!((deltas[0] - 1.0).abs() < 1e-12, "node a mean delta");
        assert!(
            (deltas[1] + 0.5).abs() < 1e-12,
            "node b mean delta (mean, not sum)"
        );
        assert!((deltas[2] - 0.0).abs() < 1e-12, "node c mean delta");
    }

    #[test]
    fn node_pass_deltas_empty_is_empty() {
        assert!(node_pass_deltas(&[]).is_empty());
    }

    // ── aggregation ───────────────────────────────────────────────────────────

    #[test]
    fn aggregate_is_mean() {
        let a = aggregate_arm(&[1., 1., 0., 1.], &[], &[]);
        assert!((a.node_pass_rate - 0.75).abs() < 1e-12);
        assert_eq!(a.judge_quality, None);
        assert_eq!(a.engine_broken_rate, None);
    }

    #[test]
    fn aggregate_judge_and_engine_present_when_non_empty() {
        let a = aggregate_arm(&[1.0], &[0.8, 0.6], &[0.1, 0.3]);
        assert!((a.judge_quality.unwrap() - 0.7).abs() < 1e-12);
        assert!((a.engine_broken_rate.unwrap() - 0.2).abs() < 1e-12);
    }

    // ── gate / exit_code ──────────────────────────────────────────────────────
    //
    // `p_two_sided: None` in these first five tests exercises the fallback
    // path (CONTRACT.md amendment): with no significance series to test against,
    // the verdict reduces to the bare point estimate, preserving the
    // pre-amendment behaviour exactly.

    #[test]
    fn gate_passes_when_no_regression() {
        // baseline 0.70, tol 0.0, treatment 0.80 → NOT regressed → exit 0.
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.80), None, None);
        assert!(!verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn gate_fails_on_regression() {
        // baseline 0.70, tol 0.0, treatment 0.60, no significance series →
        // point-estimate fallback → regressed → exit 1.
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), None, None);
        assert!(verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn gate_respects_tolerance_band() {
        // baseline 0.70, tol 0.05, treatment 0.66 → 0.66 >= 0.65 → NOT regressed.
        let verdicts = gate(&manifest_070_tol005(), &agg(0.70), &agg(0.66), None, None);
        assert!(!verdicts[0].regressed);
    }

    #[test]
    fn gate_tolerance_below_band_is_regressed() {
        // baseline 0.70, tol 0.05, treatment 0.64 → 0.64 < 0.65, no
        // significance series → point-estimate fallback → regressed → exit 1.
        let verdicts = gate(&manifest_070_tol005(), &agg(0.70), &agg(0.64), None, None);
        assert!(verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn tracked_metrics_never_gate() {
        // judge_quality / engine_broken_rate are TRACKED → only node_pass_rate
        // ever appears in gate verdicts.
        let verdicts = gate(
            &manifest_valid(),
            &agg(0.70),
            &agg_full(0.80, 1.0, 0.9),
            None,
            None,
        );
        assert!(verdicts.iter().all(|v| v.metric == "node_pass_rate"));
    }

    // ── significance-gated regression (CONTRACT.md amendment) ─────────────────

    #[test]
    fn gate_confirms_regression_when_significant() {
        // baseline 0.70, tol 0.0, treatment 0.60 (worse) + p=0.01 < alpha(0.05)
        // → confirmed regression → exit 1.
        let verdicts = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.01),
            Some(20),
        );
        assert!(verdicts[0].regressed);
        assert_eq!(verdicts[0].alpha, DEFAULT_GATE_ALPHA);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn gate_does_not_confirm_regression_when_not_significant() {
        // Worse point estimate (0.70 → 0.60) but p=0.20 >= alpha(0.05): the
        // difference is not distinguishable from noise at this alpha, so the
        // gate must not fail — this is the key honesty case: an underpowered
        // or noisy run reports "not a confirmed regression", not a false
        // failure.
        let verdicts = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.20),
            Some(20),
        );
        assert!(!verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn gate_improvement_never_regresses_regardless_of_significance() {
        // treatment 0.80 > baseline 0.70: `worse` is false, so even a highly
        // significant p-value must not flip this into a regression.
        let verdicts = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.80),
            Some(0.001),
            Some(20),
        );
        assert!(!verdicts[0].regressed);
    }

    #[test]
    fn gate_none_significance_falls_back_to_point_estimate() {
        // No paired-delta series available for this metric → the bare
        // point-estimate rule applies, unconditionally on the p-value.
        let regressed = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), None, None);
        let not_regressed = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.80), None, None);
        assert!(regressed[0].regressed);
        assert!(!not_regressed[0].regressed);
        assert!(regressed[0].p_two_sided.is_none());
    }

    #[test]
    fn gate_alpha_is_configurable_via_manifest() {
        // p=0.07 sits between the default alpha(0.05) and a manifest-set
        // alpha(0.10): NOT significant at 0.05 (default), but significant
        // at the wider 0.10 the manifest opts into.
        let default_verdicts = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.07),
            Some(20),
        );
        assert!(!default_verdicts[0].regressed);

        let widened = manifest_070_tol0_alpha(0.10);
        let widened_verdicts = gate(&widened, &agg(0.70), &agg(0.60), Some(0.07), Some(20));
        assert!(widened_verdicts[0].regressed);
        assert_eq!(widened_verdicts[0].alpha, 0.10);
    }

    // ── #3: minimum-power guard ───────────────────────────────────────────────

    #[test]
    fn gate_underpowered_when_alpha_is_unreachable() {
        // 5 non-zero pairs: the exact two-sided floor is 0.0625 > alpha 0.05. Even the
        // unanimous best case cannot fail this gate, so the verdict must be the distinct
        // UNDERPOWERED, never PASS.
        let v = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.0625),
            Some(5),
        );
        assert_eq!(v[0].outcome, GateOutcome::Underpowered);
        assert_ne!(v[0].outcome, GateOutcome::Pass);
        assert_eq!(v[0].n_nonzero, Some(5));
        assert_eq!(v[0].min_attainable_p, Some(0.0625));
        assert_eq!(exit_code(&v), EXIT_UNDERPOWERED);
    }

    #[test]
    fn gate_underpowered_even_when_the_point_estimate_improved() {
        // Direction is irrelevant to power: a battery that could not have detected a
        // regression did not establish its absence just because treatment looked better.
        let v = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.90),
            Some(1.0),
            Some(3),
        );
        assert_eq!(v[0].outcome, GateOutcome::Underpowered);
        assert_eq!(exit_code(&v), EXIT_UNDERPOWERED);
    }

    #[test]
    fn gate_zero_discordant_pairs_is_underpowered_not_pass() {
        // Every pair tied → n_nonzero 0 → floor 1.0. The arms were indistinguishable on
        // this battery; that is an absence of evidence, not evidence of absence.
        let v = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.70),
            Some(1.0),
            Some(0),
        );
        assert_eq!(v[0].outcome, GateOutcome::Underpowered);
        assert_eq!(v[0].n_nonzero, Some(0));
    }

    #[test]
    fn gate_powered_battery_still_confirms_and_still_passes() {
        // 6 non-zero pairs → floor 0.03125 ≤ 0.05: the guard stands aside entirely and
        // both real verdicts remain reachable.
        let regressed = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.03125),
            Some(6),
        );
        assert_eq!(regressed[0].outcome, GateOutcome::Fail);
        assert_eq!(exit_code(&regressed), 1);

        let clean = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.80),
            Some(0.03125),
            Some(6),
        );
        assert_eq!(clean[0].outcome, GateOutcome::Pass);
        assert_eq!(exit_code(&clean), 0);
    }

    #[test]
    fn gate_underpowered_and_regressed_are_mutually_exclusive() {
        // Structural invariant: `p < alpha` is impossible when the floor already exceeds
        // alpha, so no verdict can ever be both. If this ever fires, the two rules have
        // drifted apart and `outcome` is decided by evaluation order rather than by maths.
        for n in 0..=25_usize {
            for &p in &[0.0, 0.001, 0.03, 0.05, 0.5, 1.0] {
                let v = gate(
                    &manifest_070_tol0(),
                    &agg(0.70),
                    &agg(0.10),
                    Some(p),
                    Some(n),
                );
                let underpowered = v[0].outcome == GateOutcome::Underpowered;
                assert!(
                    !(underpowered && v[0].regressed),
                    "n={n} p={p}: a battery cannot be both underpowered and a confirmed regression"
                );
            }
        }
    }

    #[test]
    fn gate_without_a_pair_count_keeps_the_point_estimate_fallback() {
        // No paired series (`None`) → no power claim either way; the pre-existing
        // point-estimate behaviour is preserved unchanged.
        let v = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), None, None);
        assert_eq!(v[0].outcome, GateOutcome::Fail);
        assert_eq!(v[0].min_attainable_p, None);
        assert_eq!(exit_code(&v), 1);
    }

    // ── D3: gate contrast is the in-run baseline arm, not the committed baseline ──

    #[test]
    fn gate_contrast_is_the_in_run_baseline_arm() {
        // treatment 0.60 is worse than the in-run baseline ARM 0.70 and significant →
        // regressed. `baseline_value` in the verdict is the in-run arm (0.70), the same
        // series the p-value was computed against — not a committed baseline.json scalar.
        let v = gate(
            &manifest_070_tol0(),
            &agg(0.70),
            &agg(0.60),
            Some(0.01),
            Some(20),
        );
        assert!(v[0].regressed);
        assert_eq!(
            v[0].baseline_value, 0.70,
            "gate must reference the in-run baseline arm, not a committed scalar"
        );
    }

    #[test]
    fn gate_not_regressed_when_treatment_beats_in_run_baseline_arm() {
        // The committed baseline is irrelevant to the gate now: treatment 0.75 beats the
        // in-run baseline arm 0.60, so it is NOT worse — even a tiny p-value cannot flip it.
        let v = gate(
            &manifest_070_tol0(),
            &agg(0.60),
            &agg(0.75),
            Some(0.0001),
            Some(20),
        );
        assert!(!v[0].regressed);
        assert_eq!(v[0].baseline_value, 0.60);
    }

    // ── load_baseline ─────────────────────────────────────────────────────────

    #[test]
    fn load_baseline_roundtrip() {
        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/baseline");
        let path = dir.join("cxpak-context-ab.json");
        let b = load_baseline(&path).expect("load");
        assert_eq!(b.name, "cxpak-context-ab");
        assert!((b.gated["node_pass_rate"] - 0.70).abs() < 1e-12);
    }

    // ── one corpus, both statistics twins (dotclaude#56 / #60) ────────────────

    /// Loads the shared power-guard corpus both statistics twins are judged by.
    ///
    /// `tests/power-guard/vectors.json` is a **byte-identical** copy of the family's
    /// canonical corpus (`conformance/corpus/power-guard/vectors.json` in the consumer
    /// repo). The consumer's `verify-twin-lockstep.py` diffs this copy against the
    /// canonical one, so weakening a vector here cannot be a quiet local decision —
    /// exactly the arrangement `tests/id-guard/vectors.json` already has for node ids.
    ///
    /// This matters more for the statistics than for the ids. `dotclaude measure run`
    /// invokes abproof as a ghcr bridge image and **fails open to its linked in-tree twin**
    /// (ADR-0055), so if the two implementations drift the fallback silently applies
    /// different verdict semantics from the container — on the guard whose job is refusing
    /// to report a battery as PASS when it could not have failed. That path is not
    /// hypothetical: dotclaude#34 ran on it (`transport=fallback reason=exit-125`).
    fn power_guard_corpus() -> serde_json::Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/power-guard/vectors.json"
        );
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("shared power-guard corpus unreadable at {path}: {e}"));
        serde_json::from_str(&raw).expect("shared power-guard corpus is not valid JSON")
    }

    fn corpus_cases<'a>(doc: &'a serde_json::Value, section: &str) -> &'a Vec<serde_json::Value> {
        doc[section]
            .as_array()
            .unwrap_or_else(|| panic!("power-guard corpus has no `{section}` array"))
    }

    #[test]
    fn adr0056_the_power_floor_matches_the_shared_corpus() {
        let doc = power_guard_corpus();
        let rows = corpus_cases(&doc, "floor");
        // Vacuity guard: a corpus that shrank to nothing would pass any implementation.
        assert!(
            rows.len() >= 10,
            "shared corpus lost coverage: {} rows",
            rows.len()
        );

        for case in rows {
            let n = case["n_discordant"].as_u64().expect("n_discordant") as usize;
            let expected = case["min_attainable_p"].as_f64().expect("min_attainable_p");
            let alpha = case["alpha"].as_f64().expect("alpha");
            let reachable = case["alpha_reachable"].as_bool().expect("alpha_reachable");

            let actual = crate::stats::min_attainable_p(n);
            assert!(
                (actual - expected).abs() < 1e-12,
                "min_attainable_p({n}) = {actual}, corpus says {expected}"
            );
            assert_eq!(
                actual <= alpha,
                reachable,
                "n={n} alpha={alpha}: corpus says alpha_reachable={reachable}, floor {actual}"
            );
        }
    }

    #[test]
    fn adr0056_the_gate_verdict_matches_the_shared_corpus() {
        let doc = power_guard_corpus();
        let rows = corpus_cases(&doc, "verdict");
        assert!(
            rows.len() >= 5,
            "shared corpus lost coverage: {} rows",
            rows.len()
        );

        for case in rows {
            let n = case["n_discordant"].as_u64().expect("n_discordant") as usize;
            let p = case["p_two_sided"].as_f64().expect("p_two_sided");
            let worse = case["worse"].as_bool().expect("worse");
            let alpha = case["alpha"].as_f64().expect("alpha");
            let expected = case["outcome"].as_str().expect("outcome");
            let expected_exit = case["exit_code"].as_i64().expect("exit_code") as i32;
            let label = case["_case"].as_str().unwrap_or("(unlabelled)");

            // baseline arm 0.70, zero tolerance: 0.60 is a worse point estimate, 0.80 better.
            let observed = if worse { 0.60 } else { 0.80 };
            let verdicts = gate(
                &manifest_070_tol0_alpha(alpha),
                &agg(0.70),
                &agg(observed),
                Some(p),
                Some(n),
            );

            let actual = serde_json::to_value(verdicts[0].outcome)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .expect("outcome serialises as a string");
            assert_eq!(
                actual, expected,
                "{label}: n={n} p={p} worse={worse} alpha={alpha}"
            );
            assert_eq!(exit_code(&verdicts), expected_exit, "{label}: exit code");
        }
    }
}
