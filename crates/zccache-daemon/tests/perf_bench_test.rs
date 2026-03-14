//! Performance benchmark: warm-cache compilation latency.
//!
//! Compares single-file vs multi-file compilation modes on 50 translation units.
//! Each tool gets its own fresh tempdir to avoid OS page cache cross-contamination.
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

fn find_sccache() -> Option<PathBuf> {
    for path in &["sccache", "C:/tools/python13/Scripts/sccache.exe"] {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
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

/// Run clang on one file to warm the OS page cache (compiler binary + system headers).
/// This normalizes page cache state before each cold measurement so all tools
/// start from the same baseline.
fn warmup_compiler(compiler: &str, dir: &Path) {
    let src = dir.join("unit_000.cpp");
    let obj = dir.join("_warmup.o");
    let status = std::process::Command::new(compiler)
        .args(["-c", "-Iinclude", "-O2", "-std=c++17"])
        .arg(&src)
        .arg("-o")
        .arg(&obj)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("warmup compile failed");
    assert!(status.success(), "warmup compile failed");
    let _ = std::fs::remove_file(&obj);
}

/// Delete all files in dir and regenerate the project from scratch.
fn nuke_and_regenerate(dir: &Path) {
    // Remove everything inside the directory
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    generate_project(dir);
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
    session_id: &str,
    compiler: &str,
    cwd: &str,
    sources: &[String],
) -> Duration {
    clean_objects(Path::new(cwd));
    let start = Instant::now();
    for src in sources {
        client
            .send(&Request::Compile {
                session_id: session_id.to_string(),
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
                compiler: compiler.to_string(),
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
    session_id: &str,
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
            session_id: session_id.to_string(),
            args,
            cwd: cwd.into(),
            compiler: compiler.to_string(),
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
    let compiler_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no C++ compiler found");
            return;
        }
    };
    let compiler = compiler_path.to_string_lossy().to_string();
    let sources = source_names();

    eprintln!("\n=== Warm-Cache Performance Benchmark ===");
    eprintln!("  Files: {NUM_FILES} C++ translation units");
    eprintln!("  Trials: {WARM_TRIALS} (warm cache)");
    eprintln!("  Compiler: {compiler}");
    eprintln!("  Note: each tool gets its own fresh tempdir\n");

    // ── Baseline (fresh dir) ──────────────────────────────────────────
    let bl_dir = tempfile::tempdir().unwrap();
    generate_project(bl_dir.path());

    eprintln!("--- Baseline (no cache) ---");

    // Single-file: nuke, regenerate, warmup clang once to normalize page cache
    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!(
        "  Single-file cold ({NUM_FILES} invocations): {}",
        fmt_dur(bl_cold_single)
    );

    // Run again — now OS page cache is warm for this dir
    let bl_warm_single = baseline_single(&compiler, bl_dir.path(), &sources);
    eprintln!(
        "  Single-file warm ({NUM_FILES} invocations): {}",
        fmt_dur(bl_warm_single)
    );

    // Multi-file: nuke, regenerate, warmup clang
    nuke_and_regenerate(bl_dir.path());
    warmup_compiler(&compiler, bl_dir.path());
    let bl_cold_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!(
        "  Multi-file  cold (1 invocation):  {}",
        fmt_dur(bl_cold_multi)
    );

    let bl_warm_multi = baseline_multi(&compiler, bl_dir.path(), &sources);
    eprintln!(
        "  Multi-file  warm (1 invocation):  {}\n",
        fmt_dur(bl_warm_multi)
    );

    drop(bl_dir);

    // ── sccache (fresh dir) ───────────────────────────────────────────
    let sccache_cold_single;
    let sccache_cold_multi;
    let sccache_single_times;
    let sccache_multi_times;

    if let Some(sccache_bin) = find_sccache() {
        let sc_dir = tempfile::tempdir().unwrap();
        generate_project(sc_dir.path());

        // Use a fresh cache dir so previous sccache usage doesn't pollute results.
        let sc_cache_dir = tempfile::tempdir().unwrap();
        let sc_cache_str = sc_cache_dir.path().to_string_lossy().into_owned();

        // Set SCCACHE_DIR for this process so both server and client see it.
        std::env::set_var("SCCACHE_DIR", &sc_cache_str);

        eprintln!("--- sccache ({}) ---", sccache_bin.display());
        eprintln!("  Cache dir: {sc_cache_str}");

        // Helper: stop server, purge disk cache, restart with fresh SCCACHE_DIR.
        let stop_purge_start = |sccache: &Path, cache_dir: &str| {
            let _ = std::process::Command::new(sccache)
                .arg("--stop-server")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            // Purge disk cache
            let cache_path = std::path::Path::new(cache_dir);
            if cache_path.exists() {
                let _ = std::fs::remove_dir_all(cache_path);
                let _ = std::fs::create_dir_all(cache_path);
            }
            let _ = std::process::Command::new(sccache)
                .arg("--start-server")
                .env("SCCACHE_DIR", cache_dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };

        // Start fresh server with isolated cache dir
        stop_purge_start(&sccache_bin, &sc_cache_str);

        // Cold single-file: nuke dir, regenerate, warmup clang, compile (cache empty)
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("  Cold single-file...");
        let cold_s = sccache_compile_single(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!(" {}", fmt_dur(cold_s));
        sccache_cold_single = Some(cold_s);

        // Warm trials: single-file (cache populated from cold pass)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_single(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
                &sources,
            ));
        }
        print_trials("sccache single-file (warm)", &times);
        sccache_single_times = Some(times);

        // Purge cache and restart for cold multi-file
        stop_purge_start(&sccache_bin, &sc_cache_str);

        // Cold multi-file: nuke dir, regenerate, warmup clang
        nuke_and_regenerate(sc_dir.path());
        warmup_compiler(&compiler, sc_dir.path());
        eprint!("  Cold multi-file...");
        let cold_m = sccache_compile_multi(&sccache_bin, &compiler, sc_dir.path(), &sources);
        eprintln!(" {}", fmt_dur(cold_m));
        sccache_cold_multi = Some(cold_m);

        // Warm trials: multi-file (sccache can't cache this — passes through)
        let mut times = Vec::with_capacity(WARM_TRIALS);
        for _ in 0..WARM_TRIALS {
            times.push(sccache_compile_multi(
                &sccache_bin,
                &compiler,
                sc_dir.path(),
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

        std::env::remove_var("SCCACHE_DIR");
        drop(sc_dir);
        drop(sc_cache_dir);
        eprintln!();
    } else {
        eprintln!("--- sccache: not found, skipping ---\n");
        sccache_cold_single = None;
        sccache_cold_multi = None;
        sccache_single_times = None;
        sccache_multi_times = None;
    }

    // ── zccache (fresh dir, in-process daemon) ────────────────────────
    let zc_dir = tempfile::tempdir().unwrap();
    generate_project(zc_dir.path());
    let zc_cwd = zc_dir.path().to_string_lossy().into_owned();

    eprintln!("--- zccache (in-process daemon) ---");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: zc_cwd.clone(),
            log_file: None,
            track_stats: true,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // Cold single-file: nuke dir, regenerate, warmup clang, compile (daemon cache empty)
    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());
    eprint!("  Cold single-file...");
    let zc_cold_single =
        zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!(" {}", fmt_dur(zc_cold_single));

    // Warm trials: single-file (cache populated from cold pass)
    let mut zc_single_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_single_times.push(
            zccache_compile_single(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("zccache single-file (warm)", &zc_single_times);

    // Cold multi-file: clear daemon cache, nuke dir, regenerate, warmup clang
    client.send(&Request::Clear).await.unwrap();
    let _ = client.recv::<Response>().await;
    nuke_and_regenerate(zc_dir.path());
    warmup_compiler(&compiler, zc_dir.path());

    eprint!("  Cold multi-file...");
    let zc_cold_multi =
        zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await;
    eprintln!(" {}", fmt_dur(zc_cold_multi));

    // Warm trials: multi-file
    let mut zc_multi_times = Vec::with_capacity(WARM_TRIALS);
    for _ in 0..WARM_TRIALS {
        zc_multi_times.push(
            zccache_compile_multi(&mut client, &session_id, &compiler, &zc_cwd, &sources).await,
        );
    }
    print_trials("zccache multi-file (warm)", &zc_multi_times);

    // End session
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    let _ = client.recv::<Response>().await;

    shutdown.notify_one();
    server_handle.await.unwrap();

    // ── Summary ─────────────────────────────────────────────────────
    let zc_single_med = median(&zc_single_times);
    let zc_multi_med = median(&zc_multi_times);

    let scc_single_str = sccache_single_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_multi_str = sccache_multi_times.as_ref().map(|t| fmt_dur(median(t)));
    let scc_cold_s_str = sccache_cold_single.map(fmt_dur);
    let scc_cold_m_str = sccache_cold_multi.map(fmt_dur);
    let dash = "—";

    // ── Markdown table (README-ready) ────────────────────────────────
    eprintln!();
    eprintln!("## Benchmark: {NUM_FILES} C++ files, {WARM_TRIALS} warm trials");
    eprintln!();
    eprintln!("| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |");
    eprintln!("|:---------|----------:|--------:|--------:|-----------:|--------------:|");

    // Single-file, Cold
    let scc_cs = scc_cold_s_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_s = sccache_cold_single.map(|t| {
        let ratio = t.as_secs_f64() / zc_cold_single.as_secs_f64();
        format!("{ratio:.1}x faster")
    });
    let vs_bare_cold_s = {
        let ratio = bl_cold_single.as_secs_f64() / zc_cold_single.as_secs_f64();
        format!("{ratio:.1}x")
    };
    eprintln!(
        "| Single-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_single),
        scc_cs,
        fmt_dur(zc_cold_single),
        vs_scc_cold_s.as_deref().unwrap_or(dash),
        vs_bare_cold_s,
    );

    // Single-file, Warm
    let scc_ws = scc_single_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_s = sccache_single_times.as_ref().map(|t| {
        let ratio = median(t).as_secs_f64() / zc_single_med.as_secs_f64();
        format!("**{ratio:.0}x faster**")
    });
    let vs_bare_warm_s = {
        let ratio = bl_warm_single.as_secs_f64() / zc_single_med.as_secs_f64();
        format!("**{ratio:.0}x faster**")
    };
    eprintln!(
        "| Single-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_single),
        scc_ws,
        fmt_dur(zc_single_med),
        vs_scc_warm_s.as_deref().unwrap_or(dash),
        vs_bare_warm_s,
    );

    // Multi-file, Cold
    let scc_cm = scc_cold_m_str.as_deref().unwrap_or(dash);
    let vs_scc_cold_m = sccache_cold_multi.map(|t| {
        let ratio = t.as_secs_f64() / zc_cold_multi.as_secs_f64();
        format!("{ratio:.1}x faster")
    });
    let vs_bare_cold_m = {
        let ratio = bl_cold_multi.as_secs_f64() / zc_cold_multi.as_secs_f64();
        format!("{ratio:.1}x")
    };
    eprintln!(
        "| Multi-file, Cold | {} | {} | {} | {} | {} |",
        fmt_dur(bl_cold_multi),
        scc_cm,
        fmt_dur(zc_cold_multi),
        vs_scc_cold_m.as_deref().unwrap_or(dash),
        vs_bare_cold_m,
    );

    // Multi-file, Warm
    let scc_wm = scc_multi_str.as_deref().unwrap_or(dash);
    let vs_scc_warm_m = sccache_multi_times.as_ref().map(|t| {
        let ratio = median(t).as_secs_f64() / zc_multi_med.as_secs_f64();
        format!("**{ratio:.0}x faster**")
    });
    let vs_bare_warm_m = {
        let ratio = bl_warm_multi.as_secs_f64() / zc_multi_med.as_secs_f64();
        format!("**{ratio:.0}x faster**")
    };
    eprintln!(
        "| Multi-file, Warm | {} | {} | **{}** | {} | {} |",
        fmt_dur(bl_warm_multi),
        scc_wm,
        fmt_dur(zc_multi_med),
        vs_scc_warm_m.as_deref().unwrap_or(dash),
        vs_bare_warm_m,
    );

    eprintln!();
    eprintln!("> **Cold** = first compile (empty cache). **Warm** = median of {WARM_TRIALS} subsequent runs.");
    eprintln!("> Single-file = {NUM_FILES} sequential `clang++ -c unit.cpp` invocations. Multi-file = one `clang++ -c *.cpp` invocation.");
    if sccache_multi_times.is_some() {
        eprintln!("> sccache cannot cache multi-file compilations — its \"warm\" multi-file time is a full recompile.");
    }

    // ── Speedup summary ─────────────────────────────────────────────
    eprintln!();
    eprintln!("### Speedups (warm cache hit, median)");
    eprintln!();
    print_speedup(
        "zccache single vs bare clang",
        bl_warm_single,
        zc_single_med,
    );
    print_speedup("zccache multi  vs bare clang", bl_warm_multi, zc_multi_med);
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
