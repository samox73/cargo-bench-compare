mod builder;
mod candidates;
mod clean;
mod cli;
mod completions;
mod criterion;
mod git;
mod governor;
mod progress;
mod report;
mod runner;
mod stats;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};

use crate::cli::{CacheSub, Cli, Mode, Sub};
use crate::git::Cleanup;
use crate::stats::{Summary, summarize};

static CANCELLED: AtomicBool = AtomicBool::new(false);

fn main() {
    if let Err(err) = real_main() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let started = Instant::now();
    let cli = Cli::parse_from_env();
    match &cli.command {
        Some(Sub::Completions(args)) => return completions::run(args),
        Some(Sub::Cache(args)) => {
            let work_dir_root = resolve_work_dir(cli.work_dir.clone())?;
            return match &args.command {
                CacheSub::List => clean::list(&work_dir_root),
                CacheSub::Clean(args) => clean::run(args.all, &work_dir_root),
            };
        }
        Some(Sub::Candidates { kind }) => return candidates::print(*kind),
        None => {}
    }
    let mode = cli.mode()?;
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
    if cli.set_governor && !cfg!(target_os = "linux") {
        return Err(anyhow!("--set-governor is only supported on Linux"));
    }

    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::SeqCst))?;

    let repo_root = git::repo_root()?;
    let cwd = std::env::current_dir()?;
    let (target_kind, target_name) = match &mode {
        Mode::Binary { bin, .. } => (workspace::TargetKind::Bin, bin.as_str()),
        Mode::Criterion { bench } => (workspace::TargetKind::Bench, bench.as_str()),
    };
    let workspace = workspace::load(
        &repo_root,
        &cwd,
        cli.package.as_deref(),
        target_kind,
        target_name,
    )?;
    let package = workspace.package.as_str();
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
    // benchmarking the dirty worktree via :worktree is the primary workflow and
    // needs no note; only warn when local changes end up in neither side
    if dirty && base.spec != "worktree" && candidate.spec != "worktree" {
        eprintln!(
            "warning: working tree is dirty; uncommitted local changes are NOT included in either side"
        );
    }

    let manifest_rel = workspace.ws_rel.join("Cargo.toml");
    git::verify_path_in_rev(&repo_root, &base, "base", "--rev-base", &manifest_rel)?;
    git::verify_path_in_rev(&repo_root, &candidate, "candidate", "--rev", &manifest_rel)?;

    let work_dir_root = resolve_work_dir(cli.work_dir.clone())?;
    let work_dir = git::repo_work_dir(&work_dir_root, &repo_root);
    std::fs::create_dir_all(&work_dir)?;
    let repo_path = std::fs::canonicalize(&repo_root).unwrap_or_else(|_| repo_root.clone());
    std::fs::write(
        work_dir.join("repo-path.txt"),
        format!("{}\n", repo_path.display()),
    )?;
    git::sweep_stale_worktrees(&repo_root, &work_dir)?;

    let pin = runner::pin_prefix(cli.runs_on_core, cli.no_pin);
    let pinned_core = if pin.is_empty() {
        None
    } else {
        Some(cli.runs_on_core)
    };
    if cfg!(target_os = "linux")
        && let Some(core) = pinned_core
    {
        governor::validate_core(Path::new(governor::SYSFS_CPU), core)?;
    }

    let mut _governor_guard = None;
    let mut governor_set_by_tool = false;
    if cli.set_governor {
        match pinned_core {
            None => eprintln!(
                "warning: --set-governor skipped: run is not pinned (taskset unavailable)"
            ),
            Some(core) => match governor::set_performance(Path::new(governor::SYSFS_CPU), core)? {
                governor::SetOutcome::Changed(g) => {
                    eprintln!(
                        "set CPU governor on core {core}: {} -> performance (restored on exit)",
                        g.previous()
                    );
                    governor_set_by_tool = true;
                    _governor_guard = Some(g);
                }
                governor::SetOutcome::AlreadyPerformance => {}
                governor::SetOutcome::Skipped(reason) => {
                    eprintln!("warning: --set-governor skipped: {reason}");
                }
            },
        }
    }
    if cfg!(target_os = "linux")
        && let Some(w) = governor::governor_warning(Path::new(governor::SYSFS_CPU), pinned_core)
    {
        eprintln!("warning: {w}");
    }
    let governor =
        pinned_core.and_then(|core| governor::governor_of(Path::new(governor::SYSFS_CPU), core));

    let (base_wt, candidate_wt, _lock) = if cli.cold {
        let cleanup = if cli.keep_worktrees {
            Cleanup::KeepAnnounce
        } else {
            Cleanup::Remove
        };
        let base_wt = git::WorktreeGuard::create(&repo_root, &work_dir, &base, cleanup)?;
        let cleanup = if cli.keep_worktrees {
            Cleanup::KeepAnnounce
        } else {
            Cleanup::Remove
        };
        let candidate_wt = git::WorktreeGuard::create(&repo_root, &work_dir, &candidate, cleanup)?;
        (base_wt, candidate_wt, None)
    } else {
        let lock = git::RepoLock::acquire(&work_dir)?;
        let base_path = work_dir.join("warm-base");
        let candidate_path = work_dir.join("warm-candidate");
        git::prepare_warm_worktree(&repo_root, &base_path, &base)?;
        git::prepare_warm_worktree(&repo_root, &candidate_path, &candidate)?;
        (
            git::WorktreeGuard::adopt(base_path),
            git::WorktreeGuard::adopt(candidate_path),
            Some(lock),
        )
    };

    let base_ws = workspace.worktree_ws_root(&base_wt.path);
    let candidate_ws = workspace.worktree_ws_root(&candidate_wt.path);
    let build_mode = build_mode(cli.cold, &base_ws, &candidate_ws);

    let pinned_label = pinned_core
        .map(|core| format!("core {core} (taskset)"))
        .unwrap_or_else(|| "disabled".to_owned());

    let infer_hint: Option<String> = cli.package.is_none().then(|| match &mode {
        Mode::Binary { bin, .. } => format!(
            "package '{package}' was inferred from --bin '{bin}' (working-tree metadata); \
             if it is wrong for this revision, pass -p <package> explicitly"
        ),
        Mode::Criterion { bench } => format!(
            "package '{package}' was inferred from --bench '{bench}' (working-tree metadata); \
             if it is wrong for this revision, pass -p <package> explicitly"
        ),
    });

    let mut args_label = None;
    let (results, only_in_base, only_in_candidate, mode_name, mode_label, metric_label, reps_label) =
        match &mode {
            Mode::Binary {
                bin,
                args,
                metric,
                progress: progress_pattern,
            } => {
                let reps = cli.reps.unwrap_or(5);
                let status = |side| (!cli.no_progress).then_some(side);
                let exe_base = apply_infer_hint(
                    builder::build_bin(
                        &base_ws,
                        package,
                        bin,
                        &cli.profile,
                        status(progress::Side::Base),
                    ),
                    infer_hint.as_deref(),
                )?;
                check_cancelled()?;
                let exe_candidate = apply_infer_hint(
                    builder::build_bin(
                        &candidate_ws,
                        package,
                        bin,
                        &cli.profile,
                        status(progress::Side::Candidate),
                    ),
                    infer_hint.as_deref(),
                )?;
                check_cancelled()?;
                let progress = progress::Progress::new(!cli.no_progress);
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
                        progress_pattern.as_ref(),
                        &progress,
                        &CANCELLED,
                    )?;
                let comparison = stats::compare(
                    bin.clone(),
                    unit,
                    lower_is_better,
                    summarize(&base_values),
                    summarize(&candidate_values),
                );
                args_label = (!args.is_empty()).then(|| args.join(" "));
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
                let status = |side| (!cli.no_progress).then_some(side);
                apply_infer_hint(
                    builder::build_bench(
                        &base_ws,
                        package,
                        bench,
                        &cli.profile,
                        cli.json,
                        status(progress::Side::Base),
                    ),
                    infer_hint.as_deref(),
                )?;
                check_cancelled()?;
                apply_infer_hint(
                    builder::build_bench(
                        &candidate_ws,
                        package,
                        bench,
                        &cli.profile,
                        cli.json,
                        status(progress::Side::Candidate),
                    ),
                    infer_hint.as_deref(),
                )?;
                check_cancelled()?;
                let base_label = format!("bcmp-{}", base.short);
                let candidate_label = format!("bcmp-{}", candidate.short);
                criterion::remove_stale_baselines(&builder::target_dir(&base_ws), &base_label)?;
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
                criterion::remove_stale_baselines(
                    &builder::target_dir(&candidate_ws),
                    &candidate_label,
                )?;
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

                let base_results = criterion::collect(&builder::target_dir(&base_ws), &base_label)?;
                let candidate_results =
                    criterion::collect(&builder::target_dir(&candidate_ws), &candidate_label)?;
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
    let total_runtime = started.elapsed();

    if cli.json {
        report::print_json(report::JsonReportInput {
            mode: &mode_name,
            package,
            profile: &cli.profile,
            base: &base,
            candidate: &candidate,
            build: build_mode,
            total_runtime,
            pinned_core,
            governor: governor.as_deref(),
            governor_set_by_tool,
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
            args_label,
            reps_label,
            pinned_label,
            governor,
            governor_set_by_tool,
            base: &base,
            candidate: &candidate,
            build: build_mode,
            total_runtime,
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

fn build_mode(cold: bool, base_ws: &Path, candidate_ws: &Path) -> &'static str {
    if cold || !builder::target_dir(base_ws).exists() || !builder::target_dir(candidate_ws).exists()
    {
        "cold"
    } else {
        "warm"
    }
}

fn apply_infer_hint<T>(result: Result<T>, hint: Option<&str>) -> Result<T> {
    match hint {
        Some(h) => result.context(h.to_owned()),
        None => result,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_mode_is_cold_without_both_cached_targets() {
        let root = std::env::temp_dir().join(format!(
            "bcmp-build-mode-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("base");
        let candidate = root.join("candidate");

        assert_eq!(build_mode(false, &base, &candidate), "cold");
        std::fs::create_dir_all(builder::target_dir(&base)).unwrap();
        assert_eq!(build_mode(false, &base, &candidate), "cold");
        std::fs::create_dir_all(builder::target_dir(&candidate)).unwrap();
        assert_eq!(build_mode(false, &base, &candidate), "warm");
        assert_eq!(build_mode(true, &base, &candidate), "cold");

        let _ = std::fs::remove_dir_all(root);
    }
}
