use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::git;

pub fn run(all: bool, work_dir_root: &Path) -> Result<()> {
    if all {
        if !work_dir_root.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(work_dir_root)
            .with_context(|| format!("failed to read {}", work_dir_root.display()))?
        {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                clean_dir(&entry.path())?;
            }
        }
        return Ok(());
    }

    let repo_root = git::repo_root()?;
    let dir = git::repo_work_dir(work_dir_root, &repo_root);
    if !dir.exists() {
        return Err(anyhow!("nothing cached for this repo"));
    }
    clean_dir(&dir)
}

fn clean_dir(dir: &Path) -> Result<()> {
    let lock = dir.join("warm.lock");
    if git::lock_holder_alive(&lock) {
        return Err(anyhow!(
            "another cargo-bench-compare run is active on {}; wait for it, or remove {} if the pid is wrong",
            dir.display(),
            lock.display()
        ));
    }

    let repo = std::fs::read_to_string(dir.join("repo-path.txt"))
        .ok()
        .map(|s| PathBuf::from(s.trim()));
    if let Some(repo) = repo.as_deref().filter(|p| p.exists()) {
        remove_registered_worktrees(repo, dir);
    }

    let size = dir_size(dir)?;
    std::fs::remove_dir_all(dir).with_context(|| format!("failed to remove {}", dir.display()))?;

    if let Some(repo) = repo.as_deref().filter(|p| p.exists()) {
        let _ = Command::new("git")
            .args(["-C", &repo.display().to_string(), "worktree", "prune"])
            .status();
    }

    println!("removed {} ({} freed)", dir.display(), format_size(size));
    Ok(())
}

fn remove_registered_worktrees(repo: &Path, dir: &Path) {
    let output = Command::new("git")
        .args([
            "-C",
            &repo.display().to_string(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output();
    let Ok(output) = output else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let list = String::from_utf8_lossy(&output.stdout);
    for wt in list
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        let wt = PathBuf::from(wt);
        if wt.starts_with(dir) {
            let _ = Command::new("git")
                .args([
                    "-C",
                    &repo.display().to_string(),
                    "worktree",
                    "remove",
                    "--force",
                    &wt.display().to_string(),
                ])
                .status();
        }
    }
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut size = 0;
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            size += dir_size(&entry.path())?;
        } else {
            size += meta.len();
        }
    }
    Ok(size)
}

fn format_size(bytes: u64) -> String {
    let mib = bytes as f64 / 1024.0 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.1} GiB", mib / 1024.0)
    } else {
        format!("{mib:.1} MiB")
    }
}
