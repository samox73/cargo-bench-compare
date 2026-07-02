use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct ResolvedRev {
    pub spec: String,
    pub sha: String,
    pub short: String,
}

pub fn command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

pub fn output_error(program: &str, args: &[String], output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow!(
        "command failed: {}\nstatus: {}\nstderr (last 20 lines):\n{}",
        command_line(program, args),
        output.status,
        tail_lines(&stderr, 20)
    )
}

pub fn run_capture(
    program: &str,
    args: &[&str],
    dir: &Path,
    envs: &[(&str, &OsStr)],
) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().map(|(k, v)| (*k, *v)))
        .output()
        .with_context(|| format!("failed to run {}", program))?;
    if !output.status.success() {
        let owned = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        return Err(output_error(program, &owned, &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    Ok(PathBuf::from(
        run_capture("git", &["rev-parse", "--show-toplevel"], &cwd, &[])?.trim(),
    ))
}

pub fn resolve_rev(repo_root: &Path, spec: &str) -> Result<ResolvedRev> {
    let verify = format!("{spec}^{{commit}}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &verify])
        .current_dir(repo_root)
        .output()
        .with_context(|| "failed to run git rev-parse")?;
    if !output.status.success() {
        return Err(anyhow!(
            "revision '{spec}' not found in {} (tried `git rev-parse {spec}^{{commit}}`)",
            repo_root.display()
        ));
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let short = run_capture("git", &["rev-parse", "--short", &sha], repo_root, &[])?
        .trim()
        .to_owned();
    Ok(ResolvedRev {
        spec: spec.to_owned(),
        sha,
        short,
    })
}

pub fn is_dirty(repo_root: &Path) -> Result<bool> {
    Ok(
        !run_capture("git", &["status", "--porcelain"], repo_root, &[])?
            .trim()
            .is_empty(),
    )
}

pub struct WorktreeGuard {
    pub path: PathBuf,
    repo_root: PathBuf,
    keep: bool,
}

impl WorktreeGuard {
    pub fn create(
        repo_root: &Path,
        work_dir: &Path,
        rev: &ResolvedRev,
        keep: bool,
    ) -> Result<WorktreeGuard> {
        let path = work_dir.join(format!("bcmp-{}-{}", rev.short, std::process::id()));
        let repo = repo_root.display().to_string();
        let wt = path.display().to_string();
        run_capture(
            "git",
            &["-C", &repo, "worktree", "add", "--detach", &wt, &rev.sha],
            repo_root,
            &[],
        )?;

        if repo_root.join(".gitmodules").exists() {
            run_capture(
                "git",
                &["-C", &wt, "submodule", "update", "--init", "--recursive"],
                &path,
                &[],
            )?;
        } else {
            let _ = run_capture(
                "git",
                &["-C", &wt, "submodule", "update", "--init", "--recursive"],
                &path,
                &[],
            );
        }

        Ok(WorktreeGuard {
            path,
            repo_root: repo_root.to_owned(),
            keep,
        })
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        if self.keep {
            eprintln!("kept worktree: {}", self.path.display());
            return;
        }
        let repo = self.repo_root.display().to_string();
        let wt = self.path.display().to_string();
        let removed = run_capture(
            "git",
            &["-C", &repo, "worktree", "remove", "--force", &wt],
            &self.repo_root,
            &[],
        );
        if let Err(err) = removed {
            eprintln!(
                "warning: failed to remove worktree {} via git: {err:#}",
                self.path.display()
            );
            if let Err(fs_err) = std::fs::remove_dir_all(&self.path) {
                eprintln!(
                    "warning: failed to remove worktree dir {}: {fs_err}",
                    self.path.display()
                );
            }
            let _ = run_capture(
                "git",
                &["-C", &repo, "worktree", "prune"],
                &self.repo_root,
                &[],
            );
        }
    }
}

pub fn sweep_stale_worktrees(repo_root: &Path, work_dir: &Path) -> Result<()> {
    let repo = repo_root.display().to_string();
    let _ = run_capture("git", &["-C", &repo, "worktree", "prune"], repo_root, &[]);
    let list = run_capture(
        "git",
        &["-C", &repo, "worktree", "list", "--porcelain"],
        repo_root,
        &[],
    )?;
    let active = list
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect::<std::collections::HashSet<_>>();

    for entry in std::fs::read_dir(work_dir)? {
        let entry = entry?;
        let path = entry.path();
        let starts_bcmp = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("bcmp-"));
        if starts_bcmp && !active.contains(&path) {
            std::fs::remove_dir_all(&path).with_context(|| {
                format!("failed to remove stale worktree dir {}", path.display())
            })?;
            eprintln!("removed stale worktree dir: {}", path.display());
        }
    }
    Ok(())
}
