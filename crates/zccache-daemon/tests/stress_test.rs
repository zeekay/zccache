//! Adversarial stress tests for the compilation cache.
//!
//! These tests intentionally try to break the caching with corner cases:
//! - Source modifications must invalidate cache
//! - Different flags must produce different cache entries
//! - Compile errors must NOT be cached
//! - Concurrent compilations must not corrupt state
//! - Local header changes must invalidate cache
//! - Edge cases in paths, empty files, etc.

use std::path::PathBuf;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

/// Platform-correct client connection type.
#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

/// Helper: start a daemon server on a unique endpoint.
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

/// Resolve the clang++ path from ~/.clang-tool-chain.
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

/// Helper: start a session with a log file on an already-connected client.
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
            track_stats: false,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    }
}

/// Helper: send a compile request, return (exit_code, cached).
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

// ═══════════════════════════════════════════════════════════════════════
// CORRECTNESS ATTACKS
// ═══════════════════════════════════════════════════════════════════════

/// Modifying the source file between compiles MUST invalidate the cache.
/// If the cache returns stale output for modified source, it's catastrophic.
#[tokio::test]
async fn adversarial_source_modification_invalidates_cache() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("modify.cpp");
    let obj = tmp.path().join("modify.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Version 1: returns 0
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Compile v1
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "first compile must be a miss");
    let obj_v1 = std::fs::read(&obj).unwrap();

    // Version 2: returns 42 — different code, must produce different .o
    std::fs::write(&src, "int main() { return 42; }\n").unwrap();

    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "modified source MUST be a cache miss");
    let obj_v2 = std::fs::read(&obj).unwrap();

    // The two object files MUST differ (different return values = different code)
    assert_ne!(obj_v1, obj_v2, "different source must produce different .o");

    // Now compile v2 again — THIS should be a cache hit
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "unmodified v2 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_v2,
        "cached .o must match v2"
    );

    // Log should show: miss, miss, hit
    let log_text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = log_text.lines().collect();
    assert!(lines.len() >= 3, "expected 3+ log lines, got:\n{log_text}");
    assert!(lines[0].contains("cache miss"), "line 0: {}", lines[0]);
    assert!(lines[1].contains("cache miss"), "line 1: {}", lines[1]);
    assert!(lines[2].contains("cache hit"), "line 2: {}", lines[2]);

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Different compiler flags MUST produce different cache entries.
/// -O0 vs -O2 produce different machine code — returning the wrong one is catastrophic.
#[tokio::test]
async fn adversarial_different_flags_different_cache_entries() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("flags.cpp");
    let obj = tmp.path().join("flags.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(
        &src,
        r#"
static int x = 0;
int get() { return x++; }
int main() { return get() + get(); }
"#,
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Compile with -O0
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-O0",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);
    let obj_o0 = std::fs::read(&obj).unwrap();

    // Compile with -O2 — MUST NOT return the -O0 cached result
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-O2",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "-O2 compile MUST NOT hit the -O0 cache entry");
    let obj_o2 = std::fs::read(&obj).unwrap();

    // Different optimization levels should produce different object files
    assert_ne!(
        obj_o0, obj_o2,
        "-O0 and -O2 should produce different .o files"
    );

    // Now recompile -O0 — should hit the original -O0 cache
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-O0",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "-O0 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o0,
        "cached -O0 must match original"
    );

    // And -O2 should also hit its cache
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-O2",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "-O2 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o2,
        "cached -O2 must match original"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// -D preprocessor defines MUST produce different cache entries.
/// Code compiled with -DNDEBUG vs -DDEBUG can have wildly different behavior.
#[tokio::test]
async fn adversarial_define_changes_invalidate_cache() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("defines.cpp");
    let obj = tmp.path().join("defines.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(
        &src,
        r#"
#ifdef RETURN_ZERO
int main() { return 0; }
#else
int main() { return 1; }
#endif
"#,
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Without define
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);
    let obj_no_define = std::fs::read(&obj).unwrap();

    // With -DRETURN_ZERO — MUST NOT hit cache
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-DRETURN_ZERO",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "-DRETURN_ZERO MUST be a cache miss");
    let obj_with_define = std::fs::read(&obj).unwrap();

    assert_ne!(
        obj_no_define, obj_with_define,
        "different defines must produce different .o"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Compile errors MUST NOT be cached.
/// If a failed compilation is cached, subsequent fixes won't take effect.
#[tokio::test]
async fn adversarial_compile_errors_never_cached() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("errors.cpp");
    let obj = tmp.path().join("errors.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Broken source
    std::fs::write(&src, "int main() { SYNTAX ERROR HERE }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // First compile — should fail
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_ne!(exit_code, 0, "broken source should fail to compile");
    assert!(!cached, "failed compile must not be cached");

    // Compile again — must NOT get a cache hit for the error
    let (exit_code2, cached2) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_ne!(exit_code2, 0, "still broken, should still fail");
    assert!(!cached2, "failed compile must NEVER be returned from cache");

    // Now fix the source
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (exit_code3, cached3) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code3, 0, "fixed source should compile successfully");
    assert!(!cached3, "new source should be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Local header changes MUST invalidate the cache.
/// If foo.cpp includes foo.h and foo.h changes, using the old cached .o is wrong.
///
/// This is the most dangerous cache correctness issue — the cache key currently
/// only hashes the source file, not its transitive includes.
#[tokio::test]
async fn adversarial_local_header_change_must_invalidate() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let header = tmp.path().join("config.h");
    let src = tmp.path().join("with_header.cpp");
    let obj = tmp.path().join("with_header.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Header v1: VALUE = 1
    std::fs::write(&header, "#define VALUE 1\n").unwrap();
    std::fs::write(
        &src,
        r#"#include "config.h"
int main() { return VALUE; }
"#,
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Compile with header v1
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);
    let obj_v1 = std::fs::read(&obj).unwrap();

    // Change header to v2: VALUE = 99
    std::fs::write(&header, "#define VALUE 99\n").unwrap();
    // Source file is UNCHANGED — only the header changed.

    // Compile again — should this be a cache miss?
    // With the current implementation (source-only cache key), this WILL be a
    // cache hit returning stale output. This test documents the known limitation.
    let (exit_code2, cached2) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code2, 0);

    // Header hashes are included in the artifact key via DepGraph.
    // Editing a header must invalidate the cache even if the source is unchanged.
    assert!(!cached2, "header change must invalidate cache");
    let obj_v2 = std::fs::read(&obj).unwrap();
    assert_ne!(obj_v1, obj_v2, "header change should produce different .o");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// CONCURRENCY ATTACKS
// ═══════════════════════════════════════════════════════════════════════

/// Slam the daemon with many concurrent compiles of the same file.
/// No panics, no corrupted cache, all results consistent.
#[tokio::test]
async fn adversarial_concurrent_same_file() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("concurrent.cpp");
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut handles = Vec::new();
    for i in 0..8 {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let src = src.clone();
        let cwd = cwd.clone();
        let out_dir = tmp.path().join(format!("out{i}"));
        std::fs::create_dir_all(&out_dir).unwrap();
        let obj = out_dir.join("concurrent.o");
        let log = out_dir.join("log.txt");

        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            let (exit_code, _cached) = compile(
                &mut client,
                sid,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(exit_code, 0, "concurrent compile {i} should succeed");
            assert!(obj.exists(), "output should exist for compile {i}");

            // Read the .o — all should be identical
            std::fs::read(&obj).unwrap()
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }

    // All compiles succeeded (returned data). COFF timestamps may differ
    // between independent compiler invocations, so we verify all produced
    // non-empty .o files of the same size (same source, same flags).
    for (i, obj) in results.iter().enumerate() {
        assert!(!obj.is_empty(), "concurrent compile {i} produced empty .o");
    }
    // All cached results should be the same size as the first
    let first_len = results[0].len();
    for (i, obj) in results.iter().enumerate().skip(1) {
        assert_eq!(
            first_len,
            obj.len(),
            "concurrent compile {i} produced .o of different size than compile 0"
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Many concurrent compiles of DIFFERENT files.
/// Verifies no cross-contamination between cache entries.
#[tokio::test]
async fn adversarial_concurrent_different_files() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let n = 10;
    let mut handles = Vec::new();
    for i in 0..n {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let src = tmp.path().join(format!("file{i}.cpp"));
        let obj = tmp.path().join(format!("file{i}.o"));
        let log = tmp.path().join(format!("log{i}.txt"));
        // Each file returns a different value
        std::fs::write(&src, format!("int main() {{ return {i}; }}\n")).unwrap();

        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            // First compile — miss
            let (exit_code, cached) = compile(
                &mut client,
                sid,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(exit_code, 0);
            assert!(!cached, "first compile of file{i} should be a miss");
            let obj_data = std::fs::read(&obj).unwrap();

            // Delete and recompile — hit
            std::fs::remove_file(&obj).unwrap();
            let (exit_code, cached) = compile(
                &mut client,
                sid,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(exit_code, 0);
            assert!(cached, "second compile of file{i} should be a hit");
            let obj_data2 = std::fs::read(&obj).unwrap();
            assert_eq!(obj_data, obj_data2, "cached .o for file{i} must match");

            obj_data
        }));
    }

    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }

    // All files have different return values → all object files should differ
    for i in 1..results.len() {
        assert_ne!(
            results[0], results[i],
            "file0 and file{i} should produce different .o (different source)"
        );
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Cross-session cache sharing: two different sessions with the same compiler
/// should share the in-memory cache.
#[tokio::test]
async fn adversarial_cross_session_cache_sharing() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("shared.cpp");
    let obj1 = tmp.path().join("shared1.o");
    let obj2 = tmp.path().join("shared2.o");
    let log1 = tmp.path().join("log1.txt");
    let log2 = tmp.path().join("log2.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 7; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session 1: compile and cache
    let mut client1 = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid1 = start_session(&mut client1, &clang, &cwd, &log1.to_string_lossy()).await;

    let (exit_code, cached) = compile(
        &mut client1,
        sid1,
        &["-c", &src.to_string_lossy(), "-o", &obj1.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "session 1 first compile should be a miss");
    let obj1_data = std::fs::read(&obj1).unwrap();

    // Session 2: same source, same flags — should hit session 1's cache
    let mut client2 = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid2 = start_session(&mut client2, &clang, &cwd, &log2.to_string_lossy()).await;
    assert_ne!(sid1, sid2, "sessions should have different IDs");

    let (exit_code, cached) = compile(
        &mut client2,
        sid2,
        &["-c", &src.to_string_lossy(), "-o", &obj2.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "session 2 should hit session 1's cached result");
    let obj2_data = std::fs::read(&obj2).unwrap();

    assert_eq!(obj1_data, obj2_data, "cross-session cached .o must match");

    // Log files should reflect the different sessions
    let log1_text = std::fs::read_to_string(&log1).unwrap();
    let log2_text = std::fs::read_to_string(&log2).unwrap();
    assert!(log1_text.contains("cache miss"));
    assert!(log2_text.contains("cache hit"));

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// EDGE CASES
// ═══════════════════════════════════════════════════════════════════════

/// Compile request with an invalid session ID must return an error, not panic.
#[tokio::test]
async fn adversarial_invalid_session_id() {
    if find_clang().is_none() {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("invalid_session.cpp");
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Don't create a session — just send a compile with a bogus session ID
    client
        .send(&Request::Compile {
            session_id: 999999,
            args: vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                "out.o".to_string(),
            ],
            cwd,
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::Error { message }) => {
            assert!(
                message.contains("unknown session"),
                "error should mention unknown session: {message}"
            );
        }
        other => panic!("expected Error for invalid session, got: {other:?}"),
    }

    // Server should still be alive
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(
        resp,
        Some(Response::Pong),
        "server should survive bad requests"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Non-cacheable invocations (linking, preprocessing) should pass through
/// to the compiler directly without corrupting the cache.
#[tokio::test]
async fn adversarial_non_cacheable_passthrough() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("passthrough.cpp");
    let obj = tmp.path().join("passthrough.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "#include <stdio.h>\nint main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Preprocessing only (-E) — non-cacheable
    client
        .send(&Request::Compile {
            session_id: sid,
            args: vec!["-E".to_string(), src.to_string_lossy().into_owned()],
            cwd: cwd.clone(),
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "preprocessing should succeed");
            assert!(!cached, "preprocessing must not be cached");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Now do a normal cacheable compile — should not be contaminated
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "first real compile should be a miss");

    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("non-cacheable"),
        "log should mention non-cacheable"
    );
    assert!(
        log_text.contains("cache miss"),
        "log should mention cache miss"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Empty source file (valid C++) — edge case for hashing.
#[tokio::test]
async fn adversarial_empty_source_file() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    // An empty .cpp is valid C++ (no main required for -c)
    let src = tmp.path().join("empty.cpp");
    let obj = tmp.path().join("empty.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Should compile (empty TU is valid)
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "empty source should compile");
    assert!(!cached);

    // Second compile should hit cache
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "empty source recompile should hit cache");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Source with warnings but no errors — should still be cached (exit code 0).
#[tokio::test]
async fn adversarial_warnings_still_cached() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("warnings.cpp");
    let obj = tmp.path().join("warnings.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Unused variable produces a warning
    std::fs::write(&src, "int main() { int unused_var = 42; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Compile with -Wall to trigger warning
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "warnings shouldn't cause compile failure");
    assert!(!cached);

    // Second compile should hit cache despite warnings
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "warnings-only compilation should still be cached");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Same source, different output path — should share cache.
/// The -o flag path should not affect the cache key.
#[tokio::test]
async fn adversarial_output_path_does_not_affect_cache_key() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("output_path.cpp");
    let obj_a = tmp.path().join("a.o");
    let obj_b = tmp.path().join("b.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Compile to a.o — cache miss
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj_a.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);
    let data_a = std::fs::read(&obj_a).unwrap();

    // Compile to b.o — same source, same flags. Should be a cache hit.
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj_b.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(
        cached,
        "same source to different output path should hit cache"
    );
    let data_b = std::fs::read(&obj_b).unwrap();

    assert_eq!(
        data_a, data_b,
        "cached .o should be identical regardless of output path"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Rapid fire: compile → delete → compile → delete ... many times.
/// Cache should remain consistent throughout.
#[tokio::test]
async fn adversarial_rapid_recompile_cycle() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("rapid.cpp");
    let obj = tmp.path().join("rapid.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Own the strings so they live long enough for the borrow in args_owned
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();
    let args_owned: Vec<&str> = vec!["-c", &src_str, "-o", &obj_str];

    // First compile — miss
    let (exit_code, cached) = compile(&mut client, sid, &args_owned, &cwd).await;
    assert_eq!(exit_code, 0);
    assert!(!cached);
    let reference_obj = std::fs::read(&obj).unwrap();

    // 20 rapid cycles of delete + recompile — all should be cache hits
    for i in 0..20 {
        std::fs::remove_file(&obj).unwrap();
        let (exit_code, cached) = compile(&mut client, sid, &args_owned, &cwd).await;
        assert_eq!(exit_code, 0, "rapid cycle {i} should succeed");
        assert!(cached, "rapid cycle {i} should be a cache hit");
        let data = std::fs::read(&obj).unwrap();
        assert_eq!(
            data, reference_obj,
            "rapid cycle {i} .o should match reference"
        );
    }

    // Log should have 1 miss + 20 hits
    let log_text = std::fs::read_to_string(&log).unwrap();
    let miss_count = log_text.matches("cache miss").count();
    let hit_count = log_text.matches("cache hit").count();
    assert_eq!(miss_count, 1, "expected exactly 1 miss");
    assert_eq!(hit_count, 20, "expected exactly 20 hits");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Source file with spaces in the filename — path handling edge case.
#[tokio::test]
async fn adversarial_spaces_in_filename() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("hello world.cpp");
    let obj = tmp.path().join("hello world.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // First compile
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "file with spaces should compile");
    assert!(!cached);

    // Second compile — cache hit
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "recompile of file with spaces should hit cache");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Source file deleted between first successful cache and recompile attempt.
/// hash_file should fail, gracefully falling back to direct compile error.
#[tokio::test]
async fn adversarial_source_deleted_after_caching() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("disappear.cpp");
    let obj = tmp.path().join("disappear.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Cache it
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);

    // Delete the source file
    std::fs::remove_file(&src).unwrap();
    std::fs::remove_file(&obj).unwrap();

    // Try to compile again — source hashing should fail, not panic
    client
        .send(&Request::Compile {
            session_id: sid,
            args: vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone(),
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::CompileResult { exit_code, .. }) => {
            // Should fail (source doesn't exist) — either via hash_file error
            // or the compiler itself failing
            assert_ne!(exit_code, 0, "compile of deleted source should fail");
        }
        Some(Response::Error { .. }) => {
            // Also acceptable — cache key computation failed
        }
        other => panic!("expected CompileResult or Error, got: {other:?}"),
    }

    // Server should still be alive
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Multiple files with #include — each file's cache entry is independent.
/// Modifying one file should not affect another file's cache entry.
#[tokio::test]
async fn adversarial_independent_file_caches() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src_a = tmp.path().join("independent_a.cpp");
    let src_b = tmp.path().join("independent_b.cpp");
    let obj_a = tmp.path().join("independent_a.o");
    let obj_b = tmp.path().join("independent_b.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src_a, "int fa() { return 1; }\n").unwrap();
    std::fs::write(&src_b, "int fb() { return 2; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // Cache both files
    let (exit_code, _) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src_a.to_string_lossy(),
            "-o",
            &obj_a.to_string_lossy(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    let obj_a_data = std::fs::read(&obj_a).unwrap();

    let (exit_code, _) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src_b.to_string_lossy(),
            "-o",
            &obj_b.to_string_lossy(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);

    // Modify file A
    std::fs::write(&src_a, "int fa() { return 999; }\n").unwrap();

    // Recompile A — should miss (source changed)
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src_a.to_string_lossy(),
            "-o",
            &obj_a.to_string_lossy(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(!cached, "modified file A should be a miss");
    let obj_a_new = std::fs::read(&obj_a).unwrap();
    assert_ne!(
        obj_a_data, obj_a_new,
        "modified A should produce different .o"
    );

    // Recompile B — should STILL HIT (B was not modified)
    std::fs::remove_file(&obj_b).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src_b.to_string_lossy(),
            "-o",
            &obj_b.to_string_lossy(),
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "unmodified file B should still hit cache");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Werror turns warnings into errors — different exit code path.
/// -Werror compile should fail and NOT be cached. Without -Werror, same
/// source should succeed and BE cached.
#[tokio::test]
async fn adversarial_werror_vs_no_werror() {
    let clang = match find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("werror.cpp");
    let obj = tmp.path().join("werror.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Unused variable — warning with -Wall
    std::fs::write(&src, "int main() { int x = 0; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    // With -Werror: should fail (warning → error)
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
            "-Werror",
        ],
        &cwd,
    )
    .await;
    assert_ne!(exit_code, 0, "-Werror should cause compile failure");
    assert!(!cached, "failed -Werror compile must not be cached");

    // Without -Werror: should succeed
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0, "without -Werror should succeed");
    assert!(!cached, "different flags = different cache key = miss");

    // Recompile without -Werror — should hit cache
    std::fs::remove_file(&obj).unwrap();
    let (exit_code, cached) = compile(
        &mut client,
        sid,
        &[
            "-c",
            &src.to_string_lossy(),
            "-o",
            &obj.to_string_lossy(),
            "-Wall",
        ],
        &cwd,
    )
    .await;
    assert_eq!(exit_code, 0);
    assert!(cached, "-Wall recompile (no -Werror) should hit cache");

    shutdown.notify_one();
    server_handle.await.unwrap();
}
