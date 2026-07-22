---
title: Implementation plan — measurement statistics correctness
issue: Barnett-Studios/abproof#7
spec: docs/specs/0001-measurement-statistics-correctness.md
adrs: [docs/adrs/0001-paired-test-for-gated-binary-metrics.md, docs/adrs/0002-replication-unit-and-gate-contrast.md]
status: Draft
created: 2026-07-22T07:19:41Z
updated: 2026-07-22T07:19:41Z
---

# Plan 0001 — Measurement statistics correctness

RED-first throughout. Every node's `accept` is a committed failing test that passes only after
the node's edit. The statistical core (N2, N3) is **high-risk — authored by the orchestrator,
not offloaded** (a plausible-but-wrong Wilcoxon is the whole failure mode; ADR 0001 falsifier).

## Node graph

### N1 — scipy-golden oracle harness  ·  `local: false` (authored with the fix)
Add `tests/golden/oracle.py` (committed) + `tests/stats_golden.rs`. The Rust test embeds a
static table of `(deltas, scipy_p)` triples whose values are produced by scipy 1.18.0 and
pinned as literals (CI has no Python). Include: `[0,0,1,-1,2,2,-3]→0.7955`, all-same-sign,
mixed-sign mid-range (`≈0.166` fixture), tiny `n`, `n_nonzero=6` on-disk case, boundaries
`n∈{0,1}`, all-zeros, all-ties.
`accept`: `cargo test --test stats_golden` — **must FAIL on current code** (buggy 0.0769 ≠
0.7955), then pass after N2.

### N2 — corrected Pratt moments  ·  `local: false` (high-risk core)
`stats.rs::approx_wilcoxon_p`: replace signature `(w_plus, n, nonzero_abs)` →
`(w_plus, nonzero_ranks: &[f64])`; compute `μ = Σr/2`, `σ² = Σr²/4`; delete the tie-correction
block and the `n(n+1)/4` / `n(n+1)(2n+1)/24` moments. Update the call site
(`stats.rs:203`) to pass `&nonzero_ranks`. Exact path and `use_exact` guard unchanged.
`accept`: N1 golden goes green; existing `stats.rs` unit tests stay green (the all-ties z-test
and exact-path tests are invariant under the fix — verified by hand).

### N3 — property / fuzz invariants  ·  `local: false` (high-risk core)
`tests/stats_golden.rs`: random deltas (seeded, deterministic) → corrected p never below the
exact-enumeration reference (never anti-conservative); monotonic in |W+−μ|; `p∈[0,1]`;
degenerate inputs return 1.0.
`accept`: `cargo test --test stats_golden` green.

### N4 — aggregate within node (D2)  ·  `local: true`
`run.rs::run_experiment`: after `pairs` is built, group by `node_id` (BTreeMap, battery order),
compute per-node `mean(treatment)−mean(baseline)`, feed that vector to
`wilcoxon_signed_rank` / `cohens_dz` / `bootstrap_median_ci` (replaces the flat per-rep map at
`run.rs:499`).
`accept` (author-committed RED): a scripted 3-node × 4-rep experiment asserts the delta vector
handed to the test has length 3, not 12 (via a test seam exposing the aggregated deltas).

### N5 — align gate contrast (D3)  ·  `local: false` (contract change)
`score.rs::gate`: take the in-run baseline arm rate as `baseline_value` (new param
`baseline_arm: &ArmAggregate` or a plain `f64`), drop the read of `baseline.gated[metric]` for
the contrast; keep `Baseline` loaded + reported as a drift reference. `run.rs` passes
`&baseline_agg`. Update `score.rs` gate tests to the in-run contrast.
`accept` (author-committed RED): holding in-run arms fixed, two different committed
`baseline.json` values yield the **same** `regressed` verdict.

### N6 — docs + CONTRACT (docs-with-code)  ·  `local: true`
CONTRACT.md: redefine `baseline_value` as the in-run baseline arm rate; note the corrected
moments. Flip spec/ADR `status` to Accepted at merge. README gate row if affected.
`accept`: `grep` asserts CONTRACT.md no longer says the gate compares the committed baseline.

### N7 — e2e gate decision  ·  `local: true`
A `run_experiment` over the vendored `tests/corpus-fixture` (or a scripted driver) asserts the
end-to-end `gate_exit` matches the corrected statistics for a known treatment-worse and a
known-noise scenario.
`accept`: `cargo test` green; `cargo clippy --all-targets -- -D warnings` clean; `cargo fmt --check`.

## Sequencing

N1 (RED) → N2 → N3 → N4 → N5 → N6 → N7. N4 is independent of N2/N3 and offloadable; N2/N3/N5
are orchestrator-authored. Commit RED tests before each implementing edit.

## Offload split (to report in the PR)

- Orchestrator (high-risk / contract): N1, N2, N3, N5.
- Local-offloadable: N4, N6, N7.
