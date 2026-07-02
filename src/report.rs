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
    settings.push(("rev", format!("{} ({})", r.candidate.spec, r.candidate.short)));

    let mut rows = vec![[
        "benchmark".to_owned(),
        "base".to_owned(),
        "rev".to_owned(),
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
    print!("{}", report_table(&settings, &rows));
    if !r.only_in_base.is_empty() {
        println!("only in base: {}", r.only_in_base.join(", "));
    }
    if !r.only_in_candidate.is_empty() {
        println!("only in candidate: {}", r.only_in_candidate.join(", "));
    }
}

/// One box: key/value settings on top, results below a section separator.
/// Results are columns when they fit the terminal, otherwise one key/value
/// section per benchmark (piped output always uses columns).
fn report_table(settings: &[(&str, String)], results: &[[String; 5]]) -> String {
    let table = horizontal_table(settings, results);
    let table_w = table.lines().next().map_or(0, |l| l.chars().count());
    match terminal_width() {
        Some(term_w) if table_w > term_w => vertical_table(settings, results),
        _ => table,
    }
}

#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ only fills in the winsize out-param
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col as usize)
}

#[cfg(not(unix))]
fn terminal_width() -> Option<usize> {
    None
}

/// The value column of the settings section spans the four result columns.
/// The first column (settings keys / benchmark names) is shared.
fn horizontal_table(settings: &[(&str, String)], results: &[[String; 5]]) -> String {
    // pad by char count, not byte length, so multi-byte cells (±, µ) stay aligned
    let width = |s: &str| s.chars().count();
    let key_w = settings
        .iter()
        .map(|(k, _)| width(k))
        .chain(results.iter().map(|row| width(&row[0])))
        .max()
        .unwrap_or(0);
    let mut col_w = (1..5)
        .map(|col| results.iter().map(|row| width(&row[col])).max().unwrap_or(0))
        .collect::<Vec<_>>();
    // the settings value column spans the four result columns (each padded by
    // one space per side, plus three `│` separators); widen the last result
    // column if the longest settings value needs more room than the results
    let val_w_min = settings.iter().map(|(_, v)| width(v)).max().unwrap_or(0);
    let spanned = |col_w: &[usize]| col_w.iter().sum::<usize>() + 9;
    if val_w_min > spanned(&col_w) {
        col_w[3] += val_w_min - spanned(&col_w);
    }
    let val_w = spanned(&col_w);

    let mut out = String::new();
    out.push_str(&format!(
        "┌─{}─┬─{}─┐\n",
        "─".repeat(key_w),
        "─".repeat(val_w)
    ));
    for (key, value) in settings {
        out.push_str(&format!("│ {key:<key_w$} │ {value:<val_w$} │\n"));
    }
    out.push_str(&format!(
        "├─{}─┼─{}─┬─{}─┬─{}─┬─{}─┤\n",
        "─".repeat(key_w),
        "─".repeat(col_w[0]),
        "─".repeat(col_w[1]),
        "─".repeat(col_w[2]),
        "─".repeat(col_w[3])
    ));
    for (i, row) in results.iter().enumerate() {
        if i == 1 {
            out.push_str(&format!(
                "├─{}─┼─{}─┼─{}─┼─{}─┼─{}─┤\n",
                "─".repeat(key_w),
                "─".repeat(col_w[0]),
                "─".repeat(col_w[1]),
                "─".repeat(col_w[2]),
                "─".repeat(col_w[3])
            ));
        }
        out.push_str(&format!(
            "│ {:<key_w$} │ {:<w1$} │ {:<w2$} │ {:>w3$} │ {:<w4$} │\n",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w1 = col_w[0],
            w2 = col_w[1],
            w3 = col_w[2],
            w4 = col_w[3],
        ));
    }
    out.push_str(&format!(
        "└─{}─┴─{}─┴─{}─┴─{}─┴─{}─┘\n",
        "─".repeat(key_w),
        "─".repeat(col_w[0]),
        "─".repeat(col_w[1]),
        "─".repeat(col_w[2]),
        "─".repeat(col_w[3])
    ));
    out
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

    fn sample_results() -> Vec<[String; 5]> {
        vec![
            ["benchmark", "base", "rev", "Δ", "verdict"].map(str::to_owned),
            ["a-benchmark-with-a-long-name", "1.0 ± 0.1", "1.1 ± 0.2", "+10.0%", "improved"]
                .map(str::to_owned),
        ]
    }

    #[test]
    fn horizontal_table_draws_one_aligned_box() {
        let settings = vec![
            ("package", "rmc-minimal".to_owned()),
            ("metric", "µs (multi-byte value)".to_owned()),
        ];
        let results = sample_results();
        let table = horizontal_table(&settings, &results);
        let lines = table.lines().collect::<Vec<_>>();
        // 2 settings + 2 results + top, section separator, header separator, bottom
        assert_eq!(lines.len(), 8);
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
        assert!(lines[1].contains("│ package"));
        assert!(lines[3].starts_with('├') && lines[3].contains('┼') && lines[3].contains('┬'));
        assert!(lines[4].contains("│ benchmark"));
        assert!(lines[5].starts_with('├') && !lines[5].contains('┬'), "header separator");
        assert!(lines[6].contains("+10.0% │"), "Δ column right-aligned");
        assert!(lines[7].starts_with('└') && lines[7].ends_with('┘'));
        let width = lines[0].chars().count();
        for line in &lines {
            assert_eq!(line.chars().count(), width, "misaligned row: {line}");
        }

        // a very long settings value widens the results section instead of overflowing
        let settings = vec![("metric", "x".repeat(120))];
        let table = horizontal_table(&settings, &results);
        let lines = table.lines().collect::<Vec<_>>();
        let width = lines[0].chars().count();
        for line in &lines {
            assert_eq!(line.chars().count(), width, "misaligned row: {line}");
        }
    }

    #[test]
    fn vertical_table_stacks_results_as_labelled_sections() {
        let settings = vec![("package", "rmc-minimal".to_owned())];
        let mut results = sample_results();
        results.push(["fib_20", "2.0 ± 0.1", "1.9 ± 0.2", "-5.0%", "improved"].map(str::to_owned));
        let table = vertical_table(&settings, &results);
        let lines = table.lines().collect::<Vec<_>>();
        // top + 1 setting + 2 benchmarks of (separator + 5 rows) + bottom
        assert_eq!(lines.len(), 15);
        assert_eq!(lines.iter().filter(|l| l.starts_with('├')).count(), 2);
        assert!(lines.iter().any(|l| l.starts_with("│ benchmark") && l.contains("fib_20")));
        assert!(lines.iter().any(|l| l.starts_with("│ Δ") && l.contains("-5.0%")));
        let width = lines[0].chars().count();
        for line in &lines {
            assert_eq!(line.chars().count(), width, "misaligned row: {line}");
        }
    }
}
