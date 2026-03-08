# zccache Architecture

This document describes the architecture of `zccache`, a high-performance local compiler cache daemon written in Rust. It is intended to be detailed enough that an implementer can build the system from this specification alone.

---

## 1. System Overview

zccache intercepts C/C++ compiler invocations, computes a deterministic cache key from the compiler, flags, environment, and source content, and returns cached artifacts on a hit. It runs as a per-user background daemon that persists across compilations, maintaining an in-memory file metadata cache and a disk-backed artifact store.

### Component Diagram

```
+---------------------------+       +-------------------------------------------+
|          CLI              |       |               Daemon                      |
|                           |  IPC  |                                           |
|  zccache-cc / zccache-c++ |<----->|  +-------------+    +-----------------+  |
|  zccache wrap -- gcc ...  |       |  | IPC Server  |    | Compiler Manager|  |
|                           |       |  +------+------+    +--------+--------+  |
+---------------------------+       |         |                    |            |
                                    |         v                    v            |
                                    |  +------+------+    +-------+--------+   |
                                    |  |  Metadata   |    | Hashing Engine |   |
                                    |  |   Cache     |    +-------+--------+   |
                                    |  |  (DashMap)  |            |            |
                                    |  +------+------+    +-------+--------+   |
                                    |         |           | Artifact Store |   |
                                    |         v           | (disk + redb)  |   |
                                    |  +------+------+    +-------+--------+   |
                                    |  | File Watcher|            |            |
                                    |  | (notify)    |            |            |
                                    |  +-------------+            |            |
                                    +-----------------------------+------------+
                                              |                   |
                                    +---------v-------------------v-----------+
                                    |       Filesystem / Compilers            |
                                    +-----------------------------------------+
```

**CLI** — thin client that discovers the daemon, connects over IPC, sends a compilation request, and relays the result. If the daemon is not running, the CLI starts it.

**Daemon** — long-lived per-user process. Accepts IPC connections, manages the metadata cache, artifact store, file watcher, and compiler subprocess execution.

**Filesystem / Compilers** — external. The daemon reads source files, invokes compilers, and stores/retrieves artifacts on disk.

---

## 2. Component Descriptions

### 2.1 CLI

**Responsibility:** Entry point for the user. Parses the compiler command line, determines if the invocation is cacheable, and communicates with the daemon.

**Key interfaces:**
- `main()` — entry point. Parses argv to determine mode (wrapper or explicit `wrap` subcommand).
- `DaemonConnector` — locates the daemon socket, connects, starts the daemon if needed.

**Internal structure:**
- Compiler argument parsing: extracts compiler path, source files, output file, flags.
- Cacheability check: rejects linking, preprocessing-only (`-E`), multiple source files, and other non-cacheable invocations before contacting the daemon.
- A small tokio runtime (single-threaded) is created solely for the IPC exchange; the rest of the CLI is synchronous.
- If the invocation is non-cacheable, the CLI execs the compiler directly without contacting the daemon.

**Binary names:** `zccache-cc`, `zccache-c++`, `zccache-gcc`, `zccache-g++`, `zccache-clang`, `zccache-clang++`. Also invokable as `zccache wrap -- <compiler> <args...>`.

### 2.2 Daemon Core

**Responsibility:** Lifecycle management. Starts, shuts down, handles signals.

**Key interfaces:**
- `Daemon::start()` — acquires lock file, binds IPC socket, spawns subsystems.
- `Daemon::shutdown()` — drains in-flight requests, cleans up temp dirs, closes watcher.

**Internal structure:**
- Creates and owns all subsystem handles (IPC server, metadata cache, artifact store, file watcher, compiler manager).
- Runs the tokio multi-threaded runtime.
- Signal handling: SIGTERM/SIGINT trigger graceful shutdown. On Windows, uses `SetConsoleCtrlHandler`.

### 2.3 IPC Transport

**Responsibility:** Platform-abstracted bidirectional byte-stream transport between CLI and daemon.

**Key interfaces:**
```rust
#[async_trait]
trait Transport: Send + Sync {
    type Listener: TransportListener;
    type Stream: TransportStream;

    async fn bind(addr: &TransportAddr) -> Result<Self::Listener>;
    async fn connect(addr: &TransportAddr) -> Result<Self::Stream>;
}

#[async_trait]
trait TransportListener: Send + Sync {
    type Stream: TransportStream;
    async fn accept(&self) -> Result<Self::Stream>;
}

trait TransportStream: AsyncRead + AsyncWrite + Send + Unpin {}
```

**Internal structure:**
- `UnixTransport` — wraps `tokio::net::UnixListener` / `UnixStream`. Used on Linux and macOS.
- `NamedPipeTransport` — wraps `tokio::net::windows::named_pipe`. Used on Windows.
- `TransportAddr` — enum holding path (Unix) or pipe name (Windows).

### 2.4 Protocol

**Responsibility:** Serialization and framing of messages over the transport.

**Message format:** Length-prefixed bincode. Each message is:
```
[u32 little-endian: payload length][payload: bincode-serialized Message]
```

**Message types:**
```rust
enum Request {
    Compile {
        compiler: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        env: Vec<(String, String)>,  // filtered to relevant vars
    },
    Shutdown,
    Stats,
    ClearCache,
}

enum Response {
    CacheHit {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        // Artifacts already written to output paths by daemon
    },
    CacheMiss {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Passthrough {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Error {
        message: String,
    },
    Stats { /* counters */ },
    Ok,
}
```

### 2.5 File Metadata Cache

**Responsibility:** Track file metadata in memory to avoid redundant `stat` and hash calls. Provide fast answers to "has this file changed since we last saw it?"

**Key interfaces:**
```rust
struct MetadataCache {
    entries: DashMap<CanonicalPath, MetadataEntry>,
}

struct MetadataEntry {
    mtime: SystemTime,
    size: u64,
    file_id: Option<FileId>,
    content_hash: Option<Blake3Hash>,
    confidence: Confidence,
    last_verified: Instant,
}

enum Confidence {
    High,    // recently stat-verified or digest-verified
    Medium,  // watcher says unchanged
    Low,     // stale, unverified, or after watcher overflow
}
```

**Internal structure:** See section 5 for full details.

### 2.6 File Watcher

**Responsibility:** Monitor source and header directories for changes and feed events into the metadata cache.

**Key interfaces:**
```rust
struct FileWatcher {
    watcher: notify::RecommendedWatcher,
    tx: tokio::sync::mpsc::Sender<WatcherEvent>,
}
```

**Internal structure:** See section 6 for full details.

### 2.7 Artifact Store

**Responsibility:** Persist and retrieve compiled output files, keyed by content-addressed hash.

**Key interfaces:**
```rust
struct ArtifactStore {
    root: PathBuf,
    index: redb::Database,
    max_size: u64,
}

impl ArtifactStore {
    async fn lookup(&self, key: &Blake3Hash) -> Option<Artifact>;
    async fn store(&self, key: &Blake3Hash, artifact: Artifact) -> Result<()>;
    async fn evict_to_size(&self, target_size: u64) -> Result<()>;
}
```

**Internal structure:** See section 7 for full details.

### 2.8 Hashing Engine

**Responsibility:** Compute deterministic cache keys and content digests using blake3.

**Key interfaces:**
```rust
fn compute_cache_key(
    compiler_id: &Blake3Hash,
    args: &[String],
    env: &[(String, String)],
    source_hash: &Blake3Hash,
    dep_hash: &Blake3Hash,
) -> Blake3Hash;

fn hash_file(path: &Path) -> Result<Blake3Hash>;
fn hash_compiler(path: &Path) -> Result<Blake3Hash>;
```

**Why blake3:** Fast (parallelizable, SIMD-optimized), cryptographic-strength collision resistance, single hash function for all purposes. The `blake3` crate is used directly.

**Cache key construction:**
1. Hash the compiler binary (or use a cached identity hash).
2. Sort and filter CLI args to only cache-relevant flags (strip `-o`, strip debug-only flags, normalize paths).
3. Sort and filter environment variables to only those that affect compilation output (e.g., `CPATH`, `C_INCLUDE_PATH`, `SDKROOT`).
4. Hash the source file content (or use cached digest at High confidence).
5. Hash dependency content. In MVP, this is the hash of the preprocessor output (`-E`). Future versions may use `-MD` dependency files to hash individual headers.
6. Concatenate all of the above into a blake3 hasher in a defined order and finalize.

### 2.9 Compiler Manager

**Responsibility:** Spawn compiler subprocesses, capture output, manage concurrency.

**Key interfaces:**
```rust
struct CompilerManager;

impl CompilerManager {
    async fn run_compiler(
        &self,
        compiler: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<CompilerOutput>;
}

struct CompilerOutput {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_files: Vec<PathBuf>,
}
```

**Internal structure:**
- Uses `tokio::process::Command` to spawn compiler processes.
- Captures stdout and stderr.
- Multiple compilations run concurrently as independent tokio tasks. No artificial concurrency limit in the daemon; the OS scheduler and build system manage parallelism.

---

## 3. Data Flow

### 3.1 Cache Hit Path

```
User invokes:  zccache-cc -c foo.c -o foo.o

1. CLI parses argv. Determines: compiler=cc, source=foo.c, output=foo.o.
   This is a single-source compilation — cacheable.

2. CLI calls DaemonConnector::connect().
   a. Compute socket path: $XDG_RUNTIME_DIR/zccache/sock (Unix)
      or \\.\pipe\zccache-{username} (Windows).
   b. Attempt connect. If refused or socket missing:
      - Check lock file. If lock file exists and process alive, retry briefly.
      - Otherwise, clean stale socket/lock, fork/spawn daemon, wait for
        socket to appear, connect.

3. CLI sends Request::Compile { compiler, args, cwd, env } over IPC.

4. Daemon IPC server receives request, spawns a tokio task.

5. Daemon re-parses args server-side to extract canonical info.
   Resolves compiler path to absolute.

6. Daemon computes compiler identity hash:
   a. Check metadata cache for compiler binary. If High confidence and
      content_hash is Some, use it.
   b. Otherwise, stat the compiler binary, update metadata entry, hash
      the file, store in metadata cache at High confidence.

7. Daemon computes source content hash:
   a. Check metadata cache for foo.c. Suppose confidence is Medium
      (watcher says unchanged).
   b. Medium is not High — stat the file. Compare (mtime, size, file_id)
      with cached entry.
   c. Match: promote to High confidence, use cached content_hash if present.
      No match: re-hash file, update entry at High.

8. Daemon computes dependency hash:
   (MVP: run preprocessor to get dependency content hash. Future: use
   cached per-header hashes.)

9. Daemon computes cache key = blake3(compiler_id, sorted_args,
   sorted_env, source_hash, dep_hash).

10. Daemon queries ArtifactStore::lookup(key).
    a. redb index lookup by key — found, returns artifact directory path
       and metadata.
    b. Verify artifact directory exists and manifest is intact.
    c. Update last-access-time in redb index.

11. Daemon copies cached output files to the requested output paths
    (e.g., copies cached object file to foo.o).

12. Daemon sends Response::CacheHit { exit_code: 0, stdout, stderr }
    over IPC.

13. CLI receives response. Writes stdout/stderr to its own stdout/stderr.
    Exits with the cached exit code.
```

### 3.2 Cache Miss Path

```
Steps 1–9: identical to cache hit path.

10. Daemon queries ArtifactStore::lookup(key) — not found.

11. Daemon calls CompilerManager::run_compiler(compiler, args, cwd, env).
    a. Spawns the real compiler as a child process via tokio::process::Command.
    b. Waits for completion, captures stdout, stderr, exit code.

12. If exit code != 0, daemon sends Response::CacheMiss { exit_code,
    stdout, stderr }. Does NOT cache failed compilations. Done.

13. If exit code == 0, daemon stores the artifact:
    a. Create temp directory under {cache_root}/tmp/{random}.
    b. Copy output files into temp dir.
    c. Write manifest.json into temp dir.
    d. Compute artifact content hash (the cache key).
    e. Rename temp dir to {cache_root}/artifacts/{hash[0..2]}/{hash[2..4]}/{hash}.
       Atomic on same filesystem.
    f. Insert entry into redb index with current timestamp as last-access-time.
    g. If total cache size exceeds max, trigger async eviction.

14. Daemon sends Response::CacheMiss { exit_code: 0, stdout, stderr }.

15. CLI receives response. Output files already exist on disk (the real
    compiler wrote them). CLI writes stdout/stderr, exits with exit code.
```

### 3.3 Non-Cacheable Invocation Passthrough

```
User invokes:  zccache-cc foo.c bar.c -o program   (linking, multiple sources)

1. CLI parses argv. Determines this is a link invocation or multi-source
   compilation — not cacheable.

2. CLI does NOT contact the daemon.

3. CLI execs the underlying compiler directly:
   a. Determine real compiler path (from PATH, skipping zccache wrappers).
   b. execvp(compiler, original_args).

4. CLI process is replaced by the compiler. Exit code propagates to the
   build system.
```

Non-cacheable patterns detected by the CLI:
- No `-c` flag (linking invocation).
- Multiple source files.
- `-E` / `-M` / `-MM` (preprocessing / dependency generation only).
- `-` as input (stdin source).
- Unrecognized compiler.

---

## 4. IPC Model

### 4.1 Transport Abstraction

The `Transport` trait (section 2.3) abstracts over Unix domain sockets and Windows named pipes. The daemon and CLI code are written against the trait; platform selection happens at build time via conditional compilation:

```rust
#[cfg(unix)]
type PlatformTransport = UnixTransport;

#[cfg(windows)]
type PlatformTransport = NamedPipeTransport;
```

### 4.2 Socket Discovery

**Unix (Linux / macOS):**
- Socket path: `$XDG_RUNTIME_DIR/zccache/sock`
- Fallback if `$XDG_RUNTIME_DIR` is unset: `/tmp/zccache-{uid}/sock`
- Lock file: adjacent to socket as `lock`
- Directory created with mode 0700.

**Windows:**
- Named pipe: `\\.\pipe\zccache-{username}`
- Lock file: `%LOCALAPPDATA%\zccache\lock`
- Username obtained via `GetUserNameW`.

### 4.3 Connection Lifecycle

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

### 4.4 Error Handling

- If the daemon crashes mid-request, the CLI receives a broken-pipe error. The CLI falls back to running the compiler directly (non-cached) and prints a warning to stderr.
- If serialization/deserialization fails, the daemon sends `Response::Error` if possible, otherwise drops the connection. The CLI falls back.
- Timeouts: the CLI imposes a 60-second timeout on the full IPC round-trip. On timeout, it kills the request and falls back to direct compilation.

---

## 5. File Metadata Cache

### 5.1 Data Model

```rust
struct MetadataCache {
    entries: DashMap<CanonicalPath, MetadataEntry>,
}

struct CanonicalPath(PathBuf);  // always absolute, canonical

struct MetadataEntry {
    mtime: SystemTime,
    size: u64,
    file_id: Option<FileId>,   // inode on Unix, file index on Windows
    content_hash: Option<Blake3Hash>,
    confidence: Confidence,
    last_verified: Instant,    // monotonic clock
}

enum FileId {
    Unix { dev: u64, ino: u64 },
    Windows { volume_serial: u32, file_index: u64 },
}

enum Confidence {
    High,    // stat-verified or digest-verified within current session
    Medium,  // watcher reports no changes since last High
    Low,     // unverified: initial state, after watcher overflow, or stale
}
```

### 5.2 Lookup and Update Flow

When the daemon needs a file's content hash:

1. **Look up** `CanonicalPath` in `DashMap`.
2. **If not found:** stat the file, hash it, insert entry at `High` confidence. Register the file's parent directory with the file watcher. Return hash.
3. **If found at High confidence:** `last_verified` is recent (within current compilation batch). Return cached `content_hash` if present, else hash and update.
4. **If found at Medium confidence:** Watcher says unchanged, but we must verify. Stat the file. Compare `(mtime, size, file_id)` with entry:
   - **Match:** Promote to `High`. Return cached `content_hash`. (No need to re-hash; the metadata match plus watcher-no-event gives high assurance.)
   - **Mismatch:** File changed. Re-hash, update entry at `High`.
5. **If found at Low confidence:** Stat the file. Compare metadata:
   - **Match:** Upgrade to `High` but still re-hash the file (we have low trust). Update entry.
   - **Mismatch:** Re-hash, update entry at `High`.

### 5.3 Invalidation Strategy

Three layers, from fast to slow:

1. **File watcher** (fast, async): watcher event on a path → set entry to `Medium` (not `High`, because watcher events can coalesce or be delayed — we must stat to verify). If the event is a `Remove` or `Rename`, set to `Low`.
2. **Stat verification** (synchronous, per-lookup): compare `(mtime, size, file_id)`. Cheap kernel call. Catches changes the watcher might have missed or coalesced.
3. **Content hash** (expensive, on-demand): only re-computed when stat metadata changes or confidence is `Low`.

### 5.4 Race Condition Analysis

**Race: file changes between stat and read-for-hashing.**
- After hashing, stat the file again. If `(mtime, size)` changed between the pre-hash stat and the post-hash stat, discard the hash and retry (up to 3 times). If still unstable, mark the file as uncacheable for this compilation.

**Race: watcher event arrives between stat-verify and confidence promotion.**
- The `DashMap` entry is updated via `entry.and_modify()`. The watcher also calls `entry.and_modify()`. Because DashMap operations on the same key are serialized (per-shard lock), exactly one wins. If the watcher downgrades confidence after we promoted it, the next lookup will re-verify — safe, just slightly wasteful.

**Race: file replaced by a different file with same mtime/size.**
- The `file_id` field (inode/file-index) detects file replacement. If `file_id` changes, the entry is treated as a cache miss regardless of `mtime`/`size` match. On platforms where `file_id` is unavailable (rare), we accept this as a known limitation; `mtime` granularity is the sole guard.

### 5.5 Sharding Approach

`DashMap` internally shards by key hash. Default shard count is `num_cpus * 4`. No additional manual sharding is needed. The per-shard lock scope is small (hash-map bucket operations), so contention is low even under high concurrency.

---

## 6. File Watcher Integration

### 6.1 Watcher Setup

- Uses the `notify` crate with `RecommendedWatcher` (selects inotify on Linux, FSEvents on macOS, ReadDirectoryChangesW on Windows).
- The watcher runs on a **dedicated OS thread** (not a tokio task) to avoid blocking the async runtime with synchronous `notify` internals.
- Events are sent from the watcher thread to the daemon's tokio runtime via a `tokio::sync::mpsc::channel`.
- Fallback: if the native watcher fails to initialize (e.g., inotify watch limit exhausted), fall back to `notify::PollWatcher` with a 2-second interval.

### 6.2 Event Processing

A dedicated tokio task receives events from the channel and processes them:

```rust
async fn process_watcher_events(
    mut rx: mpsc::Receiver<notify::Event>,
    cache: Arc<MetadataCache>,
) {
    while let Some(event) = rx.recv().await {
        match event.kind {
            EventKind::Modify(_) | EventKind::Create(_) => {
                for path in &event.paths {
                    cache.set_confidence(path, Confidence::Medium);
                }
            }
            EventKind::Remove(_) => {
                for path in &event.paths {
                    cache.set_confidence(path, Confidence::Low);
                }
            }
            _ => {}
        }
    }
}
```

**Why Medium, not High, on modify events:** Watcher events may be batched, delayed, or refer to intermediate states. Only a stat call against the actual filesystem provides High confidence. Medium means "something happened, check before trusting."

### 6.3 Overflow Handling

When the OS event queue overflows (inotify queue full, FSEvents `kFSEventStreamEventFlagMustScanSubDirs`), `notify` delivers a `Rescan` or error event. On overflow:

1. **Downgrade all entries** in the metadata cache to `Low` confidence.
2. **Log a warning** indicating watcher overflow.
3. **Re-register watches** if the watcher was in a recoverable state.
4. Subsequent lookups will stat-verify every file — expensive but correct.

### 6.4 Scope Management

The watcher does not watch the entire filesystem. Watched directories:

- **Project root:** detected from the `cwd` of the first compilation request. Watched recursively.
- **Include directories:** discovered from `-I` flags in compilation requests. Watched non-recursively (only the specified directory, not subdirectories, unless the same directory is a project subdirectory — in which case the project root's recursive watch covers it).
- **System include directories** (e.g., `/usr/include`): NOT watched. These change rarely and watching them would be expensive and noisy. Files in unwatched directories always start at `Low` confidence and are stat-verified on every lookup.

Watch registrations are accumulated over the daemon's lifetime. Directories are never unwatched (the cost of maintaining a watch is negligible compared to the risk of missing an event).

---

## 7. Disk Artifact Cache

### 7.1 Directory Layout

```
{cache_root}/
  artifacts/
    ab/                          # first 2 hex chars of hash
      cd/                        # next 2 hex chars of hash
        abcdef0123456789.../     # full hash (64 hex chars)
          manifest.json
          output.o               # cached output file(s)
          stdout                 # captured stdout (may be empty)
          stderr                 # captured stderr (may be empty)
  tmp/
    {random}/                    # in-progress writes
  index.redb                     # redb database
```

**cache_root** defaults:
- Linux: `$XDG_CACHE_HOME/zccache` or `~/.cache/zccache`
- macOS: `~/Library/Caches/zccache`
- Windows: `%LOCALAPPDATA%\zccache`

### 7.2 Content Addressing

The artifact directory name is the full blake3 hash (64 hex characters) of the cache key. The two-level prefix directory structure (`ab/cd/`) limits the number of entries per directory, avoiding filesystem performance degradation on large caches.

### 7.3 Atomic Writes

To prevent partially-written artifacts from being read:

1. Create a temporary directory under `{cache_root}/tmp/{uuid}`.
2. Write all output files and the manifest into the temp directory.
3. `fsync` the temp directory (and files, on Linux, where `fsync` semantics require it).
4. Rename the temp directory to its final path under `artifacts/`. On POSIX, `rename()` is atomic within the same filesystem. On Windows, `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` provides equivalent semantics for directories.
5. Insert into the redb index within a write transaction.

If the daemon crashes between steps 2 and 4, the temp directory is orphaned. On startup, the daemon deletes all entries under `{cache_root}/tmp/`.

### 7.4 Manifest Format

```json
{
  "version": 1,
  "cache_key": "abcdef0123456789...",
  "compiler": "/usr/bin/gcc",
  "compiler_hash": "...",
  "args_hash": "...",
  "source": "foo.c",
  "source_hash": "...",
  "dep_hash": "...",
  "output_files": [
    { "name": "output.o", "size": 12345, "blake3": "..." }
  ],
  "created_at": "2026-03-08T12:00:00Z",
  "exit_code": 0
}
```

The manifest exists primarily for debugging and corruption detection. The cache key is the directory name; the manifest records what went into it.

### 7.5 redb Index Schema

The redb database contains two tables:

**Table `artifacts`:**
```
Key:   [u8; 32]        // blake3 hash bytes
Value: ArtifactMeta    // bincode-serialized
```

```rust
struct ArtifactMeta {
    total_size: u64,            // sum of all files in artifact dir
    last_access: SystemTime,    // updated on each lookup
    created_at: SystemTime,
    output_file_count: u32,
}
```

**Table `stats`:**
```
Key:   &str             // stat name
Value: u64              // counter
```

Tracks: `hits`, `misses`, `evictions`, `total_bytes_written`, `total_bytes_evicted`.

redb is chosen for its properties:
- Pure Rust, no external dependencies.
- ACID transactions — the index survives crashes without corruption.
- Single-file database.
- Good read concurrency (multiple concurrent readers, single writer).

### 7.6 Eviction Policy

**Policy:** LRU by `last_access` time.

**Trigger:** After each artifact store operation, check total cache size. If it exceeds `max_size` (configurable, default 10 GB):

1. Open a redb read transaction.
2. Iterate all entries, sorted by `last_access` ascending.
3. Collect entries to evict until projected size is at most `max_size * 0.9` (evict to 90% to avoid thrashing).
4. Open a redb write transaction.
5. For each entry to evict:
   a. Delete the artifact directory from disk.
   b. Remove the entry from the redb `artifacts` table.
   c. Increment the `evictions` counter.
6. Commit the transaction.

Eviction runs as a background tokio task and does not block lookups or stores.

### 7.7 Corruption Detection

On artifact lookup:
1. Verify the artifact directory exists.
2. Verify `manifest.json` exists and is parseable.
3. Verify each output file listed in the manifest exists and its size matches.
4. (Optional, not default) Verify blake3 hashes of output files match manifest.

If any check fails, remove the artifact directory and its redb entry, and treat as a cache miss. Log a warning.

On startup, the daemon does NOT do a full integrity scan (too slow for large caches). Corruption is detected lazily on lookup.

---

## 8. Concurrency Model

### 8.1 Task Topology

```
Main thread:       daemon startup, signal handling
Tokio runtime:     multi-threaded (default thread count)
  Task per IPC connection:
    - reads request
    - computes cache key (may stat/hash files)
    - looks up artifact store
    - on miss: spawns compiler, stores result
    - sends response
  Background eviction task (if triggered)
  Watcher event processing task

Dedicated OS thread:  file watcher (notify)
```

### 8.2 Synchronization Points

| Resource | Mechanism | Contention |
|---|---|---|
| Metadata cache | DashMap (sharded concurrent map) | Low — per-shard locks, short critical sections |
| Artifact store on disk | Atomic rename, no locks | None — each artifact has unique path |
| redb index | redb internal MVCC (readers never block, writer serialized) | Low — write transactions are short |
| File watcher event channel | tokio mpsc (bounded, 4096) | Low — single producer, single consumer |

### 8.3 Lock Ordering

There is no nested locking. The design avoids situations where one lock is held while acquiring another:
- DashMap lookups are point operations. The shard lock is released before any I/O.
- redb transactions do not hold DashMap locks.
- The watcher thread never acquires DashMap locks directly; it sends events through a channel.

This eliminates deadlock by design.

---

## 9. Correctness Model

### 9.1 Layered Invalidation

zccache uses a layered approach where each layer is progressively more expensive but more authoritative:

```
Layer 0: File Watcher (free, async, best-effort)
    |
    v
Layer 1: Metadata Cache lookup (in-memory, O(1))
    |
    v
Layer 2: Stat Verification (syscall, ~1us)
    |
    v
Layer 3: Content Hash (read + hash, ~1ms per file)
```

The watcher provides early warning. The metadata cache avoids redundant stats. Stat verification catches changes the watcher missed. Content hashing is the ground truth but is only invoked when cheaper layers indicate a possible change.

### 9.2 Conservative Bias

When in doubt, zccache assumes the file has changed and re-verifies. Specific policies:

- **No cached hash at any confidence level:** always hash.
- **Watcher overflow:** downgrade everything to Low, stat-verify all.
- **stat race detected (mtime changed during hashing):** retry, then treat as uncacheable.
- **Unknown file ID:** fall back to path + mtime + size (less reliable, but safe because mtime changes on write in all supported filesystems).
- **Compiler binary changed:** re-hash compiler identity on every daemon start and whenever its metadata cache entry is not High.

### 9.3 Failure Modes and Mitigations

| Failure | Impact | Mitigation |
|---|---|---|
| Watcher misses an event | Stale metadata at Medium | Stat verification on every cache key computation |
| Watcher overflows | Many stale entries | Downgrade all to Low; stat-verify everything |
| File replaced with same mtime/size | Incorrect cache hit | file_id (inode) detection; extremely rare in practice |
| Compiler updated in-place | Incorrect cache hit | Compiler binary is in metadata cache; stat-verified on use |
| Clock skew / mtime unreliable | Incorrect cache hit | file_id provides second signal; Low confidence triggers re-hash |
| Disk full during artifact write | Orphaned temp dir | Temp dir cleaned on startup; write failure returns error, CLI falls back |
| redb corruption | Index lost | redb is ACID; if corruption occurs (hardware fault), rebuild index by scanning artifact directories |

### 9.4 What zccache Does NOT Cache

- Failed compilations (non-zero exit code).
- Compilations reading from stdin.
- Compilations involving response files that cannot be fully resolved.
- Compilations where the preprocessor output is non-deterministic (detected heuristically: `__TIME__`, `__DATE__` in source — future enhancement).

---

## 10. Crash Recovery

### 10.1 Daemon Crash Recovery

**Stale socket:** The CLI detects a stale socket by attempting to connect. If the connection fails (connection refused or broken pipe), the CLI removes the socket file and lock file, then starts a fresh daemon.

**Lock file:** Contains the daemon PID. The CLI checks whether the PID is alive (`kill(pid, 0)` on Unix, `OpenProcess` on Windows). If the process is dead, the lock file is stale and is removed.

### 10.2 Metadata Cache Recovery

The in-memory metadata cache is **not persisted**. After a daemon restart, the cache is empty. Entries are rebuilt lazily: the first compilation after restart will stat and hash all referenced files, populating the cache. Subsequent compilations benefit from cached metadata.

This is a deliberate design choice. Persisting the metadata cache would add complexity (serialization, staleness on restart) for marginal benefit — the cache warms up within one full build.

### 10.3 Artifact Store Recovery

**Orphaned temp directories:** On startup, `{cache_root}/tmp/` is deleted recursively. This removes any incomplete artifact writes from a previous crash.

**Artifact directories:** Intact. Atomic rename ensures an artifact directory is either fully present or absent. If the daemon crashed after creating the temp dir but before renaming, the temp dir is cleaned up and the artifact is simply absent (cache miss; the compilation will re-run).

### 10.4 Index Recovery

**redb** provides ACID transactions. The database file is always in a consistent state, even after an unclean shutdown. If the daemon crashed mid-transaction, redb rolls back the incomplete transaction on next open.

**Index-artifact divergence:** If the daemon crashed after writing the artifact directory but before inserting the redb entry, the artifact exists on disk but is not in the index. This is a harmless orphan; it wastes disk space but does not cause incorrect behavior. A periodic (or on-demand) maintenance task can scan the artifact directories and reconcile with the index:
- Artifact on disk but not in index: add to index.
- Entry in index but no artifact on disk: remove from index.

---

## 11. Portability

### 11.1 Platform Differences

| Aspect | Linux | macOS | Windows |
|---|---|---|---|
| IPC | Unix domain socket | Unix domain socket | Named pipe |
| Socket path | `$XDG_RUNTIME_DIR/zccache/sock` | `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-{uid}/sock` | `\\.\pipe\zccache-{username}` |
| File watcher backend | inotify | FSEvents | ReadDirectoryChangesW |
| File ID | `st_dev` + `st_ino` | `st_dev` + `st_ino` | `dwVolumeSerialNumber` + `nFileIndex{High,Low}` |
| Atomic rename | `rename(2)` | `rename(2)` | `MoveFileExW` |
| Lock file PID check | `kill(pid, 0)` | `kill(pid, 0)` | `OpenProcess(SYNCHRONIZE, pid)` |
| Cache root | `~/.cache/zccache` | `~/Library/Caches/zccache` | `%LOCALAPPDATA%\zccache` |
| Daemon spawn | `fork` + `setsid` + `exec` | `fork` + `setsid` + `exec` | `CreateProcessW` (detached) |

### 11.2 Path Handling

**Canonicalization:** All paths stored in the metadata cache are canonicalized (`std::fs::canonicalize`). This resolves symlinks and relative components, ensuring that `/home/user/./foo.c` and `/home/user/foo.c` map to the same entry.

**Case sensitivity:**
- Linux: case-sensitive. No special handling.
- macOS: case-insensitive by default (HFS+/APFS). Canonicalization via `realpath` returns the filesystem's canonical casing. The metadata cache key uses the canonicalized form, which is consistent regardless of the case the user provided.
- Windows: case-insensitive. Paths are canonicalized and stored in the case returned by `GetFinalPathNameByHandleW` (via Rust's `std::fs::canonicalize`).

**UNC paths (Windows):** `std::fs::canonicalize` on Windows returns UNC-prefixed paths (`\\?\C:\...`). These are stored as-is in the metadata cache. The artifact store uses only the cache root (a local path), so UNC paths do not appear in artifact paths.

**Path separators:** Internally, all paths use the platform's native separator. Cache keys hash the **canonicalized path bytes**, so the same file always produces the same hash on a given platform. Cross-platform cache sharing is not a goal.

### 11.3 File Identity

`FileId` is obtained via:
- **Unix:** `std::fs::metadata()` → `std::os::unix::fs::MetadataExt` → `dev()`, `ino()`.
- **Windows:** Open file with `CreateFileW(OPEN_EXISTING, FILE_READ_ATTRIBUTES)`, call `GetFileInformationByHandle`, extract `dwVolumeSerialNumber` and `nFileIndexHigh`/`nFileIndexLow`.

If obtaining the file ID fails (e.g., permission denied, network filesystem that doesn't support it), `file_id` is set to `None` and the entry falls back to `(path, mtime, size)` identity only.

### 11.4 Watcher Behavior Differences

- **inotify (Linux):** Per-directory watches. Recursive watching requires registering each subdirectory. The `notify` crate handles this. Watch limit: `/proc/sys/fs/inotify/max_user_watches` (default 8192 or 65536 depending on distro). If exhausted, fall back to polling.
- **FSEvents (macOS):** Stream-based, naturally recursive. Low overhead. May deliver events with a slight delay (latency configurable, set to 100ms). Delivers `MustScanSubDirs` on overflow.
- **ReadDirectoryChangesW (Windows):** Per-directory, can be recursive. Buffer overflow possible under heavy I/O; `notify` reports this as an error.

---

## 12. Future Extension Points

### 12.1 Remote / Shared Cache

The artifact store interface can be extended with a `RemoteStore` backend:

```rust
#[async_trait]
trait ArtifactBackend {
    async fn lookup(&self, key: &Blake3Hash) -> Option<Artifact>;
    async fn store(&self, key: &Blake3Hash, artifact: Artifact) -> Result<()>;
}
```

A `ChainedStore` would check local first, then remote. Remote candidates: S3-compatible object storage, HTTP server, or a custom protocol. The content-addressed design makes this natural — the cache key is the same regardless of where the artifact is stored.

### 12.2 Distributed Build Cache

Multiple machines on a team could share a remote artifact store. Requirements:
- Compiler identity must include target triple and relevant system header hashes.
- Environment normalization must be stricter (filter more variables).
- Artifact format must be verified more carefully (hash verification on download).

### 12.3 Additional Compilers

The compiler argument parser is pluggable. Each compiler family (GCC, Clang, MSVC) has its own arg parser implementing a common trait:

```rust
trait CompilerArgParser {
    fn parse(&self, args: &[String]) -> Result<ParsedCompilation>;
    fn is_cacheable(&self, parsed: &ParsedCompilation) -> bool;
    fn cache_relevant_args(&self, parsed: &ParsedCompilation) -> Vec<String>;
    fn cache_relevant_env(&self, parsed: &ParsedCompilation) -> Vec<(String, String)>;
}
```

Adding a new compiler (e.g., MSVC `cl.exe`, `nvcc`) requires implementing this trait.

### 12.4 Preprocessor Integration

The MVP hashes preprocessor output as the dependency hash. This is correct but slow (runs the preprocessor on every compilation). Future improvements:

1. **Dependency file parsing:** After a cache miss, parse the `-MD`-generated `.d` file to discover the exact set of headers used. Cache this set. On subsequent compilations with the same source, hash only the individual headers instead of running the preprocessor.
2. **Include scanning:** Parse `#include` directives without running the preprocessor. Faster but less accurate (misses conditional includes).
3. **Persistent dependency graph:** Store the source-to-headers mapping in redb. Invalidate edges when headers change.

### 12.5 Persistent Metadata Cache

The in-memory metadata cache could be serialized to disk on shutdown and loaded on startup, avoiding the cold-start cost of stat-verifying all files. Implementation:
- Serialize to a file in the cache root on graceful shutdown.
- On startup, load the file, but set all entries to `Low` confidence (we don't know what changed while the daemon was down).
- The watcher-based promotion to Medium and stat-based promotion to High proceed as normal.

This trades a small amount of startup I/O for faster warm-up on the first build after daemon restart.

### 12.6 Build System Integration

Direct integration with build systems (CMake, Meson, Bazel) could provide richer information:
- Exact dependency lists without preprocessing.
- Compiler version and target triple from build system configuration.
- Output path and intermediate file management.

This is a non-goal for the initial implementation but the daemon's IPC interface can be extended to accept richer requests.
