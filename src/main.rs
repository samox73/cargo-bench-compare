mod builder;
mod candidates;
mod cli;
mod completions;
mod criterion;
mod git;
mod report;
mod runner;
mod stats;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};

use crate::cli::{Cli, Mode, Sub};
use crate::stats::{Summary, summarize};

static CANCELLED: AtomicBool = AtomicBool::new(false);

fn main() {
    if let Err(err) = real_main() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse_from_env();
    match &cli.command {
        Some(Sub::Completions(args)) => return completions::run(args),
        Some(Sub::Candidates { kind }) => return candidates::print(*kind),
        None => {}
    }
    let mode = cli.mode()?;
    let package = cli.package()?;
    let rev_spec = cli.rev.clone().unwrap_or_else(|| ":worktree".to_owned());
    let base_spec = cli
        .rev_base
        .clone()
        .unwrap_or_else(|| ":merge-base".to_owned());
    if rev_spec == ":worktree" && base_spec == ":worktree" {
        return Err(anyhow!(
            "candidate and base are both ':worktree'; nothing to compare"
        ));
    }

    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::SeqCst))?;

    let repo_root = git::repo_root()?;
    let workspace = workspace::load(&repo_root, package)?;
    let dirty = git::is_dirty(&repo_root)?;
    let base = git::resolve_spec(&repo_root, &base_spec)?;
    let candidate = git::resolve_spec(&repo_root, &rev_spec)?;
    if base.sha == candidate.sha {
        let hint = if cli.rev.is_none() && cli.rev_base.is_none() {
            " (working tree is clean and HEAD is already the merge base; commit something or pass --rev/--rev-base)"
        } else {
            ""
        };
        return Err(anyhow!(
            "base and candidate both resolve to {}; nothing to compare{hint}",
            base.sha
        ));
    }
    if dirty {
        if base.spec == "worktree" || candidate.spec == "worktree" {
            eprintln!(
                "note: benchmarking a snapshot of the dirty working tree (staged, unstaged, and untracked changes included)"
            );
        } else {
            eprintln!(
                "warning: working tree is dirty; uncommitted local changes are NOT included in either side"
            );
        }
    }

    let work_dir_root = resolve_work_dir(cli.work_dir.clone())?;
    let work_dir = git::repo_work_dir(&work_dir_root, &repo_root);
    std::fs::create_dir_all(&work_dir)?;
    git::sweep_stale_worktrees(&repo_root, &work_dir)?;
    runner::check_governor();

    let base_wt = git::WorktreeGuard::create(&repo_root, &work_dir, &base, cli.keep_worktrees)?;
    let candidate_wt =
        git::WorktreeGuard::create(&repo_root, &work_dir, &candidate, cli.keep_worktrees)?;

    let base_ws = workspace.worktree_ws_root(&base_wt.path);
    let candidate_ws = workspace.worktree_ws_root(&candidate_wt.path);

    let pin = runner::pin_prefix(cli.runs_on_core, cli.no_pin);
    let pinned_core = if pin.is_empty() {
        None
    } else {
        Some(cli.runs_on_core)
    };
    let pinned_label = pinned_core
        .map(|core| format!("core {core} (taskset)"))
        .unwrap_or_else(|| "disabled".to_owned());

    let (results, only_in_base, only_in_candidate, mode_name, mode_label, metric_label, reps_label) =
        match &mode {
            Mode::Binary { bin, args, metric } => {
                let reps = cli.reps.unwrap_or(5);
                let exe_base = builder::build_bin(&base_ws, package, bin, &cli.profile)?;
                check_cancelled()?;
                let exe_candidate = builder::build_bin(&candidate_ws, package, bin, &cli.profile)?;
                check_cancelled()?;
                let (base_values, candidate_values, unit, lower_is_better) =
                    runner::run_binary_interleaved(
                        runner::BinaryRun {
                            exe: &exe_base,
                            args,
                            cwd: &base_ws,
                            pin: &pin,
                        },
                        runner::BinaryRun {
                            exe: &exe_candidate,
                            args,
                            cwd: &candidate_ws,
                            pin: &pin,
                        },
                        reps,
                        metric,
                        &CANCELLED,
                    )?;
                let comparison = stats::compare(
                    bin.clone(),
                    unit,
                    lower_is_better,
                    summarize(&base_values),
                    summarize(&candidate_values),
                );
                (
                    vec![comparison],
                    Vec::new(),
                    Vec::new(),
                    "binary".to_owned(),
                    format!("binary '{bin}'"),
                    Some(report::metric_label(metric)),
                    reps.to_string(),
                )
            }
            Mode::Criterion { bench } => {
                if cli.reps.is_some() {
                    eprintln!(
                        "warning: --reps is ignored in criterion mode (criterion samples internally)"
                    );
                }
                builder::build_bench(&base_ws, package, bench, &cli.profile, cli.json)?;
                check_cancelled()?;
                builder::build_bench(&candidate_ws, package, bench, &cli.profile, cli.json)?;
                check_cancelled()?;
                runner::run_criterion(
                    &base_ws,
                    package,
                    bench,
                    &cli.profile,
                    &base.short,
                    &pin,
                    cli.json,
                )?;
                check_cancelled()?;
                runner::run_criterion(
                    &candidate_ws,
                    package,
                    bench,
                    &cli.profile,
                    &candidate.short,
                    &pin,
                    cli.json,
                )?;
                check_cancelled()?;

                let base_results = criterion::collect(
                    &builder::target_dir(&base_ws),
                    &format!("bcmp-{}", base.short),
                )?;
                let candidate_results = criterion::collect(
                    &builder::target_dir(&candidate_ws),
                    &format!("bcmp-{}", candidate.short),
                )?;
                let (comparisons, only_base, only_candidate) =
                    compare_criterion(base_results, candidate_results);
                (
                    comparisons,
                    only_base,
                    only_candidate,
                    "criterion".to_owned(),
                    format!("criterion bench '{bench}'"),
                    None,
                    "criterion-internal".to_owned(),
                )
            }
        };

    if cli.json {
        report::print_json(report::JsonReportInput {
            mode: &mode_name,
            package,
            profile: &cli.profile,
            base: &base,
            candidate: &candidate,
            pinned_core,
            dirty,
            results: &results,
            only_in_base: &only_in_base,
            only_in_candidate: &only_in_candidate,
        })?;
    } else {
        report::print_human(report::HumanReport {
            package,
            profile: &cli.profile,
            mode_label,
            metric_label,
            reps_label,
            pinned_label,
            base: &base,
            candidate: &candidate,
            dirty,
            results: &results,
            only_in_base: &only_in_base,
            only_in_candidate: &only_in_candidate,
        });
    }

    Ok(())
}

fn resolve_work_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(cache).join("cargo-bench-compare"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is unset and --work-dir was not provided"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("cargo-bench-compare"))
}

fn check_cancelled() -> Result<()> {
    if CANCELLED.load(Ordering::SeqCst) {
        Err(anyhow!("interrupted"))
    } else {
        Ok(())
    }
}

fn compare_criterion(
    base: Vec<criterion::CriterionResult>,
    candidate: Vec<criterion::CriterionResult>,
) -> (Vec<stats::Comparison>, Vec<String>, Vec<String>) {
    let base_map = base
        .into_iter()
        .map(|r| (r.full_id.clone(), r))
        .collect::<BTreeMap<_, _>>();
    let candidate_map = candidate
        .into_iter()
        .map(|r| (r.full_id.clone(), r))
        .collect::<BTreeMap<_, _>>();
    let base_ids = base_map.keys().cloned().collect::<BTreeSet<_>>();
    let candidate_ids = candidate_map.keys().cloned().collect::<BTreeSet<_>>();
    let mut comparisons = Vec::new();
    for id in base_ids.intersection(&candidate_ids) {
        let b = &base_map[id];
        let c = &candidate_map[id];
        comparisons.push(stats::compare(
            id.clone(),
            "ns".to_owned(),
            true,
            Summary {
                n: 1,
                mean: b.mean_ns,
                std_dev: b.std_dev_ns,
                min: b.mean_ns,
                max: b.mean_ns,
            },
            Summary {
                n: 1,
                mean: c.mean_ns,
                std_dev: c.std_dev_ns,
                min: c.mean_ns,
                max: c.mean_ns,
            },
        ));
    }
    let only_base = base_ids.difference(&candidate_ids).cloned().collect();
    let only_candidate = candidate_ids.difference(&base_ids).cloned().collect();
    (comparisons, only_base, only_candidate)
}
