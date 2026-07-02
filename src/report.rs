use anyhow::Result;
use serde::Serialize;

use crate::cli::MetricSource;
use crate::git::ResolvedRev;
use crate::stats::{Comparison, Verdict};

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

pub struct HumanReport<'a> {
    pub package: &'a str,
    pub profile: &'a str,
    pub mode_label: String,
    pub metric_label: Option<String>,
    pub reps_label: String,
    pub pinned_label: String,
    pub base: &'a ResolvedRev,
    pub candidate: &'a ResolvedRev,
    pub build: &'a str,
    pub dirty: bool,
    pub results: &'a [Comparison],
    pub only_in_base: &'a [String],
    pub only_in_candidate: &'a [String],
}

pub fn print_human(r: HumanReport<'_>) {
    println!(
        "comparing {} ({}, candidate) vs {} ({}, base)",
        r.candidate.spec, r.candidate.short, r.base.spec, r.base.short
    );
    println!(
        "package: {}   mode: {}   profile: {}   reps: {}",
        r.package, r.mode_label, r.profile, r.reps_label
    );
    if let Some(metric) = r.metric_label {
        println!("metric: {metric}");
    }
    println!(
        "pinning: {}   build: {}   RUSTFLAGS=\"-C target-cpu=native\"",
        r.pinned_label, r.build
    );
    if r.dirty {
        println!(
            "warning: working tree is dirty; uncommitted local changes are NOT included in either side"
        );
    }
    println!();

    let mut rows = vec![[
        "benchmark".to_owned(),
        "base".to_owned(),
        "candidate".to_owned(),
        "Δ".to_owned(),
        "verdict".to_owned(),
    ]];
    for cmp in r.results {
        let base = format!(
            "{} ± {}",
            fmt_value(cmp.base.mean, &cmp.unit),
            fmt_value(cmp.base.std_dev, &cmp.unit)
        );
        let candidate = format!(
            "{} ± {}",
            fmt_value(cmp.candidate.mean, &cmp.unit),
            fmt_value(cmp.candidate.std_dev, &cmp.unit)
        );
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
    let widths = (0..5)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect::<Vec<_>>();
    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:>w3$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        );
    }
    if !r.only_in_base.is_empty() {
        println!("only in base: {}", r.only_in_base.join(", "));
    }
    if !r.only_in_candidate.is_empty() {
        println!("only in candidate: {}", r.only_in_candidate.join(", "));
    }
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
}
