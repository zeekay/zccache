//! Adversarial corner-case tests for cache correctness.
//!
//! These tests target subtle scenarios NOT covered by other test suites:
//! - Content revert cycles (A→B→A should hit original cache)
//! - Diamond dependency invalidation (shared transitive header)
//! - Same content, different filenames (filename in cache key)
//! - Failed compile not cached (header appears after failure)
//! - Cache persistence across session boundaries
//! - Deep transitive include chains (5+ levels)
//! - Thundering herd (concurrent same-file compilation from multiple sessions)
//!
//! Run all:    uv run cargo test -p zccache-daemon --test adversarial_corner_cases -- --nocapture
//! Run single: uv run cargo test -p zccache-daemon --test adversarial_corner_cases -- <test_name> --nocapture

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

async fn start_daemon() -> (String, JoinHandle<()>, Arc<Notify>) {
    let endpoint = zccache_ipc::unique_test_endpoint();
    let mut server = DaemonServer::bind(&endpoint).unwrap();
    let shutdown = server.shutdown_handle();
    let handle = tokio::spawn(async move {
        server.run(0).await.unwrap();
    });
    (endpoint, handle, shutdown)
}

async fn start_session(client: &mut ClientConn, clang: &Path, cwd: &str, log_file: &str) -> u64 {
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

async fn compile_and_read(
    client: &mut ClientConn,
    session_id: u64,
    args: &[&str],
    cwd: &str,
    obj_path: &Path,
) -> (i32, bool, Vec<u8>) {
    let (exit_code, cached) = compile(client, session_id, args, cwd).await;
    let obj_data = if obj_path.exists() {
        std::fs::read(obj_path).unwrap()
    } else {
        vec![]
    };
    (exit_code, cached, obj_data)
}

struct TestHarness {
    #[expect(dead_code)]
    clang: PathBuf,
    tmp: tempfile::TempDir,
    #[expect(dead_code)]
    endpoint: String,
    server_handle: JoinHandle<()>,
    shutdown: Arc<Notify>,
    client: ClientConn,
    session_id: u64,
}

impl TestHarness {
    async fn new() -> Option<Self> {
        let clang = find_clang()?;
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

    async fn compile_file_read(&mut self, src: &str, obj: &str) -> (i32, bool, Vec<u8>) {
        let obj_path = self.path(obj);
        let cwd = self.cwd();
        compile_and_read(
            &mut self.client,
            self.session_id,
            &["-c", src, "-o", obj],
            &cwd,
            &obj_path,
        )
        .await
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CONTENT REVERT CYCLE
//
// Edit source A→B→A. The cache should retain the original entry and return
// a hit when content reverts to its original state.
// ═══════════════════════════════════════════════════════════════════════════

/// Content revert: A→B (miss) → A (should hit original cache).
/// This tests that the cache retains old artifact entries even after new
/// content overwrites the same filename.
#[tokio::test]
async fn corner_content_revert_hits_original_cache() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let original = "int f() { return 1; }\n";
    let modified = "int f() { return 2; }\n";

    // Compile original (miss)
    h.write_file("revert.cpp", original);
    let (_, cached, obj_original) = h.compile_file_read("revert.cpp", "revert.o").await;
    assert!(!cached, "first compile must miss");

    // Edit to different content (miss)
    h.write_file("revert.cpp", modified);
    let (_, cached, obj_modified) = h.compile_file_read("revert.cpp", "revert.o").await;
    assert!(!cached, "modified content must miss");
    assert_ne!(
        obj_original, obj_modified,
        "different content → different .o"
    );

    // Revert to original content (should hit!)
    h.write_file("revert.cpp", original);
    std::fs::remove_file(h.path("revert.o")).unwrap();
    let (_, cached, obj_reverted) = h.compile_file_read("revert.cpp", "revert.o").await;
    assert!(
        cached,
        "reverting to original content should hit the original cache entry"
    );
    assert_eq!(
        obj_original, obj_reverted,
        "reverted .o must match original .o byte-for-byte"
    );

    h.shutdown().await;
}

/// Extended revert cycle: A→B→C→A. Same principle, but with an intermediate state.
#[tokio::test]
async fn corner_content_revert_three_way_cycle() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let v1 = "int f() { return 10; }\n";
    let v2 = "int f() { return 20; }\n";
    let v3 = "int f() { return 30; }\n";

    // v1 → miss
    h.write_file("cycle.cpp", v1);
    let (_, cached, obj_v1) = h.compile_file_read("cycle.cpp", "cycle.o").await;
    assert!(!cached);

    // v2 → miss
    h.write_file("cycle.cpp", v2);
    let (_, cached, obj_v2) = h.compile_file_read("cycle.cpp", "cycle.o").await;
    assert!(!cached);
    assert_ne!(obj_v1, obj_v2);

    // v3 → miss
    h.write_file("cycle.cpp", v3);
    let (_, cached, obj_v3) = h.compile_file_read("cycle.cpp", "cycle.o").await;
    assert!(!cached);
    assert_ne!(obj_v2, obj_v3);

    // Back to v1 → should hit
    h.write_file("cycle.cpp", v1);
    std::fs::remove_file(h.path("cycle.o")).unwrap();
    let (_, cached, obj_v1_again) = h.compile_file_read("cycle.cpp", "cycle.o").await;
    assert!(cached, "revert to v1 should hit original cache");
    assert_eq!(obj_v1, obj_v1_again);

    // Back to v2 → should also hit
    h.write_file("cycle.cpp", v2);
    std::fs::remove_file(h.path("cycle.o")).unwrap();
    let (_, cached, obj_v2_again) = h.compile_file_read("cycle.cpp", "cycle.o").await;
    assert!(cached, "revert to v2 should hit its cache entry");
    assert_eq!(obj_v2, obj_v2_again);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// DIAMOND DEPENDENCY INVALIDATION
//
// A.cpp includes B.h and C.h. Both B.h and C.h include D.h.
// Editing D.h must invalidate A.cpp because D.h is a transitive dependency
// reachable through both paths.
// ═══════════════════════════════════════════════════════════════════════════

/// Diamond: A.cpp → {B.h, C.h} → D.h. Edit D.h → must invalidate.
#[tokio::test]
async fn corner_diamond_dependency_invalidation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("d.h", "#pragma once\n#define DIAMOND_VAL 1\n");
    h.write_file(
        "b.h",
        "#pragma once\n#include \"d.h\"\ninline int from_b() { return DIAMOND_VAL; }\n",
    );
    h.write_file(
        "c.h",
        "#pragma once\n#include \"d.h\"\ninline int from_c() { return DIAMOND_VAL + 10; }\n",
    );
    h.write_file(
        "diamond.cpp",
        "#include \"b.h\"\n#include \"c.h\"\nint f() { return from_b() + from_c(); }\n",
    );

    let (exit, cached, obj_v1) = h.compile_file_read("diamond.cpp", "diamond.o").await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Edit the shared leaf header D.h — source files unchanged
    h.write_file("d.h", "#pragma once\n#define DIAMOND_VAL 99\n");

    let (exit, cached, obj_v2) = h.compile_file_read("diamond.cpp", "diamond.o").await;
    assert_eq!(exit, 0);
    assert!(
        !cached,
        "diamond dependency: editing D.h must invalidate A.cpp"
    );
    assert_ne!(obj_v1, obj_v2, "different DIAMOND_VAL → different .o");

    // Verify cache hit on second compile with same state
    std::fs::remove_file(h.path("diamond.o")).unwrap();
    let (_, cached, obj_v2b) = h.compile_file_read("diamond.cpp", "diamond.o").await;
    assert!(cached, "no changes → should hit cache");
    assert_eq!(obj_v2, obj_v2b);

    h.shutdown().await;
}

/// Diamond with independent branch edit: edit only B.h (not D.h).
/// A.cpp must still miss because B.h changed.
#[tokio::test]
async fn corner_diamond_branch_edit() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("shared.h", "#pragma once\n#define SHARED 1\n");
    h.write_file(
        "left.h",
        "#pragma once\n#include \"shared.h\"\ninline int left() { return SHARED; }\n",
    );
    h.write_file(
        "right.h",
        "#pragma once\n#include \"shared.h\"\ninline int right() { return SHARED + 5; }\n",
    );
    h.write_file(
        "dia2.cpp",
        "#include \"left.h\"\n#include \"right.h\"\nint f() { return left() + right(); }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("dia2.cpp", "dia2.o").await;
    assert!(!cached);

    // Edit only left.h — right.h and shared.h unchanged
    h.write_file(
        "left.h",
        "#pragma once\n#include \"shared.h\"\ninline int left() { return SHARED + 100; }\n",
    );

    let (_, cached, obj_v2) = h.compile_file_read("dia2.cpp", "dia2.o").await;
    assert!(!cached, "editing one diamond branch must invalidate");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// SAME CONTENT, DIFFERENT FILENAMES
//
// Two source files with identical content must produce separate cache entries.
// The filename/path is part of the cache key (__FILE__ macro, debug info, etc.).
// ═══════════════════════════════════════════════════════════════════════════

/// Two files with identical content → separate cache entries, both miss initially.
/// Editing one must NOT affect the other.
#[tokio::test]
async fn corner_same_content_different_filenames() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "int f() { return 42; }\n";

    h.write_file("alpha.cpp", content);
    h.write_file("beta.cpp", content);

    // Both should miss (different filenames = different cache keys)
    let (_, cached_a, _obj_a) = h.compile_file_read("alpha.cpp", "alpha.o").await;
    assert!(!cached_a, "alpha.cpp first compile must miss");

    let (_, cached_b, _obj_b) = h.compile_file_read("beta.cpp", "beta.o").await;
    assert!(
        !cached_b,
        "beta.cpp first compile must miss (different cache key)"
    );

    // Both should hit on recompile
    std::fs::remove_file(h.path("alpha.o")).unwrap();
    let (_, cached_a, _) = h.compile_file_read("alpha.cpp", "alpha.o").await;
    assert!(cached_a, "alpha.cpp recompile should hit");

    std::fs::remove_file(h.path("beta.o")).unwrap();
    let (_, cached_b, _) = h.compile_file_read("beta.cpp", "beta.o").await;
    assert!(cached_b, "beta.cpp recompile should hit");

    // Edit alpha only → beta should still hit
    h.write_file("alpha.cpp", "int f() { return 999; }\n");
    let (_, cached_a, _) = h.compile_file_read("alpha.cpp", "alpha.o").await;
    assert!(!cached_a, "edited alpha must miss");

    std::fs::remove_file(h.path("beta.o")).unwrap();
    let (_, cached_b, _) = h.compile_file_read("beta.cpp", "beta.o").await;
    assert!(cached_b, "untouched beta must still hit");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// FAILED COMPILE NOT CACHED
//
// If a compile fails (e.g., missing header), the failure must NOT be cached.
// When the header is later created, the next compile must succeed.
// ═══════════════════════════════════════════════════════════════════════════

/// Missing header → compile fails. Create header → compile succeeds.
/// The failure must not be served from cache.
#[tokio::test]
async fn corner_failed_compile_not_cached_then_header_appears() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Source references a header that doesn't exist yet
    h.write_file(
        "needs_hdr.cpp",
        "#include \"late.h\"\nint f() { return LATE_VAL; }\n",
    );

    // First compile: fails (missing header)
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_ne!(exit_code, 0, "compile with missing header should fail");
    assert!(!cached, "failed compile must not be cached");

    // Now create the missing header
    h.write_file("late.h", "#define LATE_VAL 42\n");

    // Second compile: must succeed (not serve cached failure)
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_eq!(
        exit_code, 0,
        "compile should succeed after header is created"
    );
    assert!(
        !cached,
        "must be a fresh compile, not a cached result from the failed attempt"
    );

    // Third compile: should hit (nothing changed)
    std::fs::remove_file(h.path("needs_hdr.o")).unwrap();
    let (exit_code, cached, _) = h.compile_file_read("needs_hdr.cpp", "needs_hdr.o").await;
    assert_eq!(exit_code, 0);
    assert!(cached, "third compile with no changes should hit cache");

    h.shutdown().await;
}

/// Compile error from syntax error → fix source → must succeed.
/// Verifies compile errors are never cached regardless of cause.
#[tokio::test]
async fn corner_syntax_error_not_cached() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Broken source
    h.write_file("broken.cpp", "int f() { return }\n");
    let (exit_code, cached, _) = h.compile_file_read("broken.cpp", "broken.o").await;
    assert_ne!(exit_code, 0, "syntax error should fail");
    assert!(!cached, "failed compile must not be cached");

    // Fix source
    h.write_file("broken.cpp", "int f() { return 0; }\n");
    let (exit_code, cached, _) = h.compile_file_read("broken.cpp", "broken.o").await;
    assert_eq!(exit_code, 0, "fixed source should compile");
    assert!(!cached, "must be fresh compile, not cached failure");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// SESSION BOUNDARY PERSISTENCE
//
// Cache must survive session end/restart. Artifacts are stored on disk and
// should be available to new sessions.
// ═══════════════════════════════════════════════════════════════════════════

/// Compile in session A, end session A, start new session B on same daemon,
/// recompile → should hit cache.
#[tokio::test]
async fn corner_cache_survives_session_restart() {
    let clang = match find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let log1 = tmp.path().join("log1.txt");
    let log2 = tmp.path().join("log2.txt");

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Session A: compile
    let mut client1 = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid1 = start_session(&mut client1, &clang, &cwd, &log1.to_string_lossy()).await;

    let src = tmp.path().join("persist.cpp");
    std::fs::write(&src, "int f() { return 7; }\n").unwrap();

    let obj = tmp.path().join("persist.o");
    let (exit, cached, obj_v1) = compile_and_read(
        &mut client1,
        sid1,
        &["-c", "persist.cpp", "-o", "persist.o"],
        &cwd,
        &obj,
    )
    .await;
    assert_eq!(exit, 0);
    assert!(!cached, "session A first compile must miss");

    // End session A
    client1
        .send(&Request::SessionEnd { session_id: sid1 })
        .await
        .unwrap();
    let _: Option<Response> = client1.recv().await.unwrap();

    // Session B: new client, new session, same daemon
    let mut client2 = zccache_ipc::connect(&endpoint).await.unwrap();
    let sid2 = start_session(&mut client2, &clang, &cwd, &log2.to_string_lossy()).await;

    // Recompile same file in session B — should hit
    std::fs::remove_file(&obj).unwrap();
    let (exit, cached, obj_v2) = compile_and_read(
        &mut client2,
        sid2,
        &["-c", "persist.cpp", "-o", "persist.o"],
        &cwd,
        &obj,
    )
    .await;
    assert_eq!(exit, 0);
    assert!(
        cached,
        "session B should hit cache from session A's artifact"
    );
    assert_eq!(obj_v1, obj_v2, "same content → same .o across sessions");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// DEEP TRANSITIVE INCLUDE CHAIN
//
// A.cpp → h1.h → h2.h → h3.h → h4.h → h5.h
// Edit h5.h (the deepest leaf) → A.cpp must be invalidated.
// ═══════════════════════════════════════════════════════════════════════════

/// 5-level deep include chain. Edit the deepest header → must invalidate.
#[tokio::test]
async fn corner_deep_transitive_chain_5_levels() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("h5.h", "#pragma once\n#define LEAF_VAL 1\n");
    h.write_file("h4.h", "#pragma once\n#include \"h5.h\"\n");
    h.write_file("h3.h", "#pragma once\n#include \"h4.h\"\n");
    h.write_file("h2.h", "#pragma once\n#include \"h3.h\"\n");
    h.write_file("h1.h", "#pragma once\n#include \"h2.h\"\n");
    h.write_file(
        "deep_chain.cpp",
        "#include \"h1.h\"\nint f() { return LEAF_VAL; }\n",
    );

    let (exit, cached, obj_v1) = h.compile_file_read("deep_chain.cpp", "deep_chain.o").await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Verify cache hit
    std::fs::remove_file(h.path("deep_chain.o")).unwrap();
    let (_, cached, _) = h.compile_file_read("deep_chain.cpp", "deep_chain.o").await;
    assert!(cached, "no changes → hit");

    // Edit the DEEPEST header (5 levels down)
    h.write_file("h5.h", "#pragma once\n#define LEAF_VAL 999\n");

    let (exit, cached, obj_v2) = h.compile_file_read("deep_chain.cpp", "deep_chain.o").await;
    assert_eq!(exit, 0);
    assert!(
        !cached,
        "editing h5.h (5 levels deep) must invalidate deep_chain.cpp"
    );
    assert_ne!(obj_v1, obj_v2, "different LEAF_VAL → different .o");

    h.shutdown().await;
}

/// 5-level chain: edit a MIDDLE header (h3.h). Must invalidate.
#[tokio::test]
async fn corner_deep_chain_middle_edit() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("m5.h", "#pragma once\n#define M5 1\n");
    h.write_file("m4.h", "#pragma once\n#include \"m5.h\"\n#define M4 2\n");
    h.write_file("m3.h", "#pragma once\n#include \"m4.h\"\n#define M3 3\n");
    h.write_file("m2.h", "#pragma once\n#include \"m3.h\"\n");
    h.write_file("m1.h", "#pragma once\n#include \"m2.h\"\n");
    h.write_file(
        "mid_chain.cpp",
        "#include \"m1.h\"\nint f() { return M3 + M4 + M5; }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("mid_chain.cpp", "mid_chain.o").await;
    assert!(!cached);

    // Edit middle header h3.h — change the M3 value
    h.write_file("m3.h", "#pragma once\n#include \"m4.h\"\n#define M3 333\n");

    let (_, cached, obj_v2) = h.compile_file_read("mid_chain.cpp", "mid_chain.o").await;
    assert!(!cached, "editing middle header m3.h must invalidate");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// THUNDERING HERD
//
// Multiple sessions compile the exact same file at the same time.
// All must succeed and produce identical, non-corrupted artifacts.
// ═══════════════════════════════════════════════════════════════════════════

/// 4 sessions simultaneously compile the same source file.
/// At most 1 should be a miss; the rest should either hit or also miss
/// (depending on timing), but ALL must produce identical, valid .o files.
#[tokio::test]
async fn corner_thundering_herd_same_file() {
    let clang = match find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();

    let src = tmp.path().join("herd.cpp");
    std::fs::write(&src, "int f() { return 42; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    let n_sessions = 4;
    let mut handles = Vec::new();

    for i in 0..n_sessions {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let obj_dir = tmp.path().join(format!("out_{i}"));
        std::fs::create_dir_all(&obj_dir).unwrap();
        let log = tmp.path().join(format!("log_{i}.txt"));

        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            let obj_name = format!("out_{i}/herd.o");
            let obj_path = PathBuf::from(&cwd).join(&obj_name);
            let (exit, cached, obj) = compile_and_read(
                &mut client,
                sid,
                &["-c", "herd.cpp", "-o", &obj_name],
                &cwd,
                &obj_path,
            )
            .await;

            (exit, cached, obj)
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // All must succeed
    for (i, (exit, _, _)) in results.iter().enumerate() {
        assert_eq!(*exit, 0, "session {i} must compile successfully");
    }

    // All must produce identical .o bytes (whether cached or not)
    let reference_obj = &results[0].2;
    assert!(!reference_obj.is_empty(), "reference .o must not be empty");
    for (i, (_, _, obj)) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference_obj, obj,
            "session {i} must produce identical .o to session 0"
        );
    }

    // At least one should have been a cache miss (the first to compile)
    let miss_count = results.iter().filter(|(_, cached, _)| !cached).count();
    let hit_count = results.iter().filter(|(_, cached, _)| *cached).count();
    eprintln!("thundering herd: {miss_count} misses, {hit_count} hits out of {n_sessions}");
    assert!(miss_count >= 1, "at least one session must be a cache miss");

    shutdown.notify_one();
    server_handle.await.unwrap();
}

/// Thundering herd after cache is warm: all sessions should hit.
#[tokio::test]
async fn corner_thundering_herd_all_warm() {
    let clang = match find_clang() {
        Some(c) => c,
        None => return,
    };

    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().to_string_lossy().into_owned();
    let src = tmp.path().join("warm.cpp");
    std::fs::write(&src, "int f() { return 7; }\n").unwrap();

    let (endpoint, server_handle, shutdown) = start_daemon().await;

    // Warm the cache with a single compile
    {
        let log = tmp.path().join("warm_log.txt");
        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;
        let obj_path = tmp.path().join("warm.o");
        let (exit, cached, _) = compile_and_read(
            &mut client,
            sid,
            &["-c", "warm.cpp", "-o", "warm.o"],
            &cwd,
            &obj_path,
        )
        .await;
        assert_eq!(exit, 0);
        assert!(!cached, "warming compile must miss");
    }

    // Now 4 sessions all compile at once — all should hit
    let n_sessions = 4;
    let mut handles = Vec::new();

    for i in 0..n_sessions {
        let ep = endpoint.clone();
        let clang = clang.clone();
        let cwd = cwd.clone();
        let obj_dir = tmp.path().join(format!("warm_out_{i}"));
        std::fs::create_dir_all(&obj_dir).unwrap();
        let log = tmp.path().join(format!("warm_log_{i}.txt"));

        handles.push(tokio::spawn(async move {
            let mut client = zccache_ipc::connect(&ep).await.unwrap();
            let sid = start_session(&mut client, &clang, &cwd, &log.to_string_lossy()).await;

            let obj_name = format!("warm_out_{i}/warm.o");
            let obj_path = PathBuf::from(&cwd).join(&obj_name);
            let (exit, cached, obj) = compile_and_read(
                &mut client,
                sid,
                &["-c", "warm.cpp", "-o", &obj_name],
                &cwd,
                &obj_path,
            )
            .await;

            (exit, cached, obj)
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    // All must succeed and hit cache
    for (i, (exit, cached, _)) in results.iter().enumerate() {
        assert_eq!(*exit, 0, "session {i} must succeed");
        assert!(*cached, "session {i} must hit cache (cache was warmed)");
    }

    shutdown.notify_one();
    server_handle.await.unwrap();
}
