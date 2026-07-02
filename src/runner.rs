use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use regex::Regex;

use crate::builder;
use crate::cli::MetricSource;
use crate::git;
use crate::progress::{self, Event, Side};

static WARNED_TASKSET: AtomicBool = AtomicBool::new(false);

pub fn pin_prefix(core: u32, no_pin: bool) -> Vec<String> {
    if no_pin {
        return Vec::new();
    }
    if !cfg!(target_os = "linux") {
        warn_pinning_once("CPU pinning not supported on this platform");
        return Vec::new();
    }
    let available = Command::new("taskset")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !available {
        warn_pinning_once("taskset unavailable");
        return Vec::new();
    }
    vec!["taskset".to_owned(), "-c".to_owned(), core.to_string()]
}

fn warn_pinning_once(reason: &str) {
    if !WARNED_TASKSET.swap(true, Ordering::SeqCst) {
        eprintln!("warning: {reason}; running unpinned");
    }
}

pub fn run_criterion(
    wt_ws_root: &Path,
    package: &str,
    bench: &str,
    profile: &str,
    short_sha: &str,
    pin: &[String],
    json: bool,
) -> Result<()> {
    let mut cargo_args = builder::profile_config_args(wt_ws_root, profile)?;
    cargo_args.extend([
        "bench".to_owned(),
        "-p".to_owned(),
        package.to_owned(),
        "--bench".to_owned(),
        bench.to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
        "--".to_owned(),
        "--save-baseline".to_owned(),
        format!("bcmp-{short_sha}"),
    ]);

    let (program, args) = command_with_optional_prefix(pin, "cargo", &cargo_args);
    let mut child = Command::new(&program)
        .args(&args)
        .current_dir(wt_ws_root)
        .env("RUSTFLAGS", "-C target-cpu=native")
        .env("CARGO_TARGET_DIR", builder::target_dir(wt_ws_root))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = [0_u8; 8192];
        loop {
            match std::io::Read::read(&mut stdout, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if json {
                        let _ = std::io::Write::write_all(&mut std::io::stderr(), &buf[..n]);
                    } else {
                        let _ = std::io::Write::write_all(&mut std::io::stdout(), &buf[..n]);
                    }
                }
                Err(_) => break,
            }
        }
    });
    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || builder::tee_stderr_filtered(stderr));
    let status = child.wait()?;
    let _ = stdout_thread.join();
    let stderr = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(anyhow!(
            "command failed: {}\nstatus: {}\nstderr (last 20 lines):\n{}",
            git::command_line(&program, &args),
            status,
            git::tail_lines(&stderr, 20)
        ));
    }
    Ok(())
}

pub fn run_binary_interleaved(
    base: BinaryRun<'_>,
    candidate: BinaryRun<'_>,
    reps: u32,
    metric: &MetricSource,
    progress_pattern: Option<&Regex>,
    progress: &progress::Progress,
    cancelled: &AtomicBool,
) -> Result<(Vec<f64>, Vec<f64>, String, bool)> {
    let mut base_values = Vec::with_capacity(reps as usize);
    let mut candidate_values = Vec::with_capacity(reps as usize);
    let (unit, lower_is_better) = match metric {
        MetricSource::WallClock => ("s".to_owned(), true),
        MetricSource::Regex {
            higher_is_better, ..
        } => ("metric".to_owned(), !higher_is_better),
    };

    for i in 0..reps {
        if cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!("interrupted"));
        }
        progress.send(Event::RunStart {
            run: 2 * i + 1,
            total: 2 * reps,
            side: Side::Base,
        });
        base_values.push(run_one(&base, metric, progress_pattern, progress)?);
        progress.send(Event::RunEnd);
        if cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!("interrupted"));
        }
        progress.send(Event::RunStart {
            run: 2 * i + 2,
            total: 2 * reps,
            side: Side::Candidate,
        });
        candidate_values.push(run_one(&candidate, metric, progress_pattern, progress)?);
        progress.send(Event::RunEnd);
        if cancelled.load(Ordering::SeqCst) {
            return Err(anyhow!("interrupted"));
        }
    }
    Ok((base_values, candidate_values, unit, lower_is_better))
}

pub struct BinaryRun<'a> {
    pub exe: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub pin: &'a [String],
}

fn run_one(
    run: &BinaryRun<'_>,
    metric: &MetricSource,
    progress_pattern: Option<&Regex>,
    progress: &progress::Progress,
) -> Result<f64> {
    let exe = run.exe.display().to_string();
    let (program, args) = command_with_optional_prefix(run.pin, &exe, run.args);
    let t0 = Instant::now();
    let mut child = Command::new(&program)
        .args(&args)
        .current_dir(run.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let stdout_thread = drain_thread(stdout, progress_pattern, progress.sender());
    let stderr_thread = drain_thread(stderr, progress_pattern, progress.sender());
    let status = child.wait()?;
    let wall_secs = t0.elapsed().as_secs_f64();
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    if !status.success() {
        let output = std::process::Output {
            status,
            stdout,
            stderr,
        };
        return Err(git::output_error(&program, &args, &output));
    }
    match metric {
        MetricSource::WallClock => Ok(wall_secs),
        MetricSource::Regex { pattern, .. } => extract_regex_metric(pattern, &stdout, &stderr),
    }
}

fn drain_thread<R: std::io::Read + Send + 'static>(
    mut reader: R,
    progress_pattern: Option<&Regex>,
    progress_tx: Option<std::sync::mpsc::Sender<Event>>,
) -> std::thread::JoinHandle<Vec<u8>> {
    let progress_pattern = progress_pattern.cloned();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0_u8; 8192];
        let mut scanner = progress::LineScanner::default();
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if let (Some(pattern), Some(tx)) = (&progress_pattern, &progress_tx) {
                        scanner.push(&buf[..n], &mut |line| {
                            if let Some(fraction) = progress::progress_fraction(pattern, line) {
                                let _ = tx.send(Event::Fraction(fraction));
                            }
                        });
                    }
                }
                Err(_) => break,
            }
        }
        out
    })
}

fn extract_regex_metric(pattern: &Regex, stdout: &[u8], stderr: &[u8]) -> Result<f64> {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let caps = last_caps(pattern, &stdout).or_else(|| last_caps(pattern, &stderr));
    let caps = caps.ok_or_else(|| {
        anyhow!(
            "metric regex did not match run output; stdout (first 30 lines):\n{}",
            stdout.lines().take(30).collect::<Vec<_>>().join("\n")
        )
    })?;
    let value = caps.ok_or_else(|| anyhow!("--metric-regex must contain a capture group"))?;
    value.parse::<f64>().with_context(|| {
        format!(
            "failed to parse metric value '{value}'; stdout (first 30 lines):\n{}",
            stdout.lines().take(30).collect::<Vec<_>>().join("\n")
        )
    })
}

fn last_caps(pattern: &Regex, text: &str) -> Option<Option<String>> {
    let mut last = None;
    for caps in pattern.captures_iter(text) {
        let value = caps
            .name("value")
            .or_else(|| caps.get(1))
            .map(|m| m.as_str().to_owned());
        last = Some(value);
    }
    last
}

fn command_with_optional_prefix(
    prefix: &[String],
    program: &str,
    args: &[String],
) -> (String, Vec<String>) {
    if prefix.is_empty() {
        (program.to_owned(), args.to_owned())
    } else {
        let prefix_program = prefix[0].clone();
        let mut all = prefix[1..].to_vec();
        all.push(program.to_owned());
        all.extend(args.iter().cloned());
        (prefix_program, all)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn run_one_drains_large_output_without_deadlock() {
        let script =
            "head -c 200000 /dev/zero | tr '\\0' x; head -c 200000 /dev/zero | tr '\\0' y 1>&2";
        let args = vec!["-c".to_owned(), script.to_owned()];
        let tmp = std::env::temp_dir().join(format!("bcmp-runner-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let progress = progress::Progress::new(false);
        let run = BinaryRun {
            exe: Path::new("sh"),
            args: &args,
            cwd: &tmp,
            pin: &[],
        };
        assert!(run_one(&run, &MetricSource::WallClock, None, &progress).is_ok());
        let _ = std::fs::remove_dir(&tmp);
    }
}
