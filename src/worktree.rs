//! Node work-tree provisioner.
//!
//! `execute_node.py` requires `EXECUTE_NODE_ROOT` to point at a **clean git repo**
//! containing the RED stub file(s) and the acceptance test; it aborts
//! `FAILURE(dirty_tree)` otherwise. Nothing upstream materialized such a tree
//! from a corpus node — so every real run reached the model on zero nodes. This
//! module builds that tree: a fresh temp repo with one clean baseline commit, torn
//! down on drop.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static WT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The complete RED project to materialize into a node's work tree: every seed
/// file by relative path — the stub source(s) the model must fix, the acceptance
/// test(s), and any scaffold (`Cargo.toml`, `go.mod`, `src/` layout). Carried
/// in-memory on [`crate::corpus::NodeJson`] (`#[serde(skip)]`) — never part of the
/// JSON contract `execute_node.py` reads.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterializeSpec {
    /// `(relative path, content)` for every file in the seed project.
    pub files: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("worktree io: {0}")]
    Io(String),
    #[error("worktree git: {0}")]
    Git(String),
}

/// An owned, isolated git work tree holding a node's RED baseline. The directory is
/// removed on drop (best-effort) on every path, including panics and early returns.
pub struct NodeWorkspace {
    root: PathBuf,
}

impl NodeWorkspace {
    /// Materialize `spec` into a fresh temp git repo with a single clean commit.
    ///
    /// Hooks are disabled in the repo (`core.hooksPath` → a non-directory) so the
    /// global HITL commit-msg hook never fires on these internal fixture commits —
    /// and so `execute_node.py`'s own success commit stays hook-free too.
    pub fn create(spec: &MaterializeSpec) -> Result<NodeWorkspace, WorktreeError> {
        let n = WT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("abproof-wt-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&root)
            .map_err(|e| WorktreeError::Io(format!("create {}: {e}", root.display())))?;
        // Guard is live from here: any `?` below removes the dir on drop.
        let ws = NodeWorkspace { root };
        ws.init_repo()?;
        ws.write_files(spec)?;
        ws.commit_baseline()?;
        ws.assert_clean()?;
        Ok(ws)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn git(&self, args: &[&str]) -> Result<(), WorktreeError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .map_err(|e| WorktreeError::Io(format!("spawn git {}: {e}", args.join(" "))))?;
        if !out.status.success() {
            return Err(WorktreeError::Git(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    fn init_repo(&self) -> Result<(), WorktreeError> {
        self.git(&["init", "-q"])?;
        self.git(&["config", "user.email", "abproof@localhost"])?;
        self.git(&["config", "user.name", "abproof"])?;
        self.git(&["config", "commit.gpgsign", "false"])?;
        // /dev/null is not a directory: git finds no hooks there, so none run.
        self.git(&["config", "core.hooksPath", "/dev/null"])?;
        Ok(())
    }

    fn write_files(&self, spec: &MaterializeSpec) -> Result<(), WorktreeError> {
        for (rel, content) in &spec.files {
            self.write_one(rel, content)?;
        }
        Ok(())
    }

    fn write_one(&self, rel: &str, content: &str) -> Result<(), WorktreeError> {
        // Defense-in-depth: MaterializeSpec::files is a public field, so an external
        // caller of this library crate could hand us a path that escapes the work tree.
        // Reject absolute paths and any `..` component before joining onto root.
        let rel_path = std::path::Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(WorktreeError::Io(format!(
                "unsafe materialize path '{rel}' (absolute or contains '..') — refused"
            )));
        }
        let target = self.root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| WorktreeError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        std::fs::write(&target, content)
            .map_err(|e| WorktreeError::Io(format!("write {}: {e}", target.display())))
    }

    fn commit_baseline(&self) -> Result<(), WorktreeError> {
        self.git(&["add", "-A"])?;
        self.git(&["commit", "-q", "-m", "red baseline"])
    }

    fn assert_clean(&self) -> Result<(), WorktreeError> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["status", "--porcelain"])
            .output()
            .map_err(|e| WorktreeError::Io(format!("git status: {e}")))?;
        if !out.status.success() {
            return Err(WorktreeError::Git("git status failed".into()));
        }
        if !out.stdout.is_empty() {
            return Err(WorktreeError::Git(format!(
                "work tree not clean after baseline commit: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            )));
        }
        Ok(())
    }
}

impl Drop for NodeWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> MaterializeSpec {
        MaterializeSpec {
            files: vec![
                (
                    "calc.py".into(),
                    "def add(a, b):\n    raise NotImplementedError\n".into(),
                ),
                (
                    "acceptance_test.py".into(),
                    "from calc import add\nassert add(2, 3) == 5\n".into(),
                ),
            ],
        }
    }

    #[test]
    fn create_materializes_clean_red_tree() {
        let ws = NodeWorkspace::create(&spec()).expect("materialize");
        assert!(ws.root().join("calc.py").exists(), "stub file must exist");
        assert!(
            ws.root().join("acceptance_test.py").exists(),
            "acceptance test must exist"
        );
        assert_eq!(
            std::fs::read_to_string(ws.root().join("calc.py")).unwrap(),
            "def add(a, b):\n    raise NotImplementedError\n",
            "stub content must be written verbatim"
        );
        // Clean tree: `git status --porcelain` is empty (execute_node.py's precondition).
        let status = Command::new("git")
            .arg("-C")
            .arg(ws.root())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            status.stdout.is_empty(),
            "tree must be clean after baseline commit"
        );
        // Exactly one commit on HEAD.
        let count = Command::new("git")
            .arg("-C")
            .arg(ws.root())
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    }

    #[test]
    fn create_writes_nested_paths() {
        let s = MaterializeSpec {
            files: vec![
                ("pkg/mod.py".into(), "x = 1\n".into()),
                ("acceptance_test.py".into(), "print('ok')\n".into()),
            ],
        };
        let ws = NodeWorkspace::create(&s).expect("materialize nested");
        assert!(
            ws.root().join("pkg/mod.py").exists(),
            "nested file's parent dirs must be created"
        );
    }

    #[test]
    fn create_rejects_path_traversal_in_materialize_spec() {
        // MaterializeSpec::files is public; a `..` component must be refused, not written
        // outside the work tree.
        for bad in ["../escape.py", "pkg/../../escape.py", "/abs/escape.py"] {
            let s = MaterializeSpec {
                files: vec![(bad.into(), "x = 1\n".into())],
            };
            let err = NodeWorkspace::create(&s)
                .err()
                .unwrap_or_else(|| panic!("path '{bad}' must be refused, not materialized"));
            assert!(
                matches!(err, WorktreeError::Io(m) if m.contains("unsafe materialize path")),
                "traversal path '{bad}' must fail with the unsafe-path error"
            );
        }
    }

    #[test]
    fn drop_removes_the_directory() {
        let root = {
            let ws = NodeWorkspace::create(&spec()).expect("materialize");
            ws.root().to_path_buf()
        };
        assert!(!root.exists(), "workspace dir must be removed on drop");
    }

    /// Percentile of a sorted slice via nearest-rank (p in [0,100]).
    fn pct(sorted: &[u128], p: u128) -> u128 {
        if sorted.is_empty() {
            return 0;
        }
        let rank = ((p * (sorted.len() as u128 - 1)) / 100) as usize;
        sorted[rank]
    }

    /// Issue #2 spike (measurement, not a shipped feature): the isolated provisioning
    /// *ceiling* — how fast `NodeWorkspace::create` + `Drop` can go back-to-back with
    /// warm caches. This is a BEST-CASE number: looping warms git's object/dentry/inode
    /// caches, so real per-rep cost (each rep interleaved with a heavy model spawn on a
    /// colder path) is higher. Reported alongside the in-run fraction, never used as the
    /// gate on its own. `#[ignore]`d: run manually with
    /// `cargo test -- --ignored bench_provision_ceiling --nocapture`.
    #[test]
    #[ignore = "spike: manual provisioning ceiling benchmark; run with --ignored --nocapture"]
    fn bench_provision_ceiling() {
        let spec = spec();
        let iters: usize = 100;
        let mut create_us: Vec<u128> = Vec::with_capacity(iters);
        let mut drop_us: Vec<u128> = Vec::with_capacity(iters);
        for i in 0..iters {
            let t0 = std::time::Instant::now();
            let ws = NodeWorkspace::create(&spec).expect("create");
            let create = t0.elapsed().as_micros();
            let d0 = std::time::Instant::now();
            drop(ws);
            let drop_e = d0.elapsed().as_micros();
            // Discard iteration 0: first git/spawn warmup is not representative.
            if i == 0 {
                continue;
            }
            create_us.push(create);
            drop_us.push(drop_e);
        }
        assert!(
            create_us.len() >= 2,
            "need at least 2 post-warmup samples; got {}",
            create_us.len()
        );
        create_us.sort_unstable();
        drop_us.sort_unstable();
        let summary = format!(
            "PROVISION_CEILING samples={} \
             create_us(min/median/p90/max)={}/{}/{}/{} \
             drop_us(min/median/p90/max)={}/{}/{}/{} \
             (WARM-CACHE BEST CASE — real per-rep cost is higher; \
             compare against PROVISION_SAMPLE frac from a real-model run)",
            create_us.len(),
            create_us[0],
            pct(&create_us, 50),
            pct(&create_us, 90),
            create_us[create_us.len() - 1],
            drop_us[0],
            pct(&drop_us, 50),
            pct(&drop_us, 90),
            drop_us[drop_us.len() - 1],
        );
        eprintln!("{summary}");
        assert!(
            summary.contains("PROVISION_CEILING") && summary.contains("create_us"),
            "summary must carry the diagnostic markers; got: {summary}"
        );
    }

    /// Guards the ceiling bench's cleanup contract cheaply (3 iters, always run):
    /// every `create` must be balanced by its `Drop`. Tracks the exact directories
    /// this test creates — counting by the shared `abproof-wt-<pid>-` prefix would
    /// be racy, since sibling `worktree` tests share the PID and run in parallel.
    #[test]
    fn provision_bench_leaves_no_residue() {
        let mut roots = Vec::new();
        for _ in 0..3 {
            let ws = NodeWorkspace::create(&spec()).expect("create");
            roots.push(ws.root().to_path_buf());
            drop(ws);
        }
        for root in roots {
            assert!(
                !root.exists(),
                "each create must be balanced by its Drop — {} must not persist",
                root.display()
            );
        }
    }
}
