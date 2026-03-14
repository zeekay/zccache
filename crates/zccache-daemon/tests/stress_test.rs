//! Adversarial stress tests for the compilation cache.
//!
//! These tests intentionally try to break the caching with corner cases:
//! - Source modifications must invalidate cache
//! - Different flags must produce different cache entries
//! - Compile errors must NOT be cached
//! - Concurrent compilations must not corrupt state
//! - Local header changes must invalidate cache
//! - Edge cases in paths, empty files, etc.

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
    let handle = tokio::spawn(async move { server.run(0).await.unwrap() });
    (endpoint, handle, shutdown)
}

/// Helper: start a session. Returns (session_id, compiler_string).
async fn start_session(
    client: &mut ClientConn,
    clang: &std::path::Path,
    cwd: &str,
    log_file: &str,
) -> (String, String) {
    let compiler_str = clang.to_string_lossy().into_owned();
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string(),
            log_file: Some(log_file.to_string()),
            track_stats: false,
        })
        .await
        .unwrap();
    let session_id = match client.recv().await.unwrap() {
        Some(Response::SessionStarted { session_id, .. }) => session_id,
        other => panic!("expected SessionStarted, got: {other:?}"),
    };
    (session_id, compiler_str)
}

/// Helper: send a compile request, return (exit_code, cached).
async fn compile(
    client: &mut ClientConn,
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string(),
            compiler: compiler.to_string(),
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

// ═══════════════════════════════════════════════════════════════════════
// CORRECTNESS ATTACKS
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn adversarial_different_flags_different_cache_entries() {
    let clang = match zccache_test_support::find_clang() {
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
        "static int x=0;\nint get(){return x++;}\nint main(){return get()+get();}\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(!c);
    let obj_o0 = std::fs::read(&obj).unwrap();

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(!c, "-O2 compile MUST NOT hit the -O0 cache entry");
    let obj_o2 = std::fs::read(&obj).unwrap();
    assert_ne!(
        obj_o0, obj_o2,
        "-O0 and -O2 should produce different .o files"
    );

    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(c, "-O0 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o0,
        "cached -O0 must match original"
    );

    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(c, "-O2 recompile should be a cache hit");
    assert_eq!(
        std::fs::read(&obj).unwrap(),
        obj_o2,
        "cached -O2 must match original"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_define_changes_invalidate_cache() {
    let clang = match zccache_test_support::find_clang() {
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
        "#ifdef RETURN_ZERO\nint main(){return 0;}\n#else\nint main(){return 1;}\n#endif\n",
    )
    .unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c);
    let obj_no_define = std::fs::read(&obj).unwrap();

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(!c, "-DRETURN_ZERO MUST be a cache miss");
    let obj_with_define = std::fs::read(&obj).unwrap();
    assert_ne!(
        obj_no_define, obj_with_define,
        "different defines must produce different .o"
    );

    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_compile_errors_never_cached() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("errors.cpp");
    let obj = tmp.path().join("errors.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { SYNTAX ERROR HERE }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_ne!(ec, 0);
    assert!(!c, "failed compile must not be cached");

    let (ec2, c2) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_ne!(ec2, 0);
    assert!(!c2, "failed compile must NEVER be returned from cache");

    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let (ec3, c3) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec3, 0);
    assert!(!c3, "new source should be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// CONCURRENCY ATTACKS
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn adversarial_concurrent_same_file() {
    let clang = match zccache_test_support::find_clang() {
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
            let (sid, comp) =
                start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
            let (ec, _c) = compile(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(ec, 0, "concurrent compile {i} should succeed");
            assert!(obj.exists(), "output should exist for compile {i}");
            std::fs::read(&obj).unwrap()
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    for (i, obj) in results.iter().enumerate() {
        assert!(!obj.is_empty(), "concurrent compile {i} produced empty .o");
    }
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

#[tokio::test]
async fn adversarial_concurrent_different_files() {
    let clang = match zccache_test_support::find_clang() {
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
        std::fs::write(&src, format!("int main() {{ return {i}; }}\n")).unwrap();
        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            let (sid, comp) =
                start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
            let (ec, c) = compile(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(ec, 0);
            assert!(!c, "first compile of file{i} should be a miss");
            let obj_data = std::fs::read(&obj).unwrap();
            std::fs::remove_file(&obj).unwrap();
            let (ec, c) = compile(
                &mut client,
                &sid,
                &comp,
                &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
                &cwd,
            )
            .await;
            assert_eq!(ec, 0);
            assert!(c, "second compile of file{i} should be a hit");
            let obj_data2 = std::fs::read(&obj).unwrap();
            assert_eq!(obj_data, obj_data2, "cached .o for file{i} must match");
            obj_data
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    for i in 1..results.len() {
        assert_ne!(
            results[0], results[i],
            "file0 and file{i} should produce different .o (different source)"
        );
    }
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_cross_session_cache_sharing() {
    let clang = match zccache_test_support::find_clang() {
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
    let mut client1 = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid1, comp1) = start_session(&mut client1, &clang, &cwd, &log1.to_string_lossy()).await;
    let (ec, c) = compile(
        &mut client1,
        &sid1,
        &comp1,
        &["-c", &src.to_string_lossy(), "-o", &obj1.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "session 1 first compile should be a miss");
    let obj1_data = std::fs::read(&obj1).unwrap();

    let mut client2 = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid2, comp2) = start_session(&mut client2, &clang, &cwd, &log2.to_string_lossy()).await;
    assert_ne!(sid1, sid2, "sessions should have different IDs");
    let (ec, c) = compile(
        &mut client2,
        &sid2,
        &comp2,
        &["-c", &src.to_string_lossy(), "-o", &obj2.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "session 2 should hit session 1's cached result");
    assert_eq!(
        obj1_data,
        std::fs::read(&obj2).unwrap(),
        "cross-session cached .o must match"
    );
    assert!(std::fs::read_to_string(&log1).unwrap().contains("[MISS]"));
    assert!(std::fs::read_to_string(&log2).unwrap().contains("[HIT]"));

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// EDGE CASES
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn adversarial_invalid_session_id() {
    if zccache_test_support::find_clang().is_none() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("invalid_session.cpp");
    std::fs::write(&src, "int main() { return 0; }\n").unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();

    client
        .send(&Request::Compile {
            session_id: 999999.to_string(),
            args: vec![
                "-c".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                "out.o".into(),
            ],
            cwd,
            compiler: "/usr/bin/clang".to_string(),
            env: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::Error { message }) => assert!(
            message.contains("unknown session") || message.contains("invalid session"),
            "error should mention session error: {message}"
        ),
        other => panic!("expected Error for invalid session, got: {other:?}"),
    }
    client.send(&Request::Ping).await.unwrap();
    assert_eq!(
        client.recv().await.unwrap(),
        Some(Response::Pong),
        "server should survive bad requests"
    );
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_non_cacheable_passthrough() {
    let clang = match zccache_test_support::find_clang() {
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
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    client
        .send(&Request::Compile {
            session_id: sid.clone(),
            args: vec!["-E".into(), src.to_string_lossy().into_owned()],
            cwd: cwd.clone(),
            compiler: comp.clone(),
            env: None,
        })
        .await
        .unwrap();
    match client.recv().await.unwrap() {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_eq!(exit_code, 0, "preprocessing should succeed");
            assert!(!cached, "preprocessing must not be cached");
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c, "first real compile should be a miss");
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("[DIRECT]"),
        "log should mention direct (non-cacheable)"
    );
    assert!(log_text.contains("[MISS]"), "log should mention miss");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_empty_source_file() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("empty.cpp");
    let obj = tmp.path().join("empty.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0, "empty source should compile");
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "empty source recompile should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_warnings_still_cached() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("warnings.cpp");
    let obj = tmp.path().join("warnings.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { int unused_var = 42; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(c, "warnings-only compilation should still be cached");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_output_path_does_not_affect_cache_key() {
    let clang = match zccache_test_support::find_clang() {
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
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj_a.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(!c);
    let data_a = std::fs::read(&obj_a).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj_b.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "same source to different output path should hit cache");
    assert_eq!(
        data_a,
        std::fs::read(&obj_b).unwrap(),
        "cached .o should be identical regardless of output path"
    );
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_rapid_recompile_cycle() {
    let clang = match zccache_test_support::find_clang() {
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
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
    let src_str = src.to_string_lossy().into_owned();
    let obj_str = obj.to_string_lossy().into_owned();
    let args_owned: Vec<&str> = vec!["-c", &src_str, "-o", &obj_str];

    let (ec, c) = compile(&mut client, &sid, &comp, &args_owned, &cwd).await;
    assert_eq!(ec, 0);
    assert!(!c);
    let reference_obj = std::fs::read(&obj).unwrap();
    for i in 0..20 {
        std::fs::remove_file(&obj).unwrap();
        let (ec, c) = compile(&mut client, &sid, &comp, &args_owned, &cwd).await;
        assert_eq!(ec, 0, "rapid cycle {i} should succeed");
        assert!(c, "rapid cycle {i} should be a cache hit");
        assert_eq!(
            std::fs::read(&obj).unwrap(),
            reference_obj,
            "rapid cycle {i} .o should match reference"
        );
    }
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        log_text.matches("[MISS]").count(),
        1,
        "expected exactly 1 miss"
    );
    let hit_count = log_text.matches("[HIT]").count() + log_text.matches("[HIT_FAST]").count();
    assert_eq!(hit_count, 20, "expected exactly 20 hits");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_spaces_in_filename() {
    let clang = match zccache_test_support::find_clang() {
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
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0, "file with spaces should compile");
    assert!(!c);
    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
        &["-c", &src.to_string_lossy(), "-o", &obj.to_string_lossy()],
        &cwd,
    )
    .await;
    assert_eq!(ec, 0);
    assert!(c, "recompile of file with spaces should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

#[tokio::test]
async fn adversarial_werror_vs_no_werror() {
    let clang = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("werror.cpp");
    let obj = tmp.path().join("werror.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "int main() { int x = 0; return 0; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, comp) = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_ne!(ec, 0, "-Werror should cause compile failure");
    assert!(!c, "failed -Werror compile must not be cached");

    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0, "without -Werror should succeed");
    assert!(!c, "different flags = different cache key = miss");

    std::fs::remove_file(&obj).unwrap();
    let (ec, c) = compile(
        &mut client,
        &sid,
        &comp,
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
    assert_eq!(ec, 0);
    assert!(c, "-Wall recompile (no -Werror) should hit cache");
    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// COMPILER OVERRIDE
// ═══════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn compiler_override_uses_wrapped_compiler() {
    let clangpp = match zccache_test_support::find_clang() {
        Some(p) => p,
        None => return,
    };
    let clang = clangpp
        .parent()
        .unwrap()
        .join(if cfg!(windows) { "clang.exe" } else { "clang" });
    if !clang.exists() {
        eprintln!("SKIP: clang not found at {}", clang.display());
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("test.c");
    let obj = tmp.path().join("test.o");
    let log = tmp.path().join("log.txt");
    let cwd = tmp.path().to_string_lossy().into_owned();
    std::fs::write(&src, "struct Point { int x; int y; };\nint main(void) {\n\tstruct Point p = { .x = 1, .y = 2 };\n\treturn p.x + p.y - 3;\n}\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;
    let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
    let (sid, _clangpp_compiler) =
        start_session(&mut client, &clangpp, &cwd, &log.to_string_lossy()).await;

    client
        .send(&Request::Compile {
            session_id: sid.clone(),
            args: vec![
                "-c".into(),
                "-std=c11".into(),
                src.to_string_lossy().into_owned(),
                "-o".into(),
                obj.to_string_lossy().into_owned(),
            ],
            cwd: cwd.clone(),
            compiler: clang.to_string_lossy().into_owned(),
            env: None,
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
            let stderr_str = String::from_utf8_lossy(&stderr);
            assert_eq!(
                exit_code, 0,
                "C file with -std=c11 should compile with clang override. stderr: {stderr_str}"
            );
            assert!(!cached, "first compile should be a miss");
            assert!(
                !stderr_str.contains("not valid for C++"),
                "compiler override should use clang, not clang++. stderr: {stderr_str}"
            );
        }
        Some(Response::Error { message }) => {
            panic!("compile error (compiler override not working?): {message}")
        }
        other => panic!("expected CompileResult, got: {other:?}"),
    }
    assert!(obj.exists(), "object file should be produced");
    shutdown.notify_one();
    server_handle.await.unwrap();
}
