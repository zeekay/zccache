# zccache

[![Linux](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml)
[![macOS](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml)
[![Windows](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml)

### A blazing fast cpp compiler cache


![New Project](https://github.com/user-attachments/assets/f4d85974-0772-4710-b9f8-47bbd9439cef)

Inspired by [sccache](https://github.com/mozilla/sccache), but optimized for local-first use
with aggressive file metadata caching and filesystem watching.

## Performance

### Benchmark: 50 C++ files, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file, Cold | 12.641s | 20.632s | 13.430s | 1.5x faster | 1.1x slower |
| Single-file, Warm | 11.705s | 1.576s | **0.050s** | **32x faster** | **236x faster** |
| Multi-file, Cold | 11.358s | 11.759s | 12.867s | 1.1x slower | 1.1x slower |
| Multi-file, Warm | 11.553s | 11.530s | **0.017s** | **695x faster** | **696x faster** |

> **Cold** = first compile (empty cache). **Warm** = median of 5 subsequent runs.
> Single-file = 50 sequential `clang++ -c unit.cpp` invocations. Multi-file = one `clang++ -c *.cpp` invocation.
> sccache cannot cache multi-file compilations — its "warm" multi-file time is a full recompile.

### Response-file benchmark: 50 C++ files, ~283 expanded args, 5 warm trials

| Scenario | Bare Clang | sccache | zccache | vs sccache | vs bare clang |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Single-file RSP, Cold | 12.063s | 20.607s | 14.087s | 1.5x faster | 1.2x slower |
| Single-file RSP, Warm | 12.540s | 1.558s | **0.047s** | **33x faster** | **267x faster** |
| Multi-file RSP, Cold | 13.030s | 25.303s | 13.975s | 1.8x faster | 1.1x slower |
| Multi-file RSP, Warm | 12.049s | 12.434s | **0.019s** | **669x faster** | **648x faster** |

> All args passed via nested response files: `flags.rsp` -> `@warnings.rsp` + `@defines.rsp`.
> 200 `-D` defines + 50 `-I` paths + 30 warning flags = ~283 total expanded args per compile.

Run the benchmark yourself: `uv run perf`

### Install

pip install zccache

(run it like sccache, jusy substitute zccavhe)

### Build system integration (ninja, meson, cmake, make)

zccache is a **drop-in compiler wrapper**. Point your build system's compiler
at `zccache <real-compiler>` and it handles the rest:

```ini
# meson native file
[binaries]
c = ['zccache', '/usr/bin/clang']
cpp = ['zccache', '/usr/bin/clang++']
```

```cmake
# CMake
set(CMAKE_C_COMPILER_LAUNCHER zccache)
set(CMAKE_CXX_COMPILER_LAUNCHER zccache)
```

The first build (cold cache) runs at near-bare speed. Subsequent rebuilds
(`ninja -t clean && ninja`, or touching source files) serve cached artifacts
via hardlinks in under a second.

**Single-roundtrip IPC:** In drop-in mode, zccache sends a single
`CompileEphemeral` message that combines session creation, compilation, and
session teardown — eliminating 2 of 3 IPC roundtrips per invocation.

**Session stats:** Track hit rates per-build with `--stats`:

```bash
eval $(zccache session-start --stats --log build.log)
export ZCCACHE_SESSION_ID=...
# ... build runs ...
zccache session-stats $ZCCACHE_SESSION_ID   # query mid-build
zccache session-end $ZCCACHE_SESSION_ID     # final stats
```

**Persistent cache:** Artifacts are stored in `~/.cache/zccache/artifacts/`
(or `%LOCALAPPDATA%\zccache\artifacts\` on Windows) and survive daemon
restarts. No need to re-warm the cache after a reboot.

### Multi-file compilation (fast path)

When a build system passes multiple source files to a single compiler invocation
(e.g. `gcc -c a.cpp b.cpp c.cpp -o ...`), zccache treats this as a **fast path**:

1. Each source file is checked against the cache **in parallel**.
2. Cache hits are served immediately — their `.o` files are written from the cache.
3. Remaining cache misses are batched into a **single compiler process**, preserving
   the compiler's own process-reuse and memory-sharing benefits.
4. The outputs of the batched compilation are cached individually for future hits.

This hybrid approach means the first build populates the cache per-file, and
subsequent builds serve as many files as possible from cache while still letting
the compiler handle misses efficiently in bulk.

**Recommendation:** Configure your build system to pass multiple source files per
compiler invocation whenever possible. This gives zccache the best opportunity
to parallelize cache lookups and minimize compiler launches.

### Concurrency

The daemon uses lock-free concurrent data structures (DashMap) for artifact and
metadata lookups, so parallel compilation requests from multiple build workers
never serialize on a global lock.

## Status

**Early development** — architecture and scaffolding phase.

## Goals

- Extremely fast on local machines (daemon keeps caches warm)
- Portable across Linux, macOS, and Windows
- Correct under heavy parallel compilation (no stale cache hits)
- Simple deployment (single binary)

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full system design.

### Key components

| Crate | Purpose |
|-------|---------|
| `zccache-cli` | Command-line interface (`zccache` binary) |
| `zccache-daemon` | Daemon process (IPC server, orchestration) |
| `zccache-core` | Shared types, errors, config, path utilities |
| `zccache-protocol` | IPC message types and serialization |
| `zccache-ipc` | Transport layer (Unix sockets / named pipes) |
| `zccache-hash` | blake3 hashing and cache key computation |
| `zccache-fscache` | In-memory file metadata cache |
| `zccache-artifact` | Disk-backed artifact store with redb index |
| `zccache-watcher` | File watcher abstraction (notify backend) |
| `zccache-compiler` | Compiler detection and argument parsing |
| `zccache-test-support` | Test utilities and fixtures |

## Building

```bash
cargo build --workspace
```

## Testing

```bash
cargo test --workspace
```

## Documentation

- [Architecture](docs/ARCHITECTURE.md)
- [Design Decisions](docs/DESIGN_DECISIONS.md)
- [Roadmap](docs/ROADMAP.md)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
