# Portability & Future Extensions

Cross-platform differences and planned extension points.

---

## Platform Differences

| Aspect | Linux | macOS | Windows |
|---|---|---|---|
| IPC | Unix domain socket | Unix domain socket | Named pipe |
| Socket path | `$XDG_RUNTIME_DIR/zccache/sock` | `$XDG_RUNTIME_DIR/zccache/sock` or `/tmp/zccache-{uid}/sock` | `\\.\pipe\zccache-{username}` |
| File watcher backend | inotify | FSEvents | ReadDirectoryChangesW |
| File ID | `st_dev` + `st_ino` | `st_dev` + `st_ino` | `dwVolumeSerialNumber` + `nFileIndex{High,Low}` |
| Atomic rename | `rename(2)` | `rename(2)` | `MoveFileExW` |
| Lock file PID check | `kill(pid, 0)` | `kill(pid, 0)` | `OpenProcess(SYNCHRONIZE, pid)` |
| Cache root | `~/.zccache` | `~/.zccache` | `~/.zccache` |
| Daemon spawn | `fork` + `setsid` + `exec` | `fork` + `setsid` + `exec` | `CreateProcessW` (detached) |

## Path Handling

**Canonicalization:** All paths stored in the metadata cache are canonicalized (`std::fs::canonicalize`). This resolves symlinks and relative components, ensuring that `/home/user/./foo.c` and `/home/user/foo.c` map to the same entry.

**Case sensitivity:**
- Linux: case-sensitive. No special handling.
- macOS: case-insensitive by default (HFS+/APFS). Canonicalization via `realpath` returns the filesystem's canonical casing. The metadata cache key uses the canonicalized form, which is consistent regardless of the case the user provided.
- Windows: case-insensitive. Paths are canonicalized and stored in the case returned by `GetFinalPathNameByHandleW` (via Rust's `std::fs::canonicalize`).

**UNC paths (Windows):** `std::fs::canonicalize` on Windows returns UNC-prefixed paths (`\\?\C:\...`). These are stored as-is in the metadata cache. The artifact store uses only the cache root (a local path), so UNC paths do not appear in artifact paths.

**Path separators:** Internally, all paths use the platform's native separator. Cache keys hash the **canonicalized path bytes**, so the same file always produces the same hash on a given platform. Cross-platform cache sharing is not a goal.

## File Identity

`FileId` is obtained via:
- **Unix:** `std::fs::metadata()` → `std::os::unix::fs::MetadataExt` → `dev()`, `ino()`.
- **Windows:** Open file with `CreateFileW(OPEN_EXISTING, FILE_READ_ATTRIBUTES)`, call `GetFileInformationByHandle`, extract `dwVolumeSerialNumber` and `nFileIndexHigh`/`nFileIndexLow`.

If obtaining the file ID fails (e.g., permission denied, network filesystem that doesn't support it), `file_id` is set to `None` and the entry falls back to `(path, mtime, size)` identity only.

## Watcher Behavior Differences

- **inotify (Linux):** Per-directory watches. Recursive watching requires registering each subdirectory. The `notify` crate handles this. Watch limit: `/proc/sys/fs/inotify/max_user_watches` (default 8192 or 65536 depending on distro). If exhausted, fall back to polling.
- **FSEvents (macOS):** Stream-based, naturally recursive. Low overhead. May deliver events with a slight delay (latency configurable, set to 100ms). Delivers `MustScanSubDirs` on overflow.
- **ReadDirectoryChangesW (Windows):** Per-directory, can be recursive. Buffer overflow possible under heavy I/O; `notify` reports this as an error.

---

## Future Extension Points

### Remote / Shared Cache

The artifact store interface can be extended with a `RemoteStore` backend:

```rust
#[async_trait]
trait ArtifactBackend {
    async fn lookup(&self, key: &Blake3Hash) -> Option<Artifact>;
    async fn store(&self, key: &Blake3Hash, artifact: Artifact) -> Result<()>;
}
```

A `ChainedStore` would check local first, then remote. Remote candidates: S3-compatible object storage, HTTP server, or a custom protocol. The content-addressed design makes this natural — the cache key is the same regardless of where the artifact is stored.

### Distributed Build Cache

Multiple machines on a team could share a remote artifact store. Requirements:
- Compiler identity must include target triple and relevant system header hashes.
- Environment normalization must be stricter (filter more variables).
- Artifact format must be verified more carefully (hash verification on download).

### Additional Compilers

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

### Preprocessor Integration

The MVP hashes preprocessor output as the dependency hash. This is correct but slow (runs the preprocessor on every compilation). Future improvements:

1. **Dependency file parsing:** After a cache miss, parse the `-MD`-generated `.d` file to discover the exact set of headers used. Cache this set. On subsequent compilations with the same source, hash only the individual headers instead of running the preprocessor.
2. **Include scanning:** Parse `#include` directives without running the preprocessor. Faster but less accurate (misses conditional includes).
3. **Persistent dependency graph:** Store the source-to-headers mapping in redb. Invalidate edges when headers change.

### Persistent Metadata Cache

The in-memory metadata cache could be serialized to disk on shutdown and loaded on startup, avoiding the cold-start cost of stat-verifying all files. Implementation:
- Serialize to a file in the cache root on graceful shutdown.
- On startup, load the file, but set all entries to `Low` confidence (we don't know what changed while the daemon was down).
- The watcher-based promotion to Medium and stat-based promotion to High proceed as normal.

This trades a small amount of startup I/O for faster warm-up on the first build after daemon restart.

### Build System Integration

Direct integration with build systems (CMake, Meson, Bazel) could provide richer information:
- Exact dependency lists without preprocessing.
- Compiler version and target triple from build system configuration.
- Output path and intermediate file management.

This is a non-goal for the initial implementation but the daemon's IPC interface can be extended to accept richer requests.
