//! abproof#25 — a curation defect must surface when the battery is loaded, not when a
//! paid run finally reaches the offending node.
//!
//! `run --dry-run` is the pre-flight: it resolves the corpus, expands the battery glob and
//! loads every node, because it cannot count `loop_runs` without them. Before this, it then
//! projected a tidy 6-run/12-minute battery over nodes that a real run refuses — a false
//! *plan*, in a crate whose stated posture is that "an offline oracle that hid a broken run
//! behind a PASS would defeat its own purpose".
//!
//! The two rules are not new here. They are the ones `driver::temp_node_path` and
//! `worktree::write_one` already enforce; this asserts that the same rule is applied while
//! the nodes are in hand. The predicates are shared, so a node refused at load and a node
//! refused at run cannot disagree.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Same hand-rolled shape the other integration tests use — the crate has no `tempfile`
/// dev-dependency and this does not need one.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("abproof-curation-{}-{id}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        Self(p)
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

/// Nodes live under `bat/` because `battery_matches` only globs a `prefix/*` pattern —
/// a bare `*` resolves to a literal directory named `*` and matches nothing.
fn write_node(root: &Path, dir_name: &str, meta: &str) {
    let dir = root.join("bat").join(dir_name);
    fs::create_dir_all(dir.join("seed")).unwrap();
    fs::write(dir.join("meta.yaml"), meta).unwrap();
    fs::write(
        dir.join("seed").join("calc.py"),
        "def add(a, b):\n    raise\n",
    )
    .unwrap();
    fs::write(
        dir.join("seed").join("acceptance_test.py"),
        "from calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n",
    )
    .unwrap();
}

const GOOD: &str = r#"id: "py-good"
language: "python"
files: ["calc.py"]
accept: "python3 -m pytest -q"
requires: ["python3"]
change: "make it pass"
"#;

/// The corpus-authored id that `driver.rs` refuses. Filed with `../../qa-escaped`.
const EVIL_ID: &str = r#"id: "../../qa-escaped"
language: "python"
files: ["calc.py"]
accept: "python3 -m pytest -q"
requires: ["python3"]
change: "make it pass"
"#;

/// The `meta.files` traversal that `worktree.rs` refuses.
const EVIL_PATH: &str = r#"id: "qa-evil-path"
language: "python"
files: ["../../../../tmp/qa-pwned.txt"]
accept: "python3 -m pytest -q"
requires: ["python3"]
change: "make it pass"
"#;

fn battery(root: &Path) -> Result<Vec<abproof::corpus::CorpusNode>, abproof::corpus::CorpusError> {
    abproof::corpus::load_battery(root, &["bat/*".to_string()])
}

#[test]
fn a_battery_of_well_formed_nodes_still_loads() {
    // The control. Without it, a loader that refused everything would satisfy both
    // assertions below while making the tool unusable.
    let tmp = TempDir::new();
    write_node(tmp.path(), "py-good", GOOD);
    let nodes = battery(tmp.path()).expect("a well-formed battery must load");
    assert_eq!(nodes.len(), 1, "the good node did not load: {nodes:?}");
}

#[test]
fn a_node_id_that_is_not_a_slug_is_refused_at_load() {
    let tmp = TempDir::new();
    write_node(tmp.path(), "py-good", GOOD);
    write_node(tmp.path(), "evil-id", EVIL_ID);
    let err = battery(tmp.path()).expect_err(
        "a battery containing a non-slug node.id loaded clean — a dry-run over it \
         projects a plan for a node no run can execute",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("../../qa-escaped"),
        "the error must name the offending node so the curation defect is actionable: {msg}"
    );
    assert!(
        msg.contains("slug"),
        "the error must say which rule was broken: {msg}"
    );
}

#[test]
fn a_meta_files_traversal_is_refused_at_load() {
    let tmp = TempDir::new();
    write_node(tmp.path(), "py-good", GOOD);
    write_node(tmp.path(), "evil-path", EVIL_PATH);
    let err = battery(tmp.path()).expect_err(
        "a battery containing a meta.files traversal loaded clean — worktree.rs would \
         refuse it, but only once a paid run reached that node",
    );
    let msg = err.to_string();
    assert!(
        msg.contains("qa-evil-path"),
        "the error must name the offending node: {msg}"
    );
    assert!(
        msg.contains("../../../../tmp/qa-pwned.txt"),
        "the error must name the offending path, not just the node: {msg}"
    );
}
