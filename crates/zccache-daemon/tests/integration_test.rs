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
        server.run().await.unwrap();
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
    // Find the clang toolchain
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap();
    let clang_path = std::path::PathBuf::from(&home)
        .join(".clang-tool-chain")
        .join("clang")
        .join("win")
        .join("x86_64")
        .join("bin")
        .join("clang++.exe");

    if !clang_path.exists() {
        eprintln!("skipping test: clang not found at {}", clang_path.display());
        return;
    }

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
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap();
    let clang_path = std::path::PathBuf::from(&home)
        .join(".clang-tool-chain")
        .join("clang")
        .join("win")
        .join("x86_64")
        .join("bin")
        .join("clang++.exe");

    if !clang_path.exists() {
        eprintln!("skipping test: clang not found");
        return;
    }

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
