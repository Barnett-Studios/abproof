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
    pub regressed: bool,
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
/// the gate when significance data is available; an underpowered or noisy run that
/// fails to clear `alpha` is honestly reported as "not a confirmed regression".
pub fn gate(
    manifest: &Manifest,
    baseline_arm: &ArmAggregate,
    treatment: &ArmAggregate,
    p_two_sided: Option<f64>,
) -> Vec<GateVerdict> {
    let alpha = manifest.gate_alpha.unwrap_or(DEFAULT_GATE_ALPHA);
    manifest
        .gated_metrics()
        .into_iter()
        .filter_map(|metric| {
            let baseline_value = observed_for(metric, baseline_arm)?;
            let observed_value = observed_for(metric, treatment)?;
            let tolerance = manifest.tolerance.get(metric).copied().unwrap_or(0.0);
            let worse = observed_value < baseline_value - tolerance;
            let regressed = worse && p_two_sided.is_none_or(|p| p < alpha);
            Some(GateVerdict {
                metric: metric.to_string(),
                baseline_value,
                observed_value,
                tolerance,
                alpha,
                p_two_sided,
                regressed,
            })
        })
        .collect()
}

/// Returns 1 if any verdict regressed, else 0.
pub fn exit_code(verdicts: &[GateVerdict]) -> i32 {
    if verdicts.iter().any(|v| v.regressed) {
        1
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
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.80), None);
        assert!(!verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn gate_fails_on_regression() {
        // baseline 0.70, tol 0.0, treatment 0.60, no significance series →
        // point-estimate fallback → regressed → exit 1.
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), None);
        assert!(verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn gate_respects_tolerance_band() {
        // baseline 0.70, tol 0.05, treatment 0.66 → 0.66 >= 0.65 → NOT regressed.
        let verdicts = gate(&manifest_070_tol005(), &agg(0.70), &agg(0.66), None);
        assert!(!verdicts[0].regressed);
    }

    #[test]
    fn gate_tolerance_below_band_is_regressed() {
        // baseline 0.70, tol 0.05, treatment 0.64 → 0.64 < 0.65, no
        // significance series → point-estimate fallback → regressed → exit 1.
        let verdicts = gate(&manifest_070_tol005(), &agg(0.70), &agg(0.64), None);
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
        );
        assert!(verdicts.iter().all(|v| v.metric == "node_pass_rate"));
    }

    // ── significance-gated regression (CONTRACT.md amendment) ─────────────────

    #[test]
    fn gate_confirms_regression_when_significant() {
        // baseline 0.70, tol 0.0, treatment 0.60 (worse) + p=0.01 < alpha(0.05)
        // → confirmed regression → exit 1.
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), Some(0.01));
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
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), Some(0.20));
        assert!(!verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn gate_improvement_never_regresses_regardless_of_significance() {
        // treatment 0.80 > baseline 0.70: `worse` is false, so even a highly
        // significant p-value must not flip this into a regression.
        let verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.80), Some(0.001));
        assert!(!verdicts[0].regressed);
    }

    #[test]
    fn gate_none_significance_falls_back_to_point_estimate() {
        // No paired-delta series available for this metric → the bare
        // point-estimate rule applies, unconditionally on the p-value.
        let regressed = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), None);
        let not_regressed = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.80), None);
        assert!(regressed[0].regressed);
        assert!(!not_regressed[0].regressed);
        assert!(regressed[0].p_two_sided.is_none());
    }

    #[test]
    fn gate_alpha_is_configurable_via_manifest() {
        // p=0.07 sits between the default alpha(0.05) and a manifest-set
        // alpha(0.10): NOT significant at 0.05 (default), but significant
        // at the wider 0.10 the manifest opts into.
        let default_verdicts = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), Some(0.07));
        assert!(!default_verdicts[0].regressed);

        let widened = manifest_070_tol0_alpha(0.10);
        let widened_verdicts = gate(&widened, &agg(0.70), &agg(0.60), Some(0.07));
        assert!(widened_verdicts[0].regressed);
        assert_eq!(widened_verdicts[0].alpha, 0.10);
    }

    // ── D3: gate contrast is the in-run baseline arm, not the committed baseline ──

    #[test]
    fn gate_contrast_is_the_in_run_baseline_arm() {
        // treatment 0.60 is worse than the in-run baseline ARM 0.70 and significant →
        // regressed. `baseline_value` in the verdict is the in-run arm (0.70), the same
        // series the p-value was computed against — not a committed baseline.json scalar.
        let v = gate(&manifest_070_tol0(), &agg(0.70), &agg(0.60), Some(0.01));
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
        let v = gate(&manifest_070_tol0(), &agg(0.60), &agg(0.75), Some(0.0001));
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
}
