# IPC Model

Transport abstraction, socket discovery, connection lifecycle, and error handling for CLI-daemon communication.

For the protocol message types see [overview.md](overview.md) (section 2.4). For platform differences see [portability.md](portability.md).

---

## Transport Abstraction

The `Transport` trait (see overview.md section 2.3) abstracts over Unix domain sockets and Windows named pipes. The daemon and CLI code are written against the trait; platform selection happens at build time via conditional compilation:

```rust
#[cfg(unix)]
type PlatformTransport = UnixTransport;

#[cfg(windows)]
type PlatformTransport = NamedPipeTransport;
```

## Socket Discovery

**Unix (Linux / macOS):**
- Socket path: `$XDG_RUNTIME_DIR/zccache/sock`
- Fallback if `$XDG_RUNTIME_DIR` is unset: `/tmp/zccache-{uid}/sock`
- Lock file: adjacent to socket as `lock`
- Directory created with mode 0700.

**Windows:**
- Named pipe: `\\.\pipe\zccache-{username}`
- Lock file: `%LOCALAPPDATA%\zccache\lock`
- Username obtained via `GetUserNameW`.

## Connection Lifecycle

**CLI side (drop-in wrapper mode):**
1. Compute socket address.
2. Ensure daemon is running (auto-start if needed).
3. Connect and send a single `Request::CompileEphemeral` message.
4. Read one `Response::CompileResult`, relay stdout/stderr, exit.

This single-roundtrip flow replaced an earlier 3-message sequence
(SessionStart → Compile → SessionEnd) that added ~10-20ms overhead
per invocation.

**CLI side (session mode, `ZCCACHE_SESSION_ID` set):**
1. Connect and send `Request::Compile` with the existing session ID.
2. Read `Response::CompileResult`, relay output, exit.

**CLI side (session lifecycle):**
1. `zccache session-start [--stats] [--log FILE]` → `Request::SessionStart` → `Response::SessionStarted { session_id }`.
2. Build system sets `ZCCACHE_SESSION_ID=<uuid>`. Each compiler invocation sends `Request::Compile`.
3. `zccache session-stats <id>` → `Request::SessionStats` → `Response::SessionStatsResult`. Non-destructive; session stays active. Returns `Option<SessionStats>` (`None` if `--stats` was not used at start).
4. `zccache session-end <id>` → `Request::SessionEnd` → `Response::SessionEnded { stats }`. Removes the session.

**Daemon side:**
1. Acquire lock file (write PID).
2. Bind transport listener.
3. Loop: accept connections, spawn a tokio task per connection.
4. Each task: read requests in a loop, process them, send responses.
   A connection may carry multiple requests (session mode) or a single
   `CompileEphemeral` (drop-in mode).

## Error Handling

- If the daemon crashes mid-request, the CLI receives a broken-pipe error. The CLI falls back to running the compiler directly (non-cached) and prints a warning to stderr.
- If serialization/deserialization fails, the daemon sends `Response::Error` if possible, otherwise drops the connection. The CLI falls back.
- Timeouts: the CLI imposes a 60-second timeout on the full IPC round-trip. On timeout, it kills the request and falls back to direct compilation.
