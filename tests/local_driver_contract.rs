//! Contract tests for LocalNodeDriver: real subprocess, deterministic via EXECUTE_NODE_MOCK.
//! Requires python3 + git on PATH (dev environment).

use abproof::{
    corpus::NodeJson,
    driver::{LocalNodeDriver, RunStatus, SessionDriver},
    experiment::{ArmConfig, Backend, ContextStrategy},
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

// ---------------------------------------------------------------------------
// TempDir — isolated temp directory with automatic cleanup on drop.
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("dotclaude-lnd-{label}-{id}"));
        fs::create_dir_all(&dir).expect("create tempdir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// Initialise a repo suitable for test execution: user config, no gpg, and an
/// empty core.hooksPath so the global commit-msg hook (HITL gate) never fires.
fn git_init(dir: &Path) {
    let no_hooks = dir.join(".no-hooks");
    fs::create_dir_all(&no_hooks).expect("create no-hooks dir");

    run_git(dir, &["init", "-q"]);
    run_git(dir, &["config", "user.email", "test@test.local"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    run_git(
        dir,
        &[
            "config",
            "core.hooksPath",
            no_hooks.to_str().expect("utf8 path"),
        ],
    );
}

fn git_commit_all(dir: &Path, message: &str) {
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", message]);
}

// ---------------------------------------------------------------------------
// Script path helper
// ---------------------------------------------------------------------------

fn execute_node_script() -> PathBuf {
    execute_node_path()
}

/// The execute-node loop is the Executor component — absent when abproof is built
/// standalone (only present inside a full harness checkout). These contract tests
/// shell it, so they skip cleanly when it is not on disk.
fn loop_available() -> bool {
    execute_node_path().is_file()
}

// ---------------------------------------------------------------------------
// Contract test 1: success path via mock
// ---------------------------------------------------------------------------

#[test]
fn local_node_driver_fills_stub_via_mock() {
    // Requires python3 + git on PATH. Deterministic: EXECUTE_NODE_MOCK hands
    // execute_node.py a canned SEARCH/REPLACE block, so no local LLM is needed.
    if !loop_available() {
        eprintln!("skip: execute-node loop (Executor) absent — standalone build");
        return;
    }
    let repo = TempDir::new("lnd-ok");
    git_init(repo.path());

    fs::write(
        repo.path().join("calc.py"),
        "def add(a, b):\n    raise NotImplementedError\n",
    )
    .expect("write calc.py");
    fs::write(
        repo.path().join("acceptance_test.py"),
        "from calc import add\nassert add(2,3)==5\nprint('OK')\n",
    )
    .expect("write acceptance_test.py");
    git_commit_all(repo.path(), "red baseline");

    // One SEARCH/REPLACE block that implements add.
    // Written OUTSIDE the repo so execute_node.py's is_clean() sees a clean tree.
    let mock_dir = TempDir::new("lnd-ok-mock");
    let mock = mock_dir.path().join("mock.txt");
    fs::write(
        &mock,
        "calc.py\n<<<<<<< SEARCH\n    raise NotImplementedError\n=======\n    return a + b\n>>>>>>> REPLACE\n",
    )
    .expect("write mock.txt");

    let node = NodeJson {
        id: "py-add".into(),
        change: "Implement add".into(),
        files: vec!["calc.py".into()],
        accept: "python3 acceptance_test.py".into(),
        forbid: vec![],
        requires: vec![],
        // Prebuilt tree via arm.env EXECUTE_NODE_ROOT — no in-driver materialization.
        materialize: None,
    };

    let mut env = BTreeMap::new();
    env.insert("EXECUTE_NODE_MOCK".to_string(), mock.display().to_string());
    env.insert(
        "EXECUTE_NODE_ROOT".to_string(),
        repo.path().display().to_string(),
    );
    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "local-default".into(),
        context: ContextStrategy::None,
        env,
        backend: Backend::Local,
    };

    let driver = LocalNodeDriver {
        script: execute_node_script(),
        timeout: Duration::from_secs(60),
    };

    let out = driver.run(&arm, &node, 1).expect("driver run");
    assert_eq!(out.status, RunStatus::Success);
    assert!(out.accept_passed);
    assert!(
        out.edited_files.iter().any(|f| f.ends_with("calc.py")),
        "edited_files should contain calc.py; got {:?}",
        out.edited_files
    );
}

// ---------------------------------------------------------------------------
// Contract test 2: timeout path returns Inconclusive, never an Err
// ---------------------------------------------------------------------------

#[test]
fn local_node_driver_timeout_scores_inconclusive() {
    // accept = long sleep; timeout = 1s → execute_node.py is killed before
    // the model is even called; driver returns Inconclusive (a wall-clock
    // kill with no gradable completion, ADR-0042 timeout-collapse), never
    // propagates Err.
    if !loop_available() {
        eprintln!("skip: execute-node loop (Executor) absent — standalone build");
        return;
    }
    let repo = TempDir::new("lnd-timeout");
    git_init(repo.path());

    fs::write(
        repo.path().join("calc.py"),
        "def add(a, b):\n    raise NotImplementedError\n",
    )
    .expect("write calc.py");
    fs::write(
        repo.path().join("acceptance_test.py"),
        "from calc import add\nassert add(2,3)==5\nprint('OK')\n",
    )
    .expect("write acceptance_test.py");
    git_commit_all(repo.path(), "red baseline");

    // Mock file present (outside the repo so is_clean() stays clean) to prevent
    // any attempt to reach a local LLM if the pre-pass check returned quickly.
    let mock_dir = TempDir::new("lnd-timeout-mock");
    let mock = mock_dir.path().join("mock.txt");
    fs::write(
        &mock,
        "calc.py\n<<<<<<< SEARCH\n    raise NotImplementedError\n=======\n    return a + b\n>>>>>>> REPLACE\n",
    )
    .expect("write mock.txt");

    let node = NodeJson {
        id: "py-add-timeout".into(),
        change: "Implement add".into(),
        files: vec!["calc.py".into()],
        // Accept command sleeps 5 s; the 1 s driver timeout fires during pre_pass.
        accept: "python3 -c 'import time; time.sleep(5)'".into(),
        forbid: vec![],
        requires: vec![],
        materialize: None,
    };

    let mut env = BTreeMap::new();
    env.insert("EXECUTE_NODE_MOCK".to_string(), mock.display().to_string());
    env.insert(
        "EXECUTE_NODE_ROOT".to_string(),
        repo.path().display().to_string(),
    );
    let arm = ArmConfig {
        loop_name: "execute-node".into(),
        model: "local-default".into(),
        context: ContextStrategy::None,
        env,
        backend: Backend::Local,
    };

    let driver = LocalNodeDriver {
        script: execute_node_script(),
        timeout: Duration::from_secs(1),
    };

    let out = driver
        .run(&arm, &node, 1)
        .expect("driver run must not Err on timeout");
    assert_eq!(out.status, RunStatus::Inconclusive);
    assert!(!out.accept_passed);
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
