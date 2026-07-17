//! Contract tests for LocalNodeDriver with ClaudeCli backend:
//!   - Pure unit tests for parse_usage_line
//!   - Subprocess test via a fake `claude` binary (no EXECUTE_NODE_MOCK)
//!   - child_env unit tests asserting LLM_BACKEND / LLM_BACKEND_MODEL forwarding

use abproof::corpus::NodeJson;
use abproof::driver::{parse_usage_line, LocalNodeDriver, RunStatus, SessionDriver};
use abproof::experiment::{ArmConfig, Backend, ContextStrategy};
use std::collections::BTreeMap;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("dotclaude-cctest-{prefix}-{id}"));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git_cmd(args: &[&str], repo: &std::path::Path) {
    std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
}

fn git_init(root: &std::path::Path) {
    let no_hooks = root.join(".no-hooks");
    fs::create_dir_all(&no_hooks).unwrap();

    std::process::Command::new("git")
        .args(["init", "-q", root.to_str().unwrap()])
        .output()
        .unwrap();
    git_cmd(&["config", "user.email", "t@t.com"], root);
    git_cmd(&["config", "user.name", "Test"], root);
    git_cmd(&["config", "commit.gpgsign", "false"], root);
    git_cmd(
        &["config", "core.hooksPath", no_hooks.to_str().unwrap()],
        root,
    );
}

fn git_commit_all(root: &std::path::Path, msg: &str) {
    git_cmd(&["add", "-A"], root);
    git_cmd(&["commit", "-q", "-m", msg], root);
}

fn execute_node_script() -> std::path::PathBuf {
    execute_node_path()
}

fn write_fake_claude(bin_dir: &std::path::Path, canned_json: &str) {
    use std::os::unix::fs::PermissionsExt;
    let script = bin_dir.join("claude");
    // Use printf to avoid shell interpretation of the JSON content.
    let escaped = canned_json.replace('\'', "'\\''");
    fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{escaped}'\n")).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
}

fn canned_json() -> String {
    // Build at runtime to avoid multiline const issues.
    let sr = "calc.py\\n<<<<<<< SEARCH\\n    raise NotImplementedError\\n\
              =======\\n    return a + b\\n>>>>>>> REPLACE";
    format!(
        r#"{{"result":"{sr}","total_cost_usd":0.01,"usage":{{"input_tokens":10,"output_tokens":20}},"num_turns":1}}"#
    )
}

// ── parse_usage_line pure unit tests ────────────────────────────────────────

#[test]
fn usage_line_parsing_known_cost() {
    let u = parse_usage_line(
        "USAGE cost_usd=0.01 input_tokens=10 output_tokens=20 calls=1 num_turns=1",
    );
    assert_eq!(u.cost_usd, Some(0.01));
    assert_eq!(u.input_tokens, 10);
    assert_eq!(u.output_tokens, 20);
    assert_eq!(u.claude_calls, 1);
    assert_eq!(u.num_turns, 1);
}

#[test]
fn usage_line_parsing_unknown_cost() {
    let u = parse_usage_line(
        "USAGE cost_usd=unknown input_tokens=5 output_tokens=15 calls=2 num_turns=2",
    );
    assert_eq!(u.cost_usd, None, "unknown must parse to None");
    assert_eq!(u.input_tokens, 5);
    assert_eq!(u.output_tokens, 15);
    assert_eq!(u.claude_calls, 2);
    assert_eq!(u.num_turns, 2);
}

#[test]
fn usage_line_parsing_zero_line() {
    let u =
        parse_usage_line("USAGE cost_usd=0.0 input_tokens=0 output_tokens=0 calls=0 num_turns=0");
    assert_eq!(u.cost_usd, Some(0.0));
    assert_eq!(u.claude_calls, 0);
}

#[test]
fn usage_line_parsing_absent_returns_default() {
    let u = parse_usage_line("SUCCESS");
    assert_eq!(u.cost_usd, None);
    assert_eq!(u.claude_calls, 0);
}

// ── child_env: LLM_BACKEND / LLM_BACKEND_MODEL forwarding ───────────────────

#[test]
fn child_env_sets_llm_backend_for_claude_cli_arm() {
    use abproof::driver::child_env;

    let mut parent = BTreeMap::new();
    parent.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
    parent.insert("GH_TOKEN".to_string(), "should-be-dropped".to_string());

    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "claude-haiku-4-5".into(),
        context: ContextStrategy::None,
        env: BTreeMap::default(),
        backend: Backend::ClaudeCli,
    };

    let result = child_env(&arm, 1, &parent);

    assert_eq!(
        result.get("LLM_BACKEND").map(|s| s.as_str()),
        Some("claude-cli"),
        "ClaudeCli arm must set LLM_BACKEND=claude-cli; env={result:?}"
    );
    assert_eq!(
        result.get("LLM_BACKEND_MODEL").map(|s| s.as_str()),
        Some("claude-haiku-4-5"),
        "ClaudeCli arm with non-empty model must set LLM_BACKEND_MODEL; env={result:?}"
    );
    assert!(
        !result.contains_key("GH_TOKEN"),
        "GH_TOKEN must still be dropped; env={result:?}"
    );
}

#[test]
fn child_env_sets_llm_backend_local_for_local_arm() {
    use abproof::driver::child_env;

    let mut parent = BTreeMap::new();
    parent.insert("PATH".to_string(), "/usr/bin:/bin".to_string());

    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "local-default".into(),
        context: ContextStrategy::None,
        env: BTreeMap::default(),
        backend: Backend::Local,
    };

    let result = child_env(&arm, 1, &parent);

    assert_eq!(
        result.get("LLM_BACKEND").map(|s| s.as_str()),
        Some("local"),
        "Local arm must set LLM_BACKEND=local; env={result:?}"
    );
    // Local arm must NOT set LLM_BACKEND_MODEL (unless arm.env provides it).
    assert!(
        !result.contains_key("LLM_BACKEND_MODEL"),
        "Local arm must not set LLM_BACKEND_MODEL; env={result:?}"
    );
}

// ── Rust contract test: full subprocess via LocalNodeDriver + fake claude ────

#[test]
fn claude_cli_backend_fills_stub_via_fake_claude() {
    // Requires python3 + git on PATH. Deterministic via fake claude binary.
    // NO EXECUTE_NODE_MOCK — the real claude-cli backend path is exercised.
    let repo = TempDir::new("ccdrv-repo");
    let bin_dir = TempDir::new("ccdrv-bin");

    git_init(repo.path());
    fs::write(
        repo.path().join("calc.py"),
        "def add(a, b):\n    raise NotImplementedError\n",
    )
    .unwrap();
    fs::write(
        repo.path().join("acceptance_test.py"),
        "from calc import add\nassert add(2,3)==5\nassert add(-1,1)==0\nprint('OK')\n",
    )
    .unwrap();
    git_commit_all(repo.path(), "red");

    write_fake_claude(bin_dir.path(), &canned_json());

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{orig_path}", bin_dir.path().display());

    let mut arm_env = BTreeMap::new();
    arm_env.insert(
        "EXECUTE_NODE_ROOT".to_string(),
        repo.path().display().to_string(),
    );
    arm_env.insert("PATH".to_string(), new_path);
    // arm.env PATH override wins via the existing step-4 overlay in child_env.

    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "claude-haiku-4-5".into(),
        context: ContextStrategy::None,
        backend: Backend::ClaudeCli,
        env: arm_env,
    };
    let node = NodeJson {
        id: "py-add".into(),
        change: "Implement add so acceptance_test.py passes.".into(),
        files: vec!["calc.py".into()],
        accept: "python3 acceptance_test.py".into(),
        forbid: vec![],
        requires: vec![],
        // Tree hand-built above + EXECUTE_NODE_ROOT via arm.env — back-compat path.
        materialize: None,
    };
    let driver = LocalNodeDriver {
        script: execute_node_script(),
        timeout: Duration::from_secs(60),
    };

    let out = driver
        .run(&arm, &node, 1)
        .expect("driver run must not error");

    assert_eq!(
        out.status,
        RunStatus::Success,
        "status must be Success; stdout_tail={:?}",
        out.stdout_tail
    );
    assert!(out.accept_passed);
    assert_eq!(
        out.cost_usd,
        Some(0.01),
        "cost_usd must be parsed from canned JSON"
    );
    assert_eq!(out.input_tokens, 10);
    assert_eq!(out.output_tokens, 20);
    assert_eq!(out.claude_calls, 1);
    assert_eq!(out.num_turns, 1);
}

/// REGRESSION GUARD — the test whose absence hid the broken execution substrate.
/// `LocalNodeDriver` must materialize its OWN clean work tree from a real corpus
/// node. This is the path that previously aborted `FAILURE(dirty_tree)` because
/// nothing provisioned a tree. Deterministic via `EXECUTE_NODE_MOCK` (no model, no
/// cost); requires python3 + git on PATH.
#[test]
fn local_driver_materializes_worktree_and_succeeds_via_mock() {
    let node = abproof::corpus::load_node(&abproof::corpus::red_baseline_root().join("py-add"))
        .expect("py-add corpus node must load");

    // Mock model response: a SEARCH/REPLACE block that fixes calc.py's stub.
    let mock = TempDir::new("mock");
    let mock_file = mock.path().join("resp.txt");
    fs::write(
        &mock_file,
        "calc.py\n<<<<<<< SEARCH\n    raise NotImplementedError\n=======\n    return a + b\n>>>>>>> REPLACE\n",
    )
    .unwrap();
    // Redirect the metrics jsonl into a temp dir so the run leaves nothing behind.
    let metrics = TempDir::new("metrics");

    let mut env = BTreeMap::new();
    env.insert(
        "EXECUTE_NODE_MOCK".to_string(),
        mock_file.display().to_string(),
    );
    env.insert(
        "LOCAL_EXEC_DIR".to_string(),
        metrics.path().display().to_string(),
    );
    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "local".into(),
        context: ContextStrategy::None,
        backend: Backend::Local,
        env,
    };

    let node_json = abproof::corpus::bridge_node(&node, &arm);
    assert!(
        node_json.materialize.is_some(),
        "bridge_node must populate the materialize spec for a real corpus node"
    );

    let driver = LocalNodeDriver {
        script: execute_node_script(),
        timeout: Duration::from_secs(60),
    };
    let out = driver
        .run(&arm, &node_json, 7)
        .expect("driver run must not error");

    assert_eq!(
        out.status,
        RunStatus::Success,
        "a materialized mock run must SUCCEED (was FAILURE(dirty_tree)); tail={:?}",
        out.stdout_tail
    );
    assert!(out.accept_passed, "acceptance must pass after the fix");
    assert_eq!(out.claude_calls, 0, "mock path makes no claude calls");
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
