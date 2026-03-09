//! Adversarial mutation tests for cache invalidation correctness.
//!
//! These tests systematically mutate source files, headers, and include graphs
//! to verify the cache never returns stale results. Every test that touches
//! file content compiles the output and compares the actual object bytes.
//!
//! Run all:    uv run cargo test -p zccache-daemon --test adversarial_mutations -- --nocapture
//! Run single: uv run cargo test -p zccache-daemon --test adversarial_mutations -- <test_name> --nocapture

use std::path::{Path, PathBuf};
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

/// Compile and return (exit_code, cached, object_file_bytes).
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

/// Convenience: set up daemon + session + temp dir.
struct TestHarness {
    #[expect(dead_code)]
    clang: PathBuf,
    tmp: tempfile::TempDir,
    #[expect(dead_code)]
    endpoint: String,
    server_handle: tokio::task::JoinHandle<()>,
    shutdown: std::sync::Arc<tokio::sync::Notify>,
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

    fn log_text(&self) -> String {
        std::fs::read_to_string(self.path("log.txt")).unwrap_or_default()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// SOURCE FILE MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Edit source content → cache must miss, different .o produced.
#[tokio::test]
async fn mutation_edit_source_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("a.cpp", "int f() { return 1; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("a.cpp", "a.o").await;
    assert!(!cached, "first compile should miss");

    // Edit: change return value
    h.write_file("a.cpp", "int f() { return 2; }\n");

    let (_, cached, obj_v2) = h.compile_file_read("a.cpp", "a.o").await;
    assert!(!cached, "edited source must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "different source must produce different .o");

    h.shutdown().await;
}

/// Touch source (change mtime, same content) → cache should still HIT
/// because content hash is unchanged after rehash.
#[tokio::test]
async fn mutation_touch_source_no_invalidation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("touch.cpp", "int f() { return 42; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("touch.cpp", "touch.o").await;
    assert!(!cached);

    // Touch: rewrite identical content (changes mtime)
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure mtime differs
    h.write_file("touch.cpp", "int f() { return 42; }\n");

    std::fs::remove_file(h.path("touch.o")).unwrap();
    let (_, cached, obj_v2) = h.compile_file_read("touch.cpp", "touch.o").await;
    assert!(
        cached,
        "touch with same content should still hit cache (content hash unchanged)"
    );
    assert_eq!(obj_v1, obj_v2, "same content → same .o");

    h.shutdown().await;
}

/// Whitespace-only edit → cache must miss (content hash changes).
#[tokio::test]
async fn mutation_whitespace_edit_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("ws.cpp", "int f() { return 0; }\n");
    let (_, cached, _obj_v1) = h.compile_file_read("ws.cpp", "ws.o").await;
    assert!(!cached);

    // Add trailing whitespace — content hash changes even though compilation result is same
    h.write_file("ws.cpp", "int f() { return 0; }  \n");
    let (_, cached, _obj_v2) = h.compile_file_read("ws.cpp", "ws.o").await;
    assert!(
        !cached,
        "whitespace edit changes content hash → must be a cache miss"
    );
    // Note: .o files may or may not differ (compiler might produce identical output)
    // The important thing is the cache correctly detected the source change.

    h.shutdown().await;
}

/// Comment-only edit → cache must miss (content hash changes).
#[tokio::test]
async fn mutation_comment_edit_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("cmt.cpp", "int f() { return 0; }\n");
    let (_, cached, _) = h.compile_file_read("cmt.cpp", "cmt.o").await;
    assert!(!cached);

    h.write_file("cmt.cpp", "// added comment\nint f() { return 0; }\n");
    let (_, cached, _) = h.compile_file_read("cmt.cpp", "cmt.o").await;
    assert!(!cached, "comment edit changes content hash → cache miss");

    h.shutdown().await;
}

/// Append code at end of file → cache miss.
#[tokio::test]
async fn mutation_append_code_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("append.cpp", "int f() { return 0; }\n");
    let (_, cached, obj_v1) = h.compile_file_read("append.cpp", "append.o").await;
    assert!(!cached);

    h.write_file(
        "append.cpp",
        "int f() { return 0; }\nint g() { return 1; }\n",
    );
    let (_, cached, obj_v2) = h.compile_file_read("append.cpp", "append.o").await;
    assert!(!cached, "appending code must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "more code → different .o");

    h.shutdown().await;
}

/// Truncate source to empty → cache miss, still compiles (empty TU is valid).
#[tokio::test]
async fn mutation_truncate_to_empty_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("trunc.cpp", "int f() { return 0; }\n");
    let (_, cached, _) = h.compile_file_read("trunc.cpp", "trunc.o").await;
    assert!(!cached);

    h.write_file("trunc.cpp", "");
    let (exit_code, cached, _) = h.compile_file_read("trunc.cpp", "trunc.o").await;
    assert_eq!(exit_code, 0, "empty TU is valid C++");
    assert!(!cached, "truncation must invalidate cache");

    h.shutdown().await;
}

/// Replace file atomically (write to temp, rename over original) → cache miss.
#[tokio::test]
async fn mutation_atomic_replace_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("atomic.cpp", "int f() { return 1; }\n");
    let (_, cached, obj_v1) = h.compile_file_read("atomic.cpp", "atomic.o").await;
    assert!(!cached);

    // Atomic replace: write to temp file, then rename
    let tmp_file = h.path("atomic.cpp.tmp");
    std::fs::write(&tmp_file, "int f() { return 2; }\n").unwrap();
    std::fs::rename(&tmp_file, h.path("atomic.cpp")).unwrap();

    let (_, cached, obj_v2) = h.compile_file_read("atomic.cpp", "atomic.o").await;
    assert!(!cached, "atomic file replacement must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "different content → different .o");

    h.shutdown().await;
}

/// Rapid edit cycle: edit → compile → edit → compile, 20 rounds.
/// Each edit changes the return value; every compile must miss and produce unique .o.
#[tokio::test]
async fn mutation_rapid_edit_cycle() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let mut prev_obj: Option<Vec<u8>> = None;

    for i in 0..20 {
        h.write_file("rapid.cpp", &format!("int f() {{ return {i}; }}\n"));
        let (exit_code, cached, obj) = h.compile_file_read("rapid.cpp", "rapid.o").await;
        assert_eq!(exit_code, 0, "round {i} should compile");
        assert!(!cached, "round {i}: new content must be a cache miss");

        if let Some(ref prev) = prev_obj {
            assert_ne!(
                prev, &obj,
                "round {i}: different return value → different .o"
            );
        }
        prev_obj = Some(obj);
    }

    // Verify log: all 20 should be misses
    let log = h.log_text();
    let misses = log.matches("cache miss").count();
    assert_eq!(misses, 20, "expected 20 misses in rapid edit cycle");

    h.shutdown().await;
}

/// Edit one file in a multi-file project → only that file's cache is invalidated.
#[tokio::test]
async fn mutation_selective_invalidation() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let n = 10;
    let mut original_objs = Vec::new();

    // Compile 10 files
    for i in 0..n {
        h.write_file(
            &format!("sel_{i}.cpp"),
            &format!("int f{i}() {{ return {i}; }}\n"),
        );
        let (exit_code, cached, obj) = h
            .compile_file_read(&format!("sel_{i}.cpp"), &format!("sel_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert!(!cached);
        original_objs.push(obj);
    }

    // Edit file 5 only
    h.write_file("sel_5.cpp", "int f5() { return 999; }\n");

    // Recompile all — only file 5 should miss
    for i in 0..n {
        let _ = std::fs::remove_file(h.path(&format!("sel_{i}.o")));
        let (exit_code, cached, obj) = h
            .compile_file_read(&format!("sel_{i}.cpp"), &format!("sel_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);

        if i == 5 {
            assert!(!cached, "edited file 5 must miss");
            assert_ne!(
                original_objs[5], obj,
                "edited file 5 must produce different .o"
            );
        } else {
            assert!(cached, "unedited file {i} should still hit cache");
            assert_eq!(
                original_objs[i], obj,
                "unedited file {i} should produce same .o"
            );
        }
    }

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// HEADER MUTATIONS
//
// Headers are included in the artifact key via DepGraph. Editing any header
// (direct or transitive) must invalidate the cache for all affected sources.
// ═══════════════════════════════════════════════════════════════════════════

/// Edit a local header → must invalidate (header hashes are in the artifact key).
#[tokio::test]
async fn mutation_header_edit_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("val.h", "#define VALUE 1\n");
    h.write_file("hdr.cpp", "#include \"val.h\"\nint f() { return VALUE; }\n");

    let (_, cached, obj_v1) = h.compile_file_read("hdr.cpp", "hdr.o").await;
    assert!(!cached);

    // Edit header — source unchanged
    h.write_file("val.h", "#define VALUE 99\n");

    let (_, cached, obj_v2) = h.compile_file_read("hdr.cpp", "hdr.o").await;
    assert!(!cached, "header edit must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "header change → different .o");

    h.shutdown().await;
}

/// Transitive header change: A.cpp → B.h → C.h. Edit C.h → must invalidate.
#[tokio::test]
async fn mutation_transitive_header_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("c.h", "#define DEEP_VAL 10\n");
    h.write_file("b.h", "#include \"c.h\"\n");
    h.write_file(
        "trans.cpp",
        "#include \"b.h\"\nint f() { return DEEP_VAL; }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("trans.cpp", "trans.o").await;
    assert!(!cached);

    // Edit transitive header
    h.write_file("c.h", "#define DEEP_VAL 77\n");

    let (_, cached, obj_v2) = h.compile_file_read("trans.cpp", "trans.o").await;
    assert!(!cached, "transitive header edit must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "different header content → different .o");

    h.shutdown().await;
}

/// Add a new header and #include it from source → must invalidate
/// (source content changed because the #include line was added).
#[tokio::test]
async fn mutation_add_include_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("add_inc.cpp", "int f() { return 0; }\n");
    let (_, cached, obj_v1) = h.compile_file_read("add_inc.cpp", "add_inc.o").await;
    assert!(!cached);

    // Add a new header and #include it
    h.write_file("new.h", "#define EXTRA 42\n");
    h.write_file(
        "add_inc.cpp",
        "#include \"new.h\"\nint f() { return EXTRA; }\n",
    );

    let (_, cached, obj_v2) = h.compile_file_read("add_inc.cpp", "add_inc.o").await;
    assert!(!cached, "adding #include changes source → must miss");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Remove a #include from source → must invalidate (source changed).
#[tokio::test]
async fn mutation_remove_include_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("removable.h", "#define X 5\n");
    h.write_file(
        "rm_inc.cpp",
        "#include \"removable.h\"\nint f() { return 0; }\n",
    );
    let (_, cached, _) = h.compile_file_read("rm_inc.cpp", "rm_inc.o").await;
    assert!(!cached);

    // Remove the #include from source
    h.write_file("rm_inc.cpp", "int f() { return 0; }\n");
    let (_, cached, _) = h.compile_file_read("rm_inc.cpp", "rm_inc.o").await;
    assert!(!cached, "removing #include changes source → must miss");

    h.shutdown().await;
}

/// Delete a header that's included → cache miss, compile fails.
#[tokio::test]
async fn mutation_delete_included_header() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("doomed.h", "#define D 1\n");
    h.write_file(
        "uses_doomed.cpp",
        "#include \"doomed.h\"\nint f() { return D; }\n",
    );

    let (exit_code, cached, _) = h
        .compile_file_read("uses_doomed.cpp", "uses_doomed.o")
        .await;
    assert_eq!(exit_code, 0);
    assert!(!cached);

    // Delete the header — source unchanged
    std::fs::remove_file(h.path("doomed.h")).unwrap();

    let (exit_code, cached, _) = h
        .compile_file_read("uses_doomed.cpp", "uses_doomed.o")
        .await;
    assert!(!cached, "deleted header must invalidate cache");
    assert_ne!(exit_code, 0, "compile with missing header should fail");

    h.shutdown().await;
}

/// Replace header with completely different content → must invalidate.
#[tokio::test]
async fn mutation_header_replacement_invalidates() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("repl.h", "#define A 1\nstatic int get_a() { return A; }\n");
    h.write_file(
        "repl.cpp",
        "#include \"repl.h\"\nint f() { return get_a(); }\n",
    );

    let (_, cached, obj_v1) = h.compile_file_read("repl.cpp", "repl.o").await;
    assert!(!cached);

    // Replace header entirely
    h.write_file(
        "repl.h",
        "#define A 999\nstatic int get_a() { return A; }\n",
    );

    let (_, cached, obj_v2) = h.compile_file_read("repl.cpp", "repl.o").await;
    assert!(!cached, "header replacement must invalidate cache");
    assert_ne!(obj_v1, obj_v2, "different header content → different .o");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// MULTI-FILE MUTATION STORMS
// ═══════════════════════════════════════════════════════════════════════════

/// 20 files sharing a common header. Edit each file one at a time.
/// Only the edited file should miss; all others should hit.
#[tokio::test]
async fn mutation_storm_one_edit_at_a_time() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let n = 20;
    h.write_file(
        "shared.h",
        "#pragma once\ninline int shared() { return 0; }\n",
    );

    // Initial compile of all files
    let mut objs: Vec<Vec<u8>> = Vec::new();
    for i in 0..n {
        h.write_file(
            &format!("storm_{i}.cpp"),
            &format!("#include \"shared.h\"\nint f{i}() {{ return shared() + {i}; }}\n"),
        );
        let (exit_code, cached, obj) = h
            .compile_file_read(&format!("storm_{i}.cpp"), &format!("storm_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "initial compile of storm_{i} should miss");
        objs.push(obj);
    }

    // Edit each file one at a time, verify only that file misses
    for edit_idx in 0..n {
        h.write_file(
            &format!("storm_{edit_idx}.cpp"),
            &format!(
                "#include \"shared.h\"\nint f{edit_idx}() {{ return shared() + {val}; }}\n",
                val = edit_idx + 1000
            ),
        );

        // Recompile the edited file — must miss
        let (exit_code, cached, new_obj) = h
            .compile_file_read(
                &format!("storm_{edit_idx}.cpp"),
                &format!("storm_{edit_idx}.o"),
            )
            .await;
        assert_eq!(exit_code, 0);
        assert!(
            !cached,
            "edited storm_{edit_idx} must miss after content change"
        );
        assert_ne!(
            objs[edit_idx], new_obj,
            "edited storm_{edit_idx} must produce different .o"
        );
        objs[edit_idx] = new_obj;

        // Spot-check a few unedited files — should hit
        let check_idx = (edit_idx + 1) % n;
        let _ = std::fs::remove_file(h.path(&format!("storm_{check_idx}.o")));
        let (_, cached, obj) = h
            .compile_file_read(
                &format!("storm_{check_idx}.cpp"),
                &format!("storm_{check_idx}.o"),
            )
            .await;
        assert!(
            cached,
            "unedited storm_{check_idx} should hit while storm_{edit_idx} was edited"
        );
        assert_eq!(objs[check_idx], obj);
    }

    h.shutdown().await;
}

/// Shared header edit → all files using it must be invalidated.
#[tokio::test]
async fn mutation_storm_shared_header_edit() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let n = 10;
    h.write_file("common.h", "#pragma once\n#define BASE 100\n");

    // Compile all files
    for i in 0..n {
        h.write_file(
            &format!("sh_{i}.cpp"),
            &format!("#include \"common.h\"\nint f{i}() {{ return BASE + {i}; }}\n"),
        );
        let (exit_code, cached, _) = h
            .compile_file_read(&format!("sh_{i}.cpp"), &format!("sh_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert!(!cached);
    }

    // Edit the shared header — source files unchanged
    h.write_file("common.h", "#pragma once\n#define BASE 999\n");

    // Recompile all files — ALL must miss (shared header changed)
    for i in 0..n {
        let _ = std::fs::remove_file(h.path(&format!("sh_{i}.o")));
        let (exit_code, cached, _) = h
            .compile_file_read(&format!("sh_{i}.cpp"), &format!("sh_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert!(!cached, "file sh_{i} must miss after shared header edit");
    }

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// FILE LIFECYCLE MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Delete source, recreate with different content → must invalidate.
#[tokio::test]
async fn mutation_delete_recreate_source() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("lifecycle.cpp", "int f() { return 1; }\n");
    let (_, cached, obj_v1) = h.compile_file_read("lifecycle.cpp", "lifecycle.o").await;
    assert!(!cached);

    // Delete and recreate with different content
    std::fs::remove_file(h.path("lifecycle.cpp")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    h.write_file("lifecycle.cpp", "int f() { return 2; }\n");

    let (_, cached, obj_v2) = h.compile_file_read("lifecycle.cpp", "lifecycle.o").await;
    assert!(!cached, "delete+recreate with different content must miss");
    assert_ne!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Delete source, recreate with SAME content → should still hit
/// (content hash is the same).
#[tokio::test]
async fn mutation_delete_recreate_same_content() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let content = "int f() { return 42; }\n";
    h.write_file("same.cpp", content);
    let (_, cached, obj_v1) = h.compile_file_read("same.cpp", "same.o").await;
    assert!(!cached);

    // Delete and recreate with same content
    std::fs::remove_file(h.path("same.cpp")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // ensure different mtime
    h.write_file("same.cpp", content);

    std::fs::remove_file(h.path("same.o")).unwrap();
    let (_, cached, obj_v2) = h.compile_file_read("same.cpp", "same.o").await;
    assert!(
        cached,
        "delete+recreate with same content should hit (same content hash)"
    );
    assert_eq!(obj_v1, obj_v2);

    h.shutdown().await;
}

/// Add a brand new source file to the project → should not affect existing caches.
#[tokio::test]
async fn mutation_add_new_file_no_interference() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("existing.cpp", "int f() { return 1; }\n");
    let (_, cached, obj_existing) = h.compile_file_read("existing.cpp", "existing.o").await;
    assert!(!cached);

    // Add a brand new file
    h.write_file("brand_new.cpp", "int g() { return 2; }\n");
    let (_, cached, _) = h.compile_file_read("brand_new.cpp", "brand_new.o").await;
    assert!(!cached, "brand new file should miss");

    // Existing file should still hit
    std::fs::remove_file(h.path("existing.o")).unwrap();
    let (_, cached, obj_again) = h.compile_file_read("existing.cpp", "existing.o").await;
    assert!(
        cached,
        "existing file should still hit after adding new file"
    );
    assert_eq!(obj_existing, obj_again);

    h.shutdown().await;
}

/// Remove a source file, try to compile it → graceful error, not stale cache.
#[tokio::test]
async fn mutation_remove_source_graceful_error() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("vanish.cpp", "int f() { return 1; }\n");
    let (exit_code, cached, _) = h.compile_file_read("vanish.cpp", "vanish.o").await;
    assert_eq!(exit_code, 0);
    assert!(!cached);

    // Remove the source
    std::fs::remove_file(h.path("vanish.cpp")).unwrap();
    std::fs::remove_file(h.path("vanish.o")).unwrap();

    // Compile should fail gracefully
    let cwd = h.cwd();
    h.client
        .send(&Request::Compile {
            session_id: h.session_id,
            args: vec![
                "-c".to_string(),
                "vanish.cpp".to_string(),
                "-o".to_string(),
                "vanish.o".to_string(),
            ],
            cwd,
        })
        .await
        .unwrap();

    let resp: Option<Response> = h.client.recv().await.unwrap();
    match resp {
        Some(Response::CompileResult {
            exit_code, cached, ..
        }) => {
            assert_ne!(exit_code, 0, "compile of deleted source should fail");
            assert!(!cached, "failed compile must not be cached");
        }
        Some(Response::Error { .. }) => {
            // Also acceptable — hash_file failed
        }
        other => panic!("expected CompileResult or Error, got: {other:?}"),
    }

    // Server should still be alive
    h.client.send(&Request::Ping).await.unwrap();
    let resp: Option<Response> = h.client.recv().await.unwrap();
    assert_eq!(resp, Some(Response::Pong));

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// INCLUDE PATH AND FLAG MUTATIONS
// ═══════════════════════════════════════════════════════════════════════════

/// Same source, different -I include paths → different cache entries.
#[tokio::test]
async fn mutation_include_path_creates_different_cache_entry() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    // Two directories with same-named header but different content
    let dir_a = h.path("inc_a");
    let dir_b = h.path("inc_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    std::fs::write(dir_a.join("config.h"), "#define VAL 1\n").unwrap();
    std::fs::write(dir_b.join("config.h"), "#define VAL 2\n").unwrap();

    h.write_file(
        "inc_test.cpp",
        "#include \"config.h\"\nint f() { return VAL; }\n",
    );

    let inc_a_str = format!("-I{}", dir_a.to_string_lossy());
    let inc_b_str = format!("-I{}", dir_b.to_string_lossy());

    // Compile with -I inc_a
    let (exit_code, cached, obj_a) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_a_str],
            &cwd,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert_eq!(exit_code, 0);
    assert!(!cached);

    // Compile with -I inc_b — different include path = different cache key
    let _ = std::fs::remove_file(h.path("inc_test.o"));
    let (exit_code, cached, obj_b) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_b_str],
            &cwd,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert_eq!(exit_code, 0);
    assert!(
        !cached,
        "different -I path must create different cache entry"
    );
    assert_ne!(
        obj_a, obj_b,
        "different include dirs with different headers → different .o"
    );

    // Recompile with -I inc_a — should hit original cache
    let _ = std::fs::remove_file(h.path("inc_test.o"));
    let (_, cached, obj_a2) = {
        let obj_path = h.path("inc_test.o");
        let cwd = h.cwd();
        let (ec, c) = compile(
            &mut h.client,
            h.session_id,
            &["-c", "inc_test.cpp", "-o", "inc_test.o", &inc_a_str],
            &cwd,
        )
        .await;
        let obj = std::fs::read(&obj_path).unwrap();
        (ec, c, obj)
    };
    assert!(cached, "-I inc_a recompile should hit cache");
    assert_eq!(obj_a, obj_a2);

    h.shutdown().await;
}

/// Add -D flag → different cache entry. Remove -D → original entry.
#[tokio::test]
async fn mutation_define_flag_toggle() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file(
        "deftog.cpp",
        r#"
#ifdef FEATURE
int f() { return 1; }
#else
int f() { return 0; }
#endif
"#,
    );

    // Without -D
    let (_, cached, obj_no_d) = h.compile_file_read("deftog.cpp", "deftog.o").await;
    assert!(!cached);

    // With -DFEATURE
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            h.session_id,
            &["-c", "deftog.cpp", "-o", "deftog.o", "-DFEATURE"],
            &cwd,
        )
        .await
    };
    assert!(!cached, "-DFEATURE is a different cache key");
    let obj_with_d = std::fs::read(h.path("deftog.o")).unwrap();
    assert_ne!(obj_no_d, obj_with_d, "-DFEATURE → different .o");

    // Back to without -D — should hit original cache
    let _ = std::fs::remove_file(h.path("deftog.o"));
    let (_, cached, obj_no_d2) = h.compile_file_read("deftog.cpp", "deftog.o").await;
    assert!(cached, "recompile without -D should hit original cache");
    assert_eq!(obj_no_d, obj_no_d2);

    h.shutdown().await;
}

/// Optimization level change: -O0 vs -O2 → different cache entries.
/// Then edit source → both entries invalidated.
#[tokio::test]
async fn mutation_opt_level_then_edit() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    h.write_file("opt.cpp", "int f(int x) { return x * x; }\n");

    // Compile with -O0
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            h.session_id,
            &["-c", "opt.cpp", "-o", "opt.o", "-O0"],
            &cwd,
        )
        .await
    };
    assert!(!cached);
    let obj_o0_v1 = std::fs::read(h.path("opt.o")).unwrap();

    // Compile with -O2 — different cache key
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            h.session_id,
            &["-c", "opt.cpp", "-o", "opt.o", "-O2"],
            &cwd,
        )
        .await
    };
    assert!(!cached, "-O2 is a different cache key");

    // Edit source
    h.write_file("opt.cpp", "int f(int x) { return x + x; }\n");

    // Recompile with -O0 — must miss (source changed)
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            h.session_id,
            &["-c", "opt.cpp", "-o", "opt.o", "-O0"],
            &cwd,
        )
        .await
    };
    assert!(!cached, "-O0 must miss after source edit");
    let obj_o0_v2 = std::fs::read(h.path("opt.o")).unwrap();
    assert_ne!(obj_o0_v1, obj_o0_v2);

    // Recompile with -O2 — must also miss
    let (_, cached) = {
        let cwd = h.cwd();
        compile(
            &mut h.client,
            h.session_id,
            &["-c", "opt.cpp", "-o", "opt.o", "-O2"],
            &cwd,
        )
        .await
    };
    assert!(!cached, "-O2 must also miss after source edit");

    h.shutdown().await;
}

// ═══════════════════════════════════════════════════════════════════════════
// LARGE-SCALE MUTATION STORM
// ═══════════════════════════════════════════════════════════════════════════

/// 50 files, random edits in batches. Verifies correctness at scale.
/// Each batch edits a subset of files; only edited files should miss.
#[tokio::test]
async fn mutation_storm_large_scale() {
    let mut h = match TestHarness::new().await {
        Some(h) => h,
        None => return,
    };

    let n = 50;

    // Generate and compile all files
    let mut versions: Vec<usize> = vec![0; n];
    let mut obj_data: Vec<Vec<u8>> = Vec::new();

    for i in 0..n {
        h.write_file(
            &format!("big_{i}.cpp"),
            &format!("int f{i}() {{ return {i}; }}\n"),
        );
        let (exit_code, cached, obj) = h
            .compile_file_read(&format!("big_{i}.cpp"), &format!("big_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert!(!cached);
        obj_data.push(obj);
    }

    // 10 rounds of batch edits
    for round in 0..10 {
        // Edit files at indices: round*3, round*3+1, round*3+2 (mod n)
        let edit_indices: Vec<usize> = (0..3).map(|k| (round * 3 + k) % n).collect();

        for &idx in &edit_indices {
            versions[idx] += 1;
            let v = versions[idx];
            h.write_file(
                &format!("big_{idx}.cpp"),
                &format!("int f{idx}() {{ return {idx} + {v} * 1000; }}\n"),
            );
        }

        // Recompile edited files — must all miss
        for &idx in &edit_indices {
            let (exit_code, cached, new_obj) = h
                .compile_file_read(&format!("big_{idx}.cpp"), &format!("big_{idx}.o"))
                .await;
            assert_eq!(exit_code, 0, "round {round}, file {idx} should compile");
            assert!(
                !cached,
                "round {round}, edited file {idx} (v{}) must miss",
                versions[idx]
            );
            assert_ne!(
                obj_data[idx], new_obj,
                "round {round}, edited file {idx} must produce different .o"
            );
            obj_data[idx] = new_obj;
        }

        // Spot-check an unedited file — should hit
        let check_idx = (edit_indices[0] + n / 2) % n;
        if !edit_indices.contains(&check_idx) {
            let _ = std::fs::remove_file(h.path(&format!("big_{check_idx}.o")));
            let (_, cached, obj) = h
                .compile_file_read(
                    &format!("big_{check_idx}.cpp"),
                    &format!("big_{check_idx}.o"),
                )
                .await;
            assert!(
                cached,
                "round {round}, unedited file {check_idx} should hit"
            );
            assert_eq!(obj_data[check_idx], obj);
        }
    }

    // Final verification: recompile everything, only files at current version should hit
    let mut hits = 0;
    let mut misses = 0;
    for (i, expected_obj) in obj_data.iter().enumerate() {
        let _ = std::fs::remove_file(h.path(&format!("big_{i}.o")));
        let (exit_code, cached, obj) = h
            .compile_file_read(&format!("big_{i}.cpp"), &format!("big_{i}.o"))
            .await;
        assert_eq!(exit_code, 0);
        assert_eq!(
            expected_obj, &obj,
            "final verify: file {i} should match stored .o"
        );
        if cached {
            hits += 1;
        } else {
            misses += 1;
        }
    }
    eprintln!("Final recompile: {hits} hits, {misses} misses out of {n} files");
    // All should be cache hits since we haven't edited anything since the last compile
    assert_eq!(hits, n, "all files should hit cache on final recompile");

    h.shutdown().await;
}
