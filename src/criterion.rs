use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

#[derive(Clone)]
pub struct CriterionResult {
    pub full_id: String,
    pub mean_ns: f64,
    pub std_dev_ns: f64,
}

#[derive(Deserialize)]
struct Estimates {
    mean: Estimate,
    std_dev: Estimate,
}

#[derive(Deserialize)]
struct Estimate {
    point_estimate: f64,
}

#[derive(Deserialize)]
struct BenchmarkMeta {
    full_id: String,
}

pub fn collect(target_dir: &Path, baseline_label: &str) -> Result<Vec<CriterionResult>> {
    let root = target_dir.join("criterion");
    let mut out = Vec::new();
    if root.exists() {
        walk(&root, &root, baseline_label, &mut out)?;
    }
    if out.is_empty() {
        return Err(anyhow!(
            "no criterion results found under {}/criterion for baseline '{}' — did the bench run?",
            target_dir.display(),
            baseline_label
        ));
    }
    out.sort_by(|a, b| a.full_id.cmp(&b.full_id));
    Ok(out)
}

/// Delete every directory named `bcmp-*` under `target/criterion` whose name is
/// not `keep_label`. Silent no-op if the criterion dir does not exist.
pub fn remove_stale_baselines(target_dir: &Path, keep_label: &str) -> Result<()> {
    let root = target_dir.join("criterion");
    if root.exists() {
        prune_walk(&root, keep_label)?;
    }
    Ok(())
}

fn prune_walk(dir: &Path, keep_label: &str) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("bcmp-") && name != keep_label {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
        } else {
            prune_walk(&path, keep_label)?;
        }
    }
    Ok(())
}

fn walk(root: &Path, dir: &Path, label: &str, out: &mut Vec<CriterionResult>) -> Result<()> {
    if dir == root.join("report") {
        return Ok(());
    }
    let file_name_matches = dir.file_name().and_then(|n| n.to_str()) == Some(label);
    let estimates_path = dir.join("estimates.json");
    if file_name_matches && estimates_path.exists() {
        let estimates: Estimates = serde_json::from_str(
            &std::fs::read_to_string(&estimates_path)
                .with_context(|| format!("failed to read {}", estimates_path.display()))?,
        )
        .with_context(|| format!("failed to parse {}", estimates_path.display()))?;
        let bench_path = dir.join("benchmark.json");
        let full_id = if bench_path.exists() {
            serde_json::from_str::<BenchmarkMeta>(
                &std::fs::read_to_string(&bench_path)
                    .with_context(|| format!("failed to read {}", bench_path.display()))?,
            )
            .with_context(|| format!("failed to parse {}", bench_path.display()))?
            .full_id
        } else {
            dir.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or(label)
                .to_owned()
        };
        out.push(CriterionResult {
            full_id,
            mean_ns: estimates.mean.point_estimate,
            std_dev_ns: estimates.std_dev.point_estimate,
        });
    }

    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            walk(root, &entry.path(), label, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_stale_baselines_keeps_current_label() {
        let root = std::env::temp_dir().join(format!("bcmp-criterion-{}", std::process::id()));
        let _cleanup = TempDir(root.clone());
        std::fs::create_dir_all(root.join("criterion/g/bcmp-old")).unwrap();
        std::fs::create_dir_all(root.join("criterion/g/bcmp-new")).unwrap();
        std::fs::write(root.join("criterion/g/bcmp-old/estimates.json"), "{}").unwrap();
        std::fs::write(root.join("criterion/g/bcmp-new/estimates.json"), "{}").unwrap();

        remove_stale_baselines(&root, "bcmp-new").unwrap();

        assert!(!root.join("criterion/g/bcmp-old").exists());
        assert!(root.join("criterion/g/bcmp-new").exists());
    }

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
