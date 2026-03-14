//! Stop hook: runs full workspace lint and tests.
//!
//! Smart mode: only runs if files were actually changed during this session.
//! Session fingerprint is captured at session start (check-on-start.py) and
//! compared here. If nothing changed during the session, everything is skipped.
//!
//! Exit codes:
//!   0 - All passed or skipped (no changes during session)
//!   2 - Lint or test failures (stderr fed back to Claude)

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use wait_timeout::ChildExt;

const TIMEOUT_SECS: u64 = 120;

fn project_root() -> PathBuf {
    let current = env::current_dir().expect("cannot determine working directory");
    let mut dir = current.as_path();
    loop {
        if dir.join("Cargo.toml").exists() {
            if let Ok(content) = fs::read_to_string(dir.join("Cargo.toml")) {
                if content.contains("[workspace]") {
                    return dir.to_path_buf();
                }
            }
        }
        dir = match dir.parent() {
            Some(p) => p,
            None => return current,
        };
    }
}

fn current_fingerprint(root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    // Match Python's check-on-start.py: md5 of the stdout string as UTF-8
    Some(format!("{:x}", md5::compute(stdout.as_bytes())))
}

fn session_fingerprint(root: &Path) -> Option<String> {
    let path = root.join(".cache").join("session_fingerprint.json");
    let content = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("fingerprint")?.as_str().map(String::from)
}

fn should_skip(root: &Path) -> bool {
    match current_fingerprint(root) {
        None => true, // no changes right now
        Some(current) => match session_fingerprint(root) {
            None => false,                       // repo was clean at start → changes are new
            Some(session) => current == session, // same → no changes this session
        },
    }
}

// ---------------------------------------------------------------------------
// Process tree + thread dumping (replaces Python's tasklist/wmic/ps calls)
// ---------------------------------------------------------------------------

fn dump_process_tree(pid: u32, label: &str) {
    eprintln!("Process tree for {label} (PID {pid}):");

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    let target = sysinfo::Pid::from_u32(pid);

    // Dump target process
    if let Some(p) = sys.process(target) {
        eprintln!(
            "  PID={pid} name={} status={:?} cpu={:.1}% mem={}KB",
            p.name().to_string_lossy(),
            p.status(),
            p.cpu_usage(),
            p.memory() / 1024,
        );
        eprintln!("  cmd={:?}", p.cmd());
    }

    // Dump children recursively
    dump_children(&sys, target, 1);
}

fn dump_children(sys: &sysinfo::System, parent: sysinfo::Pid, depth: usize) {
    let indent = "  ".repeat(depth + 1);
    for (pid, p) in sys.processes() {
        if p.parent() == Some(parent) {
            eprintln!(
                "{indent}child PID={pid} name={} status={:?} cpu={:.1}% mem={}KB",
                p.name().to_string_lossy(),
                p.status(),
                p.cpu_usage(),
                p.memory() / 1024,
            );
            eprintln!("{indent}cmd={:?}", p.cmd());

            // Dump thread info where available
            dump_threads(*pid);

            // Recurse into grandchildren
            dump_children(sys, *pid, depth + 1);
        }
    }
}

/// Dump thread-level information for a process.
///
/// On Linux: enumerates `/proc/[pid]/task/` for thread IDs and status.
/// On other platforms: no-op (process-level info from sysinfo is sufficient).
fn dump_threads(pid: sysinfo::Pid) {
    #[cfg(target_os = "linux")]
    {
        let task_dir = format!("/proc/{pid}/task");
        if let Ok(entries) = fs::read_dir(&task_dir) {
            let tids: Vec<u32> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
                .collect();

            if tids.len() > 1 {
                eprintln!("    threads ({}):", tids.len());
                for tid in &tids {
                    let status_path = format!("/proc/{pid}/task/{tid}/status");
                    if let Ok(status) = fs::read_to_string(&status_path) {
                        let name = status
                            .lines()
                            .find(|l| l.starts_with("Name:"))
                            .map(|l| l.trim_start_matches("Name:").trim())
                            .unwrap_or("?");
                        let state = status
                            .lines()
                            .find(|l| l.starts_with("State:"))
                            .map(|l| l.trim_start_matches("State:").trim())
                            .unwrap_or("?");
                        eprintln!("      tid={tid} name={name} state={state}");
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
    }
}

// ---------------------------------------------------------------------------
// Pre-check: kill running daemon so cargo can replace the exe
// ---------------------------------------------------------------------------

fn kill_daemon() {
    use sysinfo::ProcessesToUpdate;

    let mut sys = sysinfo::System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    for (pid, process) in sys.processes() {
        let name = process.name().to_string_lossy();
        if name == "zccache-daemon" || name == "zccache-daemon.exe" {
            eprintln!("Killing running daemon (PID {pid}) to unlock target binaries");
            process.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Subprocess execution with timeout
// ---------------------------------------------------------------------------

fn run_streaming(root: &Path, cmd: &[String], label: &str) -> (i32, bool) {
    let mut child = match Command::new(&cmd[0])
        .args(&cmd[1..])
        .current_dir(root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{label}: failed to spawn: {e}");
            return (-1, false);
        }
    };

    match child.wait_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Some(status)) => (status.code().unwrap_or(-1), false),
        Ok(None) => {
            eprintln!("\n{}", "=".repeat(60));
            eprintln!("TIMEOUT: {label} exceeded {TIMEOUT_SECS}s — dumping process tree");
            eprintln!("{}", "=".repeat(60));

            dump_process_tree(child.id(), label);

            let _ = child.kill();
            let _ = child.wait();
            (-1, true)
        }
        Err(e) => {
            eprintln!("{label}: wait error: {e}");
            (-1, false)
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let root = project_root();

    if should_skip(&root) {
        eprintln!("Skipping stop checks (no changes during this session)");
        return ExitCode::SUCCESS;
    }

    // Kill any running daemon so cargo can replace the exe on Windows
    kill_daemon();

    eprintln!("Running full workspace checks (changes detected)");

    // Run lint first. If it fails, skip tests entirely.
    let lint_script = root.join("lint").to_string_lossy().to_string();
    let lint_cmd: Vec<String> = vec![
        "uv".into(),
        "run".into(),
        "--script".into(),
        lint_script,
        "--fix".into(),
    ];
    let (lint_rc, lint_timeout) = run_streaming(&root, &lint_cmd, "Lint");

    if lint_timeout {
        eprintln!("Lint timed out — skipping tests");
        return ExitCode::from(2);
    }
    if lint_rc != 0 {
        eprintln!("Lint failed — skipping tests");
        return ExitCode::from(2);
    }

    // Unit tests only — skip integration/stress tests (which need
    // a fully compiled binary and are gated behind --include-ignored).
    // Exclude zccache-daemon: its server tests start a real daemon with
    // IPC + file watcher and are effectively integration tests.
    let test_cmd: Vec<String> = vec![
        "uv".into(),
        "run".into(),
        "cargo".into(),
        "test".into(),
        "--workspace".into(),
        "--lib".into(),
        "--exclude".into(),
        "zccache-daemon".into(),
    ];
    let (test_rc, test_timeout) = run_streaming(&root, &test_cmd, "Tests");

    if test_timeout {
        eprintln!("Tests timed out — failing");
        return ExitCode::from(2);
    }
    if test_rc != 0 {
        eprintln!("Tests failed");
        return ExitCode::from(2);
    }

    eprintln!("All checks passed");
    ExitCode::SUCCESS
}
