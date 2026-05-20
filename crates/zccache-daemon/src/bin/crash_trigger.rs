//! Test-fixture binary used by `tests/crash_minidump_test.rs`.
//!
//! Installs the same panic + signal crash handlers the production daemon
//! installs, then deliberately triggers a specific kind of crash so the
//! test runner can assert that a crash dump file appeared on disk.
//!
//! Invocation: `crash-trigger <mode>` where `<mode>` is one of
//! `panic`, `sigsegv`, `sigabrt`, `stack-overflow`, `illegal-instruction`.

fn main() {
    // Order matters: install panic hook first so even if minidump
    // handler install fails, panics still get caught.
    zccache_daemon::crash::install_panic_hook();
    // Bind the guard so the OS-level handlers stay registered for the
    // whole process lifetime.
    let _guard = zccache_daemon::crash::install_minidump_handler();

    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "panic" => panic!("intentional test panic from crash-trigger"),
        "sigsegv" => unsafe { sadness_generator::raise_segfault() },
        "sigabrt" => unsafe { sadness_generator::raise_abort() },
        "stack-overflow" => unsafe { sadness_generator::raise_stack_overflow() },
        "illegal-instruction" => unsafe { sadness_generator::raise_illegal_instruction() },
        other => {
            eprintln!(
                "crash-trigger: unknown mode '{other}' (expected panic|sigsegv|sigabrt|stack-overflow|illegal-instruction)"
            );
            std::process::exit(2);
        }
    }
}
