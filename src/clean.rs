use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::git;

pub fn list(work_dir_root: &Path) -> Result<()> {
    if !work_dir_root.exists() {
        return Ok(());
    }

    let mut rows = Vec::new();
    for entry in std::fs::read_dir(work_dir_root)
        .with_context(|| format!("failed to read {}", work_dir_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        let repo = std::fs::read_to_string(dir.join("repo-path.txt"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_owned());
        rows.push((
            format_size(dir_size(&dir)?),
            dir.display().to_string(),
            repo,
        ));
    }
    for line in cache_list_lines(&rows) {
        println!("{line}");
    }
    Ok(())
}

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
    let size = clean_dir_inner(dir)?;
    println!("removed {} ({} freed)", dir.display(), format_size(size));
    Ok(())
}

fn clean_dir_inner(dir: &Path) -> Result<u64> {
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
    let size = dir_size(dir)?;
    if let Some(repo) = repo.as_deref().filter(|p| p.exists()) {
        remove_registered_worktrees(repo, dir);
    }

    std::fs::remove_dir_all(dir).with_context(|| format!("failed to remove {}", dir.display()))?;

    if let Some(repo) = repo.as_deref().filter(|p| p.exists()) {
        let _ = Command::new("git")
            .args(["-C", &repo.display().to_string(), "worktree", "prune"])
            .status();
    }

    Ok(size)
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

fn cache_list_lines(rows: &[(String, String, String)]) -> Vec<String> {
    let size_w = rows
        .iter()
        .map(|(size, _, _)| size.len())
        .max()
        .unwrap_or(0)
        .max("SIZE".len());
    let cache_w = rows
        .iter()
        .map(|(_, cache, _)| cache.len())
        .max()
        .unwrap_or(0)
        .max("CACHE".len());

    let mut lines = vec![format!("{:>size_w$}  {:<cache_w$}  REPO", "SIZE", "CACHE")];
    lines.extend(
        rows.iter()
            .map(|(size, cache, repo)| format!("{size:>size_w$}  {cache:<cache_w$}  {repo}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{cache_list_lines, clean_dir_inner};

    #[test]
    fn cache_list_aligns_columns_without_tabs() {
        let rows = vec![
            ("0.0 MiB".to_owned(), "/short".to_owned(), "-".to_owned()),
            (
                "395.9 MiB".to_owned(),
                "/much/longer/cache/path".to_owned(),
                "/repo".to_owned(),
            ),
        ];

        let lines = cache_list_lines(&rows);

        assert_eq!(
            lines,
            [
                "     SIZE  CACHE                    REPO",
                "  0.0 MiB  /short                   -",
                "395.9 MiB  /much/longer/cache/path  /repo",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains('\t')));
    }

    #[test]
    fn clean_reports_size_before_removing_git_worktrees() {
        let root = temp_path("bcmp-clean-size");
        let _cleanup = TempDir(root.clone());
        let repo = root.join("repo");
        let cache = root.join("cache");
        let wt = cache.join("warm-base");

        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("repo-path.txt"), format!("{}\n", repo.display())).unwrap();
        git(
            &repo,
            &["worktree", "add", "--detach", wt.to_str().unwrap(), "HEAD"],
        );
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target/blob"), vec![0; 1024 * 1024]).unwrap();

        let size = clean_dir_inner(&cache).unwrap();

        assert!(size >= 1024 * 1024);
        assert!(!cache.exists());
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=cargo-bench-compare"])
            .args(["-c", "user.email=bcmp@invalid"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    fn temp_path(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
