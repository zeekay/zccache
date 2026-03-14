//! Integration test: full CLI session flow.
//!
//! Tests the production workflow:
//!   session-start → set ZCCACHE_SESSION_ID → wrap compile → session-end
//!
//! Uses the daemon directly via IPC (same protocol the CLI uses).

use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

/// Parse `session_id` from the CLI's one-line JSON output:
/// `{"session_id":1,"started_at":1710000000}`
fn parse_session_id_from_json(json: &str) -> String {
    // Minimal parse — avoid adding serde_json as a dev-dependency.
    let key = "\"session_id\":";
    let start = json.find(key).expect("missing session_id in JSON") + key.len();
    let rest = &json[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().trim_matches('"').to_string()
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
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

/// Test the full session lifecycle: start → compile → compile (cached) → end.
/// This mirrors exactly what the CLI does in production.
#[tokio::test]
async fn cli_session_lifecycle() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("hello.cpp");
    let obj = tmp.path().join("hello.o");
    let log = tmp.path().join("session.log");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(
        &src,
        "#include <stdio.h>\nint main() { printf(\"hello\\n\"); return 0; }\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // ── Step 1: session-start (what `zccache session-start` does) ──
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: Some(log.to_string_lossy().into_owned().into()),
            track_stats: false,
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // At this point, the build system would:
    //   export ZCCACHE_SESSION_ID=<session_id>
    //   export CXX="zccache"  (or CMAKE_CXX_COMPILER_LAUNCHER=zccache)

    // ── Step 2: first compile (cache miss) ──
    // This is what `zccache clang++ -c hello.cpp -o hello.o` does.
    // The CLI strips args[0] (compiler) and sends args[1..] as Compile.
    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "first compile should succeed");
            assert!(!cached, "first compile should be a miss");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    assert!(obj.exists(), ".o should exist after first compile");
    let obj_data = std::fs::read(&obj).unwrap();

    // ── Step 3: second compile (cache hit) ──
    std::fs::remove_file(&obj).unwrap();

    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: vec![
                "-c".to_string(),
                src.to_string_lossy().into_owned(),
                "-o".to_string(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached compile should succeed");
            assert!(cached, "second compile should be a hit");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    assert!(obj.exists(), ".o should exist after cached compile");
    let cached_data = std::fs::read(&obj).unwrap();
    assert_eq!(obj_data.len(), cached_data.len(), "cached .o should match");

    // ── Step 4: session-end (what `zccache session-end <id>` does) ──
    client
        .send(&Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionEnded { .. }) => {}
        other => panic!("expected SessionEnded, got: {other:?}"),
    }

    // Session is now ended — compile with that session should fail
    client
        .send(&Request::Compile {
            session_id,
            args: vec!["-c".to_string(), src.to_string_lossy().into_owned()],
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::Error { message }) => {
            assert!(
                message.contains("unknown session"),
                "should report unknown session after end: {message}"
            );
        }
        other => panic!("expected Error after session-end, got: {other:?}"),
    }

    // ── Verify log ──
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(log_text.contains("[MISS]"), "log should show miss");
    assert!(log_text.contains("[HIT]"), "log should show hit");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test that ending a nonexistent session returns an error.
#[tokio::test]
async fn cli_session_end_invalid_id() {
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::SessionEnd {
            session_id: 999999.to_string(),
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::Error { message }) => {
            assert!(
                message.contains("unknown session") || message.contains("invalid session"),
                "expected session error, got: {message}"
            );
        }
        other => panic!("expected Error, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test the actual CLI binary end-to-end using subprocess.
/// This runs `zccache session-start`, captures the session ID,
/// then runs `zccache session-end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_binary_session_round_trip() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("cli_test.cpp");
    let obj = tmp.path().join("cli_test.o");
    let log = tmp.path().join("cli.log");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Ensure the CLI binary is built and up-to-date with the current protocol.
    // The dev-dependency on zccache-cli only ensures the library is compiled,
    // not the binary target. An explicit build guarantees the binary matches.
    let build_status = std::process::Command::new("cargo")
        .args(["build", "-p", "zccache-cli"])
        .status()
        .expect("failed to run cargo build");
    assert!(build_status.success(), "cargo build -p zccache-cli failed");

    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    let cli_binary = if cfg!(windows) {
        bin_dir.join("zccache.exe")
    } else {
        bin_dir.join("zccache")
    };
    assert!(
        cli_binary.exists(),
        "zccache binary not found at {}",
        cli_binary.display()
    );

    // session-start via CLI binary
    let output = std::process::Command::new(&cli_binary)
        .args([
            "session-start",
            "--cwd",
            &cwd,
            "--log",
            &log.to_string_lossy(),
            "--endpoint",
            &endpoint,
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "session-start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let session_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let session_id_str = parse_session_id_from_json(&session_json);

    // Pre-compute string args for the compiler invocation
    let clang_str = clang.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();

    // Compile via CLI binary (wrap mode, auto-detected)
    let output = std::process::Command::new(&cli_binary)
        .args([&clang_str, "-c", &src_str, "-o", &obj_str])
        .env("ZCCACHE_SESSION_ID", &session_id_str)
        .env("ZCCACHE_ENDPOINT", &endpoint)
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "wrap compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(obj.exists(), ".o should exist after compile");

    // Compile again — should hit cache
    std::fs::remove_file(&obj).unwrap();
    let output = std::process::Command::new(&cli_binary)
        .args([&clang_str, "-c", &src_str, "-o", &obj_str])
        .env("ZCCACHE_SESSION_ID", &session_id_str)
        .env("ZCCACHE_ENDPOINT", &endpoint)
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "cached compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(obj.exists(), ".o should exist after cached compile");

    // session-end via CLI binary
    let output = std::process::Command::new(&cli_binary)
        .args(["session-end", &session_id_str, "--endpoint", &endpoint])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "session-end failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify log shows miss then hit
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(log_text.contains("[MISS]"), "log should show miss");
    assert!(log_text.contains("[HIT]"), "log should show hit");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test ephemeral (sessionless) mode: `zccache clang++ -c foo.cpp -o foo.o`
/// without ZCCACHE_SESSION_ID. The CLI should auto-create a session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_binary_ephemeral_session() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("ephemeral.cpp");
    let obj = tmp.path().join("ephemeral.o");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Ensure CLI binary is up-to-date
    let build_status = std::process::Command::new("cargo")
        .args(["build", "-p", "zccache-cli"])
        .status()
        .expect("failed to run cargo build");
    assert!(build_status.success(), "cargo build -p zccache-cli failed");

    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    let cli_binary = if cfg!(windows) {
        bin_dir.join("zccache.exe")
    } else {
        bin_dir.join("zccache")
    };

    let clang_str = clang.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();

    // Compile WITHOUT ZCCACHE_SESSION_ID — ephemeral mode
    let output = std::process::Command::new(&cli_binary)
        .args([&clang_str, "-c", &src_str, "-o", &obj_str])
        .env("ZCCACHE_ENDPOINT", &endpoint)
        .env_remove("ZCCACHE_SESSION_ID")
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "ephemeral compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(obj.exists(), ".o should exist after ephemeral compile");

    // Compile again — should hit cache (new ephemeral session, but same cache)
    std::fs::remove_file(&obj).unwrap();
    let output = std::process::Command::new(&cli_binary)
        .args([&clang_str, "-c", &src_str, "-o", &obj_str])
        .env("ZCCACHE_ENDPOINT", &endpoint)
        .env_remove("ZCCACHE_SESSION_ID")
        .current_dir(&cwd)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "second ephemeral compile failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(obj.exists(), ".o should exist after second compile");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test that `Request::Clear` actually resets the cache:
/// session-start → compile (miss) → compile (hit) → clear → compile (miss again).
#[tokio::test]
async fn cli_clear_resets_cache() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("clear_test.cpp");
    let obj = tmp.path().join("clear_test.o");
    let cwd = tmp.path().to_string_lossy().into_owned();

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    let compile_args = vec![
        "-c".to_string(),
        src.to_string_lossy().into_owned(),
        "-o".to_string(),
        obj.to_string_lossy().into_owned(),
    ];

    // First compile → miss
    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: compile_args.clone(),
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "first compile should be a miss");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Second compile → hit
    std::fs::remove_file(&obj).unwrap();
    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: compile_args.clone(),
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0);
            assert!(cached, "second compile should be a hit");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Clear the cache
    client.send(&Request::Clear).await.unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Cleared {
            artifacts_removed, ..
        }) => {
            assert!(
                artifacts_removed > 0,
                "should have cleared at least one artifact"
            );
        }
        other => panic!("expected Cleared, got: {other:?}"),
    }

    // End old session and start a new one (old session's context was cleared)
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();
    let _: Option<Response> = client.recv().await.unwrap();

    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();

    let session_id2 = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // Compile again → should be a miss (cache was cleared)
    std::fs::remove_file(&obj).unwrap();
    client
        .send(&Request::Compile {
            session_id: session_id2,
            args: compile_args,
            cwd: cwd.into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "compile after clear should be a miss");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test that multi-file compilations work through the daemon.
///
/// When multiple source files are passed (e.g., `clang++ -c a.cpp b.cpp`),
/// the parser marks this as non-cacheable and the daemon falls back to
/// running the compiler directly. The compilation should still succeed.
#[tokio::test]
async fn cli_multi_file_compilation_runs_directly() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let src_a = tmp.path().join("multi_a.cpp");
    let src_b = tmp.path().join("multi_b.cpp");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // Two source files — each must produce its own .o
    std::fs::write(&src_a, "int foo() { return 1; }\n").unwrap();
    std::fs::write(&src_b, "int bar() { return 2; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.clone().into(),
            log_file: None,
            track_stats: true,
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // First compile: multi-file → both are cache misses
    let multi_args = vec![
        "-c".to_string(),
        src_a.to_string_lossy().into_owned(),
        src_b.to_string_lossy().into_owned(),
    ];
    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: multi_args.clone(),
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "multi-file compile should succeed");
            assert!(!cached, "first multi-file compile should be a miss");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Verify both .o files were produced
    let obj_a = tmp.path().join("multi_a.o");
    let obj_b = tmp.path().join("multi_b.o");
    assert!(obj_a.exists(), "multi_a.o should exist");
    assert!(obj_b.exists(), "multi_b.o should exist");

    // Second compile: same files → should be all cache hits
    client
        .send(&Request::Compile {
            session_id: session_id.clone(),
            args: multi_args,
            cwd: cwd.clone().into(),
            compiler: clang.to_string_lossy().into_owned().into(),
            env: None,
        })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "second multi-file compile should succeed");
            assert!(cached, "second multi-file compile should be all cache hits");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // End session and verify stats show misses from first compile, hits from second
    client
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionEnded { stats }) => {
            if let Some(s) = stats {
                assert!(
                    s.misses >= 2,
                    "first multi-file compile should have 2 misses, got: {}",
                    s.misses
                );
                assert!(
                    s.hits >= 2,
                    "second multi-file compile should have 2 hits, got: {}",
                    s.hits
                );
            }
        }
        other => panic!("expected SessionEnded, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Repro for the g++/gcc bug: session started with clang++ (C++ compiler),
/// then wrapping clang (C compiler) to compile a .c file with `-std=c11`.
///
/// Without the compiler override fix, the daemon would invoke clang++ for the
/// C file, causing "not valid for C++" warnings or outright failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_binary_compiler_override_cpp_session_c_file() {
    let clangpp = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    // Derive clang (C compiler) from clang++ path
    let clang = clangpp
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "clang.exe" } else { "clang" });
    if !clang.exists() {
        eprintln!("SKIP: clang not found at {}", clang.display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("repro.c");
    let obj = tmp.path().join("repro.o");
    let cwd = tmp.path().to_string_lossy().into_owned();

    // C code using C11 designated initializers — invalid under C++ mode
    std::fs::write(
        &src,
        "struct Point { int x; int y; };\n\
         int main(void) {\n\
         \tstruct Point p = { .x = 1, .y = 2 };\n\
         \treturn p.x + p.y - 3;\n\
         }\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Ensure CLI binary is up-to-date
    let build_status = std::process::Command::new("cargo")
        .args(["build", "-p", "zccache-cli"])
        .status()
        .expect("failed to run cargo build");
    assert!(build_status.success());

    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"))
        .parent()
        .unwrap();
    let cli_binary = if cfg!(windows) {
        bin_dir.join("zccache.exe")
    } else {
        bin_dir.join("zccache")
    };

    // Start session (compiler-agnostic now)
    let output = std::process::Command::new(&cli_binary)
        .args(["session-start", "--cwd", &cwd, "--endpoint", &endpoint])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "session-start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let session_json = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let session_id_str = parse_session_id_from_json(&session_json);

    // Wrap clang (C compiler) to compile a .c file with -std=c11.
    // The bug: without the fix, the daemon would invoke clang++ instead of clang.
    let clang_str = clang.to_string_lossy().into_owned();
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();

    let output = std::process::Command::new(&cli_binary)
        .args([&clang_str, "-std=c11", "-c", &src_str, "-o", &obj_str])
        .env("ZCCACHE_SESSION_ID", &session_id_str)
        .env("ZCCACHE_ENDPOINT", &endpoint)
        .current_dir(&cwd)
        .output()
        .unwrap();

    let stderr_text = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "C file with -std=c11 should compile when wrapping clang on a clang++ session.\n\
         This fails if the daemon uses the session compiler (clang++) instead of \
         the wrapped compiler (clang).\nstderr: {stderr_text}"
    );
    assert!(
        !stderr_text.contains("not valid for C++"),
        "compiler override should use clang, not clang++. stderr: {stderr_text}"
    );
    assert!(obj.exists(), ".o should exist");

    // Session-end
    let output = std::process::Command::new(&cli_binary)
        .args(["session-end", &session_id_str, "--endpoint", &endpoint])
        .output()
        .unwrap();
    assert!(output.status.success());

    shutdown.notify_one();
    server_handle.await.unwrap();
}
