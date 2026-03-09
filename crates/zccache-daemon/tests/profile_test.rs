//! Profiling stress test: drives many compilations and reports phase-level timing breakdown.
//!
//! Run with: uv run cargo test -p zccache-daemon --test profile_test -- --nocapture --ignored

use std::path::PathBuf;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

// ─── Config ──────────────────────────────────────────────────────────────────

const FILE_COUNT: usize = 20;
const WARM_ITERATIONS: usize = 50;
const HEADER_COUNT: usize = 10;

// ─── Tool discovery ──────────────────────────────────────────────────────────

fn find_clang() -> Option<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let candidates = [
        PathBuf::from(&home).join(".clang-tool-chain/clang/win/x86_64/bin/clang++.exe"),
        PathBuf::from(&home).join(".clang-tool-chain/clang/darwin/x86_64/bin/clang++"),
    ];
    candidates.into_iter().find(|p| p.exists())
}

// ─── Daemon helpers ──────────────────────────────────────────────────────────

async fn start_session(client: &mut ClientConn, clang: &std::path::Path, cwd: &str) -> u64 {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string(),
            compiler: clang.to_string_lossy().into_owned(),
            log_file: None,
            track_stats: true,
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
            compiler: None,
            env: None,
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

// ─── Test file generation ────────────────────────────────────────────────────

fn generate_test_files(dir: &std::path::Path, file_count: usize, header_count: usize) {
    // Shared headers
    for h in 0..header_count {
        std::fs::write(
            dir.join(format!("header_{h}.h")),
            format!(
                r#"#pragma once
inline int header_{h}_func(int x) {{ return x + {h}; }}
template<typename T> T header_{h}_generic(T a, T b) {{ return a * b + {h}; }}
"#
            ),
        )
        .unwrap();
    }

    for i in 0..file_count {
        let includes: String = (0..header_count)
            .map(|h| format!("#include \"header_{h}.h\"\n"))
            .collect();
        let calls: String = (0..header_count)
            .map(|h| {
                format!("    sum += header_{h}_func(i);\n    sum += header_{h}_generic(sum, i);\n")
            })
            .collect();

        std::fs::write(
            dir.join(format!("src_{i}.cpp")),
            format!(
                r#"{includes}
int func_{i}(int n) {{
    int sum = 0;
    for (int i = 0; i < n; i++) {{
{calls}    }}
    return sum;
}}
"#
            ),
        )
        .unwrap();
    }
}

// ─── Profile reporting ───────────────────────────────────────────────────────

fn print_profile(profile: &zccache_daemon::ProfileSnapshot) {
    let wide = "=".repeat(80);
    let dash = "-".repeat(70);

    println!("\n{wide}");
    println!("  PHASE PROFILING RESULTS");
    println!("{wide}\n");

    if profile.hit_count > 0 {
        println!("  CACHE HIT PATH ({} samples)", profile.hit_count);
        println!("  {dash}");

        let phases = [
            ("parse_args", profile.avg_parse_args_us),
            ("build_context + register", profile.avg_build_context_us),
            ("hash_source (metadata cache)", profile.avg_hash_source_us),
            ("hash_headers (metadata cache)", profile.avg_hash_headers_us),
            ("depgraph_check", profile.avg_depgraph_check_us),
            (
                "artifact_lookup (Mutex<HashMap>)",
                profile.avg_artifact_lookup_us,
            ),
            ("write_output (fs::write)", profile.avg_write_output_us),
            ("bookkeeping (stats + log)", profile.avg_bookkeeping_us),
        ];

        let total = profile.avg_total_hit_us.max(1);
        let mut accounted = 0u64;
        for (name, us) in &phases {
            let pct = (*us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            println!("  {name:<35} {us:>6}us  ({pct:>5.1}%)  {bar}");
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        println!(
            "  {:<35} {:>6}us  ({:>5.1}%)",
            "overhead/unaccounted", overhead, overhead_pct
        );
        println!("  {dash}");
        println!("  {:<35} {:>6}us", "TOTAL (avg per hit)", total);
        println!();
    }

    if profile.miss_count > 0 {
        println!("  CACHE MISS PATH ({} samples)", profile.miss_count);
        println!("  {dash}");

        let miss_phases = [
            ("compiler_exec (clang)", profile.avg_compiler_exec_us),
            ("include_scan (recursive)", profile.avg_include_scan_us),
            ("hash_all_files", profile.avg_hash_all_us),
            (
                "artifact_store (depgraph + HashMap)",
                profile.avg_artifact_store_us,
            ),
        ];

        let total = profile.avg_total_miss_us.max(1);
        let mut accounted = 0u64;
        for (name, us) in &miss_phases {
            let pct = (*us as f64 / total as f64) * 100.0;
            let bar_len = (pct / 2.0).round().max(0.0) as usize;
            let bar: String = "#".repeat(bar_len);
            println!("  {name:<35} {us:>6}us  ({pct:>5.1}%)  {bar}");
            accounted += us;
        }

        let overhead = total.saturating_sub(accounted);
        let overhead_pct = (overhead as f64 / total as f64) * 100.0;
        println!(
            "  {:<35} {:>6}us  ({:>5.1}%)",
            "overhead/unaccounted", overhead, overhead_pct
        );
        println!("  {dash}");
        println!("  {:<35} {:>6}us", "TOTAL (avg per miss)", total);
        println!();
    }

    println!("{wide}");
}

// ─── The test ────────────────────────────────────────────────────────────────

/// Profiling stress test: cold + warm compilations with phase-level timing breakdown.
///
/// Run with: uv run cargo test -p zccache-daemon --test profile_test -- --nocapture --ignored
#[tokio::test]
#[ignore]
async fn profile_compile_phases() {
    let clang = match find_clang() {
        Some(p) => p,
        None => {
            println!("SKIP: clang not found at ~/.clang-tool-chain");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    generate_test_files(tmp.path(), FILE_COUNT, HEADER_COUNT);
    let cwd = tmp.path().to_string_lossy().into_owned();

    println!();
    println!("  Config: {FILE_COUNT} files, {HEADER_COUNT} headers each, {WARM_ITERATIONS} warm iterations");
    println!("  clang:  {}", {
        let out = std::process::Command::new(&clang)
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    });

    // Start daemon — we need to keep the server object to read the profiler
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let server_handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });

    // Give server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd).await;

    // ── Cold pass (cache miss) ───────────────────────────────────────
    println!("\n  [1/2] Cold pass ({FILE_COUNT} files)...");
    let cold_start = std::time::Instant::now();
    for i in 0..FILE_COUNT {
        let src = format!("src_{i}.cpp");
        let obj = format!("src_{i}.o");
        let (exit_code, cached) = compile(&mut client, sid, &["-c", &src, "-o", &obj], &cwd).await;
        assert_eq!(exit_code, 0, "cold compile failed for {src}");
        assert!(!cached, "cold compile should be a miss");
    }
    let cold_elapsed = cold_start.elapsed();
    println!(
        "  Cold pass done: {:.1}ms total ({:.1}ms/file)",
        cold_elapsed.as_secs_f64() * 1000.0,
        cold_elapsed.as_secs_f64() * 1000.0 / FILE_COUNT as f64,
    );

    // ── Warm pass (cache hit) ────────────────────────────────────────
    println!("  [2/2] Warm pass ({FILE_COUNT} files x {WARM_ITERATIONS} iterations)...");
    let warm_start = std::time::Instant::now();
    let total_warm = FILE_COUNT * WARM_ITERATIONS;
    for iter in 0..WARM_ITERATIONS {
        for i in 0..FILE_COUNT {
            let src = format!("src_{i}.cpp");
            let obj = format!("src_{i}.o");
            let _ = std::fs::remove_file(tmp.path().join(&obj));
            let (exit_code, cached) =
                compile(&mut client, sid, &["-c", &src, "-o", &obj], &cwd).await;
            assert_eq!(exit_code, 0, "warm compile failed for {src}");
            assert!(cached, "warm compile should be a hit");
        }
        if (iter + 1) % 10 == 0 {
            eprint!("    warm: {}/{WARM_ITERATIONS} iterations\r", iter + 1);
        }
    }
    eprintln!();
    let warm_elapsed = warm_start.elapsed();
    println!(
        "  Warm pass done: {:.1}ms total ({:.3}ms/file, {} files)",
        warm_elapsed.as_secs_f64() * 1000.0,
        warm_elapsed.as_secs_f64() * 1000.0 / total_warm as f64,
        total_warm,
    );

    // ── Get profile snapshot before shutdown ──────────────────────────
    // We need to get profile data via a Status request since the server
    // object was moved into the spawned task. Use the session stats instead
    // and read from the stats endpoint.

    // End session to get stats
    client
        .send(&Request::SessionEnd { session_id: sid })
        .await
        .unwrap();
    if let Some(Response::SessionEnded { stats: Some(s), .. }) = client.recv().await.unwrap() {
        println!(
            "\n  Session stats: {} compilations, {} hits, {} misses",
            s.compilations, s.hits, s.misses
        );
    }

    // Get daemon status
    client.send(&Request::Status).await.unwrap();
    if let Some(Response::Status(status)) = client.recv().await.unwrap() {
        println!(
            "  Daemon: {} hits, {} misses, {} time saved (ms)",
            status.cache_hits, status.cache_misses, status.time_saved_ms
        );
    }

    // Shutdown
    shutdown.notify_one();
    server_handle.await.unwrap();

    // Since we can't access the server's profiler after moving it into the task,
    // print IPC-level timing analysis
    println!("\n  ── IPC-Level Timing ──");
    let avg_warm_us = warm_elapsed.as_micros() as f64 / total_warm as f64;
    let avg_cold_us = cold_elapsed.as_micros() as f64 / FILE_COUNT as f64;
    println!(
        "  Average cold compile (IPC round-trip): {avg_cold_us:.0}us ({:.2}ms)",
        avg_cold_us / 1000.0
    );
    println!(
        "  Average warm compile (IPC round-trip): {avg_warm_us:.0}us ({:.3}ms)",
        avg_warm_us / 1000.0
    );

    // The real profiling data needs the server object. Let's run a second
    // server instance specifically for profiling.
    println!("\n  ── Phase Profiling (dedicated run) ──");

    let tmp2 = tempfile::tempdir().unwrap();
    generate_test_files(tmp2.path(), FILE_COUNT, HEADER_COUNT);
    let cwd2 = tmp2.path().to_string_lossy().into_owned();

    let endpoint2 = zccache_ipc::unique_test_endpoint();
    let server2 = DaemonServer::bind(&endpoint2).unwrap();
    let shutdown2 = server2.shutdown_handle();

    // Wrap server in Arc<Mutex> so we can read the profiler while it's running
    use std::sync::Arc;
    use tokio::sync::Mutex;
    let server2 = Arc::new(Mutex::new(Some(server2)));
    let server2_clone = Arc::clone(&server2);

    let server_handle2 = tokio::spawn(async move {
        let mut srv = server2_clone.lock().await.take().unwrap();
        srv.run(0).await.unwrap();
        // Put it back so we can read the profiler
        *server2_clone.lock().await = Some(srv);
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut client2 = zccache_ipc::connect(&endpoint2).await.unwrap();
    let sid2 = start_session(&mut client2, &clang, &cwd2).await;

    // Cold pass
    for i in 0..FILE_COUNT {
        let src = format!("src_{i}.cpp");
        let obj = format!("src_{i}.o");
        let (exit_code, _) = compile(&mut client2, sid2, &["-c", &src, "-o", &obj], &cwd2).await;
        assert_eq!(exit_code, 0);
    }

    // Warm pass
    for _ in 0..WARM_ITERATIONS {
        for i in 0..FILE_COUNT {
            let src = format!("src_{i}.cpp");
            let obj = format!("src_{i}.o");
            let _ = std::fs::remove_file(tmp2.path().join(&obj));
            let (exit_code, cached) =
                compile(&mut client2, sid2, &["-c", &src, "-o", &obj], &cwd2).await;
            assert_eq!(exit_code, 0);
            assert!(cached);
        }
    }

    // Shutdown and read profiler
    shutdown2.notify_one();
    server_handle2.await.unwrap();

    // Now the server is back in the Arc<Mutex>
    let guard = server2.lock().await;
    if let Some(ref srv) = *guard {
        let profile = srv.profile_snapshot();
        print_profile(&profile);
    }
}
