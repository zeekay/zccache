//! Performance comparison: zccache (in-memory) vs sccache (disk-based).
//!
//! Benchmarks cache-hit latency for single-file compilations.
//! Run with: uv run cargo test -p zccache-daemon --test perf_test -- --nocapture --ignored

use std::path::PathBuf;
use std::time::Instant;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

fn find_clang() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let clang_path = PathBuf::from(&home)
        .join(".clang-tool-chain")
        .join("clang")
        .join("win")
        .join("x86_64")
        .join("bin")
        .join("clang++.exe");
    if clang_path.exists() {
        Some(clang_path)
    } else {
        None
    }
}

fn find_sccache() -> Option<PathBuf> {
    // Try common locations
    for path in &[
        "sccache",
        "sccache.exe",
        "/c/tools/python13/Scripts/sccache",
    ] {
        if let Ok(output) = std::process::Command::new(path).arg("--version").output() {
            if output.status.success() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

async fn start_daemon() -> (
    String,
    tokio::task::JoinHandle<()>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run().await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(
    client: &mut ClientConn,
    clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> u64 {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string(),
            compiler: clang.to_string_lossy().into_owned(),
            log_file: Some(log_file.to_string()),
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

async fn compile(
    client: &mut ClientConn,
    session_id: u64,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id,
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => (exit_code, cached),
        Some(Response::Error { message }) => panic!("compile error: {message}"),
        other => panic!("expected CompileResult, got: {other:?}"),
    }
}

/// Generate test source files of varying complexity.
fn generate_test_files(dir: &std::path::Path, count: usize) {
    // A header shared by all files
    let header = dir.join("common.h");
    std::fs::write(
        &header,
        r#"#pragma once
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

inline int common_add(int a, int b) { return a + b; }
inline int common_mul(int a, int b) { return a * b; }
"#,
    )
    .unwrap();

    for i in 0..count {
        let src = dir.join(format!("file_{i}.cpp"));
        std::fs::write(
            &src,
            format!(
                r#"#include "common.h"

static int compute_{i}(int x) {{
    int result = 0;
    for (int j = 0; j < x; j++) {{
        result = common_add(result, common_mul(j, {i}));
    }}
    return result;
}}

int func_{i}() {{
    return compute_{i}(100);
}}
"#
            ),
        )
        .unwrap();
    }
}

struct BenchResult {
    label: String,
    cold_ms: Vec<f64>,
    warm_ms: Vec<f64>,
}

/// Benchmark sccache: shell out to `sccache clang++ -c file.cpp -o file.o`.
fn bench_sccache(
    sccache: &std::path::Path,
    clang: &std::path::Path,
    src_dir: &std::path::Path,
    file_count: usize,
    warm_iterations: usize,
) -> BenchResult {
    // Clear sccache stats
    let _ = std::process::Command::new(sccache)
        .arg("--zero-stats")
        .output();

    // Start sccache server
    let _ = std::process::Command::new(sccache)
        .arg("--start-server")
        .output();

    let mut cold_ms = Vec::new();
    let mut warm_ms = Vec::new();
    let cwd = src_dir.to_string_lossy().into_owned();

    // Cold pass (cache miss)
    for i in 0..file_count {
        let src = format!("file_{i}.cpp");
        let obj = format!("file_{i}.o");

        let start = Instant::now();
        let output = std::process::Command::new(sccache)
            .arg(clang.to_string_lossy().as_ref())
            .args(["-c", &src, "-o", &obj])
            .current_dir(&cwd)
            .output()
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            output.status.success(),
            "sccache cold compile failed for {src}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        cold_ms.push(elapsed.as_secs_f64() * 1000.0);
    }

    // Warm passes (cache hit)
    for _ in 0..warm_iterations {
        for i in 0..file_count {
            let src = format!("file_{i}.cpp");
            let obj = format!("file_{i}.o");

            // Delete .o to force cache retrieval
            let _ = std::fs::remove_file(src_dir.join(&obj));

            let start = Instant::now();
            let output = std::process::Command::new(sccache)
                .arg(clang.to_string_lossy().as_ref())
                .args(["-c", &src, "-o", &obj])
                .current_dir(&cwd)
                .output()
                .unwrap();
            let elapsed = start.elapsed();

            assert!(
                output.status.success(),
                "sccache warm compile failed for {src}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            warm_ms.push(elapsed.as_secs_f64() * 1000.0);
        }
    }

    BenchResult {
        label: format!("sccache {}", sccache_version(sccache)),
        cold_ms,
        warm_ms,
    }
}

fn sccache_version(sccache: &std::path::Path) -> String {
    std::process::Command::new(sccache)
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Benchmark zccache: use IPC to daemon.
async fn bench_zccache(
    clang: &std::path::Path,
    src_dir: &std::path::Path,
    file_count: usize,
    warm_iterations: usize,
) -> BenchResult {
    let cwd = src_dir.to_string_lossy().into_owned();
    let log = src_dir.join("zccache_bench.log");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, clang, &cwd, &log.to_string_lossy()).await;

    let mut cold_ms = Vec::new();
    let mut warm_ms = Vec::new();

    // Cold pass (cache miss)
    for i in 0..file_count {
        let src = format!("file_{i}.cpp");
        let obj = format!("file_{i}.o");

        let start = Instant::now();
        let (exit_code, cached) = compile(&mut client, sid, &["-c", &src, "-o", &obj], &cwd).await;
        let elapsed = start.elapsed();

        assert_eq!(exit_code, 0, "zccache cold compile failed for {src}");
        assert!(!cached, "first compile should be a miss");
        cold_ms.push(elapsed.as_secs_f64() * 1000.0);
    }

    // Warm passes (cache hit)
    for _ in 0..warm_iterations {
        for i in 0..file_count {
            let src = format!("file_{i}.cpp");
            let obj = format!("file_{i}.o");

            // Delete .o to force cache retrieval
            let _ = std::fs::remove_file(src_dir.join(&obj));

            let start = Instant::now();
            let (exit_code, cached) =
                compile(&mut client, sid, &["-c", &src, "-o", &obj], &cwd).await;
            let elapsed = start.elapsed();

            assert_eq!(exit_code, 0, "zccache warm compile failed for {src}");
            assert!(cached, "recompile should be a hit");
            warm_ms.push(elapsed.as_secs_f64() * 1000.0);
        }
    }

    shutdown.notify_one();
    server_handle.await.unwrap();

    BenchResult {
        label: "zccache (in-memory)".to_string(),
        cold_ms,
        warm_ms,
    }
}

fn stats(values: &[f64]) -> (f64, f64, f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    (mean, median, min, max)
}

fn print_results(results: &[BenchResult]) {
    let sep = "=".repeat(78);
    let dash = "-".repeat(60);

    println!("\n{sep}");
    println!("  PERFORMANCE COMPARISON: zccache vs sccache");
    println!("{sep}\n");

    for r in results {
        let (cold_mean, cold_med, cold_min, cold_max) = stats(&r.cold_ms);
        let (warm_mean, warm_med, warm_min, warm_max) = stats(&r.warm_ms);

        println!("  {}", r.label);
        println!("  {dash}");
        println!(
            "  Cold (cache miss):  mean={cold_mean:>8.2}ms  median={cold_med:>8.2}ms  min={cold_min:>8.2}ms  max={cold_max:>8.2}ms  (n={})",
            r.cold_ms.len()
        );
        println!(
            "  Warm (cache hit):   mean={warm_mean:>8.2}ms  median={warm_med:>8.2}ms  min={warm_min:>8.2}ms  max={warm_max:>8.2}ms  (n={})",
            r.warm_ms.len()
        );
        println!();
    }

    // Speedup comparison
    if results.len() == 2 {
        let (_, sccache_warm_med, _, _) = stats(&results[0].warm_ms);
        let (_, zccache_warm_med, _, _) = stats(&results[1].warm_ms);
        let speedup = sccache_warm_med / zccache_warm_med;

        let (_, sccache_cold_med, _, _) = stats(&results[0].cold_ms);
        let (_, zccache_cold_med, _, _) = stats(&results[1].cold_ms);
        let cold_speedup = sccache_cold_med / zccache_cold_med;

        println!("  SPEEDUP (median)");
        println!("  {dash}");
        println!(
            "  Cache hit:   zccache is {speedup:>6.1}x faster  ({zccache_warm_med:.2}ms vs {sccache_warm_med:.2}ms)"
        );
        println!(
            "  Cache miss:  zccache is {cold_speedup:>6.1}x faster  ({zccache_cold_med:.2}ms vs {sccache_cold_med:.2}ms)"
        );
        println!();
    }

    println!("{sep}");
}

/// Main benchmark: compile 10 files, 50 warm iterations each.
///
/// Run with: uv run cargo test -p zccache-daemon --test perf_test -- --nocapture --ignored
#[tokio::test]
#[ignore]
async fn perf_zccache_vs_sccache() {
    let clang = match find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found at ~/.clang-tool-chain");
            return;
        }
    };
    let sccache = match find_sccache() {
        Some(p) => p,
        None => {
            println!("SKIP: sccache not found");
            return;
        }
    };

    let file_count = 10;
    let warm_iterations = 50;

    // Separate temp dirs so caches don't interfere
    let sccache_dir = tempfile::tempdir().unwrap();
    let zccache_dir = tempfile::tempdir().unwrap();

    // Generate identical test files in both dirs
    generate_test_files(sccache_dir.path(), file_count);
    generate_test_files(zccache_dir.path(), file_count);

    println!("\nBenchmarking with {file_count} files x {warm_iterations} warm iterations...\n");

    // Benchmark sccache first (blocking, runs external process)
    println!("  Running sccache benchmark...");
    let sccache_result = bench_sccache(
        &sccache,
        &clang,
        sccache_dir.path(),
        file_count,
        warm_iterations,
    );
    println!("  sccache done.");

    // Benchmark zccache
    println!("  Running zccache benchmark...");
    let zccache_result =
        bench_zccache(&clang, zccache_dir.path(), file_count, warm_iterations).await;
    println!("  zccache done.");

    print_results(&[sccache_result, zccache_result]);
}

/// Quick sanity check that both tools produce cache hits.
#[tokio::test]
#[ignore]
async fn perf_sanity_check() {
    let clang = match find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found");
            return;
        }
    };
    let sccache = match find_sccache() {
        Some(p) => p,
        None => {
            println!("SKIP: sccache not found");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    generate_test_files(tmp.path(), 1);
    let cwd = tmp.path().to_string_lossy().into_owned();

    // sccache: cold then warm
    let _ = std::process::Command::new(&sccache)
        .arg("--start-server")
        .output();
    let _ = std::process::Command::new(&sccache)
        .arg("--zero-stats")
        .output();

    let out = std::process::Command::new(&sccache)
        .arg(clang.to_string_lossy().as_ref())
        .args(["-c", "file_0.cpp", "-o", "file_0.o"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "sccache cold failed");

    std::fs::remove_file(tmp.path().join("file_0.o")).unwrap();
    let out = std::process::Command::new(&sccache)
        .arg(clang.to_string_lossy().as_ref())
        .args(["-c", "file_0.cpp", "-o", "file_0.o"])
        .current_dir(&cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "sccache warm failed");

    // Verify sccache got a hit
    let stats_out = std::process::Command::new(&sccache)
        .arg("--show-stats")
        .output()
        .unwrap();
    let stats_text = String::from_utf8_lossy(&stats_out.stdout);
    println!("sccache stats:\n{stats_text}");

    // zccache: cold then warm
    let zcc_tmp = tempfile::tempdir().unwrap();
    generate_test_files(zcc_tmp.path(), 1);
    let zcc_cwd = zcc_tmp.path().to_string_lossy().into_owned();
    let log = zcc_tmp.path().join("log.txt");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &zcc_cwd, &log.to_string_lossy()).await;

    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", "file_0.cpp", "-o", "file_0.o"],
        &zcc_cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "first compile should miss");

    std::fs::remove_file(zcc_tmp.path().join("file_0.o")).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", "file_0.cpp", "-o", "file_0.o"],
        &zcc_cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "second compile should hit");

    let log_text = std::fs::read_to_string(&log).unwrap();
    println!("zccache log:\n{log_text}");

    shutdown.notify_one();
    server_handle.await.unwrap();
    println!("\nSanity check passed: both tools produce cache hits.");
}
