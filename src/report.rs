use anyhow::Result;
use serde::Serialize;

use crate::cli::MetricSource;
use crate::git::ResolvedRev;
use crate::stats::{Comparison, Summary, Verdict};

pub fn fmt_ns(ns: f64) -> String {
    if ns >= 1e9 {
        format!("{:.2} s", ns / 1e9)
    } else if ns >= 1e6 {
        format!("{:.2} ms", ns / 1e6)
    } else if ns >= 1e3 {
        format!("{:.2} µs", ns / 1e3)
    } else {
        format!("{:.2} ns", ns)
    }
}

pub fn fmt_value(v: f64, unit: &str) -> String {
    match unit {
        "ns" => fmt_ns(v),
        "s" => fmt_ns(v * 1e9),
        "metric" => format!("{v:.4}"),
        _ => format!("{v:.4}"),
    }
}

fn fmt_summary(summary: &Summary, unit: &str) -> String {
    let rel_std_dev = if summary.mean == 0.0 {
        String::new()
    } else {
        format!(" (± {:.1}%)", summary.std_dev / summary.mean.abs() * 100.0)
    };
    format!(
        "{} ± {}{}",
        fmt_value(summary.mean, unit),
        fmt_value(summary.std_dev, unit),
        rel_std_dev
    )
}

pub struct HumanReport<'a> {
    pub package: &'a str,
    pub profile: &'a str,
    pub mode_label: String,
    pub metric_label: Option<String>,
    pub reps_label: String,
    pub pinned_label: String,
    pub governor: Option<String>,
    pub governor_set_by_tool: bool,
    pub base: &'a ResolvedRev,
    pub candidate: &'a ResolvedRev,
    pub build: &'a str,
    pub results: &'a [Comparison],
    pub only_in_base: &'a [String],
    pub only_in_candidate: &'a [String],
}

pub fn print_human(r: HumanReport<'_>) {
    let mut settings = vec![
        ("package", r.package.to_owned()),
        ("mode", r.mode_label),
        ("profile", r.profile.to_owned()),
        ("reps", r.reps_label),
    ];
    if let Some(metric) = r.metric_label {
        settings.push(("metric", metric));
    }
    settings.push(("pinning", r.pinned_label));
    if let Some(governor) = r.governor {
        settings.push((
            "governor",
            if r.governor_set_by_tool {
                format!("{governor} (set for this run)")
            } else {
                governor
            },
        ));
    }
    settings.push(("build", r.build.to_owned()));
    settings.push(("RUSTFLAGS", "-C target-cpu=native".to_owned()));
    settings.push(("base", format!("{} ({})", r.base.spec, r.base.short)));
    settings.push((
        "rev",
        format!("{} ({})", r.candidate.spec, r.candidate.short),
    ));

    let mut rows = vec![[
        "benchmark".to_owned(),
        "base".to_owned(),
        "rev".to_owned(),
        "Δ".to_owned(),
        "verdict".to_owned(),
    ]];
    for cmp in r.results {
        let base = fmt_summary(&cmp.base, &cmp.unit);
        let candidate = fmt_summary(&cmp.candidate, &cmp.unit);
        let delta = if cmp.rel_diff_pct.is_nan() {
            "n/a".to_owned()
        } else {
            format!("{:+.1}%", cmp.rel_diff_pct)
        };
        rows.push([
            cmp.id.clone(),
            base,
            candidate,
            delta,
            verdict_text(&cmp.verdict).to_owned(),
        ]);
    }
    print!("{}", report_table(&settings, &rows));
    if !r.only_in_base.is_empty() {
        println!("only in base: {}", r.only_in_base.join(", "));
    }
    if !r.only_in_candidate.is_empty() {
        println!("only in candidate: {}", r.only_in_candidate.join(", "));
    }
}

/// One box: key/value settings on top, results below a section separator.
/// Results use one key/value section per benchmark.
fn report_table(settings: &[(&str, String)], results: &[[String; 5]]) -> String {
    vertical_table(settings, results)
}

/// Narrow-terminal layout: two columns throughout, one key/value section per
/// benchmark (labelled with the result header), sections split by separators.
fn vertical_table(settings: &[(&str, String)], results: &[[String; 5]]) -> String {
    let header = &results[0];
    let mut sections = vec![
        settings
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>(),
    ];
    for row in &results[1..] {
        sections.push(
            (0..5)
                .map(|col| (header[col].as_str(), row[col].clone()))
                .collect(),
        );
    }

    let width = |s: &str| s.chars().count();
    let cells = || sections.iter().flatten();
    let key_w = cells().map(|(k, _)| width(k)).max().unwrap_or(0);
    let val_w = cells().map(|(_, v)| width(v)).max().unwrap_or(0);
    let mut out = String::new();
    out.push_str(&format!(
        "┌─{}─┬─{}─┐\n",
        "─".repeat(key_w),
        "─".repeat(val_w)
    ));
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!(
                "├─{}─┼─{}─┤\n",
                "─".repeat(key_w),
                "─".repeat(val_w)
            ));
        }
        for (key, value) in section {
            out.push_str(&format!("│ {key:<key_w$} │ {value:<val_w$} │\n"));
        }
    }
    out.push_str(&format!(
        "└─{}─┴─{}─┘\n",
        "─".repeat(key_w),
        "─".repeat(val_w)
    ));
    out
}

fn verdict_text(v: &Verdict) -> &'static str {
    match v {
        Verdict::Improved => "improved",
        Verdict::Regressed => "REGRESSED",
        Verdict::NoChange => "no change (within noise)",
    }
}

#[derive(Serialize)]
struct JsonReport<'a> {
    tool: &'static str,
    version: &'static str,
    mode: &'a str,
    package: &'a str,
    profile: &'a str,
    base: &'a ResolvedRev,
    candidate: &'a ResolvedRev,
    build: &'a str,
    pinned_core: Option<u32>,
    governor: Option<&'a str>,
    governor_set_by_tool: bool,
    dirty_worktree: bool,
    results: &'a [Comparison],
    only_in_base: &'a [String],
    only_in_candidate: &'a [String],
}

pub struct JsonReportInput<'a> {
    pub mode: &'a str,
    pub package: &'a str,
    pub profile: &'a str,
    pub base: &'a ResolvedRev,
    pub candidate: &'a ResolvedRev,
    pub build: &'a str,
    pub pinned_core: Option<u32>,
    pub governor: Option<&'a str>,
    pub governor_set_by_tool: bool,
    pub dirty: bool,
    pub results: &'a [Comparison],
    pub only_in_base: &'a [String],
    pub only_in_candidate: &'a [String],
}

pub fn print_json(input: JsonReportInput<'_>) -> Result<()> {
    let report = JsonReport {
        tool: "cargo-bench-compare",
        version: env!("CARGO_PKG_VERSION"),
        mode: input.mode,
        package: input.package,
        profile: input.profile,
        base: input.base,
        candidate: input.candidate,
        build: input.build,
        pinned_core: input.pinned_core,
        governor: input.governor,
        governor_set_by_tool: input.governor_set_by_tool,
        dirty_worktree: input.dirty,
        results: input.results,
        only_in_base: input.only_in_base,
        only_in_candidate: input.only_in_candidate,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub fn metric_label(metric: &MetricSource) -> String {
    match metric {
        MetricSource::WallClock => "wall-clock (s, lower is better)".to_owned(),
        MetricSource::Regex {
            raw,
            higher_is_better,
            ..
        } => {
            let dir = if *higher_is_better {
                "higher is better"
            } else {
                "lower is better"
            };
            format!("regex '{raw}' ({dir})")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_use_magnitude_adaptive_formatting() {
        assert_eq!(fmt_value(0.105, "s"), "105.00 ms");
        assert_eq!(fmt_value(0.0004, "s"), "400.00 µs");
    }

    #[test]
    fn summary_shows_relative_std_dev() {
        let summary = Summary {
            n: 5,
            mean: 100.0,
            std_dev: 12.8,
            min: 80.0,
            max: 120.0,
        };
        assert_eq!(
            fmt_summary(&summary, "metric"),
            "100.0000 ± 12.8000 (± 12.8%)"
        );
    }

    fn sample_results() -> Vec<[String; 5]> {
        vec![
            ["benchmark", "base", "rev", "Δ", "verdict"].map(str::to_owned),
            [
                "a-benchmark-with-a-long-name",
                "1.0 ± 0.1",
                "1.1 ± 0.2",
                "+10.0%",
                "improved",
            ]
            .map(str::to_owned),
        ]
    }

    #[test]
    fn report_table_stacks_results_as_labelled_sections() {
        let settings = vec![("package", "rmc-minimal".to_owned())];
        let mut results = sample_results();
        results.push(["fib_20", "2.0 ± 0.1", "1.9 ± 0.2", "-5.0%", "improved"].map(str::to_owned));
        let table = report_table(&settings, &results);
        let lines = table.lines().collect::<Vec<_>>();
        // top + 1 setting + 2 benchmarks of (separator + 5 rows) + bottom
        assert_eq!(lines.len(), 15);
        assert_eq!(lines.iter().filter(|l| l.starts_with('├')).count(), 2);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("│ benchmark") && l.contains("fib_20"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("│ Δ") && l.contains("-5.0%"))
        );
        let width = lines[0].chars().count();
        for line in &lines {
            assert_eq!(line.chars().count(), width, "misaligned row: {line}");
        }
    }
}
