//! Basic daemon IPC lifecycle tests.

use super::super::*;
use super::server_ipc::start_daemon;

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_ping_pong() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_shutdown_request() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Shutdown).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::ShuttingDown));

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + file watcher
async fn test_server_clear_empty() {
    crate::test_support::test_timeout(async {
        let (endpoint, server_task, shutdown) = start_daemon().await;

        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Clear).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Cleared {
                metadata_cleared,
                dep_graph_contexts_cleared,
                ..
            }) => {
                // artifacts_removed may be >0 if persistent cache has entries
                // from a prior run. Metadata and dep graph are always fresh.
                assert_eq!(metadata_cleared, 0);
                assert_eq!(dep_graph_contexts_cleared, 0);
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    })
    .await;
}
