use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::git;

#[derive(Clone, Copy)]
pub enum TargetKind {
    Bin,
    Bench,
}

impl TargetKind {
    fn as_str(self) -> &'static str {
        match self {
            TargetKind::Bin => "bin",
            TargetKind::Bench => "bench",
        }
    }
}

#[derive(Debug)]
pub struct WorkspaceInfo {
    pub ws_rel: PathBuf,
    pub package: String,
}

pub fn load(
    repo_root: &Path,
    cwd: &Path,
    package: Option<&str>,
    kind: TargetKind,
    target: &str,
) -> Result<WorkspaceInfo> {
    let raw = git::run_capture(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        cwd,
        &[],
    )?;
    let value: Value = serde_json::from_str(&raw).context("failed to parse cargo metadata")?;
    let ws_root = PathBuf::from(
        value
            .get("workspace_root")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("cargo metadata did not contain workspace_root"))?,
    );
    let packages = value
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata did not contain packages"))?;
    let package = resolve_package(packages, package, kind, target)?;
    let ws_rel = ws_root
        .strip_prefix(repo_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            anyhow!(
                "cargo workspace root {} is not inside git repo {}",
                ws_root.display(),
                repo_root.display()
            )
        })?;
    Ok(WorkspaceInfo { ws_rel, package })
}

fn resolve_package(
    packages: &[Value],
    explicit: Option<&str>,
    kind: TargetKind,
    target: &str,
) -> Result<String> {
    if let Some(pkg) = explicit {
        if packages
            .iter()
            .any(|p| p.get("name").and_then(Value::as_str) == Some(pkg))
        {
            return Ok(pkg.to_owned());
        }
        let mut names: Vec<_> = packages
            .iter()
            .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        names.sort();
        return Err(anyhow!(
            "package '{pkg}' not found in workspace; available: {}",
            names.join(", ")
        ));
    }

    let mut owners = BTreeSet::new();
    let mut available = BTreeSet::new();
    for package in packages {
        let Some(name) = package.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(targets) = package.get("targets").and_then(Value::as_array) else {
            continue;
        };
        for item in targets {
            if !target_has_kind(item, kind) {
                continue;
            }
            let Some(target_name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            available.insert(target_name.to_owned());
            if target_name == target {
                owners.insert(name.to_owned());
            }
        }
    }

    match owners.len() {
        1 => Ok(owners.into_iter().next().unwrap()),
        0 => {
            let available = if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.into_iter().collect::<Vec<_>>().join(", ")
            };
            Err(anyhow!(
                "no package in the workspace has a {} target named '{target}'; available {} targets: {available}; if the target only exists in the revision being benchmarked, pass -p <package> explicitly",
                kind.as_str(),
                kind.as_str()
            ))
        }
        _ => Err(anyhow!(
            "{} target '{target}' exists in multiple packages: {}; disambiguate with -p <package>",
            kind.as_str(),
            owners.into_iter().collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(crate) fn target_has_kind(target: &Value, kind: TargetKind) -> bool {
    target
        .get("kind")
        .and_then(Value::as_array)
        .is_some_and(|kinds| {
            kinds
                .iter()
                .any(|item| item.as_str() == Some(kind.as_str()))
        })
}

impl WorkspaceInfo {
    pub fn worktree_ws_root(&self, worktree: &Path) -> PathBuf {
        worktree.join(&self.ws_rel)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn pkg(name: &str, targets: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "name": name,
            "targets": targets.iter().map(|(kind, tname)| serde_json::json!({
                "kind": [kind], "name": tname,
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn explicit_package_is_returned_verbatim() {
        assert_eq!(
            resolve_package(&[pkg("a", &[])], Some("a"), TargetKind::Bin, "tool").unwrap(),
            "a"
        );
    }

    #[test]
    fn explicit_unknown_package_lists_available() {
        let err = resolve_package(
            &[pkg("b", &[]), pkg("a", &[])],
            Some("zzz"),
            TargetKind::Bin,
            "tool",
        )
        .unwrap_err();
        assert!(err.to_string().contains("available: a, b"));
    }

    #[test]
    fn unique_bin_owner_is_inferred() {
        assert_eq!(
            resolve_package(
                &[pkg("a", &[("bin", "tool")]), pkg("b", &[("bin", "other")])],
                None,
                TargetKind::Bin,
                "tool"
            )
            .unwrap(),
            "a"
        );
    }

    #[test]
    fn unique_bench_owner_is_inferred() {
        assert_eq!(
            resolve_package(
                &[pkg("a", &[("bench", "hot_path")])],
                None,
                TargetKind::Bench,
                "hot_path"
            )
            .unwrap(),
            "a"
        );
    }

    #[test]
    fn kind_filter_separates_bin_from_bench() {
        let packages = [pkg("a", &[("bench", "hot")]), pkg("b", &[("bin", "hot")])];
        assert_eq!(
            resolve_package(&packages, None, TargetKind::Bin, "hot").unwrap(),
            "b"
        );
        assert_eq!(
            resolve_package(&packages, None, TargetKind::Bench, "hot").unwrap(),
            "a"
        );
    }

    #[test]
    fn missing_target_lists_candidates_and_hint() {
        let err = resolve_package(
            &[pkg("a", &[("bin", "x")]), pkg("b", &[("bin", "y")])],
            None,
            TargetKind::Bin,
            "nope",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("available bin targets: x, y"));
        assert!(err.contains("pass -p <package> explicitly"));
    }

    #[test]
    fn missing_target_with_no_targets_prints_none() {
        let err = resolve_package(&[pkg("a", &[])], None, TargetKind::Bench, "nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("available bench targets: (none)"));
    }

    #[test]
    fn ambiguous_target_lists_owning_packages() {
        let err = resolve_package(
            &[pkg("b", &[("bin", "tool")]), pkg("a", &[("bin", "tool")])],
            None,
            TargetKind::Bin,
            "tool",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("multiple packages: a, b"));
        assert!(err.contains("disambiguate with -p"));
    }

    #[test]
    fn load_infers_package_in_real_workspace() {
        let raw_dir = temp_dir("bcmp-workspace-infer");
        let _ = fs::remove_dir_all(&raw_dir);
        fs::create_dir_all(raw_dir.join("alpha/src")).unwrap();
        fs::create_dir_all(raw_dir.join("beta/src")).unwrap();
        fs::write(
            raw_dir.join("Cargo.toml"),
            r#"[workspace]
members = ["alpha", "beta"]
resolver = "2"
"#,
        )
        .unwrap();
        fs::write(
            raw_dir.join("alpha/Cargo.toml"),
            r#"[package]
name = "alpha"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        fs::write(raw_dir.join("alpha/src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            raw_dir.join("beta/Cargo.toml"),
            r#"[package]
name = "beta"
version = "0.1.0"
edition = "2024"
"#,
        )
        .unwrap();
        fs::write(raw_dir.join("beta/src/lib.rs"), "").unwrap();
        let dir = fs::canonicalize(&raw_dir).unwrap();
        let _cleanup = Cleanup(dir.clone());

        let inferred = load(&dir, &dir, None, TargetKind::Bin, "alpha").unwrap();
        assert_eq!(inferred.package, "alpha");
        assert!(inferred.ws_rel.as_os_str().is_empty());

        let missing = load(&dir, &dir, None, TargetKind::Bin, "nope")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("available bin targets: alpha"));

        let explicit = load(&dir, &dir, Some("beta"), TargetKind::Bin, "alpha").unwrap();
        assert_eq!(explicit.package, "beta");
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{name}-{}", std::process::id()))
    }

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
