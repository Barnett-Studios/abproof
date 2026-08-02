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

### Why this needs a release, not just a merge

`dotclaude measure run` invokes abproof as a container and **fails open to its own in-tree
twin** when that is unavailable. A consumer pinned `abproof = "0.2"` keeps resolving 0.2.0
and keeps the fabricated PASS — the same consumption gap that left security fixes published
and unreceived across this family, closed only after ten issues read `closed` while the
vulnerable code was still what ran.
