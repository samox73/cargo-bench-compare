use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// Resolve a revision spec, including the tool's sentinels:
///   ":worktree"   -> snapshot commit of the working tree (HEAD if the tree is clean)
///   ":merge-base" -> merge base of HEAD and the detected default branch
/// Anything else is passed to `resolve_rev` unchanged.
pub fn resolve_spec(repo_root: &Path, spec: &str) -> Result<ResolvedRev> {
    match spec {
        ":worktree" => snapshot_worktree(repo_root),
        ":merge-base" => merge_base_with_default_branch(repo_root),
        other => resolve_rev(repo_root, other),
    }
}

/// If the tree is clean, resolve HEAD (spec label "HEAD").
/// If dirty, create a dangling snapshot commit of the working tree WITHOUT
/// touching the user's index, stash, refs, or working tree, and return it with
/// spec label "worktree".
fn snapshot_worktree(repo_root: &Path) -> Result<ResolvedRev> {
    if !is_dirty(repo_root)? {
        return resolve_rev(repo_root, "HEAD");
    }

    let real_index = run_capture("git", &["rev-parse", "--git-path", "index"], repo_root, &[])?;
    let real_index = PathBuf::from(real_index.trim());
    let real_index = if real_index.is_absolute() {
        real_index
    } else {
        repo_root.join(real_index)
    };
    let temp_index = std::env::temp_dir().join(format!("bcmp-index-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp_index);
    if real_index.exists() {
        std::fs::copy(&real_index, &temp_index).with_context(|| {
            format!(
                "failed to copy git index {} to {}",
                real_index.display(),
                temp_index.display()
            )
        })?;
    }

    let snapshot = (|| {
        let envs = [("GIT_INDEX_FILE", temp_index.as_os_str())];
        run_capture("git", &["add", "-A"], repo_root, &envs)?;
        let tree_sha = run_capture("git", &["write-tree"], repo_root, &envs)?
            .trim()
            .to_owned();
        let head_sha = run_capture("git", &["rev-parse", "HEAD"], repo_root, &[])?
            .trim()
            .to_owned();
        // The commit is dangling: no refs, stash entries, index changes, or worktree edits.
        // Git's default pruning window keeps it around long enough for this run.
        let snap_sha = run_capture(
            "git",
            &[
                "-c",
                "user.name=cargo-bench-compare",
                "-c",
                "user.email=bcmp@invalid",
                "commit-tree",
                &tree_sha,
                "-p",
                &head_sha,
                "-m",
                "cargo-bench-compare: working tree snapshot",
            ],
            repo_root,
            &[],
        )?
        .trim()
        .to_owned();
        let short = run_capture("git", &["rev-parse", "--short", &snap_sha], repo_root, &[])?
            .trim()
            .to_owned();
        Ok(ResolvedRev {
            spec: "worktree".to_owned(),
            sha: snap_sha,
            short,
        })
    })();

    let _ = std::fs::remove_file(&temp_index);
    snapshot
}

/// Merge base of HEAD and the detected default branch (see default_base()).
/// Spec label: "merge-base(<branch>)".
fn merge_base_with_default_branch(repo_root: &Path) -> Result<ResolvedRev> {
    let branch = default_base(repo_root)?;
    let sha = run_capture("git", &["merge-base", "HEAD", &branch], repo_root, &[])
        .map_err(|_| {
            anyhow!(
                "no merge base between HEAD and '{branch}' (unrelated histories or shallow clone?); pass --rev-base <REV>"
            )
        })?
        .trim()
        .to_owned();
    let short = run_capture("git", &["rev-parse", "--short", &sha], repo_root, &[])?
        .trim()
        .to_owned();
    Ok(ResolvedRev {
        spec: format!("merge-base({branch})"),
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
    cleanup: Cleanup,
}

pub enum Cleanup {
    Remove,
    KeepAnnounce,
    KeepQuiet,
}

impl WorktreeGuard {
    pub fn create(
        repo_root: &Path,
        work_dir: &Path,
        rev: &ResolvedRev,
        cleanup: Cleanup,
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

        init_submodules(&path)?;

        Ok(WorktreeGuard {
            path,
            repo_root: repo_root.to_owned(),
            cleanup,
        })
    }

    pub fn adopt(path: PathBuf) -> WorktreeGuard {
        WorktreeGuard {
            path,
            repo_root: PathBuf::new(),
            cleanup: Cleanup::KeepQuiet,
        }
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        match self.cleanup {
            Cleanup::KeepQuiet => return,
            Cleanup::KeepAnnounce => {
                eprintln!("kept worktree: {}", self.path.display());
                return;
            }
            Cleanup::Remove => {}
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

/// Ensure a persistent worktree at `path` checked out at `rev.sha`, preserving
/// mtimes of unchanged files (this is what keeps cargo builds warm — never
/// delete-and-recreate a healthy worktree).
pub fn prepare_warm_worktree(repo_root: &Path, path: &Path, rev: &ResolvedRev) -> Result<()> {
    let path_arg = path.display().to_string();
    let healthy = path.exists()
        && Command::new("git")
            .args(["-C", &path_arg, "rev-parse", "--git-dir"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());

    if healthy {
        run_capture(
            "git",
            &["-C", &path_arg, "checkout", "--detach", "--force", &rev.sha],
            path,
            &[],
        )?;
    } else {
        let _ = std::fs::remove_dir_all(path);
        let repo = repo_root.display().to_string();
        let _ = run_capture("git", &["-C", &repo, "worktree", "prune"], repo_root, &[]);
        run_capture(
            "git",
            &[
                "-C", &repo, "worktree", "add", "--detach", &path_arg, &rev.sha,
            ],
            repo_root,
            &[],
        )?;
    }

    run_capture(
        "git",
        &["-C", &path_arg, "clean", "-fd", "-e", "target"],
        path,
        &[],
    )?;
    init_submodules(path)?;
    let head = run_capture("git", &["-C", &path_arg, "rev-parse", "HEAD"], path, &[])?;
    if head.trim() != rev.sha {
        return Err(anyhow!(
            "warm worktree {} failed to check out {}",
            path.display(),
            rev.sha
        ));
    }
    Ok(())
}

fn init_submodules(worktree: &Path) -> Result<()> {
    let wt = worktree.display().to_string();
    let submodule_update = run_capture(
        "git",
        &["-C", &wt, "submodule", "update", "--init", "--recursive"],
        worktree,
        &[],
    );
    if worktree.join(".gitmodules").exists() {
        submodule_update?;
    } else {
        let _ = submodule_update;
    }
    Ok(())
}

pub struct RepoLock {
    path: PathBuf,
}

impl RepoLock {
    /// Create `<repo_dir>/warm.lock` with O_EXCL containing our pid.
    pub fn acquire(repo_dir: &Path) -> Result<RepoLock> {
        let path = repo_dir.join("warm.lock");
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(RepoLock { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let pid = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid = pid.trim().to_owned();
                    if !lock_holder_alive(&path) && attempt == 0 {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return Err(anyhow!(
                        "another cargo-bench-compare run (pid {pid}) is active on this repo; wait for it, or use --cold, or remove {} if the pid is wrong",
                        path.display()
                    ));
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        unreachable!()
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn lock_holder_alive(lock: &Path) -> bool {
    if !lock.exists() {
        return false;
    }
    if !cfg!(target_os = "linux") {
        return true;
    }
    let pid = std::fs::read_to_string(lock).unwrap_or_default();
    Path::new("/proc").join(pid.trim()).exists()
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
        .map(canonical_or_self)
        .collect::<std::collections::HashSet<_>>();

    for entry in std::fs::read_dir(work_dir)? {
        let entry = entry?;
        let path = entry.path();
        let canonical_path = canonical_or_self(path.clone());
        let starts_bcmp = path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("bcmp-"));
        if starts_bcmp && !active.contains(&canonical_path) {
            std::fs::remove_dir_all(&path).with_context(|| {
                format!("failed to remove stale worktree dir {}", path.display())
            })?;
            eprintln!("removed stale worktree dir: {}", path.display());
        }
    }
    Ok(())
}

pub fn repo_work_dir(work_dir: &Path, repo_root: &Path) -> PathBuf {
    let canonical_root = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_owned());
    work_dir.join(repo_key(&canonical_root))
}

fn repo_key(repo_root: &Path) -> String {
    let root = repo_root.display().to_string();
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let hash = hasher.finish();
    let safe_root = root.replace(std::path::MAIN_SEPARATOR, "-");
    format!("{safe_root}-{hash:08x}", hash = hash & 0xffff_ffff)
}

fn canonical_or_self(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

/// Default base revision: the branch origin/HEAD points at (preferring the local
/// branch of that name), falling back to a local `main`, then `master`.
pub fn default_base(repo_root: &Path) -> Result<String> {
    if let Ok(sym) = run_capture(
        "git",
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        repo_root,
        &[],
    ) {
        let sym = sym.trim();
        if let Some(name) = sym.strip_prefix("refs/remotes/origin/") {
            if local_branch_exists(repo_root, name)? {
                return Ok(name.to_owned());
            }
            if let Some(remote_ref) = sym.strip_prefix("refs/remotes/") {
                return Ok(remote_ref.to_owned());
            }
        }
    }
    for name in ["main", "master"] {
        if local_branch_exists(repo_root, name)? {
            return Ok(name.to_owned());
        }
    }
    Err(anyhow!(
        "could not determine a default base revision (no origin/HEAD, no local 'main' or 'master'); pass --rev-base <REV>"
    ))
}

fn local_branch_exists(repo_root: &Path, name: &str) -> Result<bool> {
    Ok(Command::new("git")
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ])
        .current_dir(repo_root)
        .status()
        .with_context(|| "failed to run git show-ref")?
        .success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_work_dir_namespaces_repos_under_shared_root() {
        let cache = Path::new("/tmp/cargo-bench-compare");
        let first = repo_work_dir(cache, Path::new("/tmp/one/project"));
        let second = repo_work_dir(cache, Path::new("/tmp/two/project"));

        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(cache));
        assert_eq!(second.parent(), Some(cache));
    }

    #[test]
    fn canonical_or_self_keeps_missing_path() {
        let path = PathBuf::from("/tmp/cargo-bench-compare-missing-path-for-test");
        assert_eq!(canonical_or_self(path.clone()), path);
    }

    #[test]
    fn default_base_falls_back_to_local_main_then_master() {
        for (branch, expected) in [
            ("main", Some("main")),
            ("master", Some("master")),
            ("trunk", None),
        ] {
            let dir = init_repo("bcmp-default-base", branch);
            let _cleanup = TempDir(dir.clone());
            match expected {
                Some(name) => assert_eq!(default_base(&dir).unwrap(), name),
                None => assert!(default_base(&dir).is_err()),
            }
        }
    }

    #[test]
    fn snapshot_includes_unstaged_and_untracked_and_respects_gitignore() {
        let dir = init_repo("bcmp-snapshot-dirty", "main");
        let _cleanup = TempDir(dir.clone());
        std::fs::write(dir.join("tracked.txt"), "old").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        git_in(&dir, &["add", "-A"]);
        git_in(&dir, &["commit", "-qm", "tracked"]);

        std::fs::write(dir.join("tracked.txt"), "new").unwrap();
        std::fs::write(dir.join("untracked.txt"), "untracked").unwrap();
        std::fs::write(dir.join("ignored.txt"), "ignored").unwrap();
        let status_before = git_in(&dir, &["status", "--porcelain"]);
        let head_before = git_in(&dir, &["rev-parse", "HEAD"]);

        let snapshot = resolve_spec(&dir, ":worktree").unwrap();

        assert_eq!(snapshot.spec, "worktree");
        assert_eq!(
            git_in(&dir, &["show", &format!("{}:tracked.txt", snapshot.sha)]),
            "new"
        );
        assert_eq!(
            git_in(&dir, &["show", &format!("{}:untracked.txt", snapshot.sha)]),
            "untracked"
        );
        assert!(!git_status(
            &dir,
            &["show", &format!("{}:ignored.txt", snapshot.sha)]
        ));
        assert_eq!(git_in(&dir, &["status", "--porcelain"]), status_before);
        assert_eq!(git_in(&dir, &["stash", "list"]), "");
        assert_eq!(git_in(&dir, &["rev-parse", "HEAD"]), head_before);
    }

    #[test]
    fn snapshot_on_clean_tree_is_head() {
        let dir = init_repo("bcmp-snapshot-clean", "main");
        let _cleanup = TempDir(dir.clone());

        let snapshot = resolve_spec(&dir, ":worktree").unwrap();

        assert_eq!(snapshot.spec, "HEAD");
        assert_eq!(snapshot.sha, git_in(&dir, &["rev-parse", "HEAD"]));
    }

    #[test]
    fn merge_base_resolves_fork_point() {
        let dir = init_repo("bcmp-merge-base", "main");
        let _cleanup = TempDir(dir.clone());
        let a = git_in(&dir, &["rev-parse", "HEAD"]);
        git_in(&dir, &["switch", "-c", "feat"]);
        git_in(&dir, &["commit", "--allow-empty", "-qm", "B"]);
        git_in(&dir, &["switch", "main"]);
        git_in(&dir, &["commit", "--allow-empty", "-qm", "C"]);
        git_in(&dir, &["switch", "feat"]);

        let base = resolve_spec(&dir, ":merge-base").unwrap();

        assert_eq!(base.sha, a);
        assert_eq!(base.spec, "merge-base(main)");
    }

    #[test]
    fn warm_worktree_preserves_mtimes_of_unchanged_files() {
        let dir = init_repo("bcmp-warm-mtime", "main");
        let _cleanup = TempDir(dir.clone());
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        git_in(&dir, &["add", "-A"]);
        git_in(&dir, &["commit", "-qm", "A"]);
        let sha_a = resolve_rev(&dir, "HEAD").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(dir.join("b.txt"), "bb").unwrap();
        git_in(&dir, &["commit", "-am", "B"]);
        let sha_b = resolve_rev(&dir, "HEAD").unwrap();
        let wt = dir.with_extension("warm");
        let _wt_cleanup = TempDir(wt.clone());

        prepare_warm_worktree(&dir, &wt, &sha_a).unwrap();
        let a_mtime = std::fs::metadata(wt.join("a.txt"))
            .unwrap()
            .modified()
            .unwrap();
        prepare_warm_worktree(&dir, &wt, &sha_b).unwrap();

        assert_eq!(
            std::fs::metadata(wt.join("a.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            a_mtime
        );
        assert_eq!(std::fs::read_to_string(wt.join("b.txt")).unwrap(), "bb");
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), sha_b.sha);
        prepare_warm_worktree(&dir, &wt, &sha_a).unwrap();
        assert_eq!(
            std::fs::metadata(wt.join("a.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            a_mtime
        );
    }

    #[test]
    fn warm_worktree_recovers_from_deleted_dir() {
        let dir = init_repo("bcmp-warm-recover", "main");
        let _cleanup = TempDir(dir.clone());
        let rev = resolve_rev(&dir, "HEAD").unwrap();
        let wt = dir.with_extension("warm");
        let _wt_cleanup = TempDir(wt.clone());

        prepare_warm_worktree(&dir, &wt, &rev).unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        prepare_warm_worktree(&dir, &wt, &rev).unwrap();

        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), rev.sha);
    }

    #[test]
    fn repo_lock_blocks_second_acquire_and_steals_stale() {
        let dir = std::env::temp_dir().join(format!("bcmp-lock-{}", std::process::id()));
        let _cleanup = TempDir(dir.clone());
        std::fs::create_dir_all(&dir).unwrap();

        let first = RepoLock::acquire(&dir).unwrap();
        assert!(RepoLock::acquire(&dir).is_err());
        drop(first);
        drop(RepoLock::acquire(&dir).unwrap());
        std::fs::write(dir.join("warm.lock"), u32::MAX.to_string()).unwrap();
        drop(RepoLock::acquire(&dir).unwrap());
    }

    fn init_repo(prefix: &str, branch: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{branch}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", branch],
            vec![
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        ] {
            let status = Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        }
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let mut full_args = vec![
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "user.name=Test",
        ];
        full_args.extend_from_slice(args);
        let output = Command::new("git")
            .args(&full_args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {full_args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn git_status(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    }

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
