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

/// Maps a single RunOutput to a binary pass score: Success → 1.0, everything else → 0.0.
///
/// `LocalUnavailable` maps to 0.0; abort detection in `run_experiment` catches it
/// separately before the gate is evaluated (I3). `Inconclusive` also maps to 0.0
/// here, but `run_experiment`'s pair-level exclusion (ADR-0041 §per-node
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

/// Gate verdict for a single gated metric.
#[derive(Debug, Clone, Serialize)]
pub struct GateVerdict {
    pub metric: String,
    pub baseline_value: f64,
    pub observed_value: f64,
    pub tolerance: f64,
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

/// Gate-what-you-lead: for each GATED metric, `regressed` ⇔
/// `observed < baseline_value - tolerance`.  Tracked metrics are excluded
/// by construction (they never appear in `gated_metrics()`).
pub fn gate(
    manifest: &Manifest,
    baseline: &Baseline,
    treatment: &ArmAggregate,
) -> Vec<GateVerdict> {
    manifest
        .gated_metrics()
        .into_iter()
        .filter_map(|metric| {
            let baseline_value = *baseline.gated.get(metric)?;
            let observed_value = observed_for(metric, treatment)?;
            let tolerance = manifest.tolerance.get(metric).copied().unwrap_or(0.0);
            let regressed = observed_value < baseline_value - tolerance;
            Some(GateVerdict {
                metric: metric.to_string(),
                baseline_value,
                observed_value,
                tolerance,
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
        }
    }

    /// Manifest: node_pass_rate gated, tolerance 0.05.
    fn manifest_070_tol005() -> Manifest {
        let mut m = manifest_070_tol0();
        m.tolerance.insert("node_pass_rate".to_string(), 0.05_f64);
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
        }
    }

    /// Baseline with node_pass_rate = 0.70.
    fn baseline_070() -> Baseline {
        let mut gated = BTreeMap::new();
        gated.insert("node_pass_rate".to_string(), 0.70_f64);
        Baseline {
            name: "test-exp".into(),
            gated,
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

    #[test]
    fn gate_passes_when_no_regression() {
        // baseline 0.70, tol 0.0, treatment 0.80 → NOT regressed → exit 0.
        let verdicts = gate(&manifest_070_tol0(), &baseline_070(), &agg(0.80));
        assert!(!verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 0);
    }

    #[test]
    fn gate_fails_on_regression() {
        // baseline 0.70, tol 0.0, treatment 0.60 → regressed → exit 1.
        let verdicts = gate(&manifest_070_tol0(), &baseline_070(), &agg(0.60));
        assert!(verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn gate_respects_tolerance_band() {
        // baseline 0.70, tol 0.05, treatment 0.66 → 0.66 >= 0.65 → NOT regressed.
        let verdicts = gate(&manifest_070_tol005(), &baseline_070(), &agg(0.66));
        assert!(!verdicts[0].regressed);
    }

    #[test]
    fn gate_tolerance_below_band_is_regressed() {
        // baseline 0.70, tol 0.05, treatment 0.64 → 0.64 < 0.65 → regressed.
        let verdicts = gate(&manifest_070_tol005(), &baseline_070(), &agg(0.64));
        assert!(verdicts[0].regressed);
        assert_eq!(exit_code(&verdicts), 1);
    }

    #[test]
    fn tracked_metrics_never_gate() {
        // judge_quality / engine_broken_rate are TRACKED → only node_pass_rate
        // ever appears in gate verdicts.
        let verdicts = gate(
            &manifest_valid(),
            &baseline_070(),
            &agg_full(0.80, 1.0, 0.9),
        );
        assert!(verdicts.iter().all(|v| v.metric == "node_pass_rate"));
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
