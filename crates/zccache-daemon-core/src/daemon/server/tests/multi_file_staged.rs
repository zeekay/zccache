//! Real compiler adversaries for private multi-source execution.

use super::super::*;
use super::server_ipc::start_daemon;

fn assert_markers_in_order(stream: &[u8], first: &str, second: &str) {
    let stream = String::from_utf8_lossy(stream);
    let first_index = stream
        .find(first)
        .unwrap_or_else(|| panic!("missing {first:?} in diagnostics: {stream}"));
    let second_index = stream
        .find(second)
        .unwrap_or_else(|| panic!("missing {second:?} in diagnostics: {stream}"));
    assert!(
        first_index < second_index,
        "diagnostics out of source order: {stream}"
    );
}

#[tokio::test]
#[ignore] // integration-level: starts a daemon and a real compiler
async fn failed_multi_source_compile_publishes_and_materializes_nothing() {
    let clang = match crate::test_support::find_clang() {
        Some(path) => path,
        None => return,
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.c");
        let bad = temp.path().join("bad.c");
        std::fs::write(&good, "int good(void) { return 1; }\n").unwrap();
        std::fs::write(&bad, "this is not valid C;\n").unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let (endpoint, server, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Clear).await.unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::Cleared { .. })
        ));
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".into(),
                    good.to_string_lossy().into_owned(),
                    bad.to_string_lossy().into_owned(),
                ],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code, cached, ..
            }) => {
                assert_ne!(exit_code, 0);
                assert!(!cached);
            }
            other => panic!("expected CompileResult, got {other:?}"),
        }
        assert!(!temp.path().join("good.o").exists());
        assert!(!temp.path().join("bad.o").exists());

        // Repair only the failing source. Both units must still miss: a
        // successful earlier unit from the failed batch must not have become
        // cache-visible even though its private object was complete.
        std::fs::write(&bad, "int bad(void) { return 2; }\n").unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![
                    "-c".into(),
                    good.to_string_lossy().into_owned(),
                    bad.to_string_lossy().into_owned(),
                ],
                cwd: cwd.into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::CompileResult {
                exit_code: 0,
                cached: false,
                ..
            })
        ));
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats: Some(stats) }) => {
                assert_eq!(stats.hits, 0, "failed batch must publish no unit");
                assert_eq!(stats.misses, 2, "repaired batch must rebuild both units");
            }
            other => panic!("expected SessionEnded with stats, got {other:?}"),
        }
        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: starts a daemon and a real compiler
async fn unsupported_shared_depfile_always_runs_original_batch() {
    let Some(clang) = crate::test_support::find_clang() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.cpp");
        let second = temp.path().join("second.cpp");
        let shared = temp.path().join("shared.d");
        let shared_blob = temp.path().join("sentinel.cache");
        std::fs::write(&first, "int first() { return 1; }\n").unwrap();
        std::fs::write(&second, "int second() { return 2; }\n").unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let args = vec![
            "-c".into(),
            "-MMD".into(),
            "-MF".into(),
            shared.to_string_lossy().into_owned(),
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        let (endpoint, server, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };

        for run in 0..2 {
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: args.clone(),
                    cwd: cwd.clone().into(),
                    compiler: clang.to_string_lossy().into_owned().into(),
                    env: None,
                    stdin: Vec::new(),
                })
                .await
                .unwrap();
            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code,
                    cached,
                    stderr,
                    ..
                }) => {
                    assert_eq!(
                        exit_code,
                        0,
                        "unchanged batch failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                    assert!(!cached, "unsupported run {run} must execute directly");
                }
                other => panic!("expected CompileResult, got {other:?}"),
            }
            assert!(
                shared.exists(),
                "direct invocation must recreate shared depfile"
            );
            if run == 0 {
                let first_object = temp.path().join("first.o");
                for path in [&first_object, &temp.path().join("second.o"), &shared] {
                    std::fs::remove_file(path).unwrap();
                }
                std::fs::write(&shared_blob, b"immutable shared cache bytes").unwrap();
                std::fs::hard_link(&shared_blob, &first_object).unwrap();
            } else {
                assert_eq!(
                    std::fs::read(&shared_blob).unwrap(),
                    b"immutable shared cache bytes",
                    "direct compiler overwrite must not write through a hardlink"
                );
            }
        }
        std::fs::write(&second, "this is not valid C++;\n").unwrap();
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args,
                cwd: cwd.into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::CompileResult { exit_code, .. }) if exit_code != 0
        ));
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats: Some(stats) }) => {
                assert_eq!(stats.errors, 1, "direct failure must be session-visible");
            }
            other => panic!("expected SessionEnded with stats, got {other:?}"),
        }
        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore] // integration-level: requires clang-cl and starts a daemon
async fn clang_cl_directory_outputs_survive_forced_response_spill_and_hit() {
    let Some(clang_cl) = crate::test_support::find_clang_cl() else {
        return;
    };
    crate::test_support::test_timeout(async move {
        let temp = tempfile::tempdir().unwrap();
        let source_c = temp.path().join("plain.c");
        let source_cpp = temp.path().join("space name.cpp");
        let objects = temp.path().join("objects with spaces");
        std::fs::create_dir(&objects).unwrap();
        std::fs::write(&source_c, "int plain(void) { return 1; }\n").unwrap();
        std::fs::write(&source_cpp, "int spaced() { return 2; }\n").unwrap();
        let cwd = temp.path().to_string_lossy().into_owned();
        let mut args = vec![
            "/c".into(),
            "/Tc".into(),
            source_c.to_string_lossy().into_owned(),
            format!("/Tp{}", source_cpp.display()),
            format!("/Fo{}\\", objects.display()),
        ];
        for index in 0..900 {
            args.push(format!(
                "/DRESPONSE_PADDING_{index}=\"value with spaces {index}\""
            ));
        }
        let (endpoint, server, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();
        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got {other:?}"),
        };
        let outputs = [objects.join("plain.obj"), objects.join("space name.obj")];
        for run in 0..2 {
            client
                .send(&Request::Compile {
                    session_id: session_id.clone(),
                    args: args.clone(),
                    cwd: cwd.clone().into(),
                    compiler: clang_cl.to_string_lossy().into_owned().into(),
                    env: None,
                    stdin: Vec::new(),
                })
                .await
                .unwrap();
            match client.recv().await.unwrap() {
                Some(Response::CompileResult {
                    exit_code,
                    cached,
                    stderr,
                    ..
                }) => {
                    assert_eq!(
                        exit_code,
                        0,
                        "clang-cl failed: {}",
                        String::from_utf8_lossy(&stderr)
                    );
                    assert_eq!(cached, run == 1);
                }
                other => panic!("expected CompileResult, got {other:?}"),
            }
            for output in &outputs {
                assert!(output.exists(), "missing {}", output.display());
                if run == 0 {
                    std::fs::remove_file(output).unwrap();
                }
            }
        }
        shutdown.notify_one();
        server.await.unwrap();
    })
    .await;
}

/// Multi-file compilations publish private outputs and restore them on hit.
#[tokio::test]
#[ignore] // integration-level: starts real daemon with IPC + compiler
async fn cli_multi_file_compilation_runs_directly() {
    let clang = match crate::test_support::find_clang() {
        Some(p) => p,
        None => return,
    };

    crate::test_support::test_timeout(async move {
        let tmp = tempfile::tempdir().unwrap();
        let src_a = tmp.path().join("multi_a.cpp");
        let src_b = tmp.path().join("multi_b.cpp");
        let rsp = tmp.path().join("multi.rsp");
        let cwd = tmp.path().to_string_lossy().into_owned();

        std::fs::write(&src_a, "#warning MULTI_A\nint foo() { return 1; }\n").unwrap();
        std::fs::write(&src_b, "#warning MULTI_B\nint bar() { return 2; }\n").unwrap();
        std::fs::write(
            &rsp,
            format!(
                "-c -MMD \"{}\" \"{}\"",
                src_a.to_string_lossy().replace('\\', "/"),
                src_b.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = crate::ipc::connect(&endpoint).await.unwrap();

        // Start session
        client
            .send(&Request::SessionStart {
                client_pid: std::process::id(),
                working_dir: cwd.clone().into(),
                log_file: None,
                track_stats: true,
                journal_path: None,
                profile: false,
                private_daemon: None,
            })
            .await
            .unwrap();

        let session_id = match client.recv().await.unwrap() {
            Some(Response::SessionStarted { session_id, .. }) => session_id,
            other => panic!("expected SessionStarted, got: {other:?}"),
        };
        client.send(&Request::Clear).await.unwrap();
        assert!(matches!(
            client.recv().await.unwrap(),
            Some(Response::Cleared { .. })
        ));

        // First compile: multi-file → both are cache misses
        let multi_args = vec![format!("@{}", rsp.display())];
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args.clone(),
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code,
                cached,
                stderr,
                ..
            }) => {
                assert_eq!(
                    exit_code,
                    0,
                    "multi-file compile should succeed: {}",
                    String::from_utf8_lossy(&stderr)
                );
                assert!(!cached, "first multi-file compile should be a miss");
                assert_markers_in_order(&stderr, "MULTI_A", "MULTI_B");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }

        // Verify both .o files were produced
        let obj_a = tmp.path().join("multi_a.o");
        let obj_b = tmp.path().join("multi_b.o");
        let dep_a = tmp.path().join("multi_a.d");
        let dep_b = tmp.path().join("multi_b.d");
        assert!(obj_a.exists(), "multi_a.o should exist");
        assert!(obj_b.exists(), "multi_b.o should exist");
        assert!(dep_a.exists(), "multi_a.d should exist");
        assert!(dep_b.exists(), "multi_b.d should exist");
        for depfile in [&dep_a, &dep_b] {
            let contents = std::fs::read_to_string(depfile).unwrap();
            assert!(!contents.contains(".compile-multi-"));
        }
        let miss_bytes = [
            std::fs::read(&obj_a).unwrap(),
            std::fs::read(&obj_b).unwrap(),
            std::fs::read(&dep_a).unwrap(),
            std::fs::read(&dep_b).unwrap(),
        ];
        std::fs::remove_file(&obj_a).unwrap();
        std::fs::remove_file(&obj_b).unwrap();
        std::fs::remove_file(&dep_a).unwrap();
        std::fs::remove_file(&dep_b).unwrap();

        // Second compile: same files → should be all cache hits
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: multi_args,
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code,
                cached,
                stderr,
                ..
            }) => {
                assert_eq!(exit_code, 0, "second multi-file compile should succeed");
                assert!(cached, "second multi-file compile should be all cache hits");
                assert_markers_in_order(&stderr, "MULTI_A", "MULTI_B");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
        assert!(obj_a.exists(), "cache hit should restore multi_a.o");
        assert!(obj_b.exists(), "cache hit should restore multi_b.o");
        assert!(dep_a.exists(), "cache hit should restore multi_a.d");
        assert!(dep_b.exists(), "cache hit should restore multi_b.d");
        for (path, expected) in [&obj_a, &obj_b, &dep_a, &dep_b]
            .into_iter()
            .zip(&miss_bytes)
        {
            assert_eq!(
                std::fs::read(path).unwrap(),
                expected.as_slice(),
                "cache hit restored the wrong bytes to {}",
                path.display()
            );
        }

        // Third compile: mutate the response file to retain one hit and add
        // one new source. This is deterministic without waiting for a watcher
        // event and proves response-file cache invalidation/source filtering.
        let src_c = tmp.path().join("multi_c.cpp");
        let obj_c = tmp.path().join("multi_c.o");
        let dep_c = tmp.path().join("multi_c.d");
        std::fs::write(&src_c, "#warning MULTI_C\nint baz() { return 3; }\n").unwrap();
        std::fs::write(
            &rsp,
            format!(
                "-c -MMD \"{}\" \"{}\"",
                src_a.to_string_lossy().replace('\\', "/"),
                src_c.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        for path in [&obj_a, &dep_a] {
            std::fs::remove_file(path).unwrap();
        }
        client
            .send(&Request::Compile {
                session_id: session_id.clone(),
                args: vec![format!("@{}", rsp.display())],
                cwd: cwd.clone().into(),
                compiler: clang.to_string_lossy().into_owned().into(),
                env: None,
                stdin: Vec::new(),
            })
            .await
            .unwrap();
        match client.recv().await.unwrap() {
            Some(Response::CompileResult {
                exit_code,
                cached,
                stderr,
                ..
            }) => {
                assert_eq!(exit_code, 0, "mixed hit/miss compile should succeed");
                assert!(!cached, "one changed source must make the batch a miss");
                assert_markers_in_order(&stderr, "MULTI_A", "MULTI_C");
            }
            other => panic!("expected CompileResult, got: {other:?}"),
        }
        for path in [&obj_a, &obj_c, &dep_a, &dep_c] {
            assert!(
                path.exists(),
                "mixed hit/miss must restore {}",
                path.display()
            );
        }

        // End session and verify stats
        client
            .send(&Request::SessionEnd { session_id })
            .await
            .unwrap();

        match client.recv().await.unwrap() {
            Some(Response::SessionEnded { stats }) => {
                if let Some(s) = stats {
                    assert!(
                        s.misses >= 3,
                        "multi-file requests should have at least 3 misses, got: {}",
                        s.misses
                    );
                    assert!(
                        s.hits >= 3,
                        "multi-file requests should have at least 3 hits, got: {}",
                        s.hits
                    );
                }
            }
            other => panic!("expected SessionEnded, got: {other:?}"),
        }

        shutdown.notify_one();
        server_handle.await.unwrap();
    })
    .await;
}
