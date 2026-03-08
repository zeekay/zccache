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

**CLI side:**
1. Compute socket address.
2. Attempt `Transport::connect()`.
3. On success: send request, read response, close.
4. On failure (connection refused, socket not found):
   a. Read lock file. If it contains a PID and that process is alive, wait up to 2 seconds and retry.
   b. Otherwise, remove stale lock file and socket.
   c. Spawn daemon process (detached), passing `--daemon` flag.
   d. Poll for socket availability (up to 5 seconds, 50ms intervals).
   e. Connect.

**Daemon side:**
1. Acquire lock file (write PID).
2. Bind transport listener.
3. Loop: accept connections, spawn a tokio task per connection.
4. Each task: read one `Request`, process it, send one `Response`, close.

## Error Handling

- If the daemon crashes mid-request, the CLI receives a broken-pipe error. The CLI falls back to running the compiler directly (non-cached) and prints a warning to stderr.
- If serialization/deserialization fails, the daemon sends `Response::Error` if possible, otherwise drops the connection. The CLI falls back.
- Timeouts: the CLI imposes a 60-second timeout on the full IPC round-trip. On timeout, it kills the request and falls back to direct compilation.
