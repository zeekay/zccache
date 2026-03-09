# zccache Issues Found During fbuild Integration

## Environment

- **Integrator:** fbuild (PlatformIO-compatible embedded build tool)
- **Platform:** Windows 10 x86_64, MSYS2/Git Bash, Python 3.13
- **Toolchain:** Espressif xtensa-esp-elf GCC 14.2.0 (unified toolchain)
- **zccache versions tested:** 1.0.0, 1.0.1
- **Build characteristics:** ~300+ include paths via `@file` response files, parallel compilation (8 workers), mixed C/C++ sources (~150 files per build)

---

## Issue 1: Session compiler overrides wrapped compiler (v1.0.0)

**Status: FIXED in v1.0.1**

Per-request compiler override added. The wrapped compiler (`argv[1]`) is now sent
as `Request::Compile { compiler: Some(...) }` and used by the daemon instead of
the session compiler. Test: `cli_flow_test.rs::cli_binary_compiler_override_cpp_session_c_file`.

---

## Issue 2: Daemon crash with "unexpected response: None" (v1.0.1)

**Status: MITIGATED — crash reporting infrastructure added**

Root cause investigation is ongoing. In the meantime, crash reporting has been
implemented so that daemon panics are captured and diagnosable:

1. **Panic hook** (`crash::install_panic_hook`): Captures backtrace and writes a
   timestamped crash dump to `<cache_dir>/crashes/crash-<epoch>.txt` with version,
   OS, arch, PID, panic info, and full backtrace.

2. **Startup check** (`crash::check_previous_crashes`): On daemon start, scans for
   unreported crash dumps and logs warnings. Creates `.reported` marker files to
   avoid repeat warnings.

3. **CLI management** (`zccache crashes`): Lists crash dumps with summaries.
   `zccache crashes --clear` deletes all dumps and markers.

---

## Issue 3: `--compiler` path must be native Windows path (v1.0.0, v1.0.1)

**Status: FIXED in v1.0.1**

MSYS path normalization (`/c/Users/...` → `C:\Users\...`) is handled by
`zccache_core::path::normalize_msys_path()`, applied in the CLI for both
`--compiler` in `session-start` and `argv[1]` in wrap mode.

---

## Issue 4: Sessions are mandatory — no fallback mode (v1.0.0, v1.0.1)

**Status: FIXED in v1.0.1**

Ephemeral session fallback added. When `ZCCACHE_SESSION_ID` is unset, the CLI
auto-creates a temporary session for the single compilation and ends it afterward.
Test: `cli_flow_test.rs::cli_binary_ephemeral_session`.

---

## Issue 5: `zccache clear` not implemented (v1.0.0, v1.0.1)

**Status: FIXED**

`zccache clear` now sends `Request::Clear` to the daemon, which wipes:
- In-memory artifact cache
- Metadata cache (fscache)
- Dependency graph (contexts + files)
- Fast-hit cache
- System include cache
- On-disk artifact files
- Stats and phase profiler counters

Sessions are preserved (active builds survive a cache clear).
Reports counts of cleared items. Handles daemon-not-running gracefully.
Tests: `server::test_server_clear_empty`, `cli_flow_test::cli_clear_resets_cache`.
