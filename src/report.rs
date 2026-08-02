//! Report types and rendering for measurement experiment results.

use std::io;
use std::path::Path;

/// One row in the R-table: one metric's paired-comparison statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricRow {
    pub metric: String,
    /// "gated" | "tracked"
    pub tag: String,
    pub baseline: f64,
    pub treatment: f64,
    pub delta: f64,
    /// Wilcoxon W statistic (min of W+ and W-).
    pub w: Option<f64>,
    pub p_two_sided: Option<f64>,
    pub d_z: Option<f64>,
    pub ci_lower: Option<f64>,
    pub ci_upper: Option<f64>,
    /// Gate outcome for this metric; `None` = tracked (no verdict).
    /// Three-state since #3: `UNDERPOWERED` is not a `PASS`.
    pub verdict: Option<crate::score::GateOutcome>,
    /// Wilcoxon method actually used: "ExactPratt" | "NormalApproxPratt".
    /// `None` for tracked rows (no Wilcoxon computed).
    pub wilcoxon_method: Option<String>,
    /// Number of non-zero paired deltas used in the Wilcoxon test.
    /// `None` for tracked rows.
    pub n_nonzero: Option<usize>,
}

/// Full result of one experiment run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ResultRecord {
    pub name: String,
    /// ISO 8601 UTC timestamp of the run.
    pub ts: String,
    pub reps: u32,
    pub seeds_honoured: bool,
    pub rows: Vec<MetricRow>,
    /// 0 = no regression, 1 = regression, 3 = aborted,
    /// 4 = [`crate::score::EXIT_UNDERPOWERED`] (alpha was unreachable).
    pub gate_exit: i32,
    /// True when the experiment was aborted before producing a valid measurement.
    pub aborted: bool,
    /// Human-readable abort reason; `None` when `aborted == false`.
    pub abort_reason: Option<String>,
    /// Cumulative cost across both arms; `None` if any call did not report cost.
    pub total_cost_usd: Option<f64>,
    /// Baseline-arm cumulative cost; `None` if any call did not report cost.
    pub baseline_cost_usd: Option<f64>,
    /// Treatment-arm cumulative cost; `None` if any call did not report cost.
    pub treatment_cost_usd: Option<f64>,
    /// Total number of claude-cli invocations across all arms and reps.
    pub total_claude_calls: u64,
    /// Non-empty when a validity constraint was violated (e.g. num_turns > 1 on a claude-cli arm).
    pub validity_warnings: Vec<String>,
    /// Number of (node, rep) pairs excluded because either arm was `Inconclusive`
    /// (per-node soft-exclusion).
    pub inconclusive_count: u64,
    /// `inconclusive_count / total_pairs_attempted`; `0.0` when no pairs were
    /// attempted. Compared against `INCONCLUSIVE_MAX_FRACTION` (A6).
    pub inconclusive_fraction: f64,
    /// Discordant (non-zero) paired-delta count on the gated metric — the power
    /// denominator. Reported on every result, not buried in a row: hiding it is how an
    /// underpowered null gets read as evidence of no effect (#3). `None` when the run
    /// aborted before a paired test ran.
    pub n_discordant: Option<usize>,
    /// Smallest two-sided p `n_discordant` could have produced. When this exceeds the
    /// gate's `alpha`, the battery could not have failed its own gate at any effect
    /// size. `None` when the run aborted before a paired test ran.
    pub min_attainable_p: Option<f64>,
    /// Metrics the manifest declared that produced no row because nothing measured them —
    /// an unconfigured judge, an unwired source. Named rather than merely omitted so a
    /// reader can tell "declared and unmeasured" from "never in scope", and reported as
    /// ABSENT rather than as a fabricated `0.0` (#8).
    pub absent_metrics: Vec<String>,
}

/// Relative move, in the worse direction, at which an **ungated** dimension is called out.
///
/// This is a **materiality** threshold, not a significance one, and the distinction is not
/// pedantry: no ungated dimension has a paired-delta series in the record, so there is no
/// test to run and nothing here may be read as "significant". It exists to suppress
/// rounding noise, and it is printed with the alarm so a reader knows what was filtered.
///
/// A dimension whose movement genuinely needs a verdict should be **gated** with a
/// pre-registered tolerance — that is what gating is for. This line is for the ones that
/// are not.
pub const UNGATED_ALARM_REL: f64 = 0.05;

/// Which direction is a regression for a given dimension: `Some(true)` when higher is
/// worse, `Some(false)` when lower is worse, `None` when this report does not know.
///
/// `None` means the dimension is **never alarmed**, so an unlisted metric fails silently in
/// exactly the way #4 is about. `every_tracked_metric_has_a_known_direction` keeps that
/// from happening by accident rather than by anyone remembering this comment.
fn worse_when_higher(metric: &str) -> Option<bool> {
    match metric {
        "cost_usd" | "engine_broken_rate" => Some(true),
        "node_pass_rate" | "judge_quality" | "wellformed_pct" | "pass_at_1" | "pass_at_2" => {
            Some(false)
        }
        _ => None,
    }
}

/// Relative change from `baseline` to `treatment`, in the worse direction, as a positive
/// fraction — or `None` when the move is an improvement or is immaterial.
///
/// A **zero baseline** returns `f64::INFINITY` rather than `None` when the move is the wrong
/// way. There is no relative change to divide by, but the direction is not in doubt, and the
/// materiality filter must not read "undefined" as "immaterial". The caller words the
/// infinity; it never reaches a `%` format.
///
/// Scoped to what v1 actually emits: the live case is **cost**, where a free local baseline
/// against a paid treatment gives `baseline_cost_usd == 0.0` by construction. The two row
/// metrics v1 emits — `node_pass_rate` and `judge_quality` — are both lower-is-worse, so a
/// zero baseline on either can only move the *right* way and stays silent regardless.
/// `engine_broken_rate` is the case this guard is built for and cannot exercise yet: it is
/// unwired in v1 and reports ABSENT, and its healthy baseline is exactly zero, so it becomes
/// the load-bearing one the day v2 wires a source.
fn ungated_regression(metric: &str, baseline: f64, treatment: f64) -> Option<f64> {
    let higher_is_worse = worse_when_higher(metric)?;
    if !baseline.is_finite() || !treatment.is_finite() {
        return None;
    }
    let moved_worse_by = if higher_is_worse {
        treatment - baseline
    } else {
        baseline - treatment
    };
    if baseline == 0.0 {
        return (moved_worse_by > 0.0).then_some(f64::INFINITY);
    }
    let worse_by = moved_worse_by / baseline.abs();
    (worse_by >= UNGATED_ALARM_REL).then_some(worse_by)
}

/// Render a Markdown R-table summarising the experiment result.
///
/// One row per metric. Header notes the Wilcoxon method (Pratt zeros / average-rank ties)
/// and whether seeds were honoured.
pub fn render_r_table(rec: &ResultRecord) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "## Measurement: {} — {} reps, seeds_honoured: {}\n",
        rec.name, rec.reps, rec.seeds_honoured
    ));

    // Render the actual Wilcoxon method and n_nonzero from the first row that
    // carries them, rather than hardcoding a method string.
    let method_line = rec
        .rows
        .iter()
        .find_map(|r| r.wilcoxon_method.as_deref().map(|m| (m, r.n_nonzero)))
        .map(|(m, n)| match n {
            Some(n) => format!("Wilcoxon method: {} (n_nonzero={}, two-sided)\n\n", m, n),
            None => format!("Wilcoxon method: {} (two-sided)\n\n", m),
        })
        .unwrap_or_else(|| "Wilcoxon method: N/A (no gated metric data)\n\n".to_string());
    out.push_str(&method_line);

    // Power line — the discordant count is the denominator every reader needs to tell
    // "no regression" from "no power". Rendered before the table so it cannot be missed.
    if let Some(n) = rec.n_discordant {
        match rec.min_attainable_p {
            Some(floor) => out.push_str(&format!(
                "Discordant (non-zero) pairs: {n} — minimum attainable two-sided p: {floor:.4}\n\n"
            )),
            None => out.push_str(&format!("Discordant (non-zero) pairs: {n}\n\n")),
        }
    }

    // Header
    out.push_str(
        "| metric | tag | baseline | treatment | delta | W | p | d_z | CI 95% | verdict |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|---|---|\n");

    for row in &rec.rows {
        let w_s = fmt_opt(row.w, 3);
        let p_s = fmt_opt(row.p_two_sided, 4);
        let dz_s = fmt_opt(row.d_z, 3);
        let ci_s = match (row.ci_lower, row.ci_upper) {
            (Some(lo), Some(hi)) => format!("[{:.3}, {:.3}]", lo, hi),
            _ => "—".to_string(),
        };
        let verdict_s = match row.verdict {
            Some(crate::score::GateOutcome::Pass) => "PASS".to_string(),
            Some(crate::score::GateOutcome::Fail) => "FAIL".to_string(),
            Some(crate::score::GateOutcome::Underpowered) => "UNDERPOWERED".to_string(),
            None => "—".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {:.3} | {:.3} | {:+.3} | {} | {} | {} | {} | {} |\n",
            row.metric,
            row.tag,
            row.baseline,
            row.treatment,
            row.delta,
            w_s,
            p_s,
            dz_s,
            ci_s,
            verdict_s,
        ));
    }

    // Cost footer — rendered only when a claude-cli arm actually ran.
    // Omitted entirely for local-only experiments to avoid misleading "$0.0000" output.
    if rec.total_claude_calls > 0 {
        out.push('\n');
        match rec.total_cost_usd {
            Some(total) => {
                let base_s = rec
                    .baseline_cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "N/A".to_string());
                let treat_s = rec
                    .treatment_cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "N/A".to_string());
                out.push_str(&format!(
                    "Cost (claude -p own-report — estimate only, not an invoice): \
                     ${total:.4} total, {base_s} baseline, {treat_s} treatment ({} calls)\n",
                    rec.total_claude_calls
                ));
            }
            None => {
                out.push_str(&format!(
                    "Cost: unreported (a claude-cli call did not report cost; {} calls)\n",
                    rec.total_claude_calls
                ));
            }
        }
    }

    // Inconclusive exclusions — rendered only when at least one pair was excluded
    // (per-node soft-exclusion / fail-loud floor).
    if rec.inconclusive_count > 0 {
        out.push('\n');
        out.push_str(&format!(
            "Inconclusive: {} pair(s) excluded ({:.1}% of attempted) — artifacts, not capability misses\n",
            rec.inconclusive_count,
            rec.inconclusive_fraction * 100.0,
        ));
    }

    // Gate scope (#4) — exactly which dimensions the verdict covers, and which it does
    // not. `node_pass_rate` is the sole gated metric, so a treatment that holds solve-rate
    // while doubling cost or halving wellformedness still exits 0. That is a defensible
    // pre-registration choice (one metric, no multiple-comparisons trap) but only when it
    // is stated: otherwise a PASS silently reads as "nothing regressed".
    //
    // The two lists are a PARTITION of the dimensions this run knows about, computed as a
    // set difference over the rows plus cost-when-measured — not a derived list with a hand-written
    // tail appended. The earlier version appended `["cost_usd", "duration"]`
    // unconditionally, so gating either would have produced a report claiming the gate
    // both covered and did not cover it. A scope statement that can contradict itself is
    // the wrong kind of bug in the paragraph whose only job is stating scope.
    //
    // The union with the emitted row metrics is deliberate: a dimension that reaches a row
    // without being declared above is still named. A missed registry entry then costs
    // report ordering, never a silent omission from a list the reader is entitled to read
    // as complete.
    {
        let gated: Vec<&str> = rec
            .rows
            .iter()
            .filter(|r| r.tag == "gated")
            .map(|r| r.metric.as_str())
            .collect();
        if !gated.is_empty() {
            // Cost has no row, so only this list can name it — but only when the report
            // actually produced a cost figure. That takes BOTH conditions: a paid call ran
            // (the footer is omitted entirely on local-only runs, by design, so a reader is
            // not shown a misleading `$0.0000`), and the run could price it (any call
            // reporting `cost_usd=unknown` blanks the cost fields run-wide and degrades the
            // footer to "Cost: unreported" while the call count stays positive — reachable
            // on the third cascade rung, whose openai-compat responses carry no cost field).
            //
            // Keying on the call count alone let the same report call cost *measured* here
            // and *unreported* two lines above. That is the `absent_metrics` invariant —
            // ABSENT must not also be reported as measured — which cost slipped through by
            // having no row to be absent from.
            //
            // `duration` is deliberately absent. The driver times each run, but that never
            // reaches `ResultRecord` or any output; naming it here would point a reader at
            // a number the report does not contain. It belongs on this line the day it is
            // reported, not before.
            let unrowed: &[&str] = if rec.total_claude_calls > 0 && rec.total_cost_usd.is_some() {
                &["cost_usd"]
            } else {
                &[]
            };
            let mut ungated: Vec<&str> = rec
                .rows
                .iter()
                .map(|r| r.metric.as_str())
                .chain(unrowed.iter().copied())
                .filter(|d| !gated.contains(d))
                .collect();
            let mut seen = std::collections::HashSet::new();
            ungated.retain(|d| seen.insert(*d));
            out.push('\n');
            out.push_str(&format!("Gate covers: {}.\n", gated.join(", ")));
            out.push_str(&format!(
                "UNGATED (measured, never gated — a regression in these does NOT fail the run): {}\n",
                ungated.join(", ")
            ));

            // …and, when one of them actually moved the wrong way, say so. The line above
            // states the POLICY and prints identically whether cost doubled or held, so on
            // its own a 2x cost regression produced byte-identical output to a flat run
            // (#4 acceptance criterion 1). This fires only on movement, so it stays an
            // alarm rather than becoming a second banner nobody reads.
            let mut regressions: Vec<(&str, f64)> = ungated
                .iter()
                .filter_map(|m| {
                    let row = rec.rows.iter().find(|r| r.metric == *m)?;
                    ungated_regression(m, row.baseline, row.treatment).map(|by| (*m, by))
                })
                .collect();
            if let (Some(b), Some(t)) = (rec.baseline_cost_usd, rec.treatment_cost_usd) {
                if ungated.contains(&"cost_usd") {
                    if let Some(by) = ungated_regression("cost_usd", b, t) {
                        regressions.push(("cost_usd", by));
                    }
                }
            }
            if !regressions.is_empty() {
                regressions.sort_by(|a, b| b.1.total_cmp(&a.1));
                let named: Vec<String> = regressions
                    .iter()
                    .map(|(m, by)| {
                        // An unbounded move has no percentage to print — say what happened.
                        if by.is_finite() {
                            format!("{m} {:+.1}%", by * 100.0)
                        } else {
                            format!("{m} from zero (unbounded)")
                        }
                    })
                    .collect();
                out.push_str(&format!(
                    "UNGATED REGRESSION — moved the wrong way and did not fail the run \
                     (>= {:.0}% materiality, NOT a significance test): {}\n",
                    UNGATED_ALARM_REL * 100.0,
                    named.join(", ")
                ));
            }
        }
    }

    // Absent metrics — declared, unmeasured, and named as such. A reader who sees no
    // judge_quality row must not be left to guess whether it was out of scope or missing.
    if !rec.absent_metrics.is_empty() {
        out.push('\n');
        out.push_str(&format!(
            "ABSENT (declared but not measured — no value, NOT 0.0): {}\n",
            rec.absent_metrics.join(", ")
        ));
    }

    // Underpowered banner — the whole point of #3 is that this cannot be mistaken for a
    // pass, so it is stated in prose as well as in the verdict column.
    if rec
        .rows
        .iter()
        .any(|r| r.verdict == Some(crate::score::GateOutcome::Underpowered))
    {
        out.push('\n');
        let floor = rec
            .min_attainable_p
            .map(|p| format!("{p:.4}"))
            .unwrap_or_else(|| "N/A".to_string());
        out.push_str(&format!(
            "⚠ UNDERPOWERED — NOT A PASS. With {} discordant pair(s) the smallest two-sided \
             p this battery could produce is {floor}, above the gate's alpha. No arrangement \
             of the observed data could have failed this gate, so the absence of a confirmed \
             regression is not evidence that none exists. Grow the battery (power comes from \
             more DISCORDANT nodes, not more reps on nodes that already agree) before reading \
             this result as \"no regression\".\n",
            rec.n_discordant
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".to_string()),
        ));
    }

    // Validity warnings — rendered only when non-empty.
    if !rec.validity_warnings.is_empty() {
        out.push('\n');
        out.push_str("⚠ VALIDITY WARNINGS:\n");
        for w in &rec.validity_warnings {
            out.push_str(&format!("  - {w}\n"));
        }
    }

    out
}

/// Serialise `rec` as pretty-printed JSON to `path`, creating parent directories as needed.
pub fn write_result_json(path: &Path, rec: &ResultRecord) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(rec)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(path, json)
}

// ── private helpers ──────────────────────────────────────────────────────────

fn fmt_opt(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(x) => format!("{:.prec$}", x, prec = decimals),
        None => "—".to_string(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result_record() -> ResultRecord {
        ResultRecord {
            name: "test-exp".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            reps: 30,
            seeds_honoured: false,
            rows: vec![
                MetricRow {
                    metric: "node_pass_rate".to_string(),
                    tag: "gated".to_string(),
                    baseline: 0.5,
                    treatment: 0.8,
                    delta: 0.3,
                    w: Some(15.0),
                    p_two_sided: Some(0.0625),
                    d_z: Some(0.891),
                    ci_lower: Some(0.0),
                    ci_upper: Some(1.0),
                    verdict: Some(crate::score::GateOutcome::Underpowered),
                    wilcoxon_method: Some("ExactPratt".to_string()),
                    n_nonzero: Some(5),
                },
                MetricRow {
                    metric: "judge_quality".to_string(),
                    tag: "tracked".to_string(),
                    baseline: 3.0,
                    treatment: 3.5,
                    delta: 0.5,
                    w: None,
                    p_two_sided: None,
                    d_z: None,
                    ci_lower: None,
                    ci_upper: None,
                    verdict: None,
                    wilcoxon_method: None,
                    n_nonzero: None,
                },
            ],
            gate_exit: 0,
            aborted: false,
            abort_reason: None,
            total_cost_usd: Some(0.05),
            baseline_cost_usd: Some(0.0),
            treatment_cost_usd: Some(0.05),
            total_claude_calls: 30,
            validity_warnings: vec![],
            inconclusive_count: 0,
            inconclusive_fraction: 0.0,
            n_discordant: Some(5),
            min_attainable_p: Some(0.0625),
            absent_metrics: vec!["engine_broken_rate".to_string()],
        }
    }

    #[test]
    fn r_table_lists_gated_and_tracked() {
        let rec = sample_result_record();
        let md = render_r_table(&rec);
        assert!(
            md.contains("node_pass_rate") && md.contains("gated"),
            "table must contain gated metric row"
        );
        assert!(
            md.contains("| W ") || md.contains("Wilcoxon"),
            "table must reference Wilcoxon W column or method"
        );
        assert!(
            md.contains('p') && md.contains("d_z"),
            "table must include p and d_z columns"
        );
        assert!(
            md.contains("judge_quality") && md.contains("tracked"),
            "tracked row must appear"
        );
        assert!(md.contains("PASS"), "gated PASS verdict must appear");
        // M3: actual method must appear, not the old hardcoded string.
        assert!(
            md.contains("ExactPratt"),
            "render must show the actual Wilcoxon method from the row"
        );
        assert!(
            md.contains("n_nonzero=5"),
            "render must show n_nonzero from the row"
        );
    }

    #[test]
    fn write_result_json_roundtrip() {
        let rec = sample_result_record();
        let path = std::env::temp_dir().join("abproof-report-test.json");
        write_result_json(&path, &rec).expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(v["name"].as_str(), Some("test-exp"));
        assert_eq!(v["gate_exit"].as_i64(), Some(0));
        assert!(v["rows"].is_array());
        // New cost fields must be present in JSON.
        assert!(
            v["total_cost_usd"].is_number(),
            "total_cost_usd must serialize"
        );
        assert!(
            v["total_claude_calls"].is_number(),
            "total_claude_calls must serialize"
        );
        assert!(
            v["validity_warnings"].is_array(),
            "validity_warnings must serialize"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_result_json_includes_abort_fields() {
        let rec = ResultRecord {
            name: "aborted-exp".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            reps: 5,
            seeds_honoured: false,
            rows: vec![],
            gate_exit: 3,
            aborted: true,
            abort_reason: Some("local runtime unavailable (2 reps)".to_string()),
            total_cost_usd: Some(0.0),
            baseline_cost_usd: Some(0.0),
            treatment_cost_usd: Some(0.0),
            total_claude_calls: 0,
            validity_warnings: vec![],
            inconclusive_count: 0,
            inconclusive_fraction: 0.0,
            n_discordant: Some(5),
            min_attainable_p: Some(0.0625),
            absent_metrics: vec!["engine_broken_rate".to_string()],
        };
        let path = std::env::temp_dir().join("abproof-abort-test.json");
        write_result_json(&path, &rec).expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(&content).expect("parse json");
        assert_eq!(v["aborted"].as_bool(), Some(true));
        assert_eq!(v["gate_exit"].as_i64(), Some(3));
        assert!(v["abort_reason"].as_str().is_some());
        let _ = std::fs::remove_file(&path);
    }

    // ── Cost footer helpers ───────────────────────────────────────────────────

    fn record_with_cost(
        total: Option<f64>,
        base: Option<f64>,
        treat: Option<f64>,
        calls: u64,
    ) -> ResultRecord {
        let mut r = sample_result_record();
        r.total_cost_usd = total;
        r.baseline_cost_usd = base;
        r.treatment_cost_usd = treat;
        r.total_claude_calls = calls;
        r
    }

    // ── Cost footer — three I2 cases ─────────────────────────────────────────

    #[test]
    fn r_table_footer_known_cost() {
        // claude-cli arm ran and reported cost → full footer with disclaimer.
        let md = render_r_table(&record_with_cost(Some(0.05), Some(0.02), Some(0.03), 5));
        assert!(
            md.contains("not an invoice"),
            "disclaimer must appear in footer; got: {md}"
        );
        assert!(
            md.contains("0.0500") || md.contains("$0.05"),
            "total cost must appear numerically; got: {md}"
        );
        assert!(md.contains("5 calls"), "call count must appear; got: {md}");
    }

    #[test]
    fn r_table_footer_local_only_no_footer() {
        // total_claude_calls == 0 → local-only run; cost footer must be entirely absent.
        let md = render_r_table(&record_with_cost(Some(0.0), Some(0.0), Some(0.0), 0));
        assert!(
            !md.contains("not an invoice"),
            "local-only run must not show cost footer; got: {md}"
        );
        assert!(
            !md.contains("unreported"),
            "local-only run must not show 'unreported'; got: {md}"
        );
    }

    #[test]
    fn r_table_footer_unknown_cost() {
        // total_claude_calls > 0 but total_cost_usd == None → 'unreported' line.
        let md = render_r_table(&record_with_cost(None, None, None, 3));
        assert!(
            md.contains("unreported"),
            "unknown cost must state 'unreported'; got: {md}"
        );
        assert!(
            md.contains("3 calls"),
            "call count must appear even when cost unknown; got: {md}"
        );
    }

    // ── Inconclusive exclusions (A6) — two cases ─────────────────────────────

    #[test]
    fn r_table_renders_inconclusive_line_when_present() {
        let mut rec = sample_result_record();
        rec.inconclusive_count = 3;
        rec.inconclusive_fraction = 0.1;
        let md = render_r_table(&rec);
        assert!(
            md.contains("Inconclusive"),
            "non-zero inconclusive_count must render a line; got: {md}"
        );
        assert!(
            md.contains('3'),
            "inconclusive count must appear numerically; got: {md}"
        );
        assert!(
            md.contains("10.0%"),
            "inconclusive fraction must appear as a percentage; got: {md}"
        );
    }

    #[test]
    fn r_table_omits_inconclusive_line_when_zero() {
        let rec = sample_result_record();
        assert_eq!(rec.inconclusive_count, 0);
        let md = render_r_table(&rec);
        assert!(
            !md.contains("Inconclusive"),
            "zero inconclusive_count must not render the line; got: {md}"
        );
    }

    // ── Validity warnings — two cases ────────────────────────────────────────

    #[test]
    fn render_validity_warnings_present() {
        let mut rec = sample_result_record();
        rec.validity_warnings = vec![
            "warn1: tools-off leaked".to_string(),
            "warn2: multi-turn".to_string(),
        ];
        let md = render_r_table(&rec);
        assert!(
            md.contains("VALIDITY WARNINGS"),
            "non-empty warnings must render warning block; got: {md}"
        );
        assert!(md.contains("warn1"), "first warning must appear; got: {md}");
        assert!(
            md.contains("warn2"),
            "second warning must appear; got: {md}"
        );
    }

    #[test]
    fn render_no_validity_warning_block_when_empty() {
        let rec = sample_result_record();
        assert!(
            rec.validity_warnings.is_empty(),
            "sample_result_record must have empty validity_warnings"
        );
        let md = render_r_table(&rec);
        assert!(
            !md.contains("VALIDITY WARNINGS"),
            "empty validity_warnings must not render warning block; got: {md}"
        );
    }

    // ── #4 follow-up: the scope statement must not be able to go stale ───────

    /// A metric that is gated must never also be listed as ungated.
    ///
    /// The first cut of the gate-scope footer derived the ungated list from the tracked
    /// rows and then appended `["cost_usd", "duration"]` unconditionally. If either is ever
    /// gated, the report states that the gate both covers and does not cover it — in the
    /// one paragraph whose entire job is stating scope accurately.
    #[test]
    fn a_gated_dimension_is_never_also_listed_as_ungated() {
        let mut rec = sample_result_record();
        // Promote cost_usd to the gated dimension — exactly the change the hardcoded
        // list cannot see.
        for r in rec.rows.iter_mut() {
            r.tag = "tracked".to_string();
        }
        rec.rows.push(MetricRow {
            metric: "cost_usd".to_string(),
            tag: "gated".to_string(),
            baseline: 0.10,
            treatment: 0.08,
            delta: -0.02,
            w: None,
            p_two_sided: None,
            d_z: None,
            ci_lower: None,
            ci_upper: None,
            verdict: Some(crate::score::GateOutcome::Pass),
            wilcoxon_method: None,
            n_nonzero: None,
        });

        let table = render_r_table(&rec);
        let covers = table
            .lines()
            .find(|l| l.starts_with("Gate covers:"))
            .expect("gate-scope footer must render");
        let ungated = table
            .lines()
            .find(|l| l.starts_with("UNGATED"))
            .expect("ungated line must render");

        assert!(covers.contains("cost_usd"), "gated line: {covers}");
        assert!(
            !ungated.contains("cost_usd"),
            "cost_usd is gated here, yet the report also calls it ungated:\n  {covers}\n  {ungated}"
        );
    }

    /// #4 acceptance criterion 1: an ungated regression must not pass silently.
    ///
    /// The scope line alone reports the *policy* — it prints identically whether cost
    /// doubled or did not move, so a 2x cost regression produced byte-identical output to a
    /// flat run and the reader had to spot it unaided in the deltas. That is what "silently
    /// PASS" describes. #4 accepts either gating with a correction or **loud reporting**;
    /// this is the loud-reporting branch.
    #[test]
    fn an_ungated_regression_is_reported_loudly() {
        let mut rec = sample_result_record();
        // Solve-rate holds (gated, passes) while cost more than doubles.
        rec.total_claude_calls = 12;
        rec.baseline_cost_usd = Some(0.50);
        rec.treatment_cost_usd = Some(1.06);
        rec.total_cost_usd = Some(1.56);
        // ...and a tracked quality dimension drops hard.
        rec.rows.push(MetricRow {
            metric: "wellformed_pct".to_string(),
            tag: "tracked".to_string(),
            baseline: 0.90,
            treatment: 0.45,
            delta: -0.45,
            w: None,
            p_two_sided: None,
            d_z: None,
            ci_lower: None,
            ci_upper: None,
            verdict: None,
            wilcoxon_method: None,
            n_nonzero: None,
        });

        let table = render_r_table(&rec);
        let alarm = table
            .lines()
            .find(|l| l.starts_with("UNGATED REGRESSION"))
            .unwrap_or_else(|| panic!("no ungated-regression alarm rendered:\n{table}"));

        assert!(
            alarm.contains("cost_usd"),
            "cost more than doubled: {alarm}"
        );
        assert!(
            alarm.contains("wellformed_pct"),
            "wellformedness halved: {alarm}"
        );
        assert!(
            alarm.contains("did not fail the run"),
            "the consequence must be spelled out, not inferred: {alarm}"
        );
    }

    /// Every dimension that can reach a row has a known regression direction.
    ///
    /// `worse_when_higher` returning `None` means "never alarmed", so a metric missing from
    /// it fails silently — the precise defect #4 exists to close, reintroduced one level
    /// down. This pins the premise instead of trusting a comment to be read.
    ///
    /// The list is the emission sites in `run.rs` plus the two footer dimensions; if a new
    /// metric is added there and not here, this fails rather than the metric quietly
    /// becoming un-alarmable.
    #[test]
    fn every_tracked_metric_has_a_known_direction() {
        for metric in [
            "node_pass_rate",
            "judge_quality",
            "engine_broken_rate",
            "wellformed_pct",
            "pass_at_1",
            "pass_at_2",
            "cost_usd",
        ] {
            assert!(
                worse_when_higher(metric).is_some(),
                "'{metric}' has no regression direction, so it can never be alarmed"
            );
        }
        // And the converse: an unknown metric must be inert rather than guessed at.
        assert!(worse_when_higher("some_future_metric").is_none());
    }

    /// An improvement is never an alarm, in either direction convention.
    #[test]
    fn direction_is_respected_for_both_conventions() {
        // cost: higher is worse.
        assert!(ungated_regression("cost_usd", 1.0, 2.0).is_some());
        assert!(ungated_regression("cost_usd", 2.0, 1.0).is_none());
        // wellformed_pct: lower is worse.
        assert!(ungated_regression("wellformed_pct", 0.9, 0.4).is_some());
        assert!(ungated_regression("wellformed_pct", 0.4, 0.9).is_none());
        // Immaterial moves are filtered, in both directions.
        assert!(ungated_regression("cost_usd", 1.0, 1.01).is_none());
    }

    /// A zero baseline is the loudest regression there is, not an exempt one.
    ///
    /// The materiality filter is expressed as a *relative* change, and zero has no relative
    /// change to divide by — so the first cut of this alarm returned `None` for a zero
    /// baseline and dropped the move entirely.
    ///
    /// **The live v1 case is cost.** The cross-loop experiment runs a free local baseline
    /// against a paid treatment (`measurement/experiments/cross-loop-local-vs-claude.yaml`,
    /// `backend: local` vs `backend: claude-cli`), and the local rung reports `cost_usd=0.0`
    /// rather than `unknown` — so a $0 -> $1.06 regression has a zero baseline by
    /// construction, in the experiment shape where cost matters most.
    ///
    /// **`engine_broken_rate` is not yet reachable, and is stated here as the forward case.**
    /// It is unwired in v1 and reports ABSENT, so 0% -> 40% broken cannot occur as a row
    /// today. Its healthy baseline is exactly zero, so it becomes the load-bearing case when
    /// v2 wires a source. Both v1 row metrics (`node_pass_rate`, `judge_quality`) are
    /// lower-is-worse, so a zero baseline on either can only be an improvement.
    ///
    /// The direction still decides — zero is a floor, and a metric climbing off it is only
    /// an alarm when climbing is the wrong way.
    #[test]
    fn a_regression_off_a_zero_baseline_is_not_silently_dropped() {
        // Higher-is-worse, off zero: alarm. Both real shapes.
        assert!(ungated_regression("cost_usd", 0.0, 1.06).is_some());
        assert!(ungated_regression("engine_broken_rate", 0.0, 0.4).is_some());
        // Lower-is-worse, off zero: that is an improvement, and must stay silent.
        assert!(ungated_regression("node_pass_rate", 0.0, 0.5).is_none());
        // No move at all is no alarm, whichever convention.
        assert!(ungated_regression("cost_usd", 0.0, 0.0).is_none());
        assert!(ungated_regression("node_pass_rate", 0.0, 0.0).is_none());
        // An unknown metric stays inert even here — direction is still the first gate.
        assert!(ungated_regression("some_future_metric", 0.0, 9.0).is_none());
    }

    /// The zero-baseline alarm renders as words, not as `+inf%`.
    ///
    /// It also sorts ahead of every finite regression: an unbounded move is the worst one
    /// on the line, and the line is read left to right.
    ///
    /// The fixture is a **v2 shape**, deliberately: `engine_broken_rate` is unwired in v1 and
    /// reports ABSENT rather than reaching a row. The renderer is generic over metrics, so
    /// this exercises the wording and ordering against the case the guard exists for, before
    /// a source makes it producible. The live v1 path is cost, covered separately.
    #[test]
    fn a_zero_baseline_regression_renders_and_sorts_first() {
        let mut rec = sample_result_record();
        for (metric, baseline, treatment) in [
            ("engine_broken_rate", 0.0, 0.4),
            ("wellformed_pct", 1.0, 0.5),
        ] {
            rec.rows.push(MetricRow {
                metric: metric.to_string(),
                tag: "tracked".to_string(),
                baseline,
                treatment,
                delta: treatment - baseline,
                w: None,
                p_two_sided: None,
                d_z: None,
                ci_lower: None,
                ci_upper: None,
                verdict: None,
                wilcoxon_method: None,
                n_nonzero: None,
            });
        }
        let table = render_r_table(&rec);
        let line = table
            .lines()
            .find(|l| l.starts_with("UNGATED REGRESSION"))
            .expect("a zero-baseline regression must reach the alarm line");
        assert!(
            !line.contains("inf"),
            "an unbounded move must be worded, not printed as a float: {line}"
        );
        assert!(
            line.contains("engine_broken_rate from zero"),
            "the alarm must name the metric and say it came off zero: {line}"
        );
        let broken_at = line.find("engine_broken_rate").expect("named");
        let wellformed_at = line.find("wellformed_pct").expect("named");
        assert!(
            broken_at < wellformed_at,
            "the unbounded regression must sort ahead of the finite one: {line}"
        );
    }

    /// The alarm is an alarm, not a banner: silent when nothing regressed.
    ///
    /// A line that prints on every run is the static scope statement again, one paragraph
    /// down — it would carry no information and would train a reader to skip it.
    #[test]
    fn no_alarm_when_no_ungated_dimension_regressed() {
        let mut rec = sample_result_record();
        rec.total_claude_calls = 12;
        rec.baseline_cost_usd = Some(1.00);
        rec.treatment_cost_usd = Some(0.90); // cheaper — an improvement
        rec.total_cost_usd = Some(1.90);
        for r in rec.rows.iter_mut().filter(|r| r.tag == "tracked") {
            r.baseline = 0.50;
            r.treatment = 0.60; // better
            r.delta = 0.10;
        }

        let table = render_r_table(&rec);
        assert!(
            !table.contains("UNGATED REGRESSION"),
            "nothing regressed, so nothing should be alarmed:\n{table}"
        );
    }

    /// Cost is named ungated only when it was actually measured.
    ///
    /// The cost footer is omitted entirely for local-only runs — deliberately, so a reader
    /// is not shown a misleading `$0.0000`. Naming `cost_usd` on the ungated line regardless
    /// tells that same reader the gate does not cover a number the report never produced.
    #[test]
    fn cost_is_not_called_measured_when_no_paid_call_ran() {
        let mut rec = sample_result_record();
        rec.total_claude_calls = 0;
        rec.total_cost_usd = None;
        rec.baseline_cost_usd = None;
        rec.treatment_cost_usd = None;

        let table = render_r_table(&rec);
        let ungated = table
            .lines()
            .find(|l| l.starts_with("UNGATED"))
            .expect("ungated line must render");
        assert!(
            !table.contains("Cost ("),
            "precondition: the cost footer must be absent on a local-only run"
        );
        assert!(
            !ungated.contains("cost_usd"),
            "no paid call ran, so cost was not measured:\n  {ungated}"
        );
    }

    /// Cost is named ungated only when the run could actually price it.
    ///
    /// The sibling above covers `total_claude_calls == 0`. This covers the other way cost
    /// goes unmeasured while paid calls *did* run: any call that reports `cost_usd=unknown`
    /// sets a run-wide flag and blanks all three cost fields, so the footer degrades to
    /// `Cost: unreported` while `total_claude_calls` stays positive.
    ///
    /// That is reachable on the documented cascade, not hypothetically: the third rung is an
    /// external openai-compat router whose responses carry no cost field, so
    /// `_run_openai_compat` returns `cost_usd=None` while `claude-cli` on the rung above
    /// reports real money. A single `claude -p` reply with a missing or malformed
    /// `total_cost_usd` does it too.
    ///
    /// Without this, the same report says `cost_usd` was *measured* on the ungated line and
    /// *unreported* in the footer two lines above — the exact "ABSENT must not also be
    /// reported as measured" invariant that `absent_metrics` enforces for row metrics, which
    /// cost slipped through by having no row.
    #[test]
    fn cost_is_not_called_measured_when_the_run_could_not_price_it() {
        let rec = record_with_cost(None, None, None, 3);
        let table = render_r_table(&rec);
        assert!(
            table.contains("Cost: unreported"),
            "precondition: the report must already know it could not price this run:\n{table}"
        );
        let ungated = table
            .lines()
            .find(|l| l.starts_with("UNGATED"))
            .expect("ungated line must render");
        assert!(
            !ungated.contains("cost_usd"),
            "the footer calls cost unreported, so this line must not call it measured:\n  {ungated}"
        );
        // Vacuity guard: absence must come from cost being dropped, not from the line
        // collapsing to nothing and trivially satisfying the assertion above.
        assert!(
            ungated.contains("judge_quality"),
            "the ungated line must still name the dimensions that WERE measured:\n  {ungated}"
        );
    }

    /// The report never names a dimension it does not surface.
    ///
    /// `duration_ms` is collected per run in the driver and never aggregated into
    /// `ResultRecord` or rendered anywhere. Listing it as "measured, never gated" asserts a
    /// visibility the report does not provide — the reader is told where to look and finds
    /// nothing. It belongs on this line when it is reported, not before.
    #[test]
    fn the_report_names_no_dimension_it_does_not_surface() {
        let rec = sample_result_record();
        let table = render_r_table(&rec);
        assert!(
            !table.contains("duration"),
            "duration reaches no output, so the report must not name it:\n{table}"
        );
    }

    /// The dimension with no row still reaches the report.
    ///
    /// Deriving the ungated list purely from emitted rows would satisfy the partition test
    /// above and still drop cost, which is how it came to be appended by hand in the first
    /// place.
    #[test]
    fn cost_still_reaches_the_report_when_it_was_measured() {
        let rec = sample_result_record();
        assert!(
            rec.total_claude_calls > 0,
            "precondition: this fixture must have a paid call for cost to be measured"
        );
        let table = render_r_table(&rec);
        let ungated = table
            .lines()
            .find(|l| l.starts_with("UNGATED"))
            .expect("ungated line must render");
        assert!(
            ungated.contains("cost_usd"),
            "cost has no row, so only this list can name it:\n  {ungated}"
        );
    }

    /// A metric reported ABSENT must not also be reported as measured.
    ///
    /// The ungated line reads "measured, never gated", so naming a dimension there asserts
    /// this run measured it. An earlier version of the gate-scope footer was built from a
    /// static registry of every known dimension, which said exactly that about metrics the
    /// same report declared ABSENT two lines below — the report contradicting itself, in
    /// the paragraph whose job is stating scope accurately. Same defect this footer exists
    /// to prevent, introduced by the footer.
    #[test]
    fn an_absent_metric_is_never_also_reported_as_measured() {
        let mut rec = sample_result_record();
        // judge_quality is declared but unmeasured: drop its row and name it absent.
        rec.rows.retain(|r| r.metric != "judge_quality");
        rec.absent_metrics = vec!["judge_quality".to_string()];

        let table = render_r_table(&rec);
        let ungated = table
            .lines()
            .find(|l| l.starts_with("UNGATED"))
            .expect("ungated line must render");
        let absent = table
            .lines()
            .find(|l| l.starts_with("ABSENT"))
            .expect("absent line must render");

        assert!(absent.contains("judge_quality"), "absent line: {absent}");
        assert!(
            !ungated.contains("judge_quality"),
            "judge_quality is ABSENT, yet the report also calls it measured:\n  {ungated}\n  {absent}"
        );
    }

    // ── wellformed_pct / pass@1 / pass@2 rendering ───────────────────────────

    fn record_with_wellformed_and_pass_rows() -> ResultRecord {
        let mut rec = sample_result_record();
        rec.rows.push(MetricRow {
            metric: "wellformed_pct".to_string(),
            tag: "tracked".to_string(),
            baseline: 0.8,
            treatment: 0.9,
            delta: 0.1,
            w: None,
            p_two_sided: None,
            d_z: None,
            ci_lower: None,
            ci_upper: None,
            verdict: None,
            wilcoxon_method: None,
            n_nonzero: None,
        });
        rec.rows.push(MetricRow {
            metric: "pass_at_1".to_string(),
            tag: "tracked".to_string(),
            baseline: 0.6,
            treatment: 0.7,
            delta: 0.1,
            w: None,
            p_two_sided: None,
            d_z: None,
            ci_lower: None,
            ci_upper: None,
            verdict: None,
            wilcoxon_method: None,
            n_nonzero: None,
        });
        rec.rows.push(MetricRow {
            metric: "pass_at_2".to_string(),
            tag: "tracked".to_string(),
            baseline: 0.7,
            treatment: 0.8,
            delta: 0.1,
            w: None,
            p_two_sided: None,
            d_z: None,
            ci_lower: None,
            ci_upper: None,
            verdict: None,
            wilcoxon_method: None,
            n_nonzero: None,
        });
        rec
    }

    #[test]
    fn r_table_renders_wellformed_and_pass_rows() {
        let rec = record_with_wellformed_and_pass_rows();
        let md = render_r_table(&rec);
        assert!(
            md.contains("wellformed_pct"),
            "wellformed_pct must appear in the R-table; got:\n{md}"
        );
        assert!(
            md.contains("pass_at_1"),
            "pass_at_1 must appear in the R-table; got:\n{md}"
        );
        assert!(
            md.contains("pass_at_2"),
            "pass_at_2 must appear in the R-table; got:\n{md}"
        );
        // All three are tracked → verdict column must show "—".
        let pass_at_2_line = md.lines().find(|l| l.contains("pass_at_2")).unwrap_or("");
        assert!(
            pass_at_2_line.contains('—'),
            "tracked row must render verdict as —; line={pass_at_2_line}"
        );
        // Numeric values must appear with the standard 3-decimal format.
        assert!(
            md.contains("0.800") || md.contains("0.900"),
            "wellformed_pct values must appear numerically; got:\n{md}"
        );
    }

    #[test]
    fn render_handles_none_stats_for_tracked_rows() {
        let rec = ResultRecord {
            name: "x".to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            reps: 5,
            seeds_honoured: false,
            rows: vec![MetricRow {
                metric: "engine_broken_rate".to_string(),
                tag: "tracked".to_string(),
                baseline: 0.0,
                treatment: 0.0,
                delta: 0.0,
                w: None,
                p_two_sided: None,
                d_z: None,
                ci_lower: None,
                ci_upper: None,
                verdict: None,
                wilcoxon_method: None,
                n_nonzero: None,
            }],
            gate_exit: 0,
            aborted: false,
            abort_reason: None,
            total_cost_usd: None,
            baseline_cost_usd: None,
            treatment_cost_usd: None,
            total_claude_calls: 0,
            validity_warnings: vec![],
            inconclusive_count: 0,
            inconclusive_fraction: 0.0,
            n_discordant: Some(5),
            min_attainable_p: Some(0.0625),
            absent_metrics: vec!["engine_broken_rate".to_string()],
        };
        let md = render_r_table(&rec);
        assert!(md.contains("engine_broken_rate"));
        assert!(md.contains("tracked"));
        // None fields render as —
        assert!(md.contains('—'));
    }
}
