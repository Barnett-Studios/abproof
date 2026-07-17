# abproof

**Offline A/B change-validation for an agentic coding harness — stat-gated, seed-blocked,
reusing the executor as the arm.**

abproof answers one question: *did this change to the harness actually make it better?* It runs
the **same execute-node loop** twice — baseline vs. treatment — over a corpus of RED-test-gated
tasks, with seed-blocked pairing, task-typed scoring, and a statistical gate, and prints a
PASS/FAIL verdict you can trust. It is the "better, proven" oracle the rest of the toolkit is
measured against.

Unlike a prompt-eval framework, abproof A/Bs the **whole assembly running a real loop**, not a
single model call — the executor is the arm.

> Part of the Barnett Studios agentic-harness toolkit → cxpak · commitward · **abproof** · …

## Run

```sh
# project the cost/shape of an experiment (no execution)
abproof run experiment.yaml --dry-run

# execute the A/B (bounded), print the R-table + gate verdict
abproof run experiment.yaml --confirm --max-cost 5.00 --max-calls 200
```

abproof needs two things at run time, resolved by env (or walked up from the CWD in a checkout):

| Env | What | Default |
|---|---|---|
| `ABPROOF_CORPUS` | the RED-baseline corpus dir | walk up for `measurement/corpus/red-baseline` |
| `ABPROOF_EXECUTE_NODE` | the execute-node loop (`execute_node.py`) | walk up for `skills/execute-node/execute_node.py` |
| `ABPROOF_RESULTS` | where `--out`-less results are written | `./measurement/experiments` |

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
green verdict on a broken run would be worse than useless. (It still honours the constitution —
it is offline, never in the live agent loop; "absent abproof" simply means the harness goes
unmeasured.)

See [`CONTRACT.md`](CONTRACT.md) for the full interface.
