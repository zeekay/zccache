//! Performance benchmark: warm-cache compilation latency.
//!
//! Compares single-file vs multi-file compilation modes on 50 translation units.
//! Also benchmarks against sccache if available.
//!
//! Run with: uv run cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

const NUM_FILES: usize = 50;
const WARM_TRIALS: usize = 5;

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

fn find_compiler() -> Option<PathBuf> {
    // Try custom clang-tool-chain first
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let custom = PathBuf::from(&home)
        .join(".clang-tool-chain")
        .join("clang")
        .join("win")
        .join("x86_64")
        .join("bin")
        .join("clang++.exe");
    if custom.exists() {
        return Some(custom);
    }

    // Try system LLVM
    let system = PathBuf::from("C:/Program Files/LLVM/bin/clang++.exe");
    if system.exists() {
        return Some(system);
    }

    // Try g++ on PATH
    if let Ok(output) = std::process::Command::new("g++").arg("--version").output() {
        if output.status.success() {
            return Some(PathBuf::from("g++"));
        }
    }

    None
}

fn find_sccache() -> Option<PathBuf> {
    // Check common locations
    for path in &["sccache", "C:/tools/python13/Scripts/sccache.exe"] {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
        // Try running it
        if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
            if output.status.success() {
                return Some(p);
            }
        }
    }
    None
}

/// Generate NUM_FILES lightweight C++ source files with a shared header.
fn generate_project(dir: &Path) {
    let incdir = dir.join("include");
    std::fs::create_dir_all(&incdir).unwrap();

    std::fs::write(
        incdir.join("common.h"),
        r#"#pragma once
#include <vector>
#include <string>
#include <cstdint>
namespace bench {
  template<typename T>
  inline T clamp(T v, T lo, T hi) { return v < lo ? lo : v > hi ? hi : v; }
}
"#,
    )
    .unwrap();

    for i in 0..NUM_FILES {
        let content = format!(
            r#"#include "common.h"
#include <cmath>
namespace unit_{i:03} {{
  double compute(int n) {{ return std::sin(n * 0.{i:03}1); }}
  std::vector<double> build(int n) {{
    std::vector<double> v(n);
    for (int j = 0; j < n; ++j) v[j] = compute(j);
    return v;
  }}
}}
"#,
        );
        std::fs::write(dir.join(format!("unit_{i:03}.cpp")), content).unwrap();
    }
}

fn clean_objects(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("o") {
            let _ = std::fs::remove_file(&path);
        }
    }
}

fn source_names() -> Vec<String> {
    (0..NUM_FILES).map(|i| format!("unit_{i:03}.cpp")).collect()
}

// ── zccache benchmarks (in-process daemon, no subprocess overhead) ──────

async fn zccache_compile_single(
    client: &mut zccache_ipc::IpcClientConnection,
    session_id: u64,
    compiler: &str,
    cwd: &str,
    sources: &[String],
) -> Duration {
    clean_objects(Path::new(cwd));
    let start = Instant::now();
    for src in sources {
        client
            .send(&Request::Compile {
                session_id,
                args: vec![
                    "-c".into(),
                    src.clone(),
                    "-o".into(),
                    src.replace(".cpp", ".o"),
                    "-Iinclude".into(),
                    "-O2".into(),
                    "-std=c++17".into(),
                ],
                cwd: cwd.into(),
                compiler: Some(compiler.into()),
                env: None,
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult { exit_code, .. }) => {
                assert_eq!(exit_code, 0, "compile failed for {src}");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
    }
    start.elapsed()
}

async fn zccache_compile_multi(
    client: &mut zccache_ipc::IpcClientConnection,
    session_id: u64,
    compiler: &str,
    cwd: &str,
    sources: &[String],
) -> Duration {
    clean_objects(Path::new(cwd));
    let mut args: Vec<String> = vec!["-c".into()];
    args.extend(sources.iter().cloned());
    args.extend(["-Iinclude".into(), "-O2".into(), "-std=c++17".into()]);

    let start = Instant::now();
    client
        .send(&Request::Compile {
            session_id,
            args,
            cwd: cwd.into(),
            compiler: Some(compiler.into()),
            env: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult { exit_code, .. }) => {
            assert_eq!(exit_code, 0, "multi-file compile failed");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    start.elapsed()
}

// ── sccache benchmark (subprocess) ──────────────────────────────────────

fn sccache_compile_single(
    sccache: &Path,
    compiler: &str,
    cwd: &Path,
    sources: &[String],
) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(sccache)
            .args([
                compiler,
                "-c",
                src,
                "-o",
                &src.replace(".cpp", ".o"),
                "-Iinclude",
                "-O2",
                "-std=c++17",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run sccache");
        assert!(status.success(), "sccache compile failed for {src}");
    }
    start.elapsed()
}

// sccache doesn't cache multi-file (-c a.cpp b.cpp), it passes through to compiler
fn sccache_compile_multi(
    sccache: &Path,
    compiler: &str,
    cwd: &Path,
    sources: &[String],
) -> Duration {
    clean_objects(cwd);
    let mut cmd = std::process::Command::new(sccache);
    cmd.arg(compiler).arg("-c");
    for src in sources {
        cmd.arg(src);
    }
    cmd.args(["-Iinclude", "-O2", "-std=c++17"]);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = Instant::now();
    let status = cmd.status().expect("failed to run sccache");
    assert!(status.success(), "sccache multi-file compile failed");
    start.elapsed()
}

// ── Baseline (direct compiler, no cache) ────────────────────────────────

fn baseline_single(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let start = Instant::now();
    for src in sources {
        let status = std::process::Command::new(compiler)
            .args([
                "-c",
                src,
                "-o",
                &src.replace(".cpp", ".o"),
                "-Iinclude",
                "-O2",
                "-std=c++17",
            ])
            .current_dir(cwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("failed to run compiler");
        assert!(status.success(), "compile failed for {src}");
    }
    start.elapsed()
}

fn baseline_multi(compiler: &str, cwd: &Path, sources: &[String]) -> Duration {
    clean_objects(cwd);
    let mut cmd = std::process::Command::new(compiler);
    cmd.arg("-c");
    for src in sources {
        cmd.arg(src);
    }
    cmd.args(["-Iinclude", "-O2", "-std=c++17"]);
    cmd.current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let start = Instant::now();
    let status = cmd.status().expect("failed to run compiler");
    assert!(status.success(), "multi-file compile failed");
    start.elapsed()
}

// ── Reporting ───────────────────────────────────────────────────────────

fn median(times: &[Duration]) -> Duration {
    let mut sorted: Vec<Duration> = times.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

fn fmt_dur(d: Duration) -> String {
    format!("{:.3}s", d.as_secs_f64())
}

fn print_trials(label: &str, times: &[Duration]) {
    let med = median(times);
    let min = times.iter().min().unwrap();
    let max = times.iter().max().unwrap();
    let trials: Vec<String> = times.iter().map(|t| fmt_dur(*t)).collect();
    eprintln!(
        "  {label:<40} median={:<10} min={:<10} max={:<10} trials={:?}",
        fmt_dur(med),
        fmt_dur(*min),
        fmt_dur(*max),
        trials,
    );
}

fn print_speedup(label: &str, baseline: Duration, test: Duration) {
    let speedup = baseline.as_secs_f64() / test.as_secs_f64();
    eprintln!("  {label:<45} {speedup:>6.1}x");
}

// ── Main benchmark ──────────────────────────────────────────────────────

#[tokio::test]
#[ignore] // Run explicitly: uv run cargo test -p zccache-daemon --test perf_bench_test -- --nocapture --ignored
async fn perf_warm_cache_zccache_vs_sccache() {
    let compiler_path = match find_compiler() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let sources = source_names();

    eprintln!("\n=== Warm-Cache Performance Benchmark ===");
    eprintln!("  Files: {NUM_FILES} C++ translation units");
    eprintln!("  Trials: {WARM_TRIALS} (warm cache)");
    eprintln!("  Compiler: {compiler}");
    eprintln!("  Workdir: {cwd}\n");

    generate_project(tmp.path());

    // ── Baseline ────────────────────────────────────────────────────
    eprintln!("--- Baseline (no cache) ---");
    let bl_single = baseline_single(&compiler, tmp.path(), &sources);
    eprintln!(
        "  Single-file ({NUM_FILES} invocations): {}",
        fmt_dur(bl_single)
    );
    let bl_multi = baseline_multi(&compiler, tmp.path(), &sources);
    eprintln!("  Multi-file  (1 invocation):  {}\n", fmt_dur(bl_multi));

    // ── sccache ─────────────────────────────────────────────────────
    let sccache_single_times;
    let sccache_multi_times;
    let mut sccache_cold_single = None;
    let mut sccache_cold_multi = None;

    if let Some(sccache_bin) = find_sccache() {
        eprintln!("--- sccache ({}) ---", sccache_bin.display());

        // Start fresh server
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Cold single-file pass to populate cache
        eprint!("  Cold single-file...");
        let cold_s = sccache_compile_single(&sccache_bin, &compiler, tmp.path(), &sources);
        eprintln!(" {}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        // Warm trials: single-file
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                tmp.path(),
                &sources,
            ));
        }
        print_trials("sccache single-file (warm)", &times);
        sccache_single_times = Some(times);

        // Stop server, clear cache, restart for cold multi-file
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        // Clear sccache disk cache
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--zero-stats")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new(&sccache_bin)
            .arg("--start-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        // Cold multi-file (sccache can't cache multi-file — passes through to compiler)
        eprint!("  Cold multi-file...");
        let cold_m = sccache_compile_multi(&sccache_bin, &compiler, tmp.path(), &sources);
        eprintln!(" {}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        // Warm trials: multi-file (sccache can't cache this — passes through)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                tmp.path(),
                &sources,
            ));
        }
        print_trials("sccache multi-file (warm)", &times);
        sccache_multi_times = Some(times);

        let _ = std::process::Command::new(&sccache_bin)
            .arg("--stop-server")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        eprintln!();
    } else {
        eprintln!("--- sccache: not found, skipping ---\n");
        sccache_single_times = None;
        sccache_multi_times = None;
    }

    // ── zccache (in-process daemon) ─────────────────────────────────
    eprintln!("--- zccache (in-process daemon) ---");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone(),
            compiler: compiler.clone(),
            log_file: None,
            track_stats: true,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // Cold multi-file (cache is empty)
    eprint!("  Cold multi-file...");
    let zc_cold_multi =
        zccache_compile_multi(&mut client, session_id, &compiler, &cwd, &sources).await;
    eprintln!(" {}", fmt_dur(zc_cold_multi));

    // Cold single-file (cache is now populated from multi, so clear it)
    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;

    eprint!("  Cold single-file...");
    let zc_cold_single =
        zccache_compile_single(&mut client, session_id, &compiler, &cwd, &sources).await;
    eprintln!(" {}", fmt_dur(zc_cold_single));

    // Warm trials: single-file
    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times
            .push(zccache_compile_single(&mut client, session_id, &compiler, &cwd, &sources).await);
    }
    print_trials("zccache single-file (warm)", &zc_single_times);

    // Warm trials: multi-file
    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times
            .push(zccache_compile_multi(&mut client, session_id, &compiler, &cwd, &sources).await);
    }
    print_trials("zccache multi-file (warm)", &zc_multi_times);

    // End session
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Summary ─────────────────────────────────────────────────────
    let zc_single_med = median(&zc_single_times);
    let zc_multi_med = median(&zc_multi_times);

    eprintln!("\n======================================================");
    eprintln!("RESULTS SUMMARY");
    eprintln!("======================================================");

    eprintln!("\n  --- Cold (cache empty) ---");
    eprintln!("  {:<40} {:<10}", "Scenario", "Time");
    eprintln!("  {:-<40} {:-<10}", "", "");
    eprintln!(
        "  {:<40} {}",
        "Baseline single (no cache)",
        fmt_dur(bl_single)
    );
    eprintln!(
        "  {:<40} {}",
        "Baseline multi  (no cache)",
        fmt_dur(bl_multi)
    );
    if let Some(t) = sccache_cold_single {
        eprintln!("  {:<40} {}", "sccache single  (cold)", fmt_dur(t));
    }
    if let Some(t) = sccache_cold_multi {
        eprintln!("  {:<40} {}", "sccache multi   (cold)", fmt_dur(t));
    }
    eprintln!(
        "  {:<40} {}",
        "zccache single  (cold)",
        fmt_dur(zc_cold_single)
    );
    eprintln!(
        "  {:<40} {}",
        "zccache multi   (cold)",
        fmt_dur(zc_cold_multi)
    );

    eprintln!("\n  --- Warm (median of {WARM_TRIALS} trials) ---");
    eprintln!("  {:<40} {:<10}", "Scenario", "Time");
    eprintln!("  {:-<40} {:-<10}", "", "");
    if let Some(ref t) = sccache_single_times {
        eprintln!("  {:<40} {}", "sccache single  (warm)", fmt_dur(median(t)));
    }
    if let Some(ref t) = sccache_multi_times {
        eprintln!("  {:<40} {}", "sccache multi   (warm)", fmt_dur(median(t)));
    }
    eprintln!(
        "  {:<40} {}",
        "zccache single  (warm)",
        fmt_dur(zc_single_med)
    );
    eprintln!(
        "  {:<40} {}",
        "zccache multi   (warm)",
        fmt_dur(zc_multi_med)
    );

    eprintln!("\n  --- Speedups (warm) ---");
    print_speedup(
        "zccache single vs baseline single",
        bl_single,
        zc_single_med,
    );
    print_speedup("zccache multi  vs baseline multi", bl_multi, zc_multi_med);
    if let Some(ref t) = sccache_single_times {
        print_speedup("zccache single vs sccache single", median(t), zc_single_med);
    }
    if let Some(ref t) = sccache_multi_times {
        print_speedup("zccache multi  vs sccache multi", median(t), zc_multi_med);
    }
    print_speedup(
        "zccache multi  vs zccache single",
        zc_single_med,
        zc_multi_med,
    );
    eprintln!();
}
