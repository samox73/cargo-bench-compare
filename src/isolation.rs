use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};

use crate::governor;

pub const PROC_IRQ: &str = "/proc/irq";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const SLICE_UNITS: [&str; 3] = ["user.slice", "system.slice", "init.scope"];

pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                out.extend(a..=b);
            }
        } else if let Ok(core) = part.parse::<u32>() {
            out.push(core);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub fn format_cpu_list(cores: &[u32]) -> String {
    let mut cores = cores.to_vec();
    cores.sort_unstable();
    cores.dedup();
    let mut ranges = Vec::new();
    let mut it = cores.into_iter().peekable();
    while let Some(start) = it.next() {
        let mut end = start;
        while it.peek() == Some(&(end + 1)) {
            end = it.next().unwrap();
        }
        ranges.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    ranges.join(",")
}

fn hex_mask(cores: &[u32]) -> String {
    let Some(max) = cores.iter().max().copied() else {
        return "0".to_owned();
    };
    let mut groups = vec![0_u32; (max / 32 + 1) as usize];
    for core in cores {
        groups[(core / 32) as usize] |= 1_u32 << (core % 32);
    }
    while groups.len() > 1 && groups.last() == Some(&0) {
        groups.pop();
    }
    groups
        .iter()
        .rev()
        .enumerate()
        .map(|(i, group)| {
            if i == 0 {
                format!("{group:x}")
            } else {
                format!("{group:08x}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub fn evicted_cores(sysfs: &Path, core: u32) -> Vec<u32> {
    let path = sysfs
        .join(format!("cpu{core}"))
        .join("topology")
        .join("thread_siblings_list");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| parse_cpu_list(&s))
        .filter(|cores| !cores.is_empty())
        .unwrap_or_else(|| vec![core])
}

pub fn boot_isolated(sysfs: &Path, core: u32) -> bool {
    std::fs::read_to_string(sysfs.join("isolated"))
        .ok()
        .is_some_and(|s| parse_cpu_list(&s).contains(&core))
}

fn housekeeping(sysfs: &Path, evicted: &[u32]) -> Result<Vec<u32>> {
    let evicted = evicted.iter().copied().collect::<BTreeSet<_>>();
    let cores = governor::online_cores(sysfs)
        .into_iter()
        .filter(|core| !evicted.contains(core))
        .collect::<Vec<_>>();
    if cores.is_empty() {
        let core = evicted.iter().next().copied().unwrap_or(0);
        return Err(anyhow!(
            "isolating core {core} would leave no housekeeping CPUs"
        ));
    }
    Ok(cores)
}

struct Snapshot {
    slice_prev: Vec<(&'static str, String)>,
    irq_prev: Vec<(u32, String)>,
    default_prev: Option<String>,
}

fn snapshot(sysfs: &Path, proc_irq: &Path, include_slices: bool) -> Result<Snapshot> {
    let online = format_cpu_list(&governor::online_cores(sysfs));
    let mut slice_prev = Vec::new();
    if include_slices {
        for unit in SLICE_UNITS {
            let output = Command::new("systemctl")
                .args(["show", "-p", "AllowedCPUs", "--value", unit])
                .output()
                .with_context(|| "failed to run systemctl show")?;
            if !output.status.success() {
                return Err(anyhow!("systemctl show failed for {unit}"));
            }
            let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            let value = if raw.is_empty() { online.clone() } else { raw };
            if sanitized(&value) {
                slice_prev.push((unit, value));
            } else {
                eprintln!("warning: dropped unsafe AllowedCPUs snapshot for {unit}");
            }
        }
    }

    let mut irq_prev = Vec::new();
    if let Ok(entries) = std::fs::read_dir(proc_irq) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
                continue;
            };
            let Ok(value) = std::fs::read_to_string(entry.path().join("smp_affinity_list")) else {
                continue;
            };
            let value = value.trim().to_owned();
            if sanitized(&value) {
                irq_prev.push((name, value));
            } else {
                eprintln!("warning: dropped unsafe IRQ affinity snapshot for IRQ {name}");
            }
        }
    }
    irq_prev.sort_by_key(|(irq, _)| *irq);

    let default_prev = std::fs::read_to_string(proc_irq.join("default_smp_affinity"))
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| {
            let ok = sanitized(s);
            if !ok {
                eprintln!("warning: dropped unsafe default IRQ affinity snapshot");
            }
            ok
        });

    Ok(Snapshot {
        slice_prev,
        irq_prev,
        default_prev,
    })
}

fn sanitized(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b',' | b'-'))
}

fn enter_escape_scope(scope_unit: &str) -> Result<Child> {
    // -n: credentials were primed in isolate_inner, so the 5s cgroup poll
    // below never races an interactive password prompt
    let mut child = Command::new("sudo")
        .args([
            "-n",
            "systemd-run",
            "--scope",
            "--slice=bcmp.slice",
            &format!("--unit={scope_unit}"),
            "-p",
            "Delegate=yes",
            "--quiet",
            "sleep",
            "30",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| "failed to run sudo systemd-run")?;

    let path = Path::new(CGROUP_ROOT).join("bcmp.slice").join(scope_unit);
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(child);
        }
        if child.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow!("escape scope {scope_unit} did not appear"))
}

fn apply_script(scope_unit: Option<&str>, housekeeping: &str, hk_hex: &str, pid: u32) -> String {
    let mut lines = vec!["set -e".to_owned()];
    if let Some(scope_unit) = scope_unit {
        lines.push(format!(
            "echo {pid} > /sys/fs/cgroup/bcmp.slice/{scope_unit}/cgroup.procs"
        ));
        for unit in SLICE_UNITS {
            lines.push(format!(
                "systemctl set-property --runtime {unit} AllowedCPUs={housekeeping}"
            ));
        }
    }
    lines.push(format!("echo {hk_hex} > /proc/irq/default_smp_affinity"));
    lines.push(format!(
        "for f in /proc/irq/*/smp_affinity_list; do echo {housekeeping} > \"$f\" 2>/dev/null || true; done"
    ));
    lines.join("\n")
}

fn restore_script(snapshot: &Snapshot, with_cat: bool) -> String {
    let mut lines = Vec::new();
    for (unit, value) in &snapshot.slice_prev {
        if sanitized(value) {
            lines.push(format!(
                "systemctl set-property --runtime {unit} AllowedCPUs={value} || true"
            ));
        }
    }
    if let Some(value) = &snapshot.default_prev
        && sanitized(value)
    {
        lines.push(format!(
            "echo {value} > /proc/irq/default_smp_affinity || true"
        ));
    }
    for (irq, value) in &snapshot.irq_prev {
        if sanitized(value) {
            lines.push(format!(
                "echo {value} > /proc/irq/{irq}/smp_affinity_list 2>/dev/null || true"
            ));
        }
    }
    let body = lines.join("; ");
    if with_cat {
        format!("trap '' INT TERM HUP; cat >/dev/null; {body}")
    } else {
        body
    }
}

pub enum IsolateOutcome {
    Isolated { guard: IsolationGuard, boot: bool },
    Skipped(String),
}

pub fn isolate(sysfs: &Path, proc_irq: &Path, core: u32) -> Result<IsolateOutcome> {
    // an empty housekeeping set is a configuration error and aborts the run;
    // everything inside isolate_inner is recoverable and only skips isolation
    let evicted = evicted_cores(sysfs, core);
    let housekeeping = housekeeping(sysfs, &evicted)?;
    Ok(match isolate_inner(sysfs, proc_irq, core, &evicted, &housekeeping) {
        Ok(outcome) => outcome,
        Err(err) => IsolateOutcome::Skipped(err.to_string()),
    })
}

fn isolate_inner(
    sysfs: &Path,
    proc_irq: &Path,
    core: u32,
    evicted: &[u32],
    housekeeping: &[u32],
) -> Result<IsolateOutcome> {
    let boot = boot_isolated(sysfs, core);
    if !boot {
        let controllers = Path::new(CGROUP_ROOT).join("cgroup.controllers");
        let has_cpuset = std::fs::read_to_string(&controllers)
            .ok()
            .is_some_and(|s| s.split_whitespace().any(|c| c == "cpuset"));
        if !has_cpuset {
            return Err(anyhow!("cgroup v2 cpuset controller is unavailable"));
        }
        probe_command("systemctl")?;
        probe_command("systemd-run")?;
    }

    let housekeeping_list = format_cpu_list(housekeeping);
    let hk_hex = hex_mask(housekeeping);
    let snapshot = snapshot(sysfs, proc_irq, !boot)?;
    let irqs = snapshot
        .irq_prev
        .iter()
        .map(|(irq, _)| *irq)
        .collect::<Vec<_>>();

    if !governor::sudo_is_passwordless() {
        eprintln!("requesting sudo to isolate core {core} (restored on exit)");
        // prime the credential cache interactively; every later sudo in this
        // module can then run with -n and never blocks on a prompt
        let status = Command::new("sudo")
            .arg("true")
            .status()
            .with_context(|| "failed to run sudo")?;
        if !status.success() {
            return Err(anyhow!("sudo authorization failed"));
        }
    }

    let scope_unit = (!boot).then(|| format!("bcmp-shield-{}.scope", std::process::id()));
    let escape_child = match &scope_unit {
        Some(unit) => Some(enter_escape_scope(unit)?),
        None => None,
    };

    let script = apply_script(
        scope_unit.as_deref(),
        &housekeeping_list,
        &hk_hex,
        std::process::id(),
    );
    let status = Command::new("sudo")
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| "failed to run sudo isolation script")?;
    if !status.success() {
        let _ = run_restore_direct(&snapshot);
        return Err(anyhow!("sudo isolation script failed"));
    }

    if let Some(unit) = &scope_unit {
        let migrated = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .is_some_and(|s| s.contains(unit));
        if !migrated {
            let _ = run_restore_direct(&snapshot);
            return Err(anyhow!("failed to migrate process into {unit}"));
        }
        if let Some(mut child) = escape_child {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        let status = Command::new("taskset")
            .args([
                "-a",
                "-p",
                "-c",
                &housekeeping_list,
                &std::process::id().to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !status.is_ok_and(|s| s.success()) {
            eprintln!("warning: failed to move this process to housekeeping cores");
        }
    }

    let sibling = if evicted.len() > 1 {
        let siblings = evicted
            .iter()
            .copied()
            .filter(|s| *s != core)
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(" (+SMT sibling {siblings})")
    } else {
        String::new()
    };
    if boot {
        eprintln!(
            "isolated core {core}{sibling}: core is boot-isolated (isolcpus), steered {} IRQs (restored on exit)",
            irqs.len()
        );
    } else {
        eprintln!(
            "isolated core {core}{sibling}: evicted user.slice/system.slice/init.scope to cores {housekeeping_list}, steered {} IRQs (restored on exit)",
            irqs.len()
        );
    }

    let restore = restore_script(&snapshot, false);
    let canary_prev = snapshot
        .slice_prev
        .iter()
        .find_map(|(unit, value)| (*unit == "user.slice").then(|| value.clone()))
        .unwrap_or_default();
    let helper = spawn_restore_helper(&snapshot);
    Ok(IsolateOutcome::Isolated {
        guard: IsolationGuard {
            restore,
            canary_prev,
            helper,
        },
        boot,
    })
}

fn probe_command(cmd: &str) -> Result<()> {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("{cmd} is unavailable"))?
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("{cmd} is unavailable"))
}

fn spawn_restore_helper(snapshot: &Snapshot) -> Option<Child> {
    let script = restore_script(snapshot, true);
    let mut cmd = Command::new("sudo");
    cmd.args(["-n", "sh", "-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().ok()
}

fn run_restore_direct(snapshot: &Snapshot) -> Result<()> {
    let script = restore_script(snapshot, false);
    let status = Command::new("sudo")
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .status()
        .with_context(|| "failed to run isolation restore")?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("isolation restore failed"))
}

pub struct IsolationGuard {
    restore: String,
    canary_prev: String,
    helper: Option<Child>,
}

impl Drop for IsolationGuard {
    fn drop(&mut self) {
        if let Some(mut helper) = self.helper.take() {
            drop(helper.stdin.take());
            let helper_ok = helper.wait().is_ok_and(|status| status.success());
            let restored = if self.canary_prev.is_empty() {
                helper_ok
            } else {
                helper_ok
                    && Command::new("systemctl")
                        .args(["show", "-p", "AllowedCPUs", "--value", "user.slice"])
                        .output()
                        .ok()
                        .filter(|out| out.status.success())
                        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                        .as_deref()
                        == Some(self.canary_prev.as_str())
            };
            if restored {
                return;
            }
        }
        let failed = Command::new("sudo")
            .args(["sh", "-c", &self.restore])
            .stdin(Stdio::null())
            .status()
            .map_or(true, |status| !status.success());
        if failed {
            eprintln!(
                "warning: failed to restore CPU isolation. Fix manually: sudo systemctl set-property --runtime user.slice AllowedCPUs= (repeat for system.slice and init.scope); IRQ affinities reset on reboot."
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_sysfs(name: &str, cores: &[u32]) -> Cleanup {
        let path = std::env::temp_dir().join(format!(
            "bcmp-isolation-sysfs-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        for core in cores {
            std::fs::create_dir_all(path.join(format!("cpu{core}")).join("topology")).unwrap();
        }
        Cleanup(path)
    }

    struct Cleanup(std::path::PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parse_and_format_cpu_list_roundtrip() {
        for (raw, cores) in [
            ("7", vec![7]),
            ("0-1", vec![0, 1]),
            ("3,7", vec![3, 7]),
            ("0-2,5-6", vec![0, 1, 2, 5, 6]),
            ("", vec![]),
        ] {
            assert_eq!(parse_cpu_list(raw), cores);
            assert_eq!(parse_cpu_list(&format_cpu_list(&cores)), cores);
        }
        assert_eq!(
            format_cpu_list(&(0..=6).chain(8..=13).collect::<Vec<_>>()),
            "0-6,8-13"
        );
    }

    #[test]
    fn hex_mask_matches_kernel_format() {
        assert_eq!(hex_mask(&(0..=13).collect::<Vec<_>>()), "3fff");
        assert_eq!(hex_mask(&[0, 1, 2]), "7");
        assert_eq!(hex_mask(&(0..=32).collect::<Vec<_>>()), "1,ffffffff");
    }

    #[test]
    fn evicted_includes_smt_siblings() {
        let sysfs = fake_sysfs("smt", &[0, 7, 9]);
        std::fs::write(
            sysfs
                .0
                .join("cpu0")
                .join("topology")
                .join("thread_siblings_list"),
            "0-1\n",
        )
        .unwrap();
        std::fs::write(
            sysfs
                .0
                .join("cpu7")
                .join("topology")
                .join("thread_siblings_list"),
            "7\n",
        )
        .unwrap();
        assert_eq!(evicted_cores(&sysfs.0, 0), vec![0, 1]);
        assert_eq!(evicted_cores(&sysfs.0, 7), vec![7]);
        assert_eq!(evicted_cores(&sysfs.0, 9), vec![9]);
    }

    #[test]
    fn boot_isolated_detection() {
        let sysfs = fake_sysfs("boot", &[3, 7, 9]);
        std::fs::write(sysfs.0.join("isolated"), "7\n").unwrap();
        assert!(boot_isolated(&sysfs.0, 7));
        assert!(!boot_isolated(&sysfs.0, 3));
        std::fs::write(sysfs.0.join("isolated"), "3-5,9").unwrap();
        assert!(!boot_isolated(&sysfs.0, 7));
        std::fs::write(sysfs.0.join("isolated"), "").unwrap();
        assert!(!boot_isolated(&sysfs.0, 7));
    }

    #[test]
    fn housekeeping_rejects_empty() {
        let sysfs = fake_sysfs("housekeeping", &[0, 1]);
        assert!(housekeeping(&sysfs.0, &[0, 1]).is_err());
        assert_eq!(housekeeping(&sysfs.0, &[1]).unwrap(), vec![0]);
    }

    #[test]
    fn apply_script_shape() {
        let with_scope = apply_script(Some("bcmp-shield-1.scope"), "0-6,8-13", "3f7f", 42);
        assert!(
            with_scope
                .contains("echo 42 > /sys/fs/cgroup/bcmp.slice/bcmp-shield-1.scope/cgroup.procs")
        );
        assert!(
            with_scope.contains("systemctl set-property --runtime user.slice AllowedCPUs=0-6,8-13")
        );
        assert!(
            with_scope
                .contains("systemctl set-property --runtime system.slice AllowedCPUs=0-6,8-13")
        );
        assert!(
            with_scope.contains("systemctl set-property --runtime init.scope AllowedCPUs=0-6,8-13")
        );
        assert!(with_scope.contains("echo 3f7f > /proc/irq/default_smp_affinity"));
        assert!(with_scope.contains("|| true"));

        let boot = apply_script(None, "0-6,8-13", "3f7f", 42);
        assert!(!boot.contains("cgroup.procs"));
        assert!(!boot.contains("systemctl set-property"));
    }

    #[test]
    fn restore_script_shape() {
        let snapshot = Snapshot {
            slice_prev: vec![
                ("user.slice", "0-13".to_owned()),
                ("system.slice", "$(reboot)".to_owned()),
            ],
            irq_prev: vec![(1, "0-6,8-13".to_owned()), (2, "$(reboot)".to_owned())],
            default_prev: Some("3fff".to_owned()),
        };
        let helper = restore_script(&snapshot, true);
        assert!(helper.starts_with("trap '' INT TERM HUP; cat >/dev/null;"));
        assert!(helper.contains("AllowedCPUs=0-13 || true"));
        assert!(!helper.contains("AllowedCPUs= || true"));
        assert!(!helper.contains("$(reboot)"));
        assert!(helper.contains("default_smp_affinity || true"));
        assert!(helper.contains("/proc/irq/1/smp_affinity_list 2>/dev/null || true"));
    }
}
