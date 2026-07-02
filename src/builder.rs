use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;

use crate::git;

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

fn run_cargo_capture_stdout(wt_ws_root: &Path, args: &[String]) -> Result<String> {
    let mut child = Command::new("cargo")
        .args(args)
        .current_dir(wt_ws_root)
        .env("RUSTFLAGS", "-C target-cpu=native")
        .env("CARGO_TARGET_DIR", target_dir(wt_ws_root))
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| "failed to run cargo")?;

    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match std::io::Read::read(&mut stderr, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&buf[..n]);
                    let _ = std::io::Write::write_all(&mut std::io::stderr(), &buf[..n]);
                }
                Err(_) => break,
            }
        }
        bytes
    });

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

fn run_cargo_status(wt_ws_root: &Path, args: &[String]) -> Result<()> {
    let mut child = Command::new("cargo")
        .args(args)
        .current_dir(wt_ws_root)
        .env("RUSTFLAGS", "-C target-cpu=native")
        .env("CARGO_TARGET_DIR", target_dir(wt_ws_root))
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to run cargo")?;

    let mut stderr = child.stderr.take().expect("stderr piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match std::io::Read::read(&mut stderr, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    bytes.extend_from_slice(&buf[..n]);
                    let _ = std::io::Write::write_all(&mut std::io::stderr(), &buf[..n]);
                }
                Err(_) => break,
            }
        }
        bytes
    });
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
    Ok(())
}

pub fn build_bin(wt_ws_root: &Path, package: &str, bin: &str, profile: &str) -> Result<PathBuf> {
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
    let stdout = run_cargo_capture_stdout(wt_ws_root, &args)?;
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

pub fn build_bench(wt_ws_root: &Path, package: &str, bench: &str, profile: &str) -> Result<()> {
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
    run_cargo_status(wt_ws_root, &args)
}

fn normalize_name(name: &str) -> String {
    name.replace('-', "_")
}
