/// Integration tests that exercise the zccache binary end-to-end.
///
/// These require gcc to be on PATH.  Tests gracefully skip when gcc is absent.
use std::fs;
use std::process::Command;

fn gcc_available() -> bool {
    Command::new("gcc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn zccache_bin() -> String {
    env!("CARGO_BIN_EXE_zccache").to_string()
}

#[test]
fn cache_hit_produces_identical_object() {
    if !gcc_available() {
        eprintln!("gcc not available – skipping integration test");
        return;
    }

    let src_tmp = tempfile::tempdir().unwrap();
    let cache_tmp = tempfile::tempdir().unwrap();

    let src = src_tmp.path().join("hello.c");
    fs::write(&src, "int add(int a, int b) { return a + b; }\n").unwrap();

    let obj1 = src_tmp.path().join("hello1.o");
    let obj2 = src_tmp.path().join("hello2.o");

    // First run: cache miss
    let status1 = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .env("ZCCACHE_DEBUG", "1")
        .arg("gcc")
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(&obj1)
        .status()
        .expect("first compile failed");
    assert!(status1.success(), "first compile should succeed");
    assert!(obj1.exists());

    // Second run: cache hit
    let status2 = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .env("ZCCACHE_DEBUG", "1")
        .arg("gcc")
        .arg("-c")
        .arg(&src)
        .arg("-o")
        .arg(&obj2)
        .status()
        .expect("second compile failed");
    assert!(status2.success(), "second compile should succeed");
    assert!(obj2.exists());

    let b1 = fs::read(&obj1).unwrap();
    let b2 = fs::read(&obj2).unwrap();
    assert_eq!(b1, b2, "cached object must be identical to original");
}

#[test]
fn source_change_causes_cache_miss() {
    if !gcc_available() {
        eprintln!("gcc not available – skipping integration test");
        return;
    }

    let src_tmp = tempfile::tempdir().unwrap();
    let cache_tmp = tempfile::tempdir().unwrap();
    let src = src_tmp.path().join("v.c");

    // Compile version 1
    fs::write(&src, "int val = 1;\n").unwrap();
    let obj1 = src_tmp.path().join("v1.o");
    let s1 = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .arg("gcc").arg("-c").arg(&src).arg("-o").arg(&obj1)
        .status().unwrap();
    assert!(s1.success());

    // Compile version 2 (different source)
    fs::write(&src, "int val = 2;\n").unwrap();
    let obj2 = src_tmp.path().join("v2.o");
    let s2 = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .arg("gcc").arg("-c").arg(&src).arg("-o").arg(&obj2)
        .status().unwrap();
    assert!(s2.success());

    let b1 = fs::read(&obj1).unwrap();
    let b2 = fs::read(&obj2).unwrap();
    assert_ne!(b1, b2, "different source must produce different objects");
}

#[test]
fn disable_env_bypasses_cache() {
    if !gcc_available() {
        eprintln!("gcc not available – skipping integration test");
        return;
    }

    let src_tmp = tempfile::tempdir().unwrap();
    let cache_tmp = tempfile::tempdir().unwrap();

    let src = src_tmp.path().join("d.c");
    fs::write(&src, "int x = 42;\n").unwrap();

    let obj = src_tmp.path().join("d.o");
    let status = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .env("ZCCACHE_DISABLE", "1")
        .arg("gcc").arg("-c").arg(&src).arg("-o").arg(&obj)
        .status().unwrap();

    assert!(status.success());
    assert!(obj.exists());

    // With ZCCACHE_DISABLE no stats should have been written
    let stats_file = cache_tmp.path().join("stats.json");
    // Stats may or may not exist, but if present hits/misses should both be 0
    if stats_file.exists() {
        let data = fs::read_to_string(&stats_file).unwrap();
        assert!(data.contains("\"cache_hits\": 0") || !data.contains("cache_hits"));
    }
}

#[test]
fn show_stats_subcommand_exits_zero() {
    let cache_tmp = tempfile::tempdir().unwrap();
    let status = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .arg("--show-stats")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn clear_cache_subcommand_exits_zero() {
    let cache_tmp = tempfile::tempdir().unwrap();
    let status = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .arg("--clear-cache")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn help_subcommand_exits_zero() {
    let status = Command::new(zccache_bin())
        .arg("--help")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn link_step_passes_through() {
    if !gcc_available() {
        eprintln!("gcc not available – skipping integration test");
        return;
    }

    let src_tmp = tempfile::tempdir().unwrap();
    let cache_tmp = tempfile::tempdir().unwrap();

    // First compile a source file the normal way (bypassing zccache)
    let src = src_tmp.path().join("main.c");
    fs::write(&src, "int main(void) { return 0; }\n").unwrap();
    let obj = src_tmp.path().join("main.o");
    Command::new("gcc")
        .arg("-c").arg(&src).arg("-o").arg(&obj)
        .status().unwrap();

    // Now use zccache for the link step – it should pass through
    let exe = src_tmp.path().join("main");
    let status = Command::new(zccache_bin())
        .env("ZCCACHE_DIR", cache_tmp.path())
        .arg("gcc").arg(&obj).arg("-o").arg(&exe)
        .status().unwrap();
    assert!(status.success());
    assert!(exe.exists());
}
