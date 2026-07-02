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
