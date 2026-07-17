//! Stub-driven pipeline integration tests.
//!
//! Exercises `run_experiment` end-to-end without I/O by wiring `ArmDistinctDriver`
//! (a local counting driver) and `StubJudge` over the real `py-add` corpus node.
//! The `measure run --dry-run` CLI path is tested in the binary crate
//! (`crates/dotclaude/tests/cli.rs`), where `CARGO_BIN_EXE_dotclaude` resolves.

use abproof::{
    corpus::{self, NodeJson},
    driver::{DriverError, RunOutput, RunStatus, SessionDriver, StubDriver},
    experiment::{load_manifest, ArmConfig, Backend, ContextStrategy, Manifest, MetricTag},
    judge::{JudgeScore, StubJudge},
    run,
    score::Baseline,
};
use indexmap::IndexMap;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

// ── ArmDistinctDriver ─────────────────────────────────────────────────────────

/// A test-only `SessionDriver` that returns scripted outcomes per arm context,
/// and counts all driver invocations so tests can verify the exact call count.
struct ArmDistinctDriver {
    treatment_succeeds: bool,
    baseline_succeeds: bool,
    count: Arc<AtomicU64>,
}

impl ArmDistinctDriver {
    fn new(treatment_succeeds: bool, baseline_succeeds: bool) -> (Self, Arc<AtomicU64>) {
        let count = Arc::new(AtomicU64::new(0));
        (
            Self {
                treatment_succeeds,
                baseline_succeeds,
                count: Arc::clone(&count),
            },
            count,
        )
    }

    fn make_output(succeeds: bool) -> RunOutput {
        RunOutput {
            status: if succeeds {
                RunStatus::Success
            } else {
                RunStatus::Failure
            },
            accept_passed: succeeds,
            edited_files: vec![],
            stdout_tail: if succeeds { "SUCCESS" } else { "FAILURE" }.to_string(),
            duration_ms: 0,
            cost_usd: if succeeds { Some(0.0) } else { None },
            input_tokens: 0,
            output_tokens: 0,
            claude_calls: 0,
            num_turns: 0,
            seeds_honoured: true,
        }
    }
}

impl SessionDriver for ArmDistinctDriver {
    fn run(&self, arm: &ArmConfig, _node: &NodeJson, _seed: u64) -> Result<RunOutput, DriverError> {
        self.count.fetch_add(1, Ordering::Relaxed);
        let succeeds = match arm.context {
            ContextStrategy::Cxpak => self.treatment_succeeds,
            ContextStrategy::None => self.baseline_succeeds,
        };
        Ok(Self::make_output(succeeds))
    }
}

// ── test helpers ──────────────────────────────────────────────────────────────

const REPS: u32 = 5;

fn small_manifest() -> Manifest {
    let mut metrics = IndexMap::new();
    metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
    metrics.insert("judge_quality".to_string(), MetricTag::Tracked);
    Manifest {
        name: "stub-test".to_string(),
        reps: REPS,
        seed_base: 42,
        battery: vec!["py-add".to_string()],
        baseline: ArmConfig {
            loop_name: "execute-node".to_string(),
            model: "local-default".to_string(),
            context: ContextStrategy::None,
            env: BTreeMap::default(),
            backend: Backend::Local,
        },
        treatment: ArmConfig {
            loop_name: "execute-node".to_string(),
            model: "local-default".to_string(),
            context: ContextStrategy::Cxpak,
            env: BTreeMap::default(),
            backend: Backend::Local,
        },
        metrics,
        tolerance: BTreeMap::default(),
    }
}

/// A committed-baseline floor that can be beaten (treatment pass) or failed (regression).
fn baseline_floor_05() -> Baseline {
    let mut gated = BTreeMap::new();
    gated.insert("node_pass_rate".to_string(), 0.5_f64);
    Baseline {
        name: "stub-test".to_string(),
        gated,
    }
}

fn canned_judge() -> StubJudge {
    StubJudge {
        canned: JudgeScore {
            per_criterion: IndexMap::new(),
            total: 3,
        },
    }
}

/// The vendored 2-node fixture ships inside the crate, so these pipeline tests run
/// standalone without the full ~23MB `measurement/corpus` (which is the Corpus
/// component's repo, absent from an abproof-only checkout).
fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus-fixture/red-baseline")
}

fn load_py_add() -> Vec<abproof::corpus::CorpusNode> {
    let root = fixture_root();
    corpus::load_battery(&root, &["py-add".to_string()])
        .expect("py-add must be loadable from the vendored corpus fixture")
}

// ── Existing test ─────────────────────────────────────────────────────────────

#[test]
fn crate_links_and_exposes_usage() {
    let u = abproof::cli_usage();
    assert!(
        u.contains("measure run"),
        "usage must document `measure run`, got: {u}"
    );
}

// ── Step 3/4: StubDriver orchestration tests ──────────────────────────────────

#[test]
fn stub_driver_treatment_passes_gate_exit_0() {
    // Treatment arm succeeds (Cxpak), baseline arm fails (None).
    // Treatment node_pass_rate = 1.0 ≥ committed floor 0.5 → gate_exit = 0.
    let manifest = small_manifest();
    let nodes = load_py_add();
    let (driver, call_count) = ArmDistinctDriver::new(true, false);
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    let row = rec
        .rows
        .iter()
        .find(|r| r.metric == "node_pass_rate")
        .expect("gated row node_pass_rate must exist");

    // Gated row must carry Wilcoxon p, d_z, and CI.
    assert!(
        row.p_two_sided.is_some(),
        "gated row must carry Wilcoxon p; got None"
    );
    assert!(row.d_z.is_some(), "gated row must carry d_z; got None");
    assert!(
        row.ci_lower.is_some() && row.ci_upper.is_some(),
        "gated row must carry CI bounds"
    );

    assert_eq!(rec.gate_exit, 0, "treatment passes floor → gate_exit = 0");

    // Call count: nodes.len() × reps × 2 arms.
    let expected_calls = nodes.len() as u64 * manifest.reps as u64 * 2;
    assert_eq!(
        call_count.load(Ordering::Relaxed),
        expected_calls,
        "run_experiment must perform exactly reps paired iterations (2 calls per pair)"
    );
}

#[test]
fn stub_driver_treatment_fails_gate_exit_1() {
    // Both arms fail → treatment node_pass_rate = 0.0 < floor 0.5 → gate_exit = 1.
    let manifest = small_manifest();
    let nodes = load_py_add();
    let (driver, _) = ArmDistinctDriver::new(false, true);
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );
    assert_eq!(
        rec.gate_exit, 1,
        "treatment fails floor → gate_exit = 1 (regression)"
    );
}

#[test]
fn pairs_formed_per_node_rep_call_count() {
    // Verifies: driver is called exactly reps × 2 per node, matching the pairing contract.
    let manifest = small_manifest();
    let nodes = load_py_add();
    let (driver, call_count) = ArmDistinctDriver::new(true, true);
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    let expected = nodes.len() as u64 * manifest.reps as u64 * 2;
    assert_eq!(
        call_count.load(Ordering::Relaxed),
        expected,
        "each (node, rep) pair produces exactly 2 driver calls"
    );
}

#[test]
fn gated_row_verdict_matches_gate_exit() {
    // When gate_exit = 0, the gated row verdict must be Some(true).
    // When gate_exit = 1, the gated row verdict must be Some(false).
    let manifest = small_manifest();
    let nodes = load_py_add();
    let baseline = baseline_floor_05();
    let judge = canned_judge();

    let (pass_driver, _) = ArmDistinctDriver::new(true, false);
    let rec_pass = run::run_experiment(
        &manifest,
        &nodes,
        &pass_driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );
    let row_pass = rec_pass
        .rows
        .iter()
        .find(|r| r.metric == "node_pass_rate")
        .unwrap();
    assert_eq!(
        row_pass.verdict,
        Some(true),
        "PASS: verdict must be Some(true)"
    );

    let (fail_driver, _) = ArmDistinctDriver::new(false, true);
    let rec_fail = run::run_experiment(
        &manifest,
        &nodes,
        &fail_driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );
    let row_fail = rec_fail
        .rows
        .iter()
        .find(|r| r.metric == "node_pass_rate")
        .unwrap();
    assert_eq!(
        row_fail.verdict,
        Some(false),
        "FAIL: verdict must be Some(false)"
    );
}

#[test]
fn tracked_rows_never_carry_verdict() {
    let manifest = small_manifest();
    let nodes = load_py_add();
    let (driver, _) = ArmDistinctDriver::new(true, false);
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );
    for row in rec.rows.iter().filter(|r| r.tag == "tracked") {
        assert!(
            row.verdict.is_none(),
            "tracked row '{}' must never carry a verdict",
            row.metric
        );
    }
}

// ── I2: only declared+wired metrics produce rows ──────────────────────────────

#[test]
fn run_experiment_omits_rows_for_un_wired_tracked_metrics() {
    // Manifest declares engine_broken_rate as tracked, but v1 has no wired source.
    // The row must NOT appear in results — no fabricated zeros should be shipped.
    let mut metrics = IndexMap::new();
    metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);
    metrics.insert("engine_broken_rate".to_string(), MetricTag::Tracked);
    let manifest = Manifest {
        name: "stub-no-engine".to_string(),
        reps: REPS,
        seed_base: 42,
        battery: vec!["py-add".to_string()],
        baseline: ArmConfig {
            loop_name: "execute-node".to_string(),
            model: "local-default".to_string(),
            context: ContextStrategy::None,
            env: BTreeMap::default(),
            backend: Backend::Local,
        },
        treatment: ArmConfig {
            loop_name: "execute-node".to_string(),
            model: "local-default".to_string(),
            context: ContextStrategy::Cxpak,
            env: BTreeMap::default(),
            backend: Backend::Local,
        },
        metrics,
        tolerance: BTreeMap::default(),
    };

    let nodes = load_py_add();
    let (driver, _) = ArmDistinctDriver::new(true, false);
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    assert!(
        !rec.rows.iter().any(|r| r.metric == "engine_broken_rate"),
        "engine_broken_rate has no v1 source — must not appear in rows; rows={:?}",
        rec.rows.iter().map(|r| &r.metric).collect::<Vec<_>>()
    );
}

#[test]
fn run_experiment_emits_judge_quality_row_when_declared_and_judge_has_data() {
    // Manifest declares judge_quality tracked; canned_judge returns non-zero totals
    // → judge_quality row must appear.  This ensures the fix does not accidentally
    // suppress genuinely wired+declared metrics.
    let manifest = small_manifest(); // declares judge_quality: tracked
    let nodes = load_py_add();
    let (driver, _) = ArmDistinctDriver::new(true, true);
    let judge = canned_judge(); // total = 3; both arms get real data
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    assert!(
        rec.rows
            .iter()
            .any(|r| r.metric == "judge_quality" && r.tag == "tracked"),
        "judge_quality must appear when declared and judge provides data; rows={:?}",
        rec.rows.iter().map(|r| &r.metric).collect::<Vec<_>>()
    );
}

// ── Cross-loop manifest fixture ───────────────────────────────────────────────

#[test]
fn cross_loop_manifest_parses_and_validates() {
    // Vendored alongside the corpus fixture so the parse/validate contract runs
    // standalone (the live manifest lives in the measurement component's repo).
    let path = fixture_root()
        .parent()
        .unwrap()
        .join("cross-loop-local-vs-claude.yaml");
    let m = load_manifest(&path).expect("parse cross-loop manifest");
    m.validate().expect("cross-loop manifest must be valid");
    assert!(m.is_cross_loop(), "must be flagged as cross-loop");
    assert_eq!(
        m.treatment.backend,
        Backend::ClaudeCli,
        "treatment arm must use claude-cli backend"
    );
}

// ── I3: LocalUnavailable aborts the experiment ────────────────────────────────

#[test]
fn local_unavailable_aborts_experiment_with_exit_3() {
    // StubDriver scripted to return LocalUnavailable for py-add → both arms of
    // every rep return unavailable → abort must be detected and gate must NOT
    // emit a 0 (PASS) exit code masking the outage.
    let mut scripted = HashMap::new();
    scripted.insert("py-add".to_string(), RunStatus::LocalUnavailable);
    let driver = StubDriver { scripted };

    let manifest = small_manifest();
    let nodes = load_py_add();
    let judge = canned_judge();
    let baseline = baseline_floor_05();

    let rec = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    assert!(
        rec.aborted,
        "LocalUnavailable driver must set aborted=true; gate_exit={}",
        rec.gate_exit
    );
    assert_eq!(
        rec.gate_exit, 3,
        "aborted experiment must carry exit code 3, not 0 (PASS) or 1 (FAIL)"
    );
    assert!(
        rec.abort_reason
            .as_deref()
            .map(|s| s.contains("unavailable"))
            .unwrap_or(false),
        "abort_reason must mention 'unavailable'; got {:?}",
        rec.abort_reason
    );
    assert!(
        rec.rows.is_empty(),
        "aborted experiment must not carry partial gate rows"
    );
}
