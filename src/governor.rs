use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

pub const SYSFS_CPU: &str = "/sys/devices/system/cpu";

fn governor_path(sysfs: &Path, core: u32) -> PathBuf {
    sysfs
        .join(format!("cpu{core}"))
        .join("cpufreq")
        .join("scaling_governor")
}

fn available_governors_path(sysfs: &Path, core: u32) -> PathBuf {
    sysfs
        .join(format!("cpu{core}"))
        .join("cpufreq")
        .join("scaling_available_governors")
}

/// Read + trim a core's governor; None if cpufreq is missing or unreadable.
pub fn governor_of(sysfs: &Path, core: u32) -> Option<String> {
    std::fs::read_to_string(governor_path(sysfs, core))
        .ok()
        .map(|s| s.trim().to_owned())
}

fn online_cores(sysfs: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return Vec::new();
    };
    let mut cores = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_prefix("cpu")?.parse::<u32>().ok()
        })
        .collect::<Vec<_>>();
    cores.sort_unstable();
    cores
}

/// Warning text about suboptimal governors, or None if fine/unknown.
pub fn governor_warning(sysfs: &Path, pinned_core: Option<u32>) -> Option<String> {
    if let Some(core) = pinned_core {
        let gov = governor_of(sysfs, core)?;
        return (gov != "performance").then(|| {
            format!(
                "CPU governor on core {core} is '{gov}', not 'performance'; results may be noisy"
            )
        });
    }

    let known = online_cores(sysfs)
        .into_iter()
        .filter_map(|core| governor_of(sysfs, core).map(|gov| (core, gov)))
        .collect::<Vec<_>>();
    let total = known.len();
    if total == 0 {
        return None;
    }

    let mut bad_values = known
        .iter()
        .filter_map(|(_, gov)| (gov != "performance").then_some(gov.clone()))
        .collect::<Vec<_>>();
    let bad = bad_values.len();
    if bad == 0 {
        return None;
    }
    bad_values.sort();
    bad_values.dedup();
    Some(format!(
        "CPU governor is not 'performance' on {bad} of {total} cores ({}); results may be noisy",
        bad_values.join(", ")
    ))
}

/// Err if pinning is requested for a core that does not exist.
pub fn validate_core(sysfs: &Path, core: u32) -> Result<()> {
    if sysfs.exists() && !sysfs.join(format!("cpu{core}")).exists() {
        return Err(anyhow!(
            "--runs-on-core {core}: core does not exist on this machine"
        ));
    }
    Ok(())
}

/// RAII: restores the previous governor on drop (best effort, never panics).
pub struct GovernorGuard {
    sysfs: PathBuf,
    core: u32,
    previous: String,
}

impl GovernorGuard {
    pub fn previous(&self) -> &str {
        &self.previous
    }
}

impl Drop for GovernorGuard {
    fn drop(&mut self) {
        if let Err(err) = write_governor(&self.sysfs, self.core, &self.previous) {
            eprintln!(
                "warning: failed to restore CPU governor on core {} to '{}': {err}. Fix manually: echo {} | sudo tee {}",
                self.core,
                self.previous,
                self.previous,
                governor_path(&self.sysfs, self.core).display()
            );
        }
    }
}

pub enum SetOutcome {
    Changed(GovernorGuard),
    AlreadyPerformance,
    Skipped(String),
}

pub fn set_performance(sysfs: &Path, core: u32) -> Result<SetOutcome> {
    let Some(previous) = governor_of(sysfs, core) else {
        return Ok(SetOutcome::Skipped(format!(
            "no cpufreq support on core {core}"
        )));
    };
    if previous == "performance" {
        return Ok(SetOutcome::AlreadyPerformance);
    }

    if let Ok(available) = std::fs::read_to_string(available_governors_path(sysfs, core)) {
        let governors = available.split_whitespace().collect::<Vec<_>>();
        if !governors.contains(&"performance") {
            return Ok(SetOutcome::Skipped(format!(
                "'performance' not offered by this driver (available: {})",
                governors.join(" ")
            )));
        }
    }

    match write_governor(sysfs, core, "performance") {
        Ok(()) => Ok(SetOutcome::Changed(GovernorGuard {
            sysfs: sysfs.to_owned(),
            core,
            previous,
        })),
        Err(err) => Ok(SetOutcome::Skipped(err.to_string())),
    }
}

fn write_governor(sysfs: &Path, core: u32, value: &str) -> Result<()> {
    let path = governor_path(sysfs, core);
    match std::fs::write(&path, value) {
        Ok(()) => return Ok(()),
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied && sysfs.starts_with("/sys") => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to write {}", path.display()));
        }
    }

    eprintln!("requesting sudo to write the CPU governor (will be restored on exit)");
    let mut child = Command::new("sudo")
        .arg("tee")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| "failed to run sudo tee")?;
    child
        .stdin
        .take()
        .expect("stdin piped")
        .write_all(value.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        return Err(anyhow!("sudo tee {} failed", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sysfs(name: &str, cores: &[(u32, &str, &str)]) -> Cleanup {
        let path = std::env::temp_dir().join(format!(
            "bcmp-sysfs-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        for (core, governor, available) in cores {
            let cpufreq = path.join(format!("cpu{core}")).join("cpufreq");
            std::fs::create_dir_all(&cpufreq).unwrap();
            std::fs::write(cpufreq.join("scaling_governor"), format!("{governor}\n")).unwrap();
            std::fs::write(cpufreq.join("scaling_available_governors"), available).unwrap();
        }
        Cleanup(path)
    }

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn warns_on_pinned_core_only() {
        let sysfs = fake_sysfs(
            "pinned",
            &[
                (0, "performance", "performance powersave"),
                (2, "powersave", "performance powersave"),
            ],
        );

        let warning = governor_warning(&sysfs.0, Some(2)).unwrap();
        assert!(warning.contains("core 2"));
        assert!(warning.contains("powersave"));
        assert!(governor_warning(&sysfs.0, Some(0)).is_none());
    }

    #[test]
    fn unpinned_summarizes_all_cores() {
        let sysfs = fake_sysfs(
            "unpinned",
            &[
                (0, "powersave", "performance powersave"),
                (1, "powersave", "performance powersave"),
                (2, "performance", "performance powersave"),
            ],
        );
        let warning = governor_warning(&sysfs.0, None).unwrap();
        assert!(warning.contains("2 of 3"));

        let all_performance = fake_sysfs(
            "all-performance",
            &[
                (0, "performance", "performance powersave"),
                (1, "performance", "performance powersave"),
            ],
        );
        assert!(governor_warning(&all_performance.0, None).is_none());
    }

    #[test]
    fn silent_without_cpufreq() {
        let sysfs = fake_sysfs("no-cpufreq", &[]);
        std::fs::create_dir_all(sysfs.0.join("cpu0")).unwrap();
        std::fs::create_dir_all(sysfs.0.join("cpu1")).unwrap();

        assert!(governor_warning(&sysfs.0, Some(0)).is_none());
        assert!(governor_warning(&sysfs.0, None).is_none());
    }

    #[test]
    fn validate_core_rejects_missing() {
        let sysfs = fake_sysfs("validate", &[(0, "performance", "performance powersave")]);
        assert!(validate_core(&sysfs.0, 5).is_err());
        assert!(validate_core(&sysfs.0, 0).is_ok());
        assert!(validate_core(&sysfs.0.join("missing"), 5).is_ok());
    }

    #[test]
    fn set_and_restore_roundtrip() {
        let sysfs = fake_sysfs("set", &[(3, "powersave", "performance powersave")]);

        let SetOutcome::Changed(guard) = set_performance(&sysfs.0, 3).unwrap() else {
            panic!("expected Changed");
        };
        assert_eq!(governor_of(&sysfs.0, 3).as_deref(), Some("performance"));
        drop(guard);
        assert_eq!(governor_of(&sysfs.0, 3).as_deref(), Some("powersave"));
    }

    #[test]
    fn set_skips() {
        let already = fake_sysfs("already", &[(0, "performance", "performance powersave")]);
        assert!(matches!(
            set_performance(&already.0, 0).unwrap(),
            SetOutcome::AlreadyPerformance
        ));
        assert_eq!(governor_of(&already.0, 0).as_deref(), Some("performance"));

        let unavailable = fake_sysfs("unavailable", &[(1, "powersave", "powersave")]);
        let SetOutcome::Skipped(reason) = set_performance(&unavailable.0, 1).unwrap() else {
            panic!("expected Skipped");
        };
        assert!(reason.contains("powersave"));
    }
}
