//! Integration test: full CLI session flow.
//!
//! Tests the production workflow:
//!   session-start → set ZCCACHE_SESSION_ID → wrap compile → session-end
//!
//! Uses the daemon directly via IPC (same protocol the CLI uses).

use std::path::PathBuf;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

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

/// Test the full session lifecycle: start → compile → compile (cached) → end.
/// This mirrors exactly what the CLI does in production.
#[tokio::test]
async fn cli_session_lifecycle() {
    let clang = match find_clang() {
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
            working_dir: cwd.clone(),
            compiler: clang.to_string_lossy().into_owned(),
            log_file: Some(log.to_string_lossy().into_owned()),
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
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
            session_id,
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
            session_id,
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
        .send(&Request::SessionEnd { session_id })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::SessionEnded) => {}
        other => panic!("expected SessionEnded, got: {other:?}"),
    }

    // Session is now ended — compile with that session should fail
    client
        .send(&Request::Compile {
            session_id,
            args: vec!["-c".to_string(), src.to_string_lossy().into_owned()],
            cwd: cwd.clone(),
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
    assert!(log_text.contains("cache miss"), "log should show miss");
    assert!(log_text.contains("cache hit"), "log should show hit");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test that ending a nonexistent session returns an error.
#[tokio::test]
async fn cli_session_end_invalid_id() {
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::SessionEnd { session_id: 999999 })
        .await
        .unwrap();

    match client.recv().await.unwrap() {
        Some(Response::Error { message }) => {
            assert!(message.contains("unknown session"));
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
    let clang = match find_clang() {
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

    // Find the zccache CLI binary in the same directory as the test binary.
    // CARGO_BIN_EXE_zccache-daemon resolves to the daemon binary in target/debug/deps,
    // but the CLI binary is at target/debug/zccache(.exe).
    let test_bin = std::path::Path::new(env!("CARGO_BIN_EXE_zccache-daemon"));
    // Go up from target/debug/zccache-daemon.exe to target/debug/
    let bin_dir = test_bin.parent().unwrap();
    let cli_binary = if cfg!(windows) {
        bin_dir.join("zccache.exe")
    } else {
        bin_dir.join("zccache")
    };
    if !cli_binary.exists() {
        eprintln!(
            "SKIP: zccache binary not found at {}. Run `cargo build -p zccache-cli` first.",
            cli_binary.display()
        );
        shutdown.notify_one();
        server_handle.await.unwrap();
        return;
    }

    // session-start via CLI binary
    let output = std::process::Command::new(&cli_binary)
        .args([
            "session-start",
            "--compiler",
            &clang.to_string_lossy(),
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

    let session_id_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let _session_id: u64 = session_id_str
        .parse()
        .unwrap_or_else(|_| panic!("invalid session ID: {session_id_str:?}"));

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
    assert!(log_text.contains("cache miss"), "log should show miss");
    assert!(log_text.contains("cache hit"), "log should show hit");

    shutdown.notify_one();
    server_handle.await.unwrap();
}
