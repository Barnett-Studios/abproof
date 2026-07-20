# abproof

[![CI](https://github.com/Barnett-Studios/abproof/actions/workflows/ci.yml/badge.svg)](https://github.com/Barnett-Studios/abproof/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/abproof)](https://crates.io/crates/abproof)
[![Downloads](https://img.shields.io/crates/d/abproof)](https://crates.io/crates/abproof)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

**Offline A/B change-validation for an agentic coding harness — stat-gated, seed-blocked,
reusing the executor as the arm.**

abproof answers one question: *did this change to your agent setup actually make it better?* It runs
the **same executor** twice — baseline vs. treatment — over a corpus of RED-test-gated
tasks, with seed-blocked pairing, task-typed scoring, and a gate that only fails a run when the
regression is both beyond tolerance and statistically significant (paired Wilcoxon, α = 0.05 by
default) — not a bare point estimate. A worse-but-noisy result honestly exits PASS rather than
failing on chance; an underpowered run is a validity problem for the *experiment* (raise `reps`),
not something the gate silently papers over.

Unlike a prompt-eval framework, abproof A/Bs the **whole assembly running a real loop**, not a
single model call — the executor is the arm.

> Part of the Barnett Studios agentic-harness toolkit → cxpak · commitward · **abproof** · …

## Install

```sh
brew tap Barnett-Studios/tap && brew install abproof   # macOS/Linux
cargo install abproof                                   # any platform
docker run --rm -v "$PWD:/repo" ghcr.io/barnett-studios/abproof run experiment.yaml --dry-run
```

## Run

```sh
# project the cost/shape of an experiment (no execution)
abproof run experiment.yaml --dry-run

# execute the A/B (bounded), print the R-table + gate verdict
abproof run experiment.yaml --confirm --max-cost 5.00 --max-calls 200
```

abproof needs two things at run time. A **standalone install sets these via env**; an in-tree/dev
checkout resolves them by walking up from the CWD.

| Env | What | Resolution |
|---|---|---|
| `ABPROOF_CORPUS` | the RED-baseline corpus dir | standalone: point at the [corpus](https://github.com/Barnett-Studios/corpus) repo's `red-baseline/`; dev checkout: walked up as `measurement/corpus/red-baseline` |
| `ABPROOF_EXECUTE_NODE` | the execute-node loop (`execute_node.py`) | standalone: your executor's `execute_node.py`; dev checkout: walked up as `skills/execute-node/execute_node.py` |
| `ABPROOF_RESULTS` | where `--out`-less results are written | `./measurement/experiments` (any writable dir) |

The corpus is a **separate component** (the *Corpus* slot) — abproof ships none. The reference
RED-baseline corpus is Exercism-derived and must be license-scrubbed + attributed before
redistribution (that is the Corpus component's job, not abproof's).

## The manifest

An experiment is a YAML manifest: `{name, battery:[task-ids], reps, seed_base, baseline:{loop,
model, context, backend}, treatment:{…}, metrics:{…}, tolerance:{…}}`. Baseline outcomes live
beside it as `<stem>.baseline.json`. `abproof run --dry-run` prints the projected loop-runs,
judge-calls, minutes, and claude-cli calls before you spend anything.

## Exit codes (fail-loud on measurement integrity)

| Code | Meaning |
|---|---|
| `0` | projection/dry-run printed, or the gate PASSED |
| `1` | setup error (bad manifest, missing baseline, unreadable corpus) |
| `3` | experiment **aborted** — an invalid measurement (local runtime unavailable, cost cap hit mid-battery); never presented as a result |
| gate | on `--confirm`, the process exits with the statistical gate's own code (non-zero = FAIL) |
| `64` | usage error |

abproof is deliberately **fail-loud**, not fail-open: an offline oracle that silently returned a
green verdict on a broken run would be worse than useless. (It still stays out of the way —
offline, never in the live agent loop; "absent abproof" simply means your setup goes
unmeasured.)

See [`CONTRACT.md`](CONTRACT.md) for the full interface.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
Unless you explicitly state otherwise, any contribution you intentionally submit for
inclusion in the work shall be dual-licensed as above, without any additional terms.

---

Built by [Barnett Studios](https://barnett-studios.com/) — part of the agentic-harness
toolkit: [cxpak](https://github.com/Barnett-Studios/cxpak) ·
[commitward](https://github.com/Barnett-Studios/commitward) ·
[cascadr](https://github.com/Barnett-Studios/cascadr) · **abproof** ·
[cordon](https://github.com/Barnett-Studios/cordon) ·
[slicr](https://github.com/Barnett-Studios/slicr).
