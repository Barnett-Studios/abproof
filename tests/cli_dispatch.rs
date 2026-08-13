//! Subcommand dispatch and its exit codes (#21).
//!
//! The CLI front door had no test of any kind: every existing integration test drives the
//! library. So the exit-code table in `CONTRACT.md` — the thing a wrapper scripts against —
//! was documentation with nothing holding it to the binary. That is how an unknown
//! subcommand came to exit **0**, the code assigned to a *passing gate*, for as long as the
//! CLI has existed.
//!
//! Every case below asserts the exit code AND which stream carried the text, because the
//! honest limit of the old bug was exactly that: usage went to stdout, so only an
//! exit-code-only caller was deceived. A fix that returns 64 but still prints to stdout
//! would pass a code-only assertion while leaving the diagnostic in the results stream.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_abproof");

fn fixture_corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus-fixture/red-baseline")
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("ABPROOF_CORPUS", fixture_corpus())
        .output()
        .expect("the binary under test must be runnable")
}

fn code(out: &Output) -> i32 {
    out.status
        .code()
        .expect("no case here is killed by a signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn an_unknown_subcommand_is_a_usage_error_on_stderr() {
    for bad in ["bogus", "run-experiment", "measure", "--version"] {
        let out = run(&[bad]);
        assert_eq!(
            code(&out),
            64,
            "`abproof {bad}` must exit 64. Exiting 0 hands an exit-code-only caller the \
             gate's PASS code for a run that never happened"
        );
        assert!(
            stderr(&out).contains(&format!("unknown subcommand '{bad}'")),
            "`abproof {bad}` must name what it did not recognise, got stderr {:?}",
            stderr(&out)
        );
        assert!(
            stdout(&out).is_empty(),
            "a diagnostic belongs on stderr — stdout is where a result goes, got {:?}",
            stdout(&out)
        );
    }
}

#[test]
fn a_bare_invocation_keeps_the_friendly_help_idiom() {
    let out = run(&[]);
    assert_eq!(
        code(&out),
        0,
        "no arguments is help, not a scripted invocation — nobody runs `abproof` bare and \
         reads $?, so this one cannot be mistaken for a verdict"
    );
    assert!(
        stdout(&out).contains("usage: abproof run"),
        "bare invocation prints usage on stdout, got {:?}",
        stdout(&out)
    );
}

#[test]
fn run_without_a_manifest_is_the_same_usage_error() {
    let out = run(&["run"]);
    assert_eq!(code(&out), 64);
    assert!(
        stderr(&out).contains("missing <manifest.yaml>"),
        "got stderr {:?}",
        stderr(&out)
    );
}

#[test]
fn a_projected_run_still_reaches_exit_0() {
    // The control. Without it, a binary that returned 64 for *everything* — including a
    // real projection — would satisfy every assertion above, and the suite would be
    // measuring nothing but its own severity.
    let dir = std::env::temp_dir().join(format!("abproof-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let manifest = dir.join("m.yaml");
    std::fs::write(
        &manifest,
        "name: t\nreps: 1\nbattery: [py-add]\n\
         baseline:\n  loop: execute-node\n  model: local\n  context: none\n\
         treatment:\n  loop: execute-node\n  model: local\n  context: none\n\
         metrics:\n  node_pass_rate: gated\ngate_alpha: 0.10\n",
    )
    .expect("write manifest");

    let out = run(&["run", manifest.to_str().expect("utf-8 path"), "--dry-run"]);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        code(&out),
        0,
        "a dry-run projection is the documented exit-0 path; stderr was {:?}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("dry-run projection:"),
        "the projection itself goes to stdout, got {:?}",
        stdout(&out)
    );
}
