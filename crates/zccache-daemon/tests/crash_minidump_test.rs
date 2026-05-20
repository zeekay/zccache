//! End-to-end tests for the signal/exception crash handler.
//!
//! Each test spawns the `crash-trigger` fixture binary (defined at
//! `crates/zccache-daemon/src/bin/crash_trigger.rs`) with a mode that
//! deliberately causes a specific kind of fault, then asserts that a
//! crash dump file appeared in the configured crash directory. The
//! fixture inherits `ZCCACHE_CACHE_DIR` so each test gets an isolated
//! tempdir that doesn't pollute the real `~/.zccache`.
//!
//! Why these are integration tests rather than unit tests: the unit
//! tests in `crash.rs` only construct artificial dump files; they
//! prove nothing about the OS-level signal/exception path. These
//! tests actually deliver a fault to the running fixture process and
//! verify the handler caught it AND produced a dump.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Spawn `crash-trigger <mode>` with a per-test cache dir, wait for it
/// to exit, then assert that exactly one dump with `expected_ext`
/// appeared in `<cache>/crashes/`. Returns the dump path AND the
/// `TempDir` guard — the caller must keep the guard alive while it
/// touches the path, since dropping the guard deletes the tempdir.
fn run_crash_scenario(mode: &str, expected_ext: &str) -> (PathBuf, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let cache_dir = tmp.path().join(".zccache");
    let crash_dir = cache_dir.join("crashes");

    let bin = env!("CARGO_BIN_EXE_crash-trigger");
    let output = std::process::Command::new(bin)
        .arg(mode)
        .env("ZCCACHE_CACHE_DIR", &cache_dir)
        // RUST_BACKTRACE is force_capture()d inside the handler regardless,
        // but be explicit so a developer running the test by hand with
        // RUST_BACKTRACE=0 doesn't get a surprising empty backtrace.
        .env("RUST_BACKTRACE", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));

    // We expect a non-zero exit OR a crash signal — but the panic
    // case may unwind cleanly and exit(0), so don't assert on the
    // exit status. The disk evidence is the real assertion.
    let _ = output;

    let dump = wait_for_dump_with_ext(&crash_dir, expected_ext, Duration::from_secs(5));
    let path = dump.unwrap_or_else(|| {
        panic!(
            "no .{expected_ext} dump appeared in {} after mode={mode}",
            crash_dir.display()
        )
    });
    (path, tmp)
}

/// Poll the directory for a file with the given extension. The handler
/// writes synchronously, but on Windows there's a tiny window between
/// the dump write and process exit where the directory listing may
/// not yet include the file — poll for a few seconds to absorb that.
fn wait_for_dump_with_ext(dir: &Path, ext: &str, timeout: Duration) -> Option<PathBuf> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == ext) {
                    return Some(path);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Sanity floor on dump size — a truncated handler that managed to
/// `create` the file but failed to write content would otherwise pass
/// the "file appeared" check. Our text dumps are ~1-4 KB; minidumps
/// (when we migrate the format) are typically 20+ KB.
const MIN_DUMP_BYTES: u64 = 200;

#[test]
fn panic_writes_text_dump() {
    let (dump, _tmp) = run_crash_scenario("panic", "txt");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(
        size > MIN_DUMP_BYTES,
        "panic dump suspiciously small: {size} bytes at {}",
        dump.display()
    );
    let body = std::fs::read_to_string(&dump).unwrap();
    assert!(
        body.contains("intentional test panic from crash-trigger"),
        "panic dump didn't include the panic message:\n{body}"
    );
}

#[test]
fn sigsegv_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("sigsegv", "dmp");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(
        size > MIN_DUMP_BYTES,
        "sigsegv dump suspiciously small: {size} bytes at {}",
        dump.display()
    );
}

#[test]
fn sigabrt_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("sigabrt", "dmp");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
}

#[test]
#[ignore = "stack overflow needs sigaltstack (Unix) or specific guard-page \
            handling (Windows); the handler runs on an already-exhausted \
            stack and the dump is currently not produced. Tracked separately \
            from the rest of the crash-handler coverage."]
fn stack_overflow_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("stack-overflow", "dmp");
    assert!(dump.exists());
}

// SIGILL behaviour on macOS goes through Mach exception ports in ways
// that crash-handler documents as more fragile than the Unix-signal
// path. Skip there until we either add the Mach-port workaround or
// drop down to `sigaction(SIGILL)` directly.
#[test]
#[cfg(not(target_os = "macos"))]
fn illegal_instruction_writes_signal_dump() {
    let (dump, _tmp) = run_crash_scenario("illegal-instruction", "dmp");
    let size = std::fs::metadata(&dump).unwrap().len();
    assert!(size > MIN_DUMP_BYTES);
}
