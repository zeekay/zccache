//! Adversarial tests for the file watcher integration.
//!
//! These tests verify that the watcher pipeline (NotifyWatcher → SettleBuffer →
//! CacheSystem) correctly detects file changes and invalidates the cache under
//! adversarial conditions: deep directories, rapid edits, multi-session, rename,
//! files outside watched scope, and burst coalescing.
//!
//! Run all:    uv run cargo test -p zccache-daemon --test watcher_adversarial -- --nocapture
//! Run single: uv run cargo test -p zccache-daemon --test watcher_adversarial -- <test_name> --nocapture

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
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

/// Time to let the watcher pipeline propagate:
/// OS event delivery (~10ms) + settle window (50ms) + consumer processing (~1ms).
const WATCHER_SETTLE_MS: u64 = 300;

async fn wait_for_watcher() {
    tokio::time::sleep(Duration::from_millis(WATCHER_SETTLE_MS)).await;
}

/// Test harness with watcher-aware helpers.
struct TestHarness {
    clang: PathBuf,
    tmp: tempfile::TempDir,
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

        // Give the watcher time to start watching the working directory.
        wait_for_watcher().await;

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

    /// Start a second session on the same daemon, with a different working directory.
    async fn second_session(&self, cwd: &str) -> (ClientConn, u64) {
        let mut client2 = zccache_ipc::connect(&self.endpoint).await.unwrap();
        let log = PathBuf::from(cwd).join("log2.txt");
        let sid2 = start_session(&mut client2, &self.clang, cwd, &log.to_string_lossy()).await;
        (client2, sid2)
    }

    async fn shutdown(self) {
        self.shutdown.notify_one();
        self.server_handle.await.unwrap();
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 1: WATCHER PIPELINE VERIFICATION
//
// Does the watcher actually detect changes and downgrade metadata confidence?
// ═══════════════════════════════════════════════════════════════════════════

/// Edit source file, wait for watcher to propagate, recompile → cache miss.
#[tokio::test]
async fn watcher_source_edit_detected() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.cpp", "int f() { return 1; }\n");
    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Confirm cache hit
    let (_, cached, _) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(cached);

    // Edit source and wait for watcher
    h.write_file("src.cpp", "int f() { return 2; }\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(
        !cached,
        "watcher should have invalidated cache after source edit"
    );
    assert_ne!(obj_v1, obj_v2, "different source → different .o");

    h.shutdown().await;
}

/// Edit header file, wait for watcher, recompile → cache miss.
#[tokio::test]
async fn watcher_header_edit_detected() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("val.h", "#define VALUE 1\n");
    h.write_file("src.cpp", "#include \"val.h\"\nint f() { return VALUE; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Edit header, wait for watcher
    h.write_file("val.h", "#define VALUE 99\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(
        !cached,
        "watcher should have invalidated cache after header edit"
    );
    assert_ne!(obj_v1, obj_v2, "header change → different .o");

    h.shutdown().await;
}

/// Touch source file (same content, different mtime), wait for watcher → cache hit.
/// The watcher fires and downgrades confidence, but content hash is unchanged,
/// so the artifact key is the same → cache hit.
#[tokio::test]
async fn watcher_touch_same_content_hits() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "int f() { return 42; }\n";
    h.write_file("src.cpp", content);
    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Touch: rewrite same content after a delay so mtime changes
    std::thread::sleep(Duration::from_millis(1100));
    h.write_file("src.cpp", content);
    wait_for_watcher().await;

    std::fs::remove_file(h.path("src.o")).unwrap();
    let (_, cached, obj_v2) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(
        cached,
        "same content → same hash → cache hit despite mtime change"
    );
    assert_eq!(obj_v1, obj_v2);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 2: WATCH BOUNDARY TESTS
//
// Does the system stay correct for files inside and outside the watched tree?
// ═══════════════════════════════════════════════════════════════════════════

/// Header in a directory outside the watched working dir → stat-verify catches change.
/// The watcher doesn't cover it, but lookup() always stat-verifies (DD-003).
#[tokio::test]
async fn unwatched_dir_header_edit_detected() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Create a separate directory (NOT watched by the daemon)
    let external = tempfile::tempdir().unwrap();
    let ext_hdr = external.path().join("ext.h");
    std::fs::write(&ext_hdr, "#define EXT_VAL 1\n").unwrap();

    let ext_include = external.path().to_string_lossy().into_owned();
    h.write_file(
        "src.cpp",
        "#include \"ext.h\"\nint f() { return EXT_VAL; }\n",
    );

    let obj_path = h.path("src.o");
    let cwd = h.cwd();
    let (exit, cached, obj_v1) = compile_and_read(
        &mut h.client,
        h.session_id,
        &["-c", "src.cpp", "-o", "src.o", "-I", &ext_include],
        &cwd,
        &obj_path,
    )
    .await;
    assert_eq!(exit, 0);
    assert!(!cached);

    // Edit the external header — NOT watched by the watcher
    std::thread::sleep(Duration::from_millis(1100)); // ensure different mtime
    std::fs::write(&ext_hdr, "#define EXT_VAL 99\n").unwrap();

    let (_, cached, obj_v2) = compile_and_read(
        &mut h.client,
        h.session_id,
        &["-c", "src.cpp", "-o", "src.o", "-I", &ext_include],
        &cwd,
        &obj_path,
    )
    .await;
    assert!(
        !cached,
        "stat-verify must catch change even without watcher"
    );
    assert_ne!(obj_v1, obj_v2, "different header → different .o");

    h.shutdown().await;
}

/// File 5 levels deep in nested subdirectory → watcher detects change (recursive mode).
#[tokio::test]
async fn deeply_nested_dir_watched() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("a/b/c/d/e/deep.h", "#define DEEP 1\n");
    h.write_file(
        "src.cpp",
        "#include \"a/b/c/d/e/deep.h\"\nint f() { return DEEP; }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Edit deeply nested header
    h.write_file("a/b/c/d/e/deep.h", "#define DEEP 99\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached, "watcher must detect change 5 levels deep");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Create a new subdirectory AFTER the watcher starts, put a header in it,
/// edit it → watcher should detect the change.
#[tokio::test]
async fn new_subdir_after_watch_detected() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.cpp", "int f() { return 0; }\n");
    let (_, cached, _) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Create new subdir + header AFTER watcher has started
    h.write_file("newdir/new.h", "#define NEW_VAL 1\n");
    h.write_file(
        "src.cpp",
        "#include \"newdir/new.h\"\nint f() { return NEW_VAL; }\n",
    );
    let (_, cached, obj_v1) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached, "source changed → miss");

    // Edit the header in the new subdirectory
    h.write_file("newdir/new.h", "#define NEW_VAL 99\n");
    wait_for_watcher().await;

    let (_, cached, obj_v2) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached, "watcher must detect edit in newly-created subdir");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 3: TIMING / RACE CONDITIONS
//
// Rapid file changes, settle buffer coalescing, rename tracking.
// ═══════════════════════════════════════════════════════════════════════════

/// Create and immediately delete a file within the settle window → no crash,
/// daemon remains functional.
#[tokio::test]
async fn rapid_create_delete_no_crash() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("src.cpp", "int f() { return 1; }\n");
    let (_, cached, _) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(!cached);

    // Ephemeral file: create then immediately delete
    let ephemeral = h.path("ephemeral.h");
    std::fs::write(&ephemeral, "#define TEMP 1\n").unwrap();
    std::fs::remove_file(&ephemeral).unwrap();
    wait_for_watcher().await;

    // Daemon should still function normally
    let (_, cached, _) = h.compile_file_read("src.cpp", "src.o").await;
    assert!(
        cached,
        "unrelated ephemeral file should not affect src.cpp cache"
    );

    h.shutdown().await;
}

/// Edit 100 files rapidly (within one settle window), then recompile all.
/// The settle buffer should coalesce the edits. All must miss.
#[tokio::test]
async fn burst_100_file_edits_coalesced() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let n = 100;

    // Initial compile of all files
    for i in 0..n {
        h.write_file(
            &format!("f{i}.cpp"),
            &format!("int f{i}() {{ return {i}; }}\n"),
        );
        let (exit, cached, _) = h
            .compile_file_read(&format!("f{i}.cpp"), &format!("f{i}.o"))
            .await;
        assert_eq!(exit, 0);
        assert!(!cached);
    }

    // Verify hits
    for i in 0..n {
        let (_, cached, _) = h
            .compile_file_read(&format!("f{i}.cpp"), &format!("f{i}.o"))
            .await;
        assert!(cached, "f{i} should hit before edit burst");
    }

    // Burst-edit ALL files as fast as possible
    for i in 0..n {
        h.write_file(
            &format!("f{i}.cpp"),
            &format!("int f{i}() {{ return {val}; }}\n", val = i + 1000),
        );
    }
    wait_for_watcher().await;

    // All must miss
    let mut miss_count = 0;
    for i in 0..n {
        let (exit, cached, _) = h
            .compile_file_read(&format!("f{i}.cpp"), &format!("f{i}.o"))
            .await;
        assert_eq!(exit, 0);
        if !cached {
            miss_count += 1;
        }
    }
    assert_eq!(miss_count, n, "all {n} files must miss after burst edit");

    h.shutdown().await;
}

/// Rename a source file → old path is gone, new path compiles fresh.
#[tokio::test]
async fn file_renamed_tracked() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("old.cpp", "int f() { return 1; }\n");
    let (_, cached, _) = h.compile_file_read("old.cpp", "old.o").await;
    assert!(!cached);

    // Rename the file
    std::fs::rename(h.path("old.cpp"), h.path("new.cpp")).unwrap();
    wait_for_watcher().await;

    // New path: fresh compile (different path = different cache entry)
    let (exit, cached, _) = h.compile_file_read("new.cpp", "new.o").await;
    assert_eq!(exit, 0);
    assert!(!cached, "new path should be a cache miss");

    // Old path: should fail (file doesn't exist)
    let cwd = h.cwd();
    let (exit, _) = compile(
        &mut h.client,
        h.session_id,
        &["-c", "old.cpp", "-o", "old2.o"],
        &cwd,
    )
    .await;
    assert_ne!(exit, 0, "old path should fail to compile");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// GROUP 4: MULTI-SESSION & CONCURRENCY
//
// Multiple sessions, shared/independent watching, concurrent edits.
// ═══════════════════════════════════════════════════════════════════════════

/// Two sessions watching the same directory. Edit a header → both sessions see
/// the invalidation. Second session gets a cache hit from first session's recompile.
#[tokio::test]
async fn two_sessions_same_dir_share_watcher() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let cwd = h.cwd();
    let (mut client2, sid2) = h.second_session(&cwd).await;
    wait_for_watcher().await;

    h.write_file("config.h", "#define CFG 1\n");
    h.write_file(
        "shared.cpp",
        "#include \"config.h\"\nint f() { return CFG; }\n",
    );

    // Session A compiles (miss)
    let (_, cached, _) = h.compile_file_read("shared.cpp", "shared.o").await;
    assert!(!cached);

    // Session B compiles same file (hit — cross-session cache sharing)
    let obj_path = h.path("shared_b.o");
    let (_, cached, _) = compile_and_read(
        &mut client2,
        sid2,
        &["-c", "shared.cpp", "-o", "shared_b.o"],
        &cwd,
        &obj_path,
    )
    .await;
    assert!(cached, "session B should hit from session A's cache");

    // Edit header
    h.write_file("config.h", "#define CFG 99\n");
    wait_for_watcher().await;

    // Session A recompiles (miss — header changed)
    let (_, cached, _) = h.compile_file_read("shared.cpp", "shared.o").await;
    assert!(!cached, "session A must miss after header edit");

    // Session B recompiles (hit — session A just populated the new artifact)
    let (_, cached, _) = compile_and_read(
        &mut client2,
        sid2,
        &["-c", "shared.cpp", "-o", "shared_b.o"],
        &cwd,
        &obj_path,
    )
    .await;
    assert!(cached, "session B should hit from session A's recompile");

    h.shutdown().await;
}

/// Two sessions with different working directories. Editing file A in dir A
/// should NOT affect session B's cache for file B in dir B.
#[tokio::test]
async fn two_sessions_different_dirs_independent() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Create a second working directory
    let dir_b = tempfile::tempdir().unwrap();
    let cwd_b = dir_b.path().to_string_lossy().into_owned();
    let (mut client2, sid2) = h.second_session(&cwd_b).await;
    wait_for_watcher().await;

    // Create source files in each dir
    h.write_file("a.cpp", "int fa() { return 1; }\n");
    std::fs::write(dir_b.path().join("b.cpp"), "int fb() { return 2; }\n").unwrap();

    // Compile both
    let (_, cached_a, _) = h.compile_file_read("a.cpp", "a.o").await;
    assert!(!cached_a);

    let obj_b = dir_b.path().join("b.o");
    let (_, cached_b, _) = compile_and_read(
        &mut client2,
        sid2,
        &["-c", "b.cpp", "-o", "b.o"],
        &cwd_b,
        &obj_b,
    )
    .await;
    assert!(!cached_b);

    // Edit file A
    h.write_file("a.cpp", "int fa() { return 999; }\n");
    wait_for_watcher().await;

    // Session A: miss (file A changed)
    let (_, cached_a, _) = h.compile_file_read("a.cpp", "a.o").await;
    assert!(!cached_a, "session A must miss after edit");

    // Session B: hit (file B unchanged)
    let (_, cached_b, _) = compile_and_read(
        &mut client2,
        sid2,
        &["-c", "b.cpp", "-o", "b.o"],
        &cwd_b,
        &obj_b,
    )
    .await;
    assert!(
        cached_b,
        "session B must still hit — its files are untouched"
    );

    h.shutdown().await;
}

/// Edit file B while compiling file A. After A finishes, B should be invalidated.
#[tokio::test]
async fn concurrent_edit_during_compilation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("slow.cpp", "int f() { return 1; }\n");
    h.write_file("fast.cpp", "int g() { return 2; }\n");

    // Compile both (miss)
    let (_, cached, _) = h.compile_file_read("slow.cpp", "slow.o").await;
    assert!(!cached);
    let (_, cached, _) = h.compile_file_read("fast.cpp", "fast.o").await;
    assert!(!cached);

    // Verify both hit
    let (_, cached, _) = h.compile_file_read("slow.cpp", "slow.o").await;
    assert!(cached);
    let (_, cached, _) = h.compile_file_read("fast.cpp", "fast.o").await;
    assert!(cached);

    // Edit fast.cpp, then immediately compile slow.cpp (which is still cached).
    // After slow.cpp compile returns, fast.cpp should be invalidated.
    h.write_file("fast.cpp", "int g() { return 999; }\n");

    // Compile slow.cpp — should still hit (fast.cpp's change is irrelevant)
    let (_, cached, _) = h.compile_file_read("slow.cpp", "slow.o").await;
    assert!(cached, "slow.cpp is unchanged → hit");

    wait_for_watcher().await;

    // Now compile fast.cpp — must miss (it was edited)
    let (_, cached, _) = h.compile_file_read("fast.cpp", "fast.o").await;
    assert!(!cached, "fast.cpp was edited → must miss");

    h.shutdown().await;
}

/// Touch 200+ files at once → watcher coalesces, daemon stays correct.
/// All original files should be revalidated after the storm.
#[tokio::test]
async fn watcher_handles_mass_file_storm() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Create and compile 5 real source files
    let real_count = 5;
    let mut original_objs: Vec<Vec<u8>> = Vec::new();
    for i in 0..real_count {
        h.write_file(
            &format!("real_{i}.cpp"),
            &format!("int r{i}() {{ return {i}; }}\n"),
        );
        let (exit, cached, obj) = h
            .compile_file_read(&format!("real_{i}.cpp"), &format!("real_{i}.o"))
            .await;
        assert_eq!(exit, 0);
        assert!(!cached);
        original_objs.push(obj);
    }

    // Verify all hit
    for i in 0..real_count {
        let (_, cached, _) = h
            .compile_file_read(&format!("real_{i}.cpp"), &format!("real_{i}.o"))
            .await;
        assert!(cached);
    }

    // Storm: create 200 noise files in the watched directory
    for i in 0..200 {
        h.write_file(&format!("noise_{i}.h"), &format!("// noise file {i}\n"));
    }
    wait_for_watcher().await;

    // Recompile all real files — they should still hit (only noise files changed,
    // real source files are untouched)
    for (i, expected_obj) in original_objs.iter().enumerate() {
        let (exit, cached, obj) = h
            .compile_file_read(&format!("real_{i}.cpp"), &format!("real_{i}.o"))
            .await;
        assert_eq!(exit, 0);
        assert!(
            cached,
            "real_{i}.cpp should still hit — only noise files were created"
        );
        assert_eq!(expected_obj, &obj);
    }

    // Now edit one real file amidst noise
    h.write_file("real_0.cpp", "int r0() { return 9999; }\n");
    wait_for_watcher().await;

    // Only real_0 should miss
    let (_, cached, obj_new) = h.compile_file_read("real_0.cpp", "real_0.o").await;
    assert!(!cached, "edited real_0 must miss");
    assert_ne!(original_objs[0], obj_new);

    // Others still hit
    for i in 1..real_count {
        let (_, cached, _) = h
            .compile_file_read(&format!("real_{i}.cpp"), &format!("real_{i}.o"))
            .await;
        assert!(cached, "untouched real_{i} should still hit");
    }

    h.shutdown().await;
}
