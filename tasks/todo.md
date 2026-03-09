# Persistent Single-Instance Daemon

## Status: COMPLETE

Implemented DD-014: lazy daemon startup. CLI auto-starts daemon if not running.
Daemon shuts down after idle timeout (default 1 hour).

## What was implemented

### 1. Lock file utilities (`zccache-ipc/src/lib.rs`)
- [x] `write_lock_file(pid)` — write PID to lock file
- [x] `read_lock_file_pid()` — read PID from lock file
- [x] `remove_lock_file()` — remove lock file
- [x] `is_process_alive(pid)` — platform-specific PID liveness check (Unix: kill(pid,0), Windows: OpenProcess)
- [x] `check_running_daemon()` — validates PID + cleans stale lock files/sockets

### 2. Daemon server (`zccache-daemon/src/server.rs`)
- [x] Added `shutdown` handle to `SharedState` (so Shutdown request can signal daemon)
- [x] `Shutdown` request now actually signals daemon shutdown via `state.shutdown.notify_one()`
- [x] Added `last_activity` atomic timestamp (touched on every request)
- [x] Added `start_time` for real uptime reporting in Status response
- [x] `run()` takes `idle_timeout_secs` param — spawns watchdog task that checks every 60s
- [x] Idle timeout = 0 disables watchdog (used by tests)

### 3. Daemon process lifecycle (`zccache-daemon/src/main.rs`)
- [x] Writes lock file on daemon start
- [x] Handles Ctrl+C via `tokio::signal::ctrl_c()` → graceful shutdown
- [x] Removes lock file on clean shutdown (both normal exit and error)
- [x] `--idle-timeout` arg (default 3600s)

### 4. CLI connect-or-start (`zccache-cli/src/main.rs`)
- [x] `ensure_daemon(endpoint)` — try connect → check lock file → spawn → wait for ready
- [x] `find_daemon_binary()` — looks next to CLI binary, then on PATH
- [x] `spawn_daemon()` — detached background process, `CREATE_NO_WINDOW` on Windows
- [x] `start` subcommand — ensure daemon running
- [x] `stop` subcommand — connect + send Shutdown
- [x] `session-start` auto-starts daemon via `ensure_daemon()`

## Verified
- All workspace tests pass (0 failures)
- Clippy clean
- Smoke test: `zccache start` → `zccache status` → `zccache stop` works
- Double-start is idempotent
- Lock file cleaned up on shutdown
- No console window popup on Windows (CREATE_NO_WINDOW flag)
