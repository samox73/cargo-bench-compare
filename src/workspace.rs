use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::git;

pub struct WorkspaceInfo {
    pub repo_root: PathBuf,
    pub ws_root: PathBuf,
    pub ws_rel: PathBuf,
}

pub fn load(repo_root: &Path, package: &str) -> Result<WorkspaceInfo> {
    let cwd = std::env::current_dir()?;
    let raw = git::run_capture(
        "cargo",
        &["metadata", "--format-version", "1", "--no-deps"],
        &cwd,
        &[],
    )?;
    let value: Value = serde_json::from_str(&raw).context("failed to parse cargo metadata")?;
    let ws_root = PathBuf::from(
        value
            .get("workspace_root")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("cargo metadata did not contain workspace_root"))?,
    );
    let mut names = value
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("cargo metadata did not contain packages"))?
        .iter()
        .filter_map(|pkg| pkg.get("name").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>();
    names.sort();
    if !names.iter().any(|name| name == package) {
        return Err(anyhow!(
            "package '{package}' not found in workspace; available: {}",
            names.join(", ")
        ));
    }
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
    Ok(WorkspaceInfo {
        repo_root: repo_root.to_owned(),
        ws_root,
        ws_rel,
    })
}

impl WorkspaceInfo {
    pub fn worktree_ws_root(&self, worktree: &Path) -> PathBuf {
        worktree.join(&self.ws_rel)
    }
}
