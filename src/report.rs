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
    /// `Some(true)` = passed gate, `Some(false)` = regressed, `None` = tracked (no verdict).
    pub verdict: Option<bool>,
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
    /// 0 = no regression, 1 = regression, 3 = aborted.
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
    /// (ADR-0041 §per-node soft-exclusion, A5).
    pub inconclusive_count: u64,
    /// `inconclusive_count / total_pairs_attempted`; `0.0` when no pairs were
    /// attempted. Compared against `INCONCLUSIVE_MAX_FRACTION` (A6).
    pub inconclusive_fraction: f64,
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
            Some(true) => "PASS".to_string(),
            Some(false) => "FAIL".to_string(),
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
    // (ADR-0041 §per-node soft-exclusion / A6 fail-loud floor).
    if rec.inconclusive_count > 0 {
        out.push('\n');
        out.push_str(&format!(
            "Inconclusive: {} pair(s) excluded ({:.1}% of attempted) — artifacts, not capability misses\n",
            rec.inconclusive_count,
            rec.inconclusive_fraction * 100.0,
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
                    verdict: Some(true),
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
        let path = std::env::temp_dir().join("dotclaude-measure-report-test.json");
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
        };
        let path = std::env::temp_dir().join("dotclaude-measure-abort-test.json");
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
        };
        let md = render_r_table(&rec);
        assert!(md.contains("engine_broken_rate"));
        assert!(md.contains("tracked"));
        // None fields render as —
        assert!(md.contains('—'));
    }
}
