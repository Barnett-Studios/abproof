# Changelog

All notable changes to `abproof` are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is
[SemVer](https://semver.org/), read under cargo's 0.x rule where the **minor** is the
breaking position (`0.2` and `0.3` are incompatible ranges).

This file starts at 0.3.0. Earlier releases are recoverable from the git history
(`git log --oneline --grep '^release:'`) and are not back-filled here rather than
reconstructed from memory.

## [0.3.0] — unreleased

### Changed — BREAKING

- **An underpowered battery reports `UNDERPOWERED` and exits `4`, where it previously
  exited `0`/PASS.** The exact two-sided sign-flip test has a hard floor of `2/2ⁿ` on `n`
  discordant pairs, so α = 0.05 is unreachable at `n ≤ 5`. A run under that threshold could
  not have failed its gate at any effect size, and reporting it as a pass says "we looked
  and found no regression" when it means "we could not have found one".

  **Consumers must handle exit `4`.** Anything treating non-zero as "regression" will now
  read an inconclusive run as a failing one. The guard is direction-blind on purpose: a
  battery with no power did not establish the absence of a regression just because its
  point estimate improved.

- `MetricRow.verdict` serialises as `"PASS" | "FAIL" | "UNDERPOWERED" | null` instead of a
  boolean. Consumers deserialising it as `Option<bool>` will fail.
- `WilcoxonResult` gains `min_attainable_p`; `score::gate` gains an `n_nonzero` parameter.
  Both break external callers at compile time.

### Added

- `score::GateOutcome`, `score::EXIT_UNDERPOWERED`, `stats::min_attainable_p`.
- Every result reports its discordant-pair count and power floor, whether or not the guard
  fires — the denominator a reader needs to tell "no regression" from "no power".
- `tests/power-guard/vectors.json`: the family's shared `(n_discordant, alpha) → verdict`
  corpus, byte-identical to the consumer's canonical copy and judged in CI here, so the two
  statistics twins cannot drift silently. Mirrors `tests/id-guard/vectors.json`.

- **A metric with no wired source reports `ABSENT` rather than a fabricated `0.0`.**
  `judge::AbsentJudge` is the shipped default and fails every call, so nothing enters the
  aggregate; `absent_metrics` names each declared-but-unmeasured dimension, derived from the
  rows actually emitted rather than a hand-kept list. A stub returning `0.0` is not merely
  wrong — it is a *number*, and consumers do arithmetic on numbers.

- **The report states its own gate scope.** A `Gate covers:` line names the gated metric and
  an `UNGATED (measured, never gated)` line names every dimension a regression in which does
  *not* fail the run. A PASS on one gated metric previously read as "nothing regressed". The
  two lists are a partition over what the run actually produced.

- **`UNGATED REGRESSION` alarm.** Fires only when an ungated dimension moved the wrong way by
  >= 5%, so it stays an alarm rather than a banner. It is a **materiality** threshold and is
  labelled as one — no ungated dimension has a paired-delta series in the record, so there is
  no significance test to run and none is claimed.

  A **zero baseline** is reported as unbounded (`from zero (unbounded)`) rather than skipped.
  Relative change is undefined at zero, but direction is not. This is load-bearing for cost: a
  free-local-baseline vs paid-treatment run has `baseline_cost_usd = 0` by construction, so the
  regression had nothing to divide by. Both metrics v1 emits as rows are lower-is-worse, where
  a zero baseline can only be an improvement.

### Fixed

- `cost_usd` is named as measured only when the run produced a cost figure — both that a paid
  call ran *and* that it could be priced. A call reporting `cost_usd=unknown` blanks the cost
  fields run-wide, so the previous predicate let one report call cost *measured* on the scope
  line and *unreported* in the footer two lines above.

### Why this needs a release, not just a merge

`dotclaude measure run` invokes abproof as a container and **fails open to its own in-tree
twin** when that is unavailable. A consumer pinned `abproof = "0.2"` keeps resolving 0.2.0
and keeps the fabricated PASS — the same consumption gap that left security fixes published
and unreceived across this family, closed only after ten issues read `closed` while the
vulnerable code was still what ran.
