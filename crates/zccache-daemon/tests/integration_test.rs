//! End-to-end integration tests for the daemon.
//!
//! Tests the full client → daemon → clang toolchain discovery pipeline.

use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

/// Helper: start a daemon server on a unique endpoint and return the endpoint + shutdown handle.
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

#[tokio::test]
async fn test_client_connects_and_pings_daemon() {
    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_multiple_clients_concurrent() {
    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut handles = Vec::new();
    for _ in 0..5 {
        let ep = endpoint.clone();
        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            client.send(&Request::Ping).await.unwrap();
            let resp: Option<Response> = client.recv().await.unwrap();
            assert_eq!(resp, Some(Response::Pong));
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn test_session_start_with_nonexistent_compiler() {
    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            compiler: "/nonexistent/compiler".to_string(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::Error { message }) => {
            assert!(
                message.contains("not found"),
                "expected 'not found' in error: {message}"
            );
        }
        other => panic!("expected Error response, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// The main TDD target: client connects to daemon, starts a session
/// with the real clang toolchain, and the daemon discovers system includes.
#[tokio::test]
async fn test_session_start_with_clang_toolchain() {
    let clang_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            compiler: clang_path.to_string_lossy().into_owned(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::SessionStarted {
            session_id,
            system_includes,
        }) => {
            // Session ID should be valid (starting from 0)
            eprintln!("session_id: {session_id}");
            assert!(session_id < 1000, "session ID looks reasonable");

            // Clang should discover at least some system include paths
            eprintln!("system_includes ({}):", system_includes.len());
            for inc in &system_includes {
                eprintln!("  {inc}");
            }
            assert!(
                !system_includes.is_empty(),
                "clang should discover system include paths"
            );
        }
        other => panic!("expected SessionStarted, got: {other:?}"),
    }

    // Second session should get a different ID
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            compiler: clang_path.to_string_lossy().into_owned(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();

    let resp2: Option<Response> = client.recv().await.unwrap();
    match resp2 {
        Some(Response::SessionStarted {
            session_id: id2, ..
        }) => {
            assert!(id2 >= 1, "second session should have incremented ID");
        }
        other => panic!("expected SessionStarted for second session, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Test the full flow: ping → session start → status → shutdown.
#[tokio::test]
async fn test_full_client_flow() {
    let clang_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // 1. Ping
    client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    // 2. Session start
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            compiler: clang_path.to_string_lossy().into_owned(),
            log_file: None,
            track_stats: false,
        })
        .await
        .unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert!(matches!(resp, Some(Response::SessionStarted { .. })));

    // 3. Status
    client.send(&Request::Status).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert!(matches!(resp, Some(Response::Status(_))));

    // 4. Shutdown
    client.send(&Request::Shutdown).await.unwrap();
    let resp: Option<Response> = client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::ShuttingDown));

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// TDD: Compile hello.cpp through the daemon, verify caching works.
///
/// 1. Create a temp dir with hello.cpp
/// 2. Start daemon, start session with clang + log_file
/// 3. Compile hello.cpp → expect success (cache miss)
/// 4. Remove .o, compile hello.cpp again → expect success (cache hit)
/// 5. Read log_file → verify cache miss then cache hit entries
#[tokio::test]
async fn test_compile_hello_cpp_cached() {
    let clang_path = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => {
            eprintln!("skipping test: clang not found");
            return;
        }
    };

    // Create temp dir with hello.cpp
    let tmp = tempfile::tempdir().unwrap();
    let hello_cpp = tmp.path().join("hello.cpp");
    std::fs::write(
        &hello_cpp,
        r#"#include <stdio.h>
int main() {
    printf("hello world\n");
    return 0;
}
"#,
    )
    .unwrap();

    let log_file = tmp.path().join("session.log");
    let output_obj = tmp.path().join("hello.o");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // Start session with log file
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            compiler: clang_path.to_string_lossy().into_owned(),
            log_file: Some(log_file.to_string_lossy().into_owned()),
            track_stats: false,
        })
        .await
        .unwrap();

    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };

    // First compile — should be a cache miss
    client
        .send(&Request::Compile {
            session_id,
            args: vec![
                "-c".to_string(),
                hello_cpp.to_string_lossy().into_owned(),
                "-o".to_string(),
                output_obj.to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            compiler: None,
            env: None,
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "first compile should succeed");
            assert!(!cached, "first compile should be a cache miss");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Verify .o was produced
    assert!(output_obj.exists(), "output object file should exist");
    let first_obj_size = std::fs::metadata(&output_obj).unwrap().len();
    assert!(first_obj_size > 0, "object file should not be empty");

    // Remove .o so we can verify cache restores it
    std::fs::remove_file(&output_obj).unwrap();
    assert!(!output_obj.exists(), ".o should be deleted");

    // Second compile — should be a cache hit
    client
        .send(&Request::Compile {
            session_id,
            args: vec![
                "-c".to_string(),
                hello_cpp.to_string_lossy().into_owned(),
                "-o".to_string(),
                output_obj.to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            compiler: None,
            env: None,
        })
        .await
        .unwrap();

    let resp: Option<Response> = client.recv().await.unwrap();
    match resp {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "second compile should succeed");
            assert!(cached, "second compile should be a cache hit");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    // Verify .o was restored from cache
    assert!(
        output_obj.exists(),
        "cache hit should restore the object file"
    );
    let second_obj_size = std::fs::metadata(&output_obj).unwrap().len();
    assert_eq!(
        first_obj_size, second_obj_size,
        "cached .o should be same size"
    );

    // Read the log file and verify cache miss + hit entries
    let log_contents = std::fs::read_to_string(&log_file).unwrap();
    eprintln!("=== session log ===\n{log_contents}");

    assert!(
        log_contents.contains("[MISS]"),
        "log should contain '[MISS]' for first compile"
    );
    assert!(
        log_contents.contains("[HIT]"),
        "log should contain '[HIT]' for second compile"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}
