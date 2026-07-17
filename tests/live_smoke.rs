//! Live integration smoke test.
//!
//! Exercises the full subprocess plumbing: `LocalNodeDriver` spawns `execute_node.py`
//! against a real local LLM with NO mock.  Reps are clamped to 2 for speed.
//!
//! Run only under `cargo test -- --ignored` — never in the normal CI gate.

use abproof::{
    corpus,
    driver::{LocalNodeDriver, RunStatus, SessionDriver},
    experiment::{ArmConfig, Backend, ContextStrategy, Manifest, MetricTag},
    judge::{JudgeScore, StubJudge},
    run,
    score::Baseline,
};
use indexmap::IndexMap;
use std::{collections::BTreeMap, time::Duration};

#[test]
#[ignore = "requires local-llm runtime on PATH (LLM_PORT) + python3 + git"]
fn live_one_node_battery_produces_wellformed_result() {
    // Reps clamped to 2 to keep runtime short while still exercising the full
    // pair loop: subprocess spawn, node-JSON hand-off, env wiring, artifact
    // capture, timeout, and scoring.
    let mut metrics = IndexMap::new();
    metrics.insert("node_pass_rate".to_string(), MetricTag::Gated);

    let manifest = Manifest {
        name: "live-smoke".to_string(),
        reps: 2,
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

    let corpus_root = corpus::red_baseline_root();
    let nodes =
        corpus::load_battery(&corpus_root, &manifest.battery).expect("py-add must load from disk");

    let script = execute_node_path();

    let driver = LocalNodeDriver {
        script,
        timeout: Duration::from_secs(300),
    };

    let judge = StubJudge {
        canned: JudgeScore {
            per_criterion: IndexMap::new(),
            total: 0,
        },
    };

    let baseline = Baseline {
        name: "live-smoke".to_string(),
        gated: {
            let mut m = BTreeMap::new();
            m.insert("node_pass_rate".to_string(), 0.0_f64);
            m
        },
    };

    let record = run::run_experiment(
        &manifest,
        &nodes,
        &driver,
        &judge,
        &baseline,
        &run::RunOptions::default(),
    );

    // The gated row must exist and carry finite statistics.
    let gated = record
        .rows
        .iter()
        .find(|r| r.metric == "node_pass_rate")
        .expect("gated row node_pass_rate must exist in result");

    assert!(
        gated.treatment.is_finite(),
        "node_pass_rate must be finite; got {}",
        gated.treatment
    );

    let p = gated
        .p_two_sided
        .expect("gated row must carry Wilcoxon p-value");
    assert!(p.is_finite(), "p must be finite; got {p}");

    let dz = gated.d_z.expect("gated row must carry d_z");
    assert!(dz.is_finite(), "d_z must be finite; got {dz}");

    assert!(
        gated.ci_lower.is_some() && gated.ci_upper.is_some(),
        "gated row must carry CI bounds"
    );

    assert!(
        record.gate_exit == 0 || record.gate_exit == 1,
        "gate_exit must be 0 or 1; got {}",
        record.gate_exit
    );

    // Result must round-trip through JSON without error.
    let json = serde_json::to_string(&record).expect("result must serialise to JSON");
    let _: serde_json::Value = serde_json::from_str(&json).expect("result JSON must parse");
}

/// Live cross-loop smoke: one corpus node through `LocalNodeDriver` with a real
/// `claude -p` backend.  No mock env — this costs real money.
///
/// Asserts: status ∈ {Success, Failure}, cost_usd is Some and finite, num_turns == 1.
/// Run only under `cargo test -- --ignored`.
#[test]
#[ignore = "requires real claude -p (network + auth + cost)"]
fn live_cross_loop_one_node_claude_cli_wellformed() {
    let corpus_root = corpus::red_baseline_root();
    let nodes = corpus::load_battery(&corpus_root, &["py-add".to_string()])
        .expect("py-add must load from disk");
    let node = nodes
        .first()
        .expect("py-add battery must have at least one node");

    let script = execute_node_path();

    let driver = LocalNodeDriver {
        script,
        timeout: Duration::from_secs(300),
    };

    let arm = ArmConfig {
        loop_name: "execute-node".to_string(),
        model: "claude-sonnet-4-6".to_string(),
        context: ContextStrategy::None,
        env: BTreeMap::default(),
        backend: Backend::ClaudeCli,
    };

    let node_json = corpus::bridge_node(node, &arm);

    let output = driver
        .run(&arm, &node_json, 42)
        .expect("driver must not error; verify python3 and execute_node.py are present");

    // Status must be Success or Failure — not Inconclusive, Skipped, or LocalUnavailable.
    assert!(
        matches!(output.status, RunStatus::Success | RunStatus::Failure),
        "status must be Success or Failure; got {:?}",
        output.status
    );

    // Claude-cli arm must report a finite, non-negative cost.
    let cost = output
        .cost_usd
        .expect("claude-cli arm must report cost_usd; check LLM_BACKEND=claude-cli wiring");
    assert!(
        cost.is_finite() && cost >= 0.0,
        "cost must be finite and non-negative; got {cost}"
    );

    // Single-turn constraint: tools-off means exactly 1 turn.
    assert_eq!(
        output.num_turns, 1,
        "claude-cli arm must complete in exactly 1 turn (tools-off constraint); got {}",
        output.num_turns
    );
}

/// Locate the execute-node loop: $ABPROOF_EXECUTE_NODE, else walk up from CWD for
/// skills/execute-node/execute_node.py (resolves inside a checkout).
fn execute_node_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("ABPROOF_EXECUTE_NODE") {
        return std::path::PathBuf::from(p);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        loop {
            let cand = dir.join("skills/execute-node/execute_node.py");
            if cand.is_file() {
                return cand;
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }
    std::path::PathBuf::from("skills/execute-node/execute_node.py")
}
