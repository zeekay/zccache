//! Response file (`@file`) cache key integration tests.
//!
//! Verifies that the daemon correctly expands response files before computing
//! cache keys, so that changes to response file content invalidate the cache.
//!
//! Run all:    uv run cargo test -p zccache-daemon --test response_file_cache -- --ignored --nocapture
//! Run single: uv run cargo test -p zccache-daemon --test response_file_cache -- <test_name> --ignored --nocapture

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use zccache_daemon::DaemonServer;
use zccache_protocol::{Request, Response};

// ─── Platform types ──────────────────────────────────────────────────────────

#[cfg(unix)]
type ClientConn = zccache_ipc::IpcConnection;
#[cfg(windows)]
type ClientConn = zccache_ipc::IpcClientConnection;

// ─── Helpers ─────────────────────────────────────────────────────────────────

async fn start_daemon() -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(
    client: &mut ClientConn,
    _clang: &Path,
    cwd: &str,
    log_file: &str,
) -> String {
    client
        .send(&Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.to_string().into(),
            log_file: Some(log_file.to_string().into()),
            track_stats: false,
            journal_path: None,
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
    session_id: &str,
    compiler: &str,
    args: &[&str],
    cwd: &str,
) -> (i32, bool) {
    client
        .send(&Request::Compile {
            session_id: session_id.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.to_string().into(),
            compiler: compiler.to_string().into(),
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

struct TestHarness {
    clang: PathBuf,
    tmp: tempfile::TempDir,
    #[expect(dead_code)]
    endpoint: String,
    server_handle: JoinHandle<()>,
    shutdown: Arc<Notify>,
    client: ClientConn,
    session_id: String,
}

impl TestHarness {
    async fn new() -> Option<Self> {
        let clang = zccache_test_support::find_clang()?;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("log.txt");
        let cwd = tmp.path().to_string_lossy().into_owned();

        let (endpoint, server_handle, shutdown) = start_daemon().await;
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let session_id = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

        Some(Self {
            clang,
            tmp,
            endpoint,
            server_handle,
            shutdown,
            client,
            session_id,
        })
    }

    fn cwd(&self) -> String {
        self.tmp.path().to_string_lossy().into_owned()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.tmp.path().join(name)
    }

    fn write_file(&self, name: &str, content: &str) -> PathBuf {
        let p = self.path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    async fn compile_with_args(&mut self, args: &[&str]) -> (i32, bool) {
        let cwd = self.cwd();
        let compiler = self.clang.to_string_lossy().into_owned();
        compile(&mut self.client, &self.session_id, &compiler, args, &cwd).await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// MUTATION DETECTION — content changes must invalidate cache
// ═══════════════════════════════════════════════════════════════════════════════

/// Changing optimization level in response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_content_change_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("flags.rsp", "-O2");

    // First compile: miss
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "first compile must miss");

    // Same args: hit
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(cached, "identical args must hit");

    // Change rsp content: must miss
    h.write_file("flags.rsp", "-O3");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed -O2 to -O3 in rsp must miss");

    h.shutdown().await;
}

/// Changing a define value in response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_define_change_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return VER; }\n");
    h.write_file("flags.rsp", "-DVER=1");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    h.write_file("flags.rsp", "-DVER=2");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed -DVER=1 to -DVER=2 in rsp must miss");

    h.shutdown().await;
}

/// Changing include path in response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_include_path_change_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("inc_a/hdr.h", "#define VAL 1\n");
    h.write_file("inc_b/hdr.h", "#define VAL 2\n");
    h.write_file("src.c", "#include \"hdr.h\"\nint f() { return VAL; }\n");
    h.write_file("flags.rsp", "-Iinc_a");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    h.write_file("flags.rsp", "-Iinc_b");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed -I path in rsp must miss");

    h.shutdown().await;
}

/// Adding a flag to response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_added_flag_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("flags.rsp", "-O2");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    h.write_file("flags.rsp", "-O2 -Wall");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "added -Wall to rsp must miss");

    h.shutdown().await;
}

/// Removing a flag from response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_removed_flag_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("flags.rsp", "-O2 -Wall");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    h.write_file("flags.rsp", "-O2");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "removed -Wall from rsp must miss");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// EQUIVALENCE — same expanded args = cache hit
// ═══════════════════════════════════════════════════════════════════════════════

/// Response file with inline-equivalent args should hit cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_vs_inline_equivalent() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");

    // Compile with inline args
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "-O2"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Now compile with same args in a response file
    h.write_file("flags.rsp", "-O2");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(
        cached,
        "@flags.rsp with -O2 should hit cache from inline -O2"
    );

    h.shutdown().await;
}

/// Extra whitespace in response file should not affect cache key.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_whitespace_irrelevant() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("flags.rsp", "-O2 -Wall");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Rewrite with extra whitespace
    h.write_file("flags.rsp", "  -O2  \n  -Wall  \n");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(
        cached,
        "extra whitespace in rsp should not change cache key"
    );

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// NESTED RESPONSE FILES
// ═══════════════════════════════════════════════════════════════════════════════

/// Changing inner nested response file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn nested_rsp_inner_change_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("inner.rsp", "-O2");
    // outer.rsp references inner.rsp using absolute path
    let inner_abs = h.path("inner.rsp");
    h.write_file("outer.rsp", &format!("-Wall @{}", inner_abs.display()));

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@outer.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Change inner.rsp
    h.write_file("inner.rsp", "-O3");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@outer.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed inner.rsp must invalidate");

    h.shutdown().await;
}

/// 3-level nesting: change deepest file must invalidate cache.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn nested_rsp_deep_chain() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("level3.rsp", "-O2");
    let l3_abs = h.path("level3.rsp");
    h.write_file("level2.rsp", &format!("@{}", l3_abs.display()));
    let l2_abs = h.path("level2.rsp");
    h.write_file("level1.rsp", &format!("-Wall @{}", l2_abs.display()));

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@level1.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Change deepest file
    h.write_file("level3.rsp", "-O0");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@level1.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed level3.rsp must invalidate");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// EDGE CASES
// ═══════════════════════════════════════════════════════════════════════════════

/// Empty response file should still be cacheable.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn empty_rsp_cacheable() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("empty.rsp", "");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@empty.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "first compile must miss");

    // Second compile: hit
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@empty.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(cached, "second compile with empty rsp must hit");

    h.shutdown().await;
}

/// All args (source, output, flags) in response file.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn all_args_in_rsp() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("all.rsp", "-c src.c -o src.o -O2");

    let (exit, cached) = h.compile_with_args(&["@all.rsp"]).await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Second compile: hit
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h.compile_with_args(&["@all.rsp"]).await;
    assert_eq!(exit, 0);
    assert!(cached, "second compile must hit");

    h.shutdown().await;
}

/// Multiple response files: changing one must invalidate.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn multiple_rsp_files() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("opt.rsp", "-O2");
    h.write_file("warn.rsp", "-Wall");

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@opt.rsp", "@warn.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Change only one of the two
    h.write_file("opt.rsp", "-O0");
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@opt.rsp", "@warn.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached, "changed one of two rsp files must miss");

    h.shutdown().await;
}

/// Quoted strings in response file handled correctly.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn rsp_with_quoted_args() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    h.write_file("flags.rsp", r#"-DMSG="hello" -O2"#);

    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Same content: hit
    std::fs::remove_file(h.path("src.o")).unwrap();
    let (exit, cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@flags.rsp"])
        .await;
    assert_eq!(exit, 0);
    assert!(cached, "same quoted rsp must hit");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// ERROR HANDLING
// ═══════════════════════════════════════════════════════════════════════════════

/// Missing response file should fall back to compiler (which handles @file natively).
#[tokio::test]
#[ignore] // integration: spawns clang
async fn missing_rsp_falls_back() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    // @nonexistent.rsp doesn't exist — expansion fails, raw args passed to compiler.
    // Compiler will also fail on @nonexistent.rsp, but the point is we don't panic.
    let (exit, _cached) = h
        .compile_with_args(&["-c", "src.c", "-o", "src.o", "@nonexistent.rsp"])
        .await;
    // Compiler may fail (clang errors on missing @file) — that's fine.
    // The key assertion: we didn't crash/panic.
    eprintln!("missing rsp exit code: {exit} (non-zero is expected)");

    h.shutdown().await;
}

/// Circular response file reference should fall back gracefully.
#[tokio::test]
#[ignore] // integration: spawns clang
async fn circular_rsp_falls_back() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.c", "int f() { return 0; }\n");
    let a_abs = h.path("a.rsp");
    let b_abs = h.path("b.rsp");
    std::fs::write(&a_abs, format!("@{}", b_abs.display())).unwrap();
    std::fs::write(&b_abs, format!("@{}", a_abs.display())).unwrap();

    // Expansion fails due to circular reference → falls back to raw args
    // Compiler will also fail on circular @file, but we don't panic.
    let (exit, _cached) = h
        .compile_with_args(&[
            "-c",
            "src.c",
            "-o",
            "src.o",
            &format!("@{}", a_abs.display()),
        ])
        .await;
    eprintln!("circular rsp exit code: {exit} (non-zero is expected)");

    h.shutdown().await;
}
