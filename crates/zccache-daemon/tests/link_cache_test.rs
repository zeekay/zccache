//! Integration tests for static library (.a) caching.
//!
//! Tests the full flow: compile .o files → `ar rcsD` → cache hit/miss.

use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

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

/// Create fake object files (ar doesn't validate content).
fn write_fake_objects(dir: &std::path::Path, names: &[&str]) {
    for (i, name) in names.iter().enumerate() {
        let content = format!("fake object file {} content {}", name, i);
        std::fs::write(dir.join(name), content).unwrap();
    }
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_cache_miss_then_hit() {
    let ar_path = match zccache_test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o", "b.o"]);

    let output_lib = tmp.path().join("libfoo.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // First link — should be a cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
                tmp.path().join("b.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code,
            cached,
            warning,
            ..
        }) => {
            assert_eq!(exit_code, 0, "ar should succeed");
            assert!(!cached, "first link should be a cache miss");
            assert!(warning.is_none(), "D flag present — no warning expected");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    assert!(
        output_lib.exists(),
        "libfoo.a should exist after first link"
    );
    let first_size = std::fs::metadata(&output_lib).unwrap().len();
    assert!(first_size > 0, "archive should not be empty");
    let first_contents = std::fs::read(&output_lib).unwrap();

    // Delete the output so we can verify cache restores it
    std::fs::remove_file(&output_lib).unwrap();
    assert!(!output_lib.exists(), "libfoo.a should be deleted");

    // Second link — should be a cache hit
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
                tmp.path().join("b.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "cached ar should succeed");
            assert!(cached, "second link should be a cache hit");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // Verify the cached output was restored
    assert!(
        output_lib.exists(),
        "cache hit should restore the archive file"
    );
    let second_contents = std::fs::read(&output_lib).unwrap();
    assert_eq!(
        first_contents, second_contents,
        "cached archive should be byte-identical"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_cache_invalidated_on_input_change() {
    let ar_path = match zccache_test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["x.o", "y.o"]);

    let output_lib = tmp.path().join("libbar.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    let make_args = |lib: &std::path::Path, dir: &std::path::Path| -> Vec<String> {
        vec![
            "rcsD".to_string(),
            lib.to_string_lossy().into_owned(),
            dir.join("x.o").to_string_lossy().into_owned(),
            dir.join("y.o").to_string_lossy().into_owned(),
        ]
    };

    // First link — cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: make_args(&output_lib, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match &resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(*exit_code, 0);
            assert!(!cached, "first link should miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    let original_archive = std::fs::read(&output_lib).unwrap();

    // Modify one input file
    std::fs::write(tmp.path().join("x.o"), "MODIFIED content for x.o").unwrap();

    // Delete output so we can verify it gets recreated
    std::fs::remove_file(&output_lib).unwrap();

    // Third link — should be a cache miss (input changed)
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: make_args(&output_lib, tmp.path()),
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0);
            assert!(!cached, "link after input change should be a cache miss");
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // The new archive should differ from the original
    let new_archive = std::fs::read(&output_lib).unwrap();
    assert_ne!(
        original_archive, new_archive,
        "archive should differ after input change"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_non_deterministic_warning() {
    let ar_path = match zccache_test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o"]);

    let output_lib = tmp.path().join("libwarn.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // ar rcs (no D flag) — should warn about non-determinism
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: vec![
                "rcs".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("a.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code,
            cached,
            warning,
            ..
        }) => {
            assert_eq!(exit_code, 0, "ar should succeed even without D flag");
            assert!(!cached, "first invocation should be a cache miss");
            assert!(
                warning.is_some(),
                "should warn about non-deterministic invocation"
            );
            let w = warning.unwrap();
            assert!(
                w.contains("non-deterministic"),
                "warning should mention non-determinism: {w}"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    // The archive should still be produced
    assert!(output_lib.exists(), "ar should produce output");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_ar_non_cacheable_passthrough() {
    let ar_path = match zccache_test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["a.o"]);

    // First, create an archive we can list
    let lib_path = tmp.path().join("liblist.a");
    let status = std::process::Command::new(&ar_path)
        .args([
            "rcsD",
            &lib_path.to_string_lossy(),
            &tmp.path().join("a.o").to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "ar rcsD should succeed");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // ar t (list operation) — non-cacheable, should pass through
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: vec!["t".to_string(), lib_path.to_string_lossy().into_owned()],
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();

    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::LinkResult {
            exit_code,
            cached,
            stdout,
            ..
        }) => {
            assert_eq!(exit_code, 0, "ar t should succeed");
            assert!(!cached, "non-cacheable operation should not be cached");
            let output = String::from_utf8_lossy(&stdout);
            assert!(
                output.contains("a.o"),
                "ar t should list archive members: {output}"
            );
        }
        other => panic!("expected LinkResult, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
#[ignore] // Integration test — starts a real daemon. Run with `test --full`.
async fn test_link_stats_in_status() {
    let ar_path = match zccache_test_support::find_on_path("ar") {
        Some(p) => p,
        None => {
            eprintln!("skipping test: ar not found on PATH");
            return;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    write_fake_objects(tmp.path(), &["s.o"]);

    let output_lib = tmp.path().join("libstats.a");

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    // One deterministic link — cache miss
    client
        .send(&Request::LinkEphemeral {
            client_pid: std::process::id(),
            working_dir: tmp.path().to_string_lossy().into_owned(),
            tool: ar_path.to_string_lossy().into_owned(),
            args: vec![
                "rcsD".to_string(),
                output_lib.to_string_lossy().into_owned(),
                tmp.path().join("s.o").to_string_lossy().into_owned(),
            ],
            cwd: tmp.path().to_string_lossy().into_owned(),
            env: None,
        })
        .await
        .unwrap();
    let _: Option<Response> = client.recv().await.unwrap(); // consume response

    // Check status — should show link stats
    client.send(&Request::Status).await.unwrap();
    let resp = client.recv().await.unwrap();
    match resp {
        Some(Response::Status(s)) => {
            assert!(
                s.total_links >= 1,
                "status should show at least 1 link: total_links={}",
                s.total_links
            );
            assert!(
                s.link_misses >= 1,
                "status should show at least 1 link miss: link_misses={}",
                s.link_misses
            );
        }
        other => panic!("expected Status response, got: {other:?}"),
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}
