# zccache Design Decisions

This document records the key architectural and technical decisions for **zccache**, a high-performance local compiler cache daemon written in Rust. Each decision follows an ADR (Architecture Decision Record) style: Context, Decision, Rationale, Alternatives Considered, and Consequences.

---

## DD-001: Why Rust

**Context:** zccache is a systems-level compiler cache. It must be fast (on the critical path of every compilation), correct (a wrong cache hit is catastrophic), and portable (Linux, macOS, Windows). The implementation language directly affects all three properties.

**Decision:** Implement zccache in Rust.

**Rationale:**
- Memory safety without a garbage collector. No GC pauses on the daemon's hot path.
- Strong type system catches entire classes of bugs at compile time (null safety, exhaustive matching, ownership).
- Excellent cross-compilation story via `rustup target add` and `cross`.
- Easy to produce fully static binaries with no runtime dependencies, simplifying distribution.
- Rich ecosystem for systems tools: `tokio`, `blake3`, `notify`, `redb`, `dashmap`, `clap`, etc.
- Cargo provides a modern, reproducible build and dependency management system.

**Alternatives Considered:**
| Language | Why not |
|----------|---------|
| C++ | Manual memory management, no standard package manager, undefined behavior risks. |
| Go | GC pauses under load, less control over memory layout, larger binaries with runtime. |
| C | Manual memory management, no modern type safety, slower development velocity. |

**Consequences:**
- Team members must know Rust. The learning curve is real but pays off in fewer production bugs.
- Compile times are longer than C or Go. Mitigated by workspace crate splitting and incremental compilation.
- The Rust ecosystem is younger than C/C++; some niche libraries may be less mature.

---

## DD-002: Why Daemonized Architecture

**Context:** Compiler caches sit on the critical path of every build invocation. A cold-start per invocation (process spawn, config parse, metadata cache load) adds latency that compounds across thousands of translation units in a large build.

**Decision:** zccache runs as a long-lived daemon process that maintains in-memory caches and serves requests over local IPC. The daemon is lazily started on first CLI use, requiring no system service configuration.

**Rationale:**
- Amortizes startup cost across all invocations in a build session.
- Keeps the metadata cache (file stats, content hashes) warm in memory.
- Enables file watching for proactive cache invalidation.
- Handles concurrent compilation requests naturally (a daemon is already running, ready to accept connections).
- Lazy startup means zero configuration: the CLI spawns the daemon if it is not already running.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| Per-invocation process | Metadata cache must be loaded from disk on every call. Too slow for large builds. |
| Shared memory region | Complex, platform-specific, hard to coordinate lifecycle and eviction. |

**Consequences:**
- The daemon must be robust: handle crashes gracefully, avoid leaking resources, support clean shutdown.
- Need an IPC mechanism between CLI and daemon.
- Must handle daemon lifecycle (startup, idle timeout, restart after crash).

---

## DD-003: Watcher-Assisted but Not Watcher-Dependent Invalidation

**Context:** File system watchers (`inotify`, `FSEvents`, `ReadDirectoryChangesW`) can miss events due to queue overflow, race conditions, or platform-specific quirks. Builds must never return stale artifacts because a watcher dropped an event.

**Decision:** File watchers update a *confidence level* on cached metadata entries. All cache lookups perform at least a stat-verify (mtime + size check) before returning a hit. Watchers accelerate the common case (high confidence means we can skip content hashing) but are never the sole source of truth for file state.

**Rationale:**
- **Correctness over performance.** A wrong cache hit is catastrophic (silent miscompilation). A redundant `stat()` call is cheap (~1 microsecond on modern file systems).
- Watchers are treated as an optimization hint: when the watcher reports no changes, confidence is high, and content re-hashing can be skipped. But the stat check is always the final arbiter.
- The daemon's ultra-fast path (`fast_hit_cache`) provides an additional layer: if the journal clock hasn't advanced since the last verified hit, all hash computation is skipped entirely. This is safe because the clock advances on every watcher event.
- This design degrades gracefully: if the watcher crashes or overflows, the system becomes slightly slower (more re-hashes) but never incorrect.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| Trust watcher fully (zero syscalls) | Incorrect under overflow, delayed events, or watcher crashes. Unacceptable for a compiler cache. |
| Ignore watcher entirely | Slower: must content-hash every file on every lookup with no fast-path optimization. |

**Consequences:**
- Every cache lookup on the slow path includes at least one `stat()` call per file. This is acceptable given stat latency (~1µs). The fast path skips content hashing when stat matches.
- Watcher code can be simpler because it does not need to guarantee delivery.
- The confidence-level design adds a small amount of complexity to the metadata cache.

---

## DD-004: Content Hash vs Mtime-Based Heuristics

**Context:** Cache invalidation requires detecting file changes. Two main approaches exist: checking stat metadata (mtime, size) and computing content hashes. Each has different cost and correctness profiles.

**Decision:** Layered approach:
1. The metadata cache stores `(mtime, size, file_id)` for each tracked file.
2. Content hashes (blake3) are computed when needed for cache key derivation.
3. If stat metadata is unchanged and watcher confidence is high, skip re-hashing and reuse the cached content hash.

**Rationale:**
- `stat()` is approximately 10x faster than reading and hashing file contents.
- Most source files do not change between consecutive builds. Stat metadata serves as a fast pre-filter that avoids unnecessary I/O.
- When stat metadata *has* changed, a content hash is computed to determine the actual cache key. This catches the edge case where a file is touched but not modified (mtime changes, content does not).
- The hash is the ground truth for the cache key; mtime is only an acceleration heuristic.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| Hash only | Correct but slow. Every lookup reads and hashes every input file. |
| Mtime only | Fast but incorrect. Misses same-mtime edits (e.g., `git checkout`). |

**Consequences:**
- Cache keys are content-based, so builds are reproducible regardless of timestamps.
- The metadata cache must track both stat metadata and the last-known content hash per file.
- A "touch without modify" correctly results in a cache hit (hash unchanged).

---

## DD-005: blake3 for Hashing

**Context:** zccache needs a hash function for content hashing (file contents) and cache key derivation (combining input hashes, compiler flags, environment). The hash must be fast, correct, and have negligible collision probability.

**Decision:** Use blake3 for all hashing.

**Rationale:**
- **Fast:** blake3 is faster than SHA-256 and faster than xxhash for large inputs on modern CPUs, thanks to a tree structure that enables SIMD parallelism and multi-threading.
- **Cryptographic strength:** 256-bit output with no known collision attacks. Collision risk is negligible for a compiler cache.
- **Streaming API:** Can hash data incrementally without buffering the entire input.
- **Pure Rust with optional asm:** No C dependencies. Optional assembly acceleration for x86, ARM.
- **`no_std` compatible:** Can be used in constrained contexts if needed.

**Alternatives Considered:**
| Hash | Why not |
|------|---------|
| SHA-256 | Slower than blake3 on all platforms. No advantage for this use case. |
| xxhash | Not collision-resistant. Fine for hash tables, risky for cache keys. |
| BLAKE2 | Slower than blake3. blake3 is its successor. |

**Consequences:**
- Adds a dependency on the `blake3` crate. This is well-maintained and widely used.
- Hash output is 32 bytes (256 bits), which is used as the basis for cache key paths.
- Changing the hash function later would invalidate the entire artifact cache (acceptable but worth noting).

**Performance note (profiled):** Content hashing accounts for only ~0.3% of cold cache miss time. The real cold-start bottlenecks are compiler execution (~49%) and include scanning (~47%). An mtime-only prototype was tested and showed no measurable improvement, confirming that blake3 hashing cost is negligible relative to the actual compilation and dependency discovery work.

---

## DD-006: Tokio for Async Runtime

**Context:** The daemon must handle concurrent IPC connections, file watching events, compiler subprocess execution, and cache management simultaneously. An async runtime is a natural fit for this I/O-heavy, concurrent workload.

**Decision:** Use tokio as the async runtime for the daemon.

**Rationale:**
- Most mature and widely used Rust async runtime.
- Rich ecosystem: `tokio::net` (sockets, pipes), `tokio::process` (child process management), `tokio::fs` (async file operations), `tokio::sync` (channels, semaphores).
- Well-tested in production at scale. Extensive documentation.
- The daemon workload is a natural fit for async: many concurrent connections, mostly waiting on I/O.

**Alternatives Considered:**
| Runtime | Why not |
|---------|---------|
| async-std | Smaller ecosystem, less adoption. |
| smol | Minimal runtime, less ecosystem support. |
| OS threads only | More complex for managing many concurrent IPC connections. Higher resource usage. |

**Consequences:**
- Only the daemon is fully async. The CLI uses a small tokio runtime solely for IPC communication.
- Library crates expose synchronous interfaces where possible to avoid forcing async on consumers.
- The file watcher runs on a dedicated OS thread (not a tokio task) and communicates with the async runtime via a tokio channel. This avoids blocking the async executor with synchronous watcher APIs.

---

## DD-007: IPC via Unix Domain Sockets / Named Pipes

**Context:** The CLI (or compiler wrapper) and the daemon run on the same machine. IPC must be fast, secure (no network exposure), and portable.

**Decision:** Use Unix domain sockets on Linux/macOS and named pipes on Windows. Abstract the transport behind a trait so platform-specific code is contained.

**Rationale:**
- Both mechanisms are local-only by design. No network exposure, no firewall issues.
- Both are faster than TCP loopback (no TCP/IP stack overhead).
- Both are well-supported by tokio (`tokio::net::UnixListener`, `tokio::net::windows::named_pipe`).
- The transport trait keeps the rest of the codebase platform-agnostic.

**Alternatives Considered:**
| Mechanism | Why not |
|-----------|---------|
| Local TCP | Exposes a port. Firewall interference. Slightly slower. |
| gRPC | Heavyweight for local IPC. Adds protobuf dependency. Overkill for request/response between two local processes. |
| Shared memory | Complex lifecycle management. Platform-specific. Hard to debug. |

**Consequences:**
- Platform-specific code is isolated behind the transport trait.
- The socket/pipe path must be discoverable by both CLI and daemon (e.g., a well-known path under `$XDG_RUNTIME_DIR` or equivalent).
- Need to handle stale socket files from crashed daemons.

---

## DD-008: redb for Artifact Index

**Context:** The artifact cache needs a persistent index that maps cache keys to artifact locations and tracks access times for LRU eviction. The index must survive daemon restarts and be corruption-resistant.

**Decision:** Use redb (Rust Embedded DataBase) for the artifact index.

**Rationale:**
- **Pure Rust.** No C dependencies, no `libsqlite3` to link. Easy to cross-compile.
- **ACID transactions.** Crash-safe writes. No partial updates.
- **Single-file database.** Simple deployment and backup.
- **Simple API.** Key-value with typed tables. Fits the access pattern (lookup by cache key, scan by access time).
- **Good performance.** B-tree based, suitable for the read-heavy workload of a compiler cache.

**Alternatives Considered:**
| Store | Why not |
|-------|---------|
| SQLite (rusqlite) | C dependency complicates cross-compilation. More powerful than needed. |
| sled | Pre-1.0, history of data loss bugs. |
| Plain files | No efficient range queries for eviction (e.g., "find oldest entries"). |
| Append-only log | Requires compaction. More complex for random-access lookups. |

**Consequences:**
- The redb file lives in the cache directory. If corrupted, it can be deleted and rebuilt by scanning the content-addressed artifact store.
- redb's API is simpler than SQL, which limits future query flexibility. Acceptable for the current access patterns.

---

## DD-009: Content-Addressed Artifact Storage

**Context:** Compilation outputs (object files, dependency info) must be stored on disk and retrieved by cache key.

**Decision:** Store artifacts in a content-addressed layout using the cache key hash. Use two-level directory sharding: first 2 hex characters, then next 2 hex characters. Each artifact is a directory containing the output files and a manifest.

Example: cache key `a1b2c3d4...` is stored at `<cache_root>/artifacts/a1/b2/a1b2c3d4.../`.

**Rationale:**
- **Deterministic layout.** Given a cache key, the artifact path is immediately computable. No index required for existence checks.
- **Natural deduplication.** Identical compilation outputs (same inputs, same flags) map to the same cache key and location.
- **Easy to inspect and debug.** A developer can navigate the cache directory and examine artifacts directly.
- **Two-level sharding** prevents any single directory from accumulating too many entries, which degrades file system performance on all platforms.

**Alternatives Considered:**
| Layout | Why not |
|--------|---------|
| Flat directory | Too many entries in one directory. Performance degrades beyond ~10K entries on some file systems. |
| Invocation-addressed | No deduplication. Same compilation stored multiple times. |
| Blob store | Harder to inspect. Requires unpacking to examine artifacts. |

**Consequences:**
- Directory sharding is fixed at two levels. This supports billions of entries without performance issues.
- Artifact directories may contain multiple files (e.g., `.o`, `.d`, manifest). The manifest records metadata (creation time, input hash, compiler version).
- Orphaned temp directories from interrupted writes can be cleaned up by a periodic maintenance task.

---

## DD-010: Atomic Artifact Writes

**Context:** The daemon may crash or be killed during artifact storage. Multiple concurrent compilations may produce the same artifact simultaneously.

**Decision:** Write artifacts to a temporary directory within the cache root, then atomically rename to the final content-addressed location. If the final location already exists, the rename is a no-op (another process already stored it).

**Rationale:**
- `rename()` is atomic on all target platforms (Linux, macOS, Windows). A crash during rename either completes the rename or leaves the temp directory in place (cleaned up later).
- No partial or corrupt artifacts can ever exist at a final path.
- Concurrent stores of the same artifact are safe: the first rename wins, subsequent renames see the existing directory and skip.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| Write in place | Crash leaves partial artifacts. Concurrent writes corrupt data. |
| File locking | Complex, platform-specific, risk of deadlock or stale locks. |

**Consequences:**
- Temp directories accumulate if the daemon crashes repeatedly. A startup cleanup task removes any temp directories older than a threshold.
- The cache root must be on a single file system (rename across file systems is not atomic).

---

## DD-011: File Identity via Path + File ID

**Context:** A source file at a given path may be *replaced* by a different file (e.g., `git checkout`, `cp`, editor save-by-replace). If the replacement has the same mtime (unlikely but possible), an mtime-only check would miss the change.

**Decision:** Track file identity as `(path, file_id)` where `file_id` is the inode number on Unix or `nFileIndex` (volume serial + file index) on Windows. Fall back to path-only identity on platforms or file systems where file ID is unavailable or unreliable (e.g., some network file systems).

**Rationale:**
- Catches file replacement even when mtime is preserved.
- Low overhead: the file ID is available from the same `stat()` call used for mtime and size.
- The fallback ensures the system works on all platforms, albeit with slightly weaker detection on exotic file systems.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| Path only | Misses file replacement with same mtime. |
| Content hash only | Correct but expensive (must read file on every check). |

**Consequences:**
- The metadata cache key includes the file ID. If a file is replaced (new inode), the old metadata entry is invalidated.
- On network file systems with synthetic inodes, file ID may not be meaningful. The fallback to path-only is acceptable because network file system use is uncommon for local compilation.

---

## DD-012: DashMap for Metadata Cache

**Context:** The in-memory metadata cache maps file paths to cached stat metadata and content hashes. Multiple tokio tasks (one per IPC connection) access it concurrently.

**Decision:** Use `DashMap` (a sharded concurrent hash map) for the metadata cache.

**Rationale:**
- Lock-free reads for non-colliding shards. Excellent concurrent read throughput for the common case (cache hit).
- Simple API: drop-in replacement for `HashMap` with interior mutability.
- Widely used and well-tested in the Rust ecosystem.
- Better than `RwLock<HashMap>` because readers on different shards do not contend with each other.
- Simpler than a custom lock-free data structure.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| `RwLock<HashMap>` | Write lock blocks all readers. Poor under concurrent load. |
| Custom lock-free map | High implementation complexity. Not justified for v1. |
| Per-task local caches | Stale data. Complex invalidation. |

**Consequences:**
- DashMap uses sharding internally (default: CPU count shards). Memory overhead is slightly higher than a plain HashMap.
- Iteration over DashMap acquires shard locks. Avoid full scans on the hot path.

---

## DD-013: LRU Eviction for Artifact Cache

**Context:** Without eviction, the disk cache grows without bound, eventually consuming all available disk space.

**Decision:** Evict artifacts by LRU (Least Recently Used) based on last-access time. Track access times in the redb index. A background eviction task runs when the cache size exceeds a configurable threshold (default: 10 GB).

**Rationale:**
- Simple and predictable. Easy to understand, easy to debug.
- Last-access time is a good heuristic for compiler caches: recently used artifacts correspond to the current working set and are likely needed again.
- A configurable threshold gives users control over disk usage.

**Alternatives Considered:**
| Strategy | Why not |
|----------|---------|
| LFU (Least Frequently Used) | More complex to implement. Marginal benefit over LRU for this workload. |
| TTL-based expiry | Poor fit. Artifacts do not have a natural expiration time. A rarely-changed library might be cached for weeks and still be valid. |
| Size-weighted LRU | Premature optimization. Adds complexity for marginal benefit in v1. |

**Consequences:**
- Every cache hit updates the access time in redb. This is a write on the read path, but redb writes are fast and batched.
- Eviction runs in the background and does not block cache lookups.
- Users can manually clear the cache or adjust the threshold via configuration.

---

## DD-014: Lazy Daemon Startup

**Context:** Requiring users to install and configure a system service (systemd unit, launchd plist, Windows service) creates friction and platform-specific complexity.

**Decision:** The CLI auto-starts the daemon if it is not already running. The daemon shuts down after a configurable idle timeout (default: 1 hour of no requests).

**Rationale:**
- Zero configuration for users. Run the compiler wrapper; the daemon starts automatically.
- No platform-specific service installation. Works the same on Linux, macOS, and Windows.
- Idle timeout ensures the daemon does not consume resources indefinitely when not in use.

**Alternatives Considered:**
| Approach | Why not |
|----------|---------|
| System service | Platform-specific. Requires root/admin. Friction for casual users. |
| Always-running daemon | Wastes resources when not building. |

**Consequences:**
- First compilation in a session has a small startup delay (~50-200ms for daemon launch).
- The CLI must detect whether the daemon is running (attempt connection, start if refused).
- A PID file or socket existence check is used to avoid spawning duplicate daemons.
- If the daemon crashes, the next CLI invocation restarts it automatically.

---

## DD-015: Conservative Correctness Bias

**Context:** A compiler cache has an asymmetric failure cost. A cache miss costs one extra compilation (seconds). A wrong cache hit returns incorrect object code, which can cause subtle bugs that take hours to diagnose and erode trust in the tool.

**Decision:** Always verify cache key inputs before returning a hit. When in doubt, treat the lookup as a miss. Never rely on a single source of truth for file state.

**Rationale:**
- The cost of a miss is bounded and obvious (one recompilation).
- The cost of a wrong hit is unbounded and silent (incorrect binary, debugging time, lost trust).
- Users who experience even one wrong hit will disable the cache entirely, negating all performance benefits.

**Consequences:**
- Some performance is left on the table. Redundant stat calls, re-hashing when confidence is low.
- The design philosophy is "fast enough and always correct" rather than "fastest possible."
- Every optimization that skips verification must be carefully analyzed for correctness impact.

---

## Technical Analysis: The 10 Questions

The following sections provide detailed analysis on specific technical questions that shaped the implementation.

### Q1: Metadata Cache — What to Cache and How

**What is stored:** For each tracked source file, the metadata cache holds:
- **Path** (canonical, absolute)
- **File ID** (inode on Unix, nFileIndex on Windows)
- **Stat metadata:** mtime (nanosecond precision where available), file size in bytes
- **Content digest:** blake3 hash of the file contents (computed lazily, cached opportunistically)
- **Watcher confidence:** a flag or level indicating whether the file watcher has reported changes since the last stat

**How it works:**
1. On first access, stat the file, store metadata, compute and cache the content hash.
2. On subsequent access, compare current stat metadata against cached values.
3. If stat metadata matches and watcher confidence is high, return the cached content hash without re-reading the file.
4. If stat metadata differs, re-read the file, recompute the hash, update the cache.
5. If watcher confidence is low (overflow, missed events), stat-verify but still use the cached hash if stat matches.

**Why this design:** Stat is cheap (~1 microsecond). Hashing is expensive (~100 microseconds for a typical source file). Caching the content hash and gating recomputation on stat changes gives near-optimal performance with high correctness.

---

### Q2: File Identity — Path + File ID

File identity is `(path, file_id)`:
- **Unix:** `file_id = (dev, ino)` from `stat()`.
- **Windows:** `file_id = (dwVolumeSerialNumber, nFileIndexHigh, nFileIndexLow)` from `GetFileInformationByHandle`.
- **Fallback:** If file ID is not available (e.g., some FUSE file systems), use path only and accept reduced detection of file replacement.

This catches the scenario where a file is deleted and a new file is created at the same path (different inode). Without file ID tracking, the metadata cache would incorrectly match on path alone and potentially return a stale hash if the mtime happened to match.

---

### Q3: Artifact Index — redb

The redb database contains two tables:

1. **`artifacts`** table: `cache_key (bytes) -> ArtifactRecord`
   - `ArtifactRecord`: path to artifact directory, creation timestamp, total size in bytes.

2. **`access_log`** table: `cache_key (bytes) -> last_access_timestamp`
   - Updated on every cache hit.
   - Scanned during eviction to find LRU entries.

redb provides ACID transactions, so a crash during an index update never leaves the database in an inconsistent state. If the redb file is corrupted beyond recovery, it can be deleted and rebuilt by scanning the content-addressed artifact directory.

---

### Q4: Async-First with Tokio

The daemon is built async-first on tokio:
- **IPC listener** runs as a tokio task, spawning a new task per connection.
- **Compiler execution** uses `tokio::process::Command` for non-blocking child process management.
- **Cache operations** are synchronous but fast (in-memory DashMap lookups, redb reads). They run on the tokio runtime without blocking issues because individual operations complete in microseconds.
- **Disk I/O** for artifact reads/writes uses `tokio::fs` or `spawn_blocking` for operations that may take longer.

The CLI is *not* fully async. It uses `tokio::runtime::Runtime::block_on` to make a single IPC request and wait for the response.

---

### Q5: IPC via Tokio, Watcher via Dedicated Thread

- **IPC:** Fully async via tokio. The daemon listens for connections, reads requests, processes them, and sends responses using async I/O.
- **File watcher:** Runs on a dedicated OS thread, not a tokio task. The `notify` crate's API is synchronous (callback-based). The watcher thread receives events from `notify` and forwards them to the async runtime via a `tokio::sync::mpsc` channel. A tokio task consumes from this channel and updates the metadata cache.

**Why a dedicated thread for the watcher:** The `notify` crate blocks on its internal event loop. Running it inside a tokio task would block the executor. A dedicated thread isolates this blocking behavior.

---

### Q6: Watcher — notify Crate with Polling Fallback

- **Primary:** `notify` crate using native backends (`inotify` on Linux, `FSEvents` on macOS, `ReadDirectoryChangesW` on Windows).
- **Fallback:** `notify`'s polling backend for platforms or file systems where native watchers are unavailable or unreliable (e.g., NFS, CIFS, some FUSE mounts).
- **Overflow handling:** When the OS event queue overflows, the watcher sets confidence to low for all tracked files. The next cache lookup stat-verifies everything. This is slower but correct.

The watcher watches directories containing tracked source files, not individual files. This reduces the number of watch descriptors and handles editor save patterns that create new files (e.g., save to `.tmp` then rename).

---

### Q7: Incorrect Cache Hit — Failure Modes and Mitigations

An incorrect cache hit means returning a cached artifact when the correct action is to recompile. The following scenarios are analyzed:

| # | Failure Mode | Cause | Mitigation |
|---|-------------|-------|------------|
| 1 | **File modified within mtime granularity** | Two writes in the same second (on file systems with 1s mtime resolution). Stat metadata is identical despite content change. | Content hash is the ground truth for cache keys. If stat matches but this is the first access, hash is computed. Subsequent accesses rely on watcher + stat. Risk is low and bounded to the first access after a rapid edit. |
| 2 | **Watcher event overflow** | OS event queue full. Changes are silently dropped. | Confidence drops to low on overflow. Next lookup stat-verifies all files. |
| 3 | **File replaced with same mtime** | `git checkout` or `cp --preserve` replaces a file with a different file but identical mtime. | File ID (inode) changes on replacement. Detected by the `(path, file_id)` identity check. |
| 4 | **Compiler upgrade not detected** | Compiler binary is updated but the cache does not notice. | The compiler binary path and version string are part of the cache key. The compiler binary itself is tracked in the metadata cache (stat + hash). |
| 5 | **Environment variable change** | A build-relevant environment variable (e.g., `CFLAGS`, `CPATH`) changes. | Selected environment variables are included in the cache key. The set of included variables is configurable. |
| 6 | **Header search path change** | Include paths change (e.g., new `-I` flag) without changing any file contents. | Compiler flags (including `-I` paths) are part of the cache key. Different flags produce different keys. |
| 7 | **Symlink target change** | A symlink in an include path is retargeted. The symlink mtime is unchanged; the target file is different. | Resolve symlinks to canonical paths before tracking. Stat the canonical path, not the symlink. |
| 8 | **Clock skew / mtime set to future** | System clock jumps, or a build tool sets mtime to an arbitrary value. | Content hash is the authority. Mtime is only a fast-path heuristic. If the hash has not been computed yet (first access), it is computed regardless of mtime. |
| 9 | **Race between stat and read** | File is modified after stat but before content hash is computed. | The cache key is derived from the content hash. If the file changes between stat and hash, the hash reflects the new content. On next access, stat detects the change and re-hashes. Worst case: one compilation uses a hash that is slightly newer than expected, which is correct (it just might not match a future lookup, resulting in a miss, not a wrong hit). |
| 10 | **Preprocessor non-determinism** | Macros like `__TIME__`, `__DATE__`, `__COUNTER__` produce different preprocessor output for identical inputs. | Detect and warn on non-deterministic macros. Optionally disable caching for translation units that use them. In v1, this is documented as a known limitation. |

**Design principle:** Every mitigation errs on the side of a cache miss rather than a wrong hit.

---

### Q8: Crash Recovery Strategy

The daemon can crash at any point. The recovery strategy ensures no data corruption and minimal data loss:

1. **Artifact store:** Content-addressed and written atomically (DD-010). A crash during a write leaves a temp directory that is cleaned up on next startup. No corrupt artifacts at final paths.

2. **redb index:** ACID transactions. A crash during a write rolls back the incomplete transaction. The database is consistent on recovery. If the database file is irrecoverably corrupted, delete it and rebuild from the artifact directory.

3. **Metadata cache (in-memory):** Lost on crash. Rebuilt lazily on next access. This is by design: the metadata cache is a performance optimization, not a source of truth.

4. **Daemon socket/pipe:** Stale socket files are detected on startup (attempt to connect; if refused, remove and re-create). PID files, if used, include the PID and are validated against the process table.

5. **In-flight compilations:** If the daemon crashes during a compilation, the compiler wrapper (CLI) detects the broken IPC connection, falls back to running the compiler directly (without caching), and reports a warning.

**Startup sequence after crash:**
1. Clean up stale temp directories in the artifact store.
2. Open (or rebuild) the redb index.
3. Remove stale socket/pipe files.
4. Start listening for connections.

---

### Q9: LRU Eviction — Details

**Trigger:** A background task periodically checks total cache size (sum of artifact sizes tracked in redb). When the size exceeds the configured threshold (default: 10 GB), eviction begins.

**Process:**
1. Query the `access_log` table in redb, sorted by `last_access_timestamp` ascending.
2. Delete the oldest artifacts (remove directory from disk, remove entries from redb) until cache size drops below `threshold * 0.9` (the 90% low-water mark prevents eviction from triggering again immediately).
3. Eviction runs in a background tokio task with lower priority. It yields between deletions to avoid starving IPC handlers.

**Access time tracking:** Every cache hit writes the current timestamp to the `access_log` table. These writes are batched (e.g., flushed every 5 seconds or every 100 hits) to avoid write amplification.

**Manual controls:**
- `zccache clear` removes all artifacts and resets the index.
- `zccache config set max_cache_size <bytes>` adjusts the threshold.

---

### Q10: What Stays Simple in v1

v1 is deliberately minimal. The goal is a correct, useful tool for the most common case, not a feature-complete solution.

**In scope for v1:**
- Single C compiler support (GCC or Clang). The compiler is invoked via a wrapper (`zccache gcc ...`).
- Caching of compilation (`.c` to `.o`). Not linking, not preprocessing-only.
- Cache key derived from: preprocessed source hash, compiler binary hash, compiler flags, selected environment variables.
- Header dependency tracking via the preprocessor (`-M` / `-MD` flags). No custom header parsing or dep scanning.
- Local artifact cache only. No remote/shared cache.
- No compression of cached artifacts. Disk is cheap; compression adds CPU cost and complexity.
- Simple configuration: cache directory, max cache size, idle timeout, log level. TOML file or environment variables.
- Statistics: hit rate, miss rate, cache size. Available via `zccache stats`.

**Explicitly deferred to later versions:**
- C++ support (more complex flag handling, modules).
- Remote/distributed cache (S3, GCS, HTTP).
- Compression of cached artifacts (zstd).
- Sophisticated header dependency tracking (include graph analysis).
- Multi-compiler support in a single daemon instance.
- IDE integration.
- Cache sharing between users on the same machine.

**Rationale for simplicity:** Every feature adds surface area for bugs. v1 must be *correct* above all else. Features are added incrementally, each validated for correctness before merging.

---

## DD-016: Single-Roundtrip Ephemeral Compile

**Context:** In drop-in wrapper mode (`zccache clang++ -c foo.cpp -o foo.o`), each invocation created an ephemeral session using 3 IPC roundtrips: SessionStart → Compile → SessionEnd. With ~170ms per-invocation overhead, this dominated compilation time for small files and caused zccache-warm to be 1.79x slower than bare clang in ninja-based builds.

**Decision:** Add a `Request::CompileEphemeral` protocol message that combines session creation, compilation, and session teardown into a single IPC roundtrip. The daemon handles all three steps internally.

**Rationale:**
- Eliminates 2 of 3 IPC roundtrips, saving ~10-20ms per invocation.
- Most zccache invocations from build systems (ninja, make) are ephemeral — they don't use long-lived sessions.
- Backward-compatible: session mode (`ZCCACHE_SESSION_ID`) still uses the 3-message flow for build system integrations that manage sessions explicitly.

**Alternatives Considered:**
| Alternative | Why not |
|-------------|---------|
| Persistent session per build dir | Requires build system cooperation; not transparent drop-in. |
| Pipelining 3 messages on one connection | Still 3 serialization/deserialization cycles server-side. |
| UDP for IPC | Unreliable; can't guarantee delivery. Named pipes don't support datagram mode on Windows. |

**Consequences:**
- Protocol grows by one variant (`CompileEphemeral`). Old daemons reject it with `Error`; the CLI could fall back to 3-message flow (not implemented — upgrade both).
- Drop-in mode is now as fast as session mode from an IPC perspective.

---

## DD-017: Persistent Artifact Storage

**Context:** Artifacts were stored in `$TMPDIR/zccache-artifacts-{pid}`, lost on daemon restart. After a reboot or daemon crash, the entire cache was cold.

**Decision:** Store artifacts in `~/.cache/zccache/artifacts/` (Linux/macOS) or `%LOCALAPPDATA%\zccache\artifacts\` (Windows). Write `.meta` sidecar files (bincode-serialized `ArtifactData`) alongside output files. On startup, scan for `.meta` files and rebuild the in-memory artifact map.

**Rationale:**
- Eliminates cold-start penalty after daemon restarts.
- The cache directory is the same one used for the redb index and other persistent state.
- `.meta` sidecars are simple and atomic (write-then-rename pattern).

**Alternatives Considered:**
| Alternative | Why not |
|-------------|---------|
| SQLite/redb for artifact data | Over-engineered; artifacts are already on disk as files. |
| Memory-mapped file | Complex, platform-specific, fragile on crash. |
| No persistence | Status quo; cold-start penalty was measurable in benchmarks. |

**Consequences:**
- Disk usage is slightly higher (~1x metadata overhead per artifact).
- `zccache clear` must delete `.meta` files alongside output files (already handled).
- Dep graph is NOT persisted — cold compiles still re-discover include dependencies. Only the artifact data survives restarts.
