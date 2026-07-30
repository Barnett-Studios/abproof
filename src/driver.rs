//! Experiment driver — orchestrates baseline/candidate runs against the corpus.

use crate::corpus::NodeJson;
use crate::experiment::ArmConfig;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// ADR-0056 rule 1's length bound, mirroring `ID_MAX_LEN` in the producer twins.
const ID_MAX_LEN: usize = 100;

/// Measurement sampling temperature. The harness pins this (> 0) for local runs
/// so the per-rep `LLM_SEED` actually drives the draw — at temp 0 the seed is a
/// no-op and every rep collapses to the identical greedy output (degenerate
/// multi-rep statistics). A manifest `arm.env` may override it (e.g. back to "0").
const MEASURE_SAMPLING_TEMPERATURE: &str = "0.7";

#[derive(Debug, Clone, PartialEq)]
pub enum RunStatus {
    Success,
    Failure,
    Skipped,
    LocalUnavailable,
    /// A per-node artifact with no gradable completion: a
    /// generation/wall-clock timeout, a transport-malformed response, or a
    /// healthy-then-conn-refused rung. Soft-excludes the pair; never scored
    /// as a capability miss. Subsumes the former `Timeout` variant — a
    /// executor-side SIGKILL is exactly this per-node non-verdict.
    Inconclusive,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub status: RunStatus,
    pub accept_passed: bool,
    pub edited_files: Vec<String>,
    pub stdout_tail: String,
    pub duration_ms: u128,
    /// `None` when the backend reported `cost_usd=unknown`.
    pub cost_usd: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Number of `claude -p` invocations; 0 for local/mock runs.
    pub claude_calls: u64,
    /// Sum of num_turns across all claude calls; 0 for local/mock runs.
    pub num_turns: u64,
    /// True when this run's effective sampling temperature was > 0, i.e. the pinned
    /// `LLM_SEED` actually drove the draw (seed honoured). False for greedy (temp 0)
    /// runs, where the seed is a no-op, and for mock runs.
    pub seeds_honoured: bool,
}

/// Values parsed from a `USAGE ...` line emitted by execute_node.py.
/// Returns a zeroed default when the line is absent or malformed (non-fatal).
#[derive(Debug, Default, Clone)]
pub struct ParsedUsage {
    pub cost_usd: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub claude_calls: u64,
    pub num_turns: u64,
}

/// Parse a single `USAGE key=val ...` line into a [`ParsedUsage`].
///
/// Returns a zeroed default when `line` does not start with `"USAGE "` or
/// any field is malformed — never panics, never returns an error.
pub fn parse_usage_line(line: &str) -> ParsedUsage {
    let mut out = ParsedUsage::default();
    let body = match line.strip_prefix("USAGE ") {
        Some(b) => b,
        None => return out,
    };
    for token in body.split_whitespace() {
        if let Some((key, val)) = token.split_once('=') {
            match key {
                "cost_usd" => {
                    out.cost_usd = if val == "unknown" {
                        None
                    } else {
                        val.parse().ok()
                    };
                }
                "input_tokens" => out.input_tokens = val.parse().unwrap_or(0),
                "output_tokens" => out.output_tokens = val.parse().unwrap_or(0),
                "calls" => out.claude_calls = val.parse().unwrap_or(0),
                "num_turns" => out.num_turns = val.parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    out
}

/// Parse the last non-empty stdout line from `execute_node.py` into a [`RunStatus`].
///
/// Recognises the literal terminal markers `SUCCESS` / `LOCAL_UNAVAILABLE`, and the
/// prefixes `SKIPPED(...)` / `INCONCLUSIVE(...)`. Any other line
/// — including the empty string produced when the executor SIGKILLs the process before
/// it can emit a final line — falls open to `Failure` (never a silent capability pass).
pub fn parse_status_line(last_line: &str) -> RunStatus {
    if last_line == "SUCCESS" {
        RunStatus::Success
    } else if last_line.starts_with("SKIPPED") {
        RunStatus::Skipped
    } else if last_line == "LOCAL_UNAVAILABLE" {
        RunStatus::LocalUnavailable
    } else if last_line.starts_with("INCONCLUSIVE") {
        RunStatus::Inconclusive
    } else {
        RunStatus::Failure
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DriverError {
    #[error("{0}")]
    Spawn(String),
    #[error("{0}")]
    Io(String),
    /// A corpus-authored `node.id` that is not a slug. Refused, never rewritten
    /// — see [`temp_node_path`].
    #[error("{0}")]
    InvalidNode(String),
}

/// Temp path for a node's serialized JSON, with the corpus-authored `node.id`
/// **validated rather than sanitized**.
///
/// `node.id` comes from corpus `meta.yaml` — the same producer whose `meta.files`
/// `worktree.rs` already treats as untrusted. This is the RC-1 trust boundary
/// (ADR-0056 rule 7), and the rule is reject, not rewrite: abproof is a measurement
/// instrument, so an id of `a/b` quietly becoming `a_b` would mean the run, its temp
/// artifact, and every report derived from them describe a node identity that is not
/// in the corpus. A malformed corpus entry is a curation defect to surface, not to
/// paper over.
///
/// Charset matches the producer's `id` rule (ADR-0056 rule 1) so the twins agree on
/// what a legal id is, and the wording matches `dotclaude-measure`'s `temp_node_path`
/// so rule 6's lockstep claim holds in fact.
///
/// This implements the documented pattern in full — first character alphanumeric,
/// `[A-Za-z0-9._-]` after it, at most `ID_MAX_LEN` characters. The consumer twin's code
/// implemented neither clause (it rejected only a leading `.`, so `-rf` passed, and no
/// twin enforced the length), which is the drift filed as dotclaude#47 and fixed there in
/// lockstep with this. Both sides are now judged against the same corpus —
/// `tests/id-guard/vectors.json` here, byte-identical upstream — so the next divergence
/// fails a test instead of waiting for a port to notice it.
///
/// Extraction is part of the fix: the path used to be built inline inside
/// `LocalNodeDriver::run`, where no test could reach it.
fn temp_node_path(node_id: &str, counter: u64) -> Result<PathBuf, DriverError> {
    // `chars().count()`, not `len()`: the Python producer's `len()` counts characters, and
    // a twin pair that disagrees about what "100" measures is drift waiting to happen.
    // Every non-ASCII id is rejected by the charset clause regardless, so this changes no
    // verdict today — it keeps the implementations saying the same thing.
    let legal = node_id.chars().count() <= ID_MAX_LEN
        && node_id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && node_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !legal {
        return Err(DriverError::InvalidNode(format!(
            "corpus node.id {node_id:?} is not a slug ([A-Za-z0-9][A-Za-z0-9._-]*, at most \
             {ID_MAX_LEN} characters) — refused rather than rewritten, which would run the \
             node under an identity that is not in the corpus"
        )));
    }
    // Counter first, so the constant `abproof-node-{counter}-` prefix owns the whole
    // first path component. Defence in depth behind the check above, not a substitute
    // for it: with the id first (as it was), a separator in `node_id` split the prefix
    // and the leading component stopped being constant.
    Ok(std::env::temp_dir().join(format!("abproof-node-{counter}-{node_id}.json")))
}

pub trait SessionDriver {
    fn run(&self, arm: &ArmConfig, node: &NodeJson, seed: u64) -> Result<RunOutput, DriverError>;
}

pub struct StubDriver {
    pub scripted: HashMap<String, RunStatus>,
}

impl SessionDriver for StubDriver {
    fn run(&self, _arm: &ArmConfig, node: &NodeJson, _seed: u64) -> Result<RunOutput, DriverError> {
        let status = self
            .scripted
            .get(&node.id)
            .cloned()
            .unwrap_or(RunStatus::Failure);
        let stdout_tail = match &status {
            RunStatus::Success => "SUCCESS",
            RunStatus::Failure => "FAILURE",
            RunStatus::Skipped => "SKIPPED",
            RunStatus::LocalUnavailable => "LOCAL_UNAVAILABLE",
            RunStatus::Inconclusive => "INCONCLUSIVE(stub)",
        }
        .to_string();
        let accept_passed = status == RunStatus::Success;
        Ok(RunOutput {
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

pub struct LocalNodeDriver {
    pub script: PathBuf,
    pub timeout: Duration,
}

/// Returns `true` when `tool` is found as an executable file in any directory
/// listed in the `PATH` environment variable.
///
/// Pure filesystem probe — no subprocess spawned, no shell injection surface.
/// On platforms without execute-bit metadata (non-Unix) we fall back to a plain
/// `is_file()` check; the `#[cfg(unix)]` branch also verifies the execute bit.
fn tool_on_path(tool: &str) -> bool {
    let path_val = match std::env::var_os("PATH") {
        Some(v) => v,
        None => return false,
    };
    std::env::split_paths(&path_val).any(|dir| {
        let candidate = dir.join(tool);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            candidate
                .metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            candidate.is_file()
        }
    })
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn get_edited_files(root: &std::path::Path, status: &RunStatus) -> Vec<String> {
    let args: &[&str] = if *status == RunStatus::Success {
        &["diff", "--name-only", "HEAD~1", "HEAD"]
    } else {
        &["diff", "--name-only", "HEAD"]
    };
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the child process environment for `LocalNodeDriver::run`:
///
/// 1. Apply `crate::env_filter::filter_child_env` on the parent env
///    (keeps PATH/HOME/TMPDIR/AWS_*/ANTHROPIC_*, drops GH_TOKEN/OPENAI_*/…).
/// 2. Forward harness passthrough vars from parent if present
///    (EXECUTE_NODE_MOCK / LLM_PORT / LLM_TIMEOUT / LLM_MAX_ATTEMPTS).
/// 3. Always pin LLM_SEED to `seed.to_string()`, and LLM_TEMPERATURE to
///    [`MEASURE_SAMPLING_TEMPERATURE`] so the pinned seed actually drives the draw.
/// 4. Overlay all of `arm.env` — wins over every prior step so the arm is authoritative.
///
/// Pure: no I/O, no global state — fully unit-testable in isolation.
pub fn child_env(
    arm: &ArmConfig,
    seed: u64,
    parent: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    // Step 1: allowlist filter — drops GH_TOKEN, OPENAI_*, etc.
    let mut env = crate::env_filter::filter_child_env(parent);

    // Step 2: forward harness passthrough vars from parent if present; these
    // configure the local runtime and test harness without being credentials.
    for key in &[
        "EXECUTE_NODE_MOCK",
        "LLM_PORT",
        "LLM_TIMEOUT",
        "LLM_MAX_ATTEMPTS",
    ] {
        if let Some(v) = parent.get(*key) {
            env.insert((*key).to_string(), v.clone());
        }
    }

    // Step 3: always pin LLM_SEED (overrides parent forwarding, yielding to arm.env).
    env.insert("LLM_SEED".to_string(), seed.to_string());

    // Step 3a: always pin LLM_TEMPERATURE > 0 so the pinned LLM_SEED actually
    // drives the draw (at temp 0 the local sampler ignores the seed entirely).
    env.insert(
        "LLM_TEMPERATURE".to_string(),
        MEASURE_SAMPLING_TEMPERATURE.to_string(),
    );

    // Step 3b: forward LLM_BACKEND and LLM_BACKEND_MODEL from arm.backend.
    // arm.env overlay (step 4) can still override these.
    let backend_str = match arm.backend {
        crate::experiment::Backend::ClaudeCli => "claude-cli",
        crate::experiment::Backend::Local => "local",
    };
    env.insert("LLM_BACKEND".to_string(), backend_str.to_string());
    if arm.backend == crate::experiment::Backend::ClaudeCli && !arm.model.is_empty() {
        env.insert("LLM_BACKEND_MODEL".to_string(), arm.model.clone());
    }

    // Step 4: arm.env overlay — authoritative; wins over every prior step.
    for (k, v) in &arm.env {
        env.insert(k.clone(), v.clone());
    }

    env
}

/// Whether `arm`'s effective sampling temperature is > 0, i.e. whether the
/// pinned `LLM_SEED` actually drives the draw for this arm. Mirrors
/// `child_env`'s precedence: an `arm.env["LLM_TEMPERATURE"]` override wins
/// over the pinned [`MEASURE_SAMPLING_TEMPERATURE`] default. A malformed
/// override falls open to `false` (never assume the seed was honoured).
fn seeds_honoured_for(arm: &ArmConfig) -> bool {
    arm.env
        .get("LLM_TEMPERATURE")
        .map(String::as_str)
        .unwrap_or(MEASURE_SAMPLING_TEMPERATURE)
        .parse::<f64>()
        .map(|t| t > 0.0)
        .unwrap_or(false)
}

impl SessionDriver for LocalNodeDriver {
    fn run(&self, arm: &ArmConfig, node: &NodeJson, seed: u64) -> Result<RunOutput, DriverError> {
        // Toolchain gate — checked BEFORE any I/O so tests with a nonexistent
        // script path can verify the short-circuit without spawning python.
        if let Some(missing) = node.requires.iter().find(|t| !tool_on_path(t)) {
            return Ok(RunOutput {
                status: RunStatus::Skipped,
                accept_passed: false,
                edited_files: vec![],
                stdout_tail: format!("SKIPPED(missing_tool:{missing})"),
                duration_ms: 0,
                cost_usd: Some(0.0),
                input_tokens: 0,
                output_tokens: 0,
                claude_calls: 0,
                num_turns: 0,
                seeds_honoured: seeds_honoured_for(arm),
            });
        }

        let t0 = std::time::Instant::now();

        // Serialize node to a temp file; the guard removes it on every path.
        let node_json = serde_json::to_string(node)
            .map_err(|e| DriverError::Io(format!("serialize node: {e}")))?;
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = temp_node_path(&node.id, id)?;
        std::fs::write(&temp_path, &node_json)
            .map_err(|e| DriverError::Io(format!("write node temp: {e}")))?;
        let _temp_guard = TempFile(temp_path.clone());

        let mut cmd = std::process::Command::new("python3");
        cmd.arg(&self.script)
            .arg(&temp_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Build a clean child env: clear the parent env entirely, apply the core
        // allowlist (keeps PATH/HOME/TMPDIR/AWS_*/ANTHROPIC_*, drops GH_TOKEN/OPENAI_*/…),
        // forward harness passthrough vars, pin LLM_SEED, then overlay arm.env.
        // This prevents credentials from the parent process leaking into python3 / the
        // local model / the accept shell command run against model-edited files.
        let parent_env: BTreeMap<String, String> = std::env::vars().collect();
        let mut env_map = child_env(arm, seed, &parent_env);

        // Materialize a clean RED work tree for real corpus nodes and point
        // EXECUTE_NODE_ROOT at it. execute_node.py aborts FAILURE(dirty_tree) without
        // a clean git tree holding the stub + acceptance test. Materialization failure
        // is infra (git broken) — it surfaces as DriverError, never a measured Failure.
        // The guard tears the tree down on every return path below. When `materialize`
        // is None the caller supplies EXECUTE_NODE_ROOT via arm.env (prebuilt-tree tests).
        let workspace = match &node.materialize {
            Some(spec) => {
                let ws = crate::worktree::NodeWorkspace::create(spec)
                    .map_err(|e| DriverError::Io(format!("materialize work tree: {e}")))?;
                env_map.insert(
                    "EXECUTE_NODE_ROOT".to_string(),
                    ws.root().display().to_string(),
                );
                Some(ws)
            }
            None => None,
        };

        cmd.env_clear();
        for (k, v) in &env_map {
            cmd.env(k, v);
        }

        // Own process group: kill(-pgid, sig) reaps python3 and all grandchildren.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        let child = cmd
            .spawn()
            .map_err(|e| DriverError::Spawn(format!("spawn execute_node.py: {e}")))?;

        // Capture pid (u32) before moving child into the wait thread.
        let pid: u32 = child.id();
        let timeout = self.timeout;

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });

        match rx.recv_timeout(timeout) {
            Err(_elapsed) => {
                let duration_ms = t0.elapsed().as_millis();
                // Escalating kill on the whole process group (negative pid = pgid).
                // SAFETY: pid is the pid of a child we just spawned; negating it targets
                // the process group we set via process_group(0). ESRCH (already exited)
                // and EPERM (pid recycled) are accepted as no-ops.
                #[cfg(unix)]
                {
                    unsafe {
                        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
                    }
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    unsafe {
                        let _ = libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
                Ok(RunOutput {
                    status: RunStatus::Inconclusive,
                    accept_passed: false,
                    edited_files: vec![],
                    stdout_tail: String::new(),
                    duration_ms,
                    cost_usd: None,
                    input_tokens: 0,
                    output_tokens: 0,
                    claude_calls: 0,
                    num_turns: 0,
                    seeds_honoured: seeds_honoured_for(arm),
                })
            }
            Ok(Err(e)) => Err(DriverError::Io(format!("wait for execute_node.py: {e}"))),
            Ok(Ok(output)) => {
                let duration_ms = t0.elapsed().as_millis();
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

                let non_empty: Vec<&str> =
                    stdout.lines().filter(|l| !l.trim().is_empty()).collect();
                let last_line = non_empty.last().copied().unwrap_or("").to_string();

                // Scan from the end (skipping the status line) for a USAGE line.
                let usage_str = non_empty
                    .iter()
                    .rev()
                    .skip(1)
                    .find(|l| l.starts_with("USAGE "))
                    .copied()
                    .unwrap_or("");
                let usage = parse_usage_line(usage_str);

                let status = parse_status_line(&last_line);

                let accept_passed = status == RunStatus::Success;
                // Prefer the materialized work tree; fall back to an out-of-band root.
                let root = workspace
                    .as_ref()
                    .map(|ws| ws.root().to_path_buf())
                    .or_else(|| {
                        arm.env
                            .get("EXECUTE_NODE_ROOT")
                            .map(std::path::PathBuf::from)
                    });
                let edited_files = root
                    .map(|r| get_edited_files(&r, &status))
                    .unwrap_or_default();

                Ok(RunOutput {
                    status,
                    accept_passed,
                    edited_files,
                    stdout_tail: last_line,
                    duration_ms,
                    cost_usd: usage.cost_usd,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    claude_calls: usage.claude_calls,
                    num_turns: usage.num_turns,
                    seeds_honoured: seeds_honoured_for(arm),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::NodeJson;
    use crate::experiment::{ArmConfig, Backend, ContextStrategy};
    use std::collections::BTreeMap;

    fn any_arm() -> ArmConfig {
        ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::None,
            env: BTreeMap::default(),
            backend: Backend::Local,
        }
    }

    #[test]
    fn child_env_drops_secrets_keeps_path_and_overlays_arm() {
        // Parent has credentials that must be dropped plus allowlisted vars.
        let mut parent = BTreeMap::new();
        parent.insert("GH_TOKEN".to_string(), "secret-token".to_string());
        parent.insert("OPENAI_API_KEY".to_string(), "sk-12345".to_string());
        parent.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        parent.insert("HOME".to_string(), "/home/user".to_string());
        // parent has EXECUTE_NODE_MOCK; arm.env wins over parent passthrough.
        parent.insert(
            "EXECUTE_NODE_MOCK".to_string(),
            "/tmp/parent-mock.txt".to_string(),
        );

        let mut arm_env = BTreeMap::new();
        arm_env.insert("EXECUTE_NODE_ROOT".to_string(), "/tmp/root".to_string());
        arm_env.insert(
            "EXECUTE_NODE_MOCK".to_string(),
            "/tmp/arm-mock.txt".to_string(),
        );
        let arm = ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::None,
            env: arm_env,
            backend: Backend::Local,
        };

        let result = child_env(&arm, 42, &parent);

        // Credentials must be absent.
        assert!(
            !result.contains_key("GH_TOKEN"),
            "GH_TOKEN must be dropped; env={result:?}"
        );
        assert!(
            !result.contains_key("OPENAI_API_KEY"),
            "OPENAI_API_KEY must be dropped; env={result:?}"
        );

        // Allowlisted vars must survive.
        assert!(
            result.contains_key("PATH"),
            "PATH must be kept; env={result:?}"
        );

        // Seed must be pinned.
        assert_eq!(
            result.get("LLM_SEED").map(|s| s.as_str()),
            Some("42"),
            "LLM_SEED must equal the seed argument"
        );

        // arm.env vars must be present.
        assert_eq!(
            result.get("EXECUTE_NODE_ROOT").map(|s| s.as_str()),
            Some("/tmp/root"),
            "EXECUTE_NODE_ROOT from arm.env must be present"
        );
        // arm.env wins over parent passthrough for EXECUTE_NODE_MOCK.
        assert_eq!(
            result.get("EXECUTE_NODE_MOCK").map(|s| s.as_str()),
            Some("/tmp/arm-mock.txt"),
            "arm.env must win over parent passthrough for EXECUTE_NODE_MOCK"
        );

        // LLM_BACKEND must be forwarded from arm.backend.
        assert!(
            result.contains_key("LLM_BACKEND"),
            "LLM_BACKEND must be forwarded from arm.backend; env={result:?}"
        );

        // Temperature must be pinned > 0 by default so LLM_SEED is honoured.
        assert_eq!(
            result.get("LLM_TEMPERATURE").map(|s| s.as_str()),
            Some(MEASURE_SAMPLING_TEMPERATURE),
            "LLM_TEMPERATURE must be pinned to MEASURE_SAMPLING_TEMPERATURE by default"
        );
    }

    #[test]
    fn child_env_arm_overrides_pinned_temperature() {
        let parent: BTreeMap<String, String> = BTreeMap::new();
        let mut arm_env = BTreeMap::new();
        arm_env.insert("LLM_TEMPERATURE".to_string(), "0".to_string());
        let arm = ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::None,
            env: arm_env,
            backend: Backend::Local,
        };

        let result = child_env(&arm, 7, &parent);

        assert_eq!(
            result.get("LLM_TEMPERATURE").map(|s| s.as_str()),
            Some("0"),
            "arm.env must be able to override the pinned LLM_TEMPERATURE back to 0"
        );
    }

    #[test]
    fn seeds_honoured_for_true_by_default() {
        assert!(
            seeds_honoured_for(&any_arm()),
            "default arm (no LLM_TEMPERATURE override) pins temp > 0 — seed must be honoured"
        );
    }

    #[test]
    fn seeds_honoured_for_false_when_arm_pins_greedy() {
        let mut arm = any_arm();
        arm.env
            .insert("LLM_TEMPERATURE".to_string(), "0".to_string());
        assert!(
            !seeds_honoured_for(&arm),
            "arm.env LLM_TEMPERATURE=0 must report seeds NOT honoured"
        );
    }

    #[test]
    fn seeds_honoured_for_false_when_arm_temperature_malformed() {
        let mut arm = any_arm();
        arm.env
            .insert("LLM_TEMPERATURE".to_string(), "not-a-float".to_string());
        assert!(
            !seeds_honoured_for(&arm),
            "malformed LLM_TEMPERATURE must fall open to seeds NOT honoured"
        );
    }

    #[test]
    fn stub_driver_returns_scripted_status() {
        let mut scripted = HashMap::new();
        scripted.insert("py-add".to_string(), RunStatus::Success);
        let d = StubDriver { scripted };
        let node = NodeJson {
            id: "py-add".into(),
            change: "x".into(),
            files: vec!["calc.py".into()],
            accept: "true".into(),
            forbid: vec![],
            requires: vec![],
            materialize: None,
        };
        let out = d.run(&any_arm(), &node, 123).unwrap();
        assert_eq!(out.status, RunStatus::Success);
        assert!(out.accept_passed);
    }

    #[test]
    fn stub_driver_defaults_failure_for_unknown_node() {
        let d = StubDriver {
            scripted: HashMap::new(),
        };
        let node = NodeJson {
            id: "unknown".into(),
            change: "x".into(),
            files: vec![],
            accept: "true".into(),
            forbid: vec![],
            requires: vec![],
            materialize: None,
        };
        let out = d.run(&any_arm(), &node, 0).unwrap();
        assert_eq!(out.status, RunStatus::Failure);
        assert!(!out.accept_passed);
    }

    // ── status-line parsing ─────────────────────────────────────

    #[test]
    fn parse_status_line_inconclusive_with_reason() {
        assert_eq!(
            parse_status_line("INCONCLUSIVE(gen_timeout)"),
            RunStatus::Inconclusive
        );
        assert_eq!(
            parse_status_line("INCONCLUSIVE(malformed)"),
            RunStatus::Inconclusive
        );
    }

    #[test]
    fn parse_status_line_recognises_all_terminal_markers() {
        assert_eq!(parse_status_line("SUCCESS"), RunStatus::Success);
        assert_eq!(
            parse_status_line("SKIPPED(missing_tool:foo)"),
            RunStatus::Skipped
        );
        assert_eq!(
            parse_status_line("LOCAL_UNAVAILABLE"),
            RunStatus::LocalUnavailable
        );
    }

    #[test]
    fn parse_status_line_falls_open_to_failure_on_unrecognised_or_empty() {
        // Empty stdout (the executor-side SIGKILL path constructs Inconclusive
        // directly rather than through this parser — see the recv_timeout
        // branch) and any unrecognised line both fall open to Failure here.
        assert_eq!(parse_status_line(""), RunStatus::Failure);
        assert_eq!(
            parse_status_line("FAILURE(verify_error:bad_output)"),
            RunStatus::Failure
        );
    }

    // ── toolchain gate ────────────────────────────────────────────────────────

    #[test]
    fn tool_on_path_finds_known_and_rejects_absent() {
        // cargo is always on PATH inside a Rust build/test environment.
        assert!(
            tool_on_path("cargo"),
            "cargo must be found on PATH in a Rust test environment"
        );
        // A deliberately nonsensical name must not be found.
        assert!(
            !tool_on_path("definitely-not-a-real-tool-xyz"),
            "absent tool must not be found on PATH"
        );
    }

    #[test]
    fn missing_tool_in_requires_returns_skipped_without_spawning_python() {
        // Script path is deliberately non-existent — if the gate short-circuits
        // correctly, python3 is never spawned and there is no I/O error.
        let driver = LocalNodeDriver {
            script: PathBuf::from("/nonexistent/execute_node.py"),
            timeout: Duration::from_secs(30),
        };
        let node = NodeJson {
            id: "test-node".into(),
            change: "x".into(),
            files: vec!["stub.py".into()],
            accept: "true".into(),
            forbid: vec![],
            requires: vec!["definitely-not-a-real-tool-xyz".to_string()],
            materialize: None,
        };
        let out = driver.run(&any_arm(), &node, 42).unwrap();
        assert_eq!(
            out.status,
            RunStatus::Skipped,
            "missing required tool must produce Skipped, not Failure"
        );
        assert!(!out.accept_passed, "Skipped must not set accept_passed");
        assert!(
            out.stdout_tail.contains("missing_tool"),
            "stdout_tail must identify the reason; got: {:?}",
            out.stdout_tail
        );
        assert!(
            out.stdout_tail.contains("definitely-not-a-real-tool-xyz"),
            "stdout_tail must name the missing tool; got: {:?}",
            out.stdout_tail
        );
    }

    #[test]
    fn present_tool_in_requires_does_not_short_circuit() {
        // `cargo` is present on PATH → the gate passes and the driver proceeds
        // to spawn the (nonexistent) script, producing a DriverError::Spawn —
        // NOT a Skipped outcome.
        let driver = LocalNodeDriver {
            script: PathBuf::from("/nonexistent/execute_node.py"),
            timeout: Duration::from_secs(30),
        };
        let node = NodeJson {
            id: "test-node".into(),
            change: "x".into(),
            files: vec![],
            accept: "true".into(),
            forbid: vec![],
            requires: vec!["cargo".to_string()],
            materialize: None,
        };
        let result = driver.run(&any_arm(), &node, 42);
        let is_not_skipped = match result {
            Err(_) => true,
            Ok(ref out) => out.status != RunStatus::Skipped,
        };
        assert!(
            is_not_skipped,
            "present required tool must not produce Skipped; result={result:?}"
        );
    }

    // ── RC-1 / ADR-0056 rule 7: corpus-authored node.id must not steer the temp path ──
    //
    // `node.id` reaches a filesystem sink straight from corpus `meta.yaml`. The twin in
    // `dotclaude-measure` carries the identical guard; these assertions mirror its
    // coverage and extend it, because the two crates must refuse the same inputs
    // (rule 6) — one refusing and the other rewriting is exactly the drift the lockstep
    // rule exists to prevent.

    #[test]
    fn rc1_temp_node_path_rejects_hostile_ids() {
        for hostile in [
            "../../evil",
            "/etc/passwd",
            "a/b",
            "..",
            "evil\n",
            "a\0b",
            // Beyond the twin's list — same class, different disguise.
            "",        // empty: joins to a bare "abproof-node-0-.json", identity-less
            ".",       // current dir
            ".hidden", // leading dot: a dotfile is not a corpus node id
            "a\\b",    // Windows separator
            "~",       // shell-expanded elsewhere in the pipeline
            "$HOME",
            "a b",      // whitespace: survives fs, breaks every shell that sees it
            "-rf",      // leads with a dash: reads as a flag to anything downstream
            "\u{5c0f}", // non-ASCII, even though it is filesystem-legal
        ] {
            let r = temp_node_path(hostile, 0);
            assert!(
                r.is_err(),
                "node.id {hostile:?} must be REJECTED, not silently rewritten — a \
                 measurement instrument that renames its inputs reports results under \
                 an identity that never existed (ADR-0056)"
            );
        }
    }

    #[test]
    fn rc1_temp_node_path_rejection_names_the_offending_id() {
        let err = temp_node_path("a/b", 0).expect_err("must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("a/b"),
            "the error must name the rejected id so a bad corpus entry is findable; got {msg:?}"
        );
    }

    #[test]
    fn rc1_temp_node_path_stays_in_temp_dir() {
        // Guard against the rejection tests passing by rejecting *everything*: legal ids
        // must still be accepted, and must land directly in temp_dir().
        let tmp = std::env::temp_dir();
        for legit in [
            "legit-id",
            "N1-surprising-cochange",
            "a",
            "A.B_C-1",
            "0",
            "node.v2",
        ] {
            let p = temp_node_path(legit, 0).expect("a slug id is accepted");
            assert_eq!(
                p.parent(),
                Some(tmp.as_path()),
                "accepted ids must still land in temp_dir(): {p:?}"
            );
        }
    }

    #[test]
    fn rc1_temp_node_path_keeps_ordinary_ids_readable() {
        let p = temp_node_path("N1-surprising-cochange", 7).expect("legitimate slug id");
        assert!(
            p.to_string_lossy()
                .ends_with("abproof-node-7-N1-surprising-cochange.json"),
            "validation must not mangle a legitimate slug id: {p:?}"
        );
    }

    /// One corpus, every guard (dotclaude#48).
    ///
    /// `tests/id-guard/vectors.json` is a **byte-identical** copy of the family's canonical
    /// corpus (`conformance/corpus/id-guard/vectors.json` in the consumer repo, where the
    /// Rust and Python twins run the same file). The consumer's CI diffs this copy against
    /// the canonical one, so weakening a vector here cannot be a quiet local decision — and
    /// a guard that is stricter or laxer than its sibling fails a test rather than a review,
    /// which is how the leading-`-` drift went unnoticed until it was ported.
    ///
    /// Verdicts, not messages: error-text parity is asserted by
    /// `rc1_temp_node_path_rejection_names_the_offending_id` and its twin.
    #[test]
    fn adr0056_every_id_in_the_shared_corpus_gets_the_corpus_verdict() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/id-guard/vectors.json");
        let raw = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("shared id corpus unreadable at {path}: {e}"));
        let doc: serde_json::Value =
            serde_json::from_str(&raw).expect("shared id corpus is not valid JSON");
        let vectors = doc["vectors"]
            .as_array()
            .expect("shared id corpus has no `vectors` array");

        // Vacuity guards. A corpus that shrank to nothing, or to rejects only, would let a
        // guard that refuses every id — including legitimate ones — pass this test.
        assert!(
            vectors.len() >= 20,
            "the shared corpus lost coverage: {} vectors",
            vectors.len()
        );
        let accepts = vectors.iter().filter(|v| v["verdict"] == "accept").count();
        assert!(
            accepts >= 5,
            "the shared corpus has only {accepts} accept vectors — a reject-everything \
             guard would pass vacuously"
        );

        let mut wrong = Vec::new();
        for v in vectors {
            let id = v["id"].as_str().expect("every vector needs a string `id`");
            let want = v["verdict"]
                .as_str()
                .expect("every vector needs a `verdict`");
            let got = if temp_node_path(id, 0).is_ok() {
                "accept"
            } else {
                "reject"
            };
            if got != want {
                let why = v["why"].as_str().unwrap_or("");
                wrong.push(format!(
                    "  id={id:?}: corpus says {want}, this twin says {got} — {why}"
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "this twin disagrees with the shared ADR-0056 corpus on {} id(s) — that is the \
             drift rule 6 exists to prevent:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    #[test]
    fn rc1_hostile_node_id_is_refused_by_the_driver_not_just_the_seam() {
        // The seam is only worth having if `run` actually goes through it. Drives the
        // real `LocalNodeDriver` and asserts it refuses BEFORE the script spawn — the
        // script path is deliberately nonexistent, so a Spawn error would prove the
        // guard was bypassed and the write already happened.
        let driver = LocalNodeDriver {
            script: PathBuf::from("/nonexistent/execute_node.py"),
            timeout: Duration::from_secs(30),
        };
        let node = NodeJson {
            id: "../../evil".into(),
            change: "x".into(),
            files: vec![],
            accept: "true".into(),
            forbid: vec![],
            requires: vec![],
            materialize: None,
        };
        match driver.run(&any_arm(), &node, 42) {
            Err(DriverError::InvalidNode(msg)) => assert!(
                msg.contains("../../evil"),
                "must name the offending id; got {msg:?}"
            ),
            other => panic!(
                "a hostile node.id must be refused by the driver itself, before any \
                 filesystem write or process spawn; got {other:?}"
            ),
        }
    }
}
