use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::git;
use crate::progress::{self, Side};

pub fn target_dir(wt_ws_root: &Path) -> PathBuf {
    wt_ws_root.join("target")
}

pub fn profile_config_args(wt_ws_root: &Path, profile: &str) -> Result<Vec<String>> {
    if matches!(profile, "release" | "dev" | "test" | "bench") {
        return Ok(Vec::new());
    }
    let manifest = wt_ws_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("failed to read {}", manifest.display()))?;
    if text.contains(&format!("[profile.{profile}]")) {
        Ok(Vec::new())
    } else {
        Ok(vec![
            "--config".to_owned(),
            format!("profile.{profile}.inherits=\"release\""),
        ])
    }
}

/// Tee a child's stderr to ours while dropping cargo's `Finished ...` status
/// lines (pure noise between runs); returns everything read, unfiltered, for
/// error reporting. Compile progress and diagnostics still stream through.
pub fn tee_stderr_filtered(mut stderr: impl std::io::Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut line = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match std::io::Read::read(&mut stderr, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                for &byte in &buf[..n] {
                    line.push(byte);
                    if byte == b'\n' {
                        forward_stderr_line(&line);
                        line.clear();
                    }
                }
            }
            Err(_) => break,
        }
    }
    forward_stderr_line(&line);
    bytes
}

fn forward_stderr_line(line: &[u8]) {
    if String::from_utf8_lossy(line)
        .trim_start()
        .starts_with("Finished ")
    {
        return;
    }
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line);
}

enum BuildLine {
    Compiling(String),
    Status(String),
    Warning,
    Other,
}

fn classify_build_line(line: &str) -> BuildLine {
    let trimmed = line.trim();
    if trimmed.starts_with("Compiling ") {
        return BuildLine::Compiling(trimmed.to_owned());
    }
    for status in [
        "Downloading ",
        "Downloaded ",
        "Updating ",
        "Locking ",
        "Blocking ",
        "Checking ",
    ] {
        if trimmed.starts_with(status) {
            return BuildLine::Status(trimmed.to_owned());
        }
    }
    if trimmed.starts_with("warning") {
        return BuildLine::Warning;
    }
    BuildLine::Other
}

/// Drain a cargo build's stderr. With a status bar (TTY, progress enabled):
/// a single updating line shows the crate currently compiled and a counter,
/// while everything read is still captured for error reporting. Without one:
/// tee through like before.
fn drain_build_stderr(mut stderr: impl std::io::Read, status: Option<Side>) -> Vec<u8> {
    let Some(bar) = progress::build_status_bar(status) else {
        return tee_stderr_filtered(stderr);
    };
    let mut bytes = Vec::new();
    let mut scanner = progress::LineScanner::default();
    let mut compiled: u32 = 0;
    let mut warnings = false;
    let mut buf = [0_u8; 8192];
    loop {
        match std::io::Read::read(&mut stderr, &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                scanner.push(&buf[..n], &mut |line| match classify_build_line(line) {
                    BuildLine::Compiling(text) => {
                        compiled += 1;
                        let noun = if compiled == 1 { "crate" } else { "crates" };
                        bar.set_message(format!("{text} · {compiled} {noun}"));
                    }
                    BuildLine::Status(text) => bar.set_message(text),
                    BuildLine::Warning => warnings = true,
                    BuildLine::Other => {}
                });
            }
            Err(_) => break,
        }
    }
    bar.finish_and_clear();
    if warnings {
        eprintln!("warning: cargo emitted warnings during the build (hidden by the status line)");
    }
    bytes
}

fn run_cargo_capture_stdout(
    wt_ws_root: &Path,
    args: &[String],
    status_side: Option<Side>,
) -> Result<String> {
    let mut child = Command::new("cargo")
        .args(args)
        .current_dir(wt_ws_root)
        .env("RUSTFLAGS", "-C target-cpu=native")
        .env("CARGO_TARGET_DIR", target_dir(wt_ws_root))
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| "failed to run cargo")?;

    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || drain_build_stderr(stderr, status_side));

    let mut stdout = Vec::new();
    std::io::Read::read_to_end(child.stdout.as_mut().expect("stdout piped"), &mut stdout)?;
    let status = child.wait()?;
    let stderr = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(anyhow!(
            "command failed: {}\nstatus: {}\nstderr (last 20 lines):\n{}",
            git::command_line("cargo", args),
            status,
            git::tail_lines(&stderr, 20)
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn run_cargo_status(
    wt_ws_root: &Path,
    args: &[String],
    json: bool,
    status_side: Option<Side>,
) -> Result<()> {
    let mut child = Command::new("cargo")
        .args(args)
        .current_dir(wt_ws_root)
        .env("RUSTFLAGS", "-C target-cpu=native")
        .env("CARGO_TARGET_DIR", target_dir(wt_ws_root))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to run cargo")?;

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
    let stderr_thread = std::thread::spawn(move || drain_build_stderr(stderr, status_side));
    let status = child.wait()?;
    let _ = stdout_thread.join();
    let stderr = stderr_thread.join().unwrap_or_default();

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(anyhow!(
            "command failed: {}\nstatus: {}\nstderr (last 20 lines):\n{}",
            git::command_line("cargo", args),
            status,
            git::tail_lines(&stderr, 20)
        ));
    }
    Ok(())
}

pub fn build_bin(
    wt_ws_root: &Path,
    package: &str,
    bin: &str,
    profile: &str,
    status_side: Option<Side>,
) -> Result<PathBuf> {
    let mut args = profile_config_args(wt_ws_root, profile)?;
    args.extend([
        "build".to_owned(),
        "-p".to_owned(),
        package.to_owned(),
        "--bin".to_owned(),
        bin.to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
        "--message-format=json-render-diagnostics".to_owned(),
    ]);
    let stdout = run_cargo_capture_stdout(wt_ws_root, &args, status_side)?;
    let wanted = normalize_name(bin);
    let mut executable = None;
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            continue;
        }
        let target_name = value
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(normalize_name);
        if target_name.as_deref() != Some(&wanted) {
            continue;
        }
        if let Some(path) = value.get("executable").and_then(Value::as_str) {
            executable = Some(PathBuf::from(path));
        }
    }
    executable
        .ok_or_else(|| anyhow!("build succeeded but no executable artifact found for bin '{bin}'"))
}

pub fn build_bench(
    wt_ws_root: &Path,
    package: &str,
    bench: &str,
    profile: &str,
    json: bool,
    status_side: Option<Side>,
) -> Result<()> {
    let mut args = profile_config_args(wt_ws_root, profile)?;
    args.extend([
        "bench".to_owned(),
        "-p".to_owned(),
        package.to_owned(),
        "--bench".to_owned(),
        bench.to_owned(),
        "--profile".to_owned(),
        profile.to_owned(),
        "--no-run".to_owned(),
    ]);
    run_cargo_status(wt_ws_root, &args, json, status_side)
}

fn normalize_name(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_cargo_build_lines() {
        assert!(matches!(
            classify_build_line("   Compiling proc-macro2 v1.0.106"),
            BuildLine::Compiling(t) if t == "Compiling proc-macro2 v1.0.106"
        ));
        assert!(matches!(
            classify_build_line("    Blocking waiting for file lock on build directory"),
            BuildLine::Status(t) if t.starts_with("Blocking")
        ));
        assert!(matches!(
            classify_build_line("warning: unused variable: `x`"),
            BuildLine::Warning
        ));
        // diagnostics and the Finished line must NOT drive the status message
        assert!(matches!(
            classify_build_line("error[E0308]: mismatched types"),
            BuildLine::Other
        ));
        assert!(matches!(
            classify_build_line("    Finished `release-tuned` profile [optimized] target(s)"),
            BuildLine::Other
        ));
    }
}
