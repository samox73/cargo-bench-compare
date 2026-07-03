use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow};

use crate::cli::CandidateKind;
use crate::git;
use crate::workspace;

pub fn print(kind: CandidateKind) -> Result<()> {
    if let Ok(values) = candidates(
        kind,
        &std::env::current_dir().unwrap_or_else(|_| ".".into()),
    ) {
        for value in values {
            println!("{value}");
        }
    }
    Ok(())
}

pub fn candidates(kind: CandidateKind, cwd: &Path) -> Result<Vec<String>> {
    match kind {
        CandidateKind::Revs => revs(cwd),
        CandidateKind::Packages => package_names(cwd),
        CandidateKind::Bins => target_names(cwd, workspace::TargetKind::Bin),
        CandidateKind::Benches => target_names(cwd, workspace::TargetKind::Bench),
        CandidateKind::Profiles => profiles(cwd),
    }
    .or_else(|_| Ok(Vec::new()))
}

fn revs(cwd: &Path) -> Result<Vec<String>> {
    let output = git::run_capture(
        "git",
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/tags",
        ],
        cwd,
        &[],
    )?;
    let mut values = Vec::from([
        ":worktree".to_owned(),
        ":merge-base".to_owned(),
        "HEAD".to_owned(),
    ]);
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !values.iter().any(|value| value == line) {
            values.push(line.to_owned());
        }
    }
    Ok(values)
}

fn package_names(cwd: &Path) -> Result<Vec<String>> {
    let json = metadata_json(cwd)?;
    let packages = json
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata output missing packages"))?;
    let mut values = BTreeSet::new();
    for package in packages {
        if let Some(name) = package.get("name").and_then(serde_json::Value::as_str) {
            values.insert(name.to_owned());
        }
    }
    Ok(values.into_iter().collect())
}

fn target_names(cwd: &Path, kind: workspace::TargetKind) -> Result<Vec<String>> {
    let json = metadata_json(cwd)?;
    let packages = json
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata output missing packages"))?;
    let mut values = BTreeSet::new();
    for package in packages {
        if let Some(targets) = package.get("targets").and_then(serde_json::Value::as_array) {
            for target in targets {
                if workspace::target_has_kind(target, kind)
                    && let Some(name) = target.get("name").and_then(serde_json::Value::as_str)
                {
                    values.insert(name.to_owned());
                }
            }
        }
    }
    Ok(values.into_iter().collect())
}

fn profiles(cwd: &Path) -> Result<Vec<String>> {
    let json = metadata_json(cwd)?;
    let root = json
        .get("workspace_root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("cargo metadata output missing workspace_root"))?;
    let manifest = std::fs::read_to_string(Path::new(root).join("Cargo.toml"))?;
    let re = regex::Regex::new(r"(?m)^\s*\[profile\.([A-Za-z0-9_-]+)\]")?;
    let mut values = BTreeSet::from([
        "bench".to_owned(),
        "dev".to_owned(),
        "release".to_owned(),
        "test".to_owned(),
    ]);
    for cap in re.captures_iter(&manifest) {
        values.insert(cap[1].to_owned());
    }
    Ok(values.into_iter().collect())
}

fn metadata_json(cwd: &Path) -> Result<serde_json::Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("cargo metadata failed"));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    #[test]
    fn fixture_candidates_cover_revs_packages_and_benches() {
        let dir = temp_dir("bcmp-candidates-fixture");
        let _cleanup = Cleanup(dir.clone());
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("benches")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            r#"[package]
name = "fixture"
version = "0.1.0"
edition = "2024"

[[bench]]
name = "fixture_bench"
harness = false

[profile.release-tuned]
inherits = "release"
"#,
        )
        .unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("benches/fixture_bench.rs"), "fn main() {}\n").unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["add", "-A"]);
        git(
            &dir,
            &[
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=Test",
                "commit",
                "-qm",
                "init",
            ],
        );
        git(&dir, &["tag", "fixture-a"]);
        git(
            &dir,
            &[
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-qm",
                "second",
            ],
        );
        git(&dir, &["tag", "fixture-b"]);

        let revs = candidates(CandidateKind::Revs, &dir).unwrap();
        assert!(revs.contains(&":worktree".to_owned()));
        assert!(revs.contains(&":merge-base".to_owned()));
        assert!(revs.contains(&"HEAD".to_owned()));
        assert!(revs.contains(&"main".to_owned()));
        assert!(revs.contains(&"fixture-a".to_owned()));
        assert!(revs.contains(&"fixture-b".to_owned()));
        assert_eq!(
            candidates(CandidateKind::Packages, &dir).unwrap(),
            vec!["fixture"]
        );
        assert!(
            candidates(CandidateKind::Benches, &dir)
                .unwrap()
                .contains(&"fixture_bench".to_owned())
        );
        assert!(
            candidates(CandidateKind::Profiles, &dir)
                .unwrap()
                .contains(&"release-tuned".to_owned())
        );
    }

    #[test]
    fn empty_dir_returns_empty_candidates_for_every_kind() {
        let dir = temp_dir("bcmp-candidates-empty");
        let _cleanup = Cleanup(dir.clone());
        fs::create_dir_all(&dir).unwrap();
        for kind in [
            CandidateKind::Packages,
            CandidateKind::Bins,
            CandidateKind::Benches,
            CandidateKind::Revs,
            CandidateKind::Profiles,
        ] {
            assert_eq!(candidates(kind, &dir).unwrap(), Vec::<String>::new());
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
