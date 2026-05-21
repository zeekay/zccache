# Concurrency, Correctness & Crash Recovery

Runtime behavior of the daemon: task topology, synchronization, correctness guarantees, failure modes, and crash recovery.

For component details see [overview.md](overview.md). For platform differences see [portability.md](portability.md).

---

## Concurrency Model

### Task Topology

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
Dedicated OS thread:  event log writer (daemon.log)
Dedicated OS thread:  compile journal writer (compile_journal.jsonl + per-session journals)
```

For the on-disk record shape and the closed `miss_reason` enum, see
[journal-schema.md](../journal-schema.md).

### Synchronization Points

| Resource | Mechanism | Contention |
|---|---|---|
| Metadata cache | DashMap (sharded concurrent map) | Low — per-shard locks, short critical sections |
| Artifact store on disk | Atomic rename, no locks | None — each artifact has unique path |
| redb index | redb internal MVCC (readers never block, writer serialized) | Low — write transactions are short |
| File watcher event channel | tokio mpsc (bounded, 4096) | Low — single producer, single consumer |
| Event log channel | tokio mpsc (unbounded) | None — lock-free send, single consumer thread |
| Compile journal channel | tokio mpsc (unbounded) | None — lock-free send, single consumer thread (writes global + per-session files) |

### Lock Ordering

There is no nested locking. The design avoids situations where one lock is held while acquiring another:
- DashMap lookups are point operations. The shard lock is released before any I/O.
- redb transactions do not hold DashMap locks.
- The watcher thread never acquires DashMap locks directly; it sends events through a channel.

This eliminates deadlock by design.

---

## Correctness Model

### Layered Invalidation

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

### Conservative Bias

When in doubt, zccache assumes the file has changed and re-verifies. Specific policies:

- **No cached hash at any confidence level:** always hash.
- **Watcher overflow:** downgrade everything to Low, stat-verify all.
- **stat race detected (mtime changed during hashing):** retry, then treat as uncacheable.
- **Unknown file ID:** fall back to path + mtime + size (less reliable, but safe because mtime changes on write in all supported filesystems).
- **Compiler binary changed:** re-hash compiler identity on every daemon start and whenever its metadata cache entry is not High.

### Failure Modes and Mitigations

| Failure | Impact | Mitigation |
|---|---|---|
| Watcher misses an event | Stale metadata at Medium | Stat verification on every cache key computation (stat guard in `lookup_since()` catches changes even without watcher) |
| Watcher overflows | Many stale entries | Downgrade all to Low; stat-verify everything |
| File replaced with same mtime/size | Incorrect cache hit | file_id (inode) detection; extremely rare in practice |
| Compiler updated in-place | Incorrect cache hit | Compiler binary is in metadata cache; stat-verified on use |
| Clock skew / mtime unreliable | Incorrect cache hit | file_id provides second signal; Low confidence triggers re-hash |
| Disk full during artifact write | Orphaned temp dir | Temp dir cleaned on startup; write failure returns error, CLI falls back |
| redb corruption | Index lost | redb is ACID; if corruption occurs (hardware fault), rebuild index by scanning artifact directories |

### What zccache Does NOT Cache

- Failed compilations (non-zero exit code).
- Compilations reading from stdin.
- Compilations involving response files that cannot be fully resolved.
- Compilations where the preprocessor output is non-deterministic (detected heuristically: `__TIME__`, `__DATE__` in source — future enhancement).

---

## Crash Recovery

### Daemon Crash Recovery

**Stale socket:** The CLI detects a stale socket by attempting to connect. If the connection fails (connection refused or broken pipe), the CLI removes the socket file and lock file, then starts a fresh daemon.

**Lock file:** Contains the daemon PID. The CLI checks whether the PID is alive (`kill(pid, 0)` on Unix, `OpenProcess` on Windows). If the process is dead, the lock file is stale and is removed.

### Metadata Cache Recovery

The in-memory metadata cache is **not persisted**. After a daemon restart, the cache is empty. Entries are rebuilt lazily: the first compilation after restart will stat and hash all referenced files, populating the cache. Subsequent compilations benefit from cached metadata.

This is a deliberate design choice. Persisting the metadata cache would add complexity (serialization, staleness on restart) for marginal benefit — the cache warms up within one full build.

### Dep Graph Recovery

The dep graph **is** persisted across daemon restarts (issue #262). At graceful shutdown, and again every 5 minutes while running, the daemon flushes the current `DepGraph` to `<cache_dir>/depgraph/depgraph.bin` using a rkyv zero-copy snapshot. The on-disk format carries a magic header (`ZCDG`) plus a `DEPGRAPH_VERSION` (currently 4) so old snapshots written by an incompatible build are rejected rather than misread.

On startup, the daemon attempts to load the snapshot:

- **Success:** the in-memory graph is populated from the file and `DaemonStatus.dep_graph_persisted` reports `true`. CI runs that restore `<cache_dir>` from a cache store skip the cold-seed compile entirely.
- **Missing file / `VersionMismatch` / corrupt bytes:** a warning is logged and the daemon starts with an empty graph (the pre-fix behavior).

The `dep_graph_persisted` flag is also flipped to `true` when a periodic or shutdown save completes successfully, so a daemon that started cold but has since flushed reports itself as persisted. `zccache status` surfaces this as either `vN, persisted, X.YZ MB on disk` or `vN, not persisted`.

### Artifact Store Recovery

**Orphaned temp directories:** On startup, `{cache_root}/tmp/` is deleted recursively. This removes any incomplete artifact writes from a previous crash.

**Artifact directories:** Intact. Atomic rename ensures an artifact directory is either fully present or absent. If the daemon crashed after creating the temp dir but before renaming, the temp dir is cleaned up and the artifact is simply absent (cache miss; the compilation will re-run).

### Index Recovery

**redb** provides ACID transactions. The database file is always in a consistent state, even after an unclean shutdown. If the daemon crashed mid-transaction, redb rolls back the incomplete transaction on next open.

**Index-artifact divergence:** If the daemon crashed after writing the artifact directory but before inserting the redb entry, the artifact exists on disk but is not in the index. This is a harmless orphan; it wastes disk space but does not cause incorrect behavior. A periodic (or on-demand) maintenance task can scan the artifact directories and reconcile with the index:
- Artifact on disk but not in index: add to index.
- Entry in index but no artifact on disk: remove from index.

---

## Cache root invariants

The "cache root" is the directory resolved by
[`zccache_core::config::resolve_cache_root`][resolve]. Wrappers (notably
[soldr](https://github.com/zackees/soldr)) excludable this single directory
from Windows Defender / on-access scanners and trust that **no zccache
persistent write escapes it**. Issue #275 closes that contract.

[resolve]: ../../crates/zccache-core/src/config.rs

### Resolution rules

| Source | When it fires | `cache-root --json` value |
|---|---|---|
| `ZCCACHE_CACHE_DIR` | Env var set and non-empty | `env:ZCCACHE_CACHE_DIR` |
| Same-volume colocation | `ZCCACHE_COLOCATE` is truthy *and* CWD is on a different volume from `$HOME` (issue #296) | `colocate:cross_volume` |
| Default | Otherwise | `default:platform_dirs` (`~/.zccache`) |

`zccache cache-root` (default) prints the resolved absolute path; `--json`
adds the `source` field so wrappers can verify at runtime that their
redirect was honored.

### Persistent writes — exhaustive table

Every persistent write the daemon and CLI perform lands under the resolved
cache root via one of the helpers in `zccache_core::config`:

| Subpath | Owner | Resolver |
|---|---|---|
| `artifacts/` | daemon — content-addressed artifact store + sibling tmp files for atomic rename | `artifacts_dir_from_cache_dir` |
| `tmp/` | daemon — recursively wiped on startup (orphaned in-progress writes) | `tmp_dir_from_cache_dir` |
| `tmp/depfiles/<pid>-<instance>/` | daemon — compiler-injected depfiles and Windows response files (`*.rsp`) | `depfile_dir_from_cache_dir` |
| `depgraph/depgraph.bin` | daemon — rkyv snapshot of the dep graph | `depgraph_file_path` |
| `logs/daemon.log[.<ts>]` | daemon — rolling event log | `log_dir_from_cache_dir` |
| `logs/daemon-lifecycle.log[.1]` | daemon + CLI — JSONL lifecycle events (spawn / shutdown / version mismatch) | `lifecycle::log_file_path` |
| `logs/compile_journal.jsonl` + per-session `*.jsonl` | daemon — compile decisions | derives from `log_dir_from_cache_dir` |
| `crashes/crash-*.{txt,dmp}` + `.reported` | daemon — panic & signal dumps | `crash_dump_dir_from_cache_dir` |
| `symbols/<version>-<triple>/` + `.symref` sidecars next to dumps | CLI — `zccache symbols install` + `symbolicate` | `symbols_cache_dir_from_cache_dir` |
| `index.bin` (+ sibling tmp) | daemon — bincode artifact index, atomic-rename writes | `index_path_from_cache_dir` |
| `ino/<key>.ino.cpp` | CLI — Arduino preprocessor cache | `default_cache_dir().join("ino")` |
| `kv/<namespace>/<hex>.bin` | CLI — namespaced key/value store | derives from `default_cache_dir` |
| `daemon.lock` | CLI + daemon — PID lock | `lock_file_path` |
| `daemon.sock` (Unix, only when env override is set) | daemon — IPC socket co-located with the cache root | `default_endpoint` |

The cache-root-rooted invariant for the well-known subpaths is asserted in
the unit test `cache_root_invariant_all_subpaths_rooted` in
`crates/zccache-core/src/config.rs`.

### Legitimate exceptions (documented and stable)

A small set of writes is intentionally *outside* the cache root. soldr
excludes these separately if Defender scanning ever becomes an issue:

- **IPC socket (Unix, no env override):** `$XDG_RUNTIME_DIR/zccache/sock`
  or `/tmp/zccache-$USER/sock`. The socket inode lives in `tmpfs` on Linux
  so it is not a real on-disk write. When `ZCCACHE_CACHE_DIR` is set, the
  socket moves into `<cache>/daemon.sock` automatically — see
  `zccache_ipc::endpoint_for_cache_dir`.
- **Named pipe (Windows):** `\\.\pipe\zccache-<username>` (default) or
  `\\.\pipe\zccache-<stable-id>` (when `ZCCACHE_CACHE_DIR` is set). Named
  pipes have no filesystem footprint — nothing for Defender to scan.
- **OS-managed working directory (`std::env::set_current_dir(temp_dir())`):**
  the daemon's `trampoline::release_cwd()` chdirs the process to `$TMPDIR`
  *only* to release the inherited CWD handle. No file is written there.
- **`.claude/`, `target/`, scratch tempdirs in dev-mode tests:** test code
  paths and dev tooling. Production runtime never writes here. The
  `ban_unrooted_tempdir` dylint blocks new ad-hoc `tempfile::tempdir()`
  call sites in production code; legacy call sites are listed explicitly
  in `dylints/ban_unrooted_tempdir/src/allowlist.txt`.

Any new persistent write must either pick a helper from the table above or
get its own row plus a one-line justification here. The dylint catches
unrooted `$TMPDIR` writes at compile time, but it cannot catch writes that
hardcode an absolute path outside the cache root — those have to be caught
in review against this section.
