//! abproof — offline A/B change-validation harness. `abproof run <manifest.yaml>`
//! projects and (with --confirm) executes a seed-blocked, stat-gated A/B of a
//! harness change over the RED-baseline corpus, reusing the execute-node loop as
//! the arm. Fail-loud on infrastructure faults (unknown cost, aborted run) — a
//! measurement must never present a misleading PASS.

use std::io::Read;
use std::path::PathBuf;

const USAGE: &str = "usage: abproof run <manifest.yaml> \
[--dry-run | --confirm] [--out <path>] [--max-cost <usd>] [--max-calls <n>]\n\
       abproof run-json   (ADR-0052 envelope: request on stdin, response on stdout)";

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // `run-json`: the ADR-0052 response-envelope surface consumed by `dotclaude measure`
    // via ComponentInvoker. Reads a request envelope on stdin, runs (or projects) the
    // experiment, writes a `{schema_version, status, body}` envelope on stdout, exits 0 —
    // the decision (and any abort) is carried in the envelope, never the exit code.
    if args.len() >= 2 && args[1] == "run-json" {
        std::process::exit(run_json_cli());
    }

    // Bare `abproof` or unknown subcommand → usage, exit 0.
    if args.len() < 2 || args[1] != "run" {
        println!("{USAGE}");
        std::process::exit(0);
    }

    let mut dry_run = false;
    let mut confirm = false;
    let mut out_path: Option<PathBuf> = None;
    let mut manifest_path: Option<PathBuf> = None;
    let mut max_cost: Option<f64> = None;
    let mut max_calls: Option<u64> = None;

    let mut i = 2usize;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => dry_run = true,
            "--confirm" => confirm = true,
            "--out" => {
                i += 1;
                match args.get(i) {
                    Some(v) => out_path = Some(PathBuf::from(v)),
                    None => die64("run: --out requires an argument"),
                }
            }
            "--max-cost" => {
                i += 1;
                match args.get(i).map(|v| v.parse::<f64>()) {
                    Some(Ok(v)) if v > 0.0 => max_cost = Some(v),
                    _ => die64("run: --max-cost must be a positive number"),
                }
            }
            "--max-calls" => {
                i += 1;
                match args.get(i).map(|v| v.parse::<u64>()) {
                    Some(Ok(v)) => max_calls = Some(v),
                    _ => die64("run: --max-calls must be a non-negative integer"),
                }
            }
            arg if arg.starts_with('-') => die64(&format!("run: unknown flag '{arg}'")),
            arg => {
                if manifest_path.is_some() {
                    die64(&format!("run: unexpected argument '{arg}'"));
                }
                manifest_path = Some(PathBuf::from(arg));
            }
        }
        i += 1;
    }

    let manifest_path = match manifest_path {
        Some(p) => p,
        None => die64("run: missing <manifest.yaml>"),
    };

    let manifest = match abproof::experiment::load_manifest(&manifest_path) {
        Ok(m) => m,
        Err(e) => die1(&format!("run: {e}")),
    };
    if let Err(e) = manifest.validate() {
        die1(&format!("run: {e}"));
    }

    if manifest.is_cross_loop() {
        eprintln!("run: cross-loop manifest (local vs claude-cli)");
    } else {
        eprintln!("run: single-backend A/B manifest");
    }

    let corpus_root = abproof::corpus::red_baseline_root();
    let nodes = match abproof::corpus::load_battery(&corpus_root, &manifest.battery) {
        Ok(n) => n,
        Err(e) => die1(&format!("run: {e}")),
    };

    let judged_tasks = if manifest.tracked_metrics().contains(&"judge_quality") {
        nodes.len()
    } else {
        0
    };
    let dry = abproof::run::project(&manifest, judged_tasks, manifest.reps);

    if dry_run {
        print_projection(&dry);
        std::process::exit(0);
    }
    if !confirm {
        print_projection(&dry);
        println!();
        println!("re-run with --confirm to execute");
        std::process::exit(0);
    }

    if let Some(cap) = max_calls {
        if dry.projected_claude_calls > cap {
            die64(&format!(
                "run: projected claude-cli calls ({}) exceed --max-calls ({cap})",
                dry.projected_claude_calls
            ));
        }
    }

    let baseline_path = manifest_path.with_file_name(format!(
        "{}.baseline.json",
        manifest_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("manifest")
    ));
    let baseline = match abproof::score::load_baseline(&baseline_path) {
        Ok(b) => b,
        Err(e) => die1(&format!("run: baseline {}: {e}", baseline_path.display())),
    };

    let driver = abproof::driver::LocalNodeDriver {
        script: execute_node_path(),
        timeout: std::time::Duration::from_secs(300),
    };
    let judge = abproof::judge::StubJudge {
        canned: abproof::judge::JudgeScore {
            per_criterion: Default::default(),
            total: 0,
        },
    };
    let opts = abproof::run::RunOptions { max_cost };
    let record = abproof::run::run_experiment(&manifest, &nodes, &driver, &judge, &baseline, &opts);

    let result_path =
        out_path.unwrap_or_else(|| results_dir().join(format!("{}.result.json", manifest.name)));
    if let Err(e) = abproof::report::write_result_json(&result_path, &record) {
        die1(&format!("run: write result {}: {e}", result_path.display()));
    }

    if record.aborted {
        eprintln!(
            "EXPERIMENT ABORTED: {}",
            record
                .abort_reason
                .as_deref()
                .unwrap_or("local runtime unavailable")
        );
        std::process::exit(3);
    }

    print!("{}", abproof::report::render_r_table(&record));
    std::process::exit(record.gate_exit);
}

/// The `run-json` request: everything abproof needs to project or run an experiment, inlined.
#[derive(serde::Deserialize)]
struct RunRequest {
    #[serde(default)]
    manifest_yaml: String,
    #[serde(default)]
    baseline_json: Option<String>,
    /// Project only (no arms executed) — needs no baseline, driver, or network.
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    max_cost: Option<f64>,
    #[serde(default)]
    max_calls: Option<u64>,
}

/// Read a request envelope from stdin, evaluate, print the response envelope. Returns the
/// process exit code — always 0 on a well-formed request (the decision is in the body); 1 only
/// on an unreadable stdin or a malformed request, so a broken call is never a false clean pass.
fn run_json_cli() -> i32 {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        println!("{}", error_envelope(&format!("failed to read stdin: {e}")));
        return 1;
    }
    match run_json(&input) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(e) => {
            println!("{}", error_envelope(&e));
            1
        }
    }
}

fn run_json(input: &str) -> Result<String, String> {
    let req: RunRequest =
        serde_json::from_str(input).map_err(|e| format!("invalid run request JSON: {e}"))?;

    let manifest: abproof::experiment::Manifest = serde_yaml::from_str(&req.manifest_yaml)
        .map_err(|e| format!("manifest parse error: {e}"))?;
    manifest
        .validate()
        .map_err(|e| format!("manifest invalid: {e}"))?;

    let corpus_root = abproof::corpus::red_baseline_root();
    let nodes = abproof::corpus::load_battery(&corpus_root, &manifest.battery)
        .map_err(|e| format!("load battery: {e}"))?;

    let judged_tasks = if manifest.tracked_metrics().contains(&"judge_quality") {
        nodes.len()
    } else {
        0
    };
    let dry = abproof::run::project(&manifest, judged_tasks, manifest.reps);

    // Dry-run: project only — needs no baseline, driver, or network.
    if req.dry_run {
        let body = serde_json::to_value(&dry).map_err(|e| format!("encode projection: {e}"))?;
        return Ok(ok_envelope(
            serde_json::json!({ "mode": "dry_run", "projection": body }),
        ));
    }

    if let Some(cap) = req.max_calls {
        if dry.projected_claude_calls > cap {
            return Err(format!(
                "projected claude-cli calls ({}) exceed max_calls ({cap})",
                dry.projected_claude_calls
            ));
        }
    }

    let baseline_json = req
        .baseline_json
        .as_deref()
        .ok_or("baseline_json is required for a confirmed run")?;
    let baseline: abproof::score::Baseline =
        serde_json::from_str(baseline_json).map_err(|e| format!("baseline parse error: {e}"))?;

    let driver = abproof::driver::LocalNodeDriver {
        script: execute_node_path(),
        timeout: std::time::Duration::from_secs(300),
    };
    let judge = abproof::judge::StubJudge {
        canned: abproof::judge::JudgeScore {
            per_criterion: Default::default(),
            total: 0,
        },
    };
    let opts = abproof::run::RunOptions {
        max_cost: req.max_cost,
    };
    let record = abproof::run::run_experiment(&manifest, &nodes, &driver, &judge, &baseline, &opts);

    // An aborted experiment is an infra fault — a measurement that must never be trusted as a
    // PASS. Signal it as `error` so the consumer falls open to its linked path (ADR-0052/0055)
    // rather than emitting an `ok` envelope wrapping an invalid result.
    if record.aborted {
        return Err(format!(
            "experiment aborted: {}",
            record
                .abort_reason
                .as_deref()
                .unwrap_or("local runtime unavailable")
        ));
    }

    let body = serde_json::to_value(&record).map_err(|e| format!("encode result: {e}"))?;
    Ok(ok_envelope(
        serde_json::json!({ "mode": "run", "result": body }),
    ))
}

/// An `ok`-status ADR-0052 envelope wrapping a computed body.
fn ok_envelope(body: serde_json::Value) -> String {
    serde_json::json!({ "schema_version": "1", "status": "ok", "body": body }).to_string()
}

/// An `error`-status ADR-0052 envelope. The consumer treats any non-`ok` status as a miss and
/// falls open to its linked implementation.
fn error_envelope(message: &str) -> String {
    serde_json::json!({ "schema_version": "1", "status": "error", "body": { "message": message } })
        .to_string()
}

fn print_projection(dry: &abproof::run::DryRun) {
    println!("dry-run projection:");
    println!("  loop_runs:              {}", dry.loop_runs);
    println!("  judge_calls:            {}", dry.judge_calls);
    println!("  est_minutes:            {:.1}", dry.est_minutes);
    println!("  projected_claude_calls: {}", dry.projected_claude_calls);
    println!("  baseline:               {}", dry.baseline);
    println!("  treatment:              {}", dry.treatment);
    if dry.projected_claude_calls > 0 {
        println!("  (cost measured live, bounded by --max-cost if set)");
    }
}

/// Locate the execute-node loop: `$ABPROOF_EXECUTE_NODE`, else walk up from CWD
/// for `skills/execute-node/execute_node.py`.
fn execute_node_path() -> PathBuf {
    if let Ok(p) = std::env::var("ABPROOF_EXECUTE_NODE") {
        return PathBuf::from(p);
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
    PathBuf::from("skills/execute-node/execute_node.py")
}

/// Default results directory: `$ABPROOF_RESULTS`, else `./measurement/experiments`.
fn results_dir() -> PathBuf {
    std::env::var("ABPROOF_RESULTS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("measurement/experiments"))
}

fn die64(msg: &str) -> ! {
    eprintln!("{msg}");
    eprintln!("{USAGE}");
    std::process::exit(64);
}

fn die1(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod run_json_tests {
    use super::*;

    fn body_of(out: &str) -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_str(out).expect("envelope is JSON");
        assert_eq!(v["schema_version"], "1");
        v.clone()
    }

    const MANIFEST: &str = r#"
name: t
reps: 1
battery: [py-add]
baseline:
  loop: execute-node
  model: local
  context: none
treatment:
  loop: execute-node
  model: local
  context: none
metrics:
  node_pass_rate: gated
gate_alpha: 0.10
"#;

    #[test]
    fn invalid_request_json_is_a_hard_error() {
        assert!(run_json("not json").is_err());
    }

    #[test]
    fn manifest_parse_error_is_reported() {
        let req = serde_json::json!({ "manifest_yaml": "::: not a manifest :::" }).to_string();
        assert!(run_json(&req).is_err());
    }

    #[test]
    fn dry_run_emits_an_ok_projection_envelope() {
        // Point the corpus resolver at the vendored fixture (py-add battery lives there).
        std::env::set_var(
            "ABPROOF_CORPUS",
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/corpus-fixture/red-baseline"),
        );
        let req = serde_json::json!({ "manifest_yaml": MANIFEST, "dry_run": true }).to_string();
        let out = run_json(&req).expect("dry-run projects");
        std::env::remove_var("ABPROOF_CORPUS");

        let v = body_of(&out);
        assert_eq!(v["status"], "ok");
        assert_eq!(v["body"]["mode"], "dry_run");
        // The projection carries the loop-run count — proof the manifest+battery resolved.
        assert!(
            v["body"]["projection"]["loop_runs"].is_number(),
            "projection body must carry loop_runs: {v}"
        );
    }
}
