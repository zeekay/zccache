# Crates Architecture

## Dependency Graph

```
zccache-daemon (bin) ─────────────────────────────────────────┐
  ├─ zccache-ipc ─── zccache-protocol ─── zccache-core       │
  ├─ zccache-fscache ─── zccache-core                        │
  ├─ zccache-artifact ─── zccache-hash ─── zccache-core      │
  ├─ zccache-watcher ─── zccache-fscache                     │
  └─ zccache-compiler ─── zccache-hash                       │
                                                              │
zccache-cli (bin: "zccache") ─────────────────────────────────┤
  ├─ zccache-ipc                                              │
  ├─ zccache-protocol                                         │
  └─ zccache-core                                             │
                                                              │
zccache-test-support (test utilities) ────────────────────────┘
```

## Crate Responsibilities

- **zccache-core** — Shared error types (`Error`/`Result`), `Config`, `NormalizedPath` for cross-platform path handling
- **zccache-hash** — `ContentHash` (blake3), `CacheKeyBuilder` with domain-separated deterministic hashing
- **zccache-protocol** — `Request`/`Response` enums, `ArtifactData`, length-prefixed bincode framing
- **zccache-ipc** — Platform IPC endpoint discovery (`default_endpoint()`: Unix sockets vs named pipes)
- **zccache-fscache** — `MetadataCache` (DashMap-backed) with `Confidence` levels and time-based decay
- **zccache-artifact** — Content-addressed disk store with 2-level hex sharding, redb index for LRU eviction
- **zccache-watcher** — `FileWatcher` trait over notify crate; dedicated OS thread, events via tokio channel
- **zccache-compiler** — `CompilerFamily` detection, `ParsedInvocation` for cacheability checks
- **zccache-daemon** — Tokio async runtime, IPC server, orchestrates all subsystems
- **zccache-cli** — Subcommands: start, stop, status, clear, wrap, inspect

## Key Design Patterns

**Correctness model (layered invalidation):** Watcher events set confidence to Medium, never High. All cache lookups stat-verify before returning a hit. Content hashing is ground truth. A wrong cache hit is catastrophic; an extra stat is cheap.

**IPC:** Unix domain sockets on Linux/macOS, named pipes on Windows, behind a transport trait. Messages are length-prefixed bincode. Daemon is lazily started by CLI if not running.

**File identity:** Tracked as (path, file_id) where file_id = inode on Unix, nFileIndex on Windows. Catches file replacement even when mtime is unchanged.

**Cache keys:** blake3 hash of: compiler identity + sorted args + sorted env vars + source content hash + dependency hashes. Domain separation tag "zccache-cache-key-v1".

**Concurrency:** Tokio tasks for IPC, DashMap for metadata cache (sharded lock-free reads), redb MVCC for artifact index, file watcher on dedicated OS thread.

## Current Status

Phase 0 (scaffolding) is complete. All 11 crates are stubbed with real types, traits, and tests. Phase 1 (daemon + CLI + IPC) is next. See @docs/ROADMAP.md for the full phased plan.
