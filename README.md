# zccache

[![Linux](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-linux.yml)
[![macOS](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-macos.yml)
[![Windows](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml/badge.svg)](https://github.com/zackees/zccache/actions/workflows/ci-windows.yml)

### A blazing fast compiler cache for C/C++ and Rust


![New Project](https://github.com/user-attachments/assets/f4d85974-0772-4710-b9f8-47bbd9439cef)

Inspired by [sccache](https://github.com/mozilla/sccache), but optimized for local-first use
with aggressive file metadata caching and filesystem watching.

## Performance

### Rust Benchmark: 50 .rs files, 5 warm trials

| Scenario | Bare rustc | sccache | zccache | vs sccache | vs bare rustc |
|:---------|----------:|--------:|--------:|-----------:|--------------:|
| Build, Cold | 7.119s | 10.023s | 8.507s | 1.2x faster | 1.2x slower |
| Build, Warm | 6.592s | 8.604s | **0.045s** | **193x faster** | **148x faster** |
| Check, Cold | 4.289s | 7.056s | 5.060s | 1.4x faster | 1.2x slower |
| Check, Warm | 3.716s | 5.922s | **0.049s** | **121x faster** | **76x faster** |

> **Build** = `--emit=dep-info,metadata,link` (cargo build). **Check** = `--emit=dep-info,metadata` (cargo check).
> **Cold** = first compile (empty cache). **Warm** = median of 5 subsequent runs.
> Each file is an independent `rustc --crate-type lib` invocation with `--out-dir`
> (same flags cargo passes).
>
> sccache gets cache hits but each hit still costs ~170ms subprocess overhead.
> zccache serves hits in ~1ms via in-process IPC — no subprocess, no re-hashing.

#### Why is zccache 120-193x faster than sccache on warm hits?

The difference comes from **architecture**, not better caching:

| | sccache | zccache |
|---|---------|---------|
| **IPC model** | Subprocess per invocation (fork + exec + connect) | Persistent daemon, single IPC message per compile |
| **Cache lookup** | Client hashes inputs, sends to server, server checks disk | Daemon has inputs in memory (file watcher + metadata cache) |
| **On hit** | Server reads artifact from disk, sends back via IPC | Daemon hardlinks cached file to output path (1 syscall) |
| **Per-hit cost** | ~170ms (process spawn + hash + disk I/O + IPC) | ~1ms (in-memory lookup + hardlink) |

sccache was designed for **distributed** caching (S3, GCS, Redis) where network
latency dwarfs local overhead. zccache is designed for **local-first** use where
every millisecond of wrapper overhead matters.

### C++ Benchmark: 50 C++ files, 5 warm trials

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

Run the benchmark yourself: `./perf`

### Install

```bash
pip install zccache
```

This installs **native Rust binaries** (`zccache` and `zccache-daemon`) directly
onto your PATH — no Python runtime dependency. Pre-built wheels are available for:

| Platform | Architecture |
|----------|-------------|
| Linux | x86_64, aarch64 |
| macOS | x86_64, Apple Silicon |
| Windows | x86_64 |

Verify the install:

```bash
zccache --version
```

Use it as a drop-in replacement for sccache — just substitute `zccache`:

### Rust / Cargo integration

```bash
# cargo build (cached)
RUSTC_WRAPPER=zccache cargo build

# cargo check (cached)
RUSTC_WRAPPER=zccache cargo check
```

Add to `.cargo/config.toml` for automatic use:

```toml
[build]
rustc-wrapper = "zccache"
```

Supports `--emit=metadata` (cargo check), `--emit=dep-info,metadata,link` (cargo build),
extern crate content hashing (dependency changes cause cache misses), and all
cacheable crate types (`lib`, `rlib`, `staticlib`). Proc-macro and binary crates
are passed through without caching (same as sccache).

### C/C++ build system integration (ninja, meson, cmake, make)

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

**Persistent cache:** Artifacts are stored in `~/.zccache/artifacts/`
and survive daemon restarts. No need to re-warm the cache after a reboot.

**Compile journal (build replay):** Every compile and link command is recorded
to `~/.zccache/logs/compile_journal.jsonl` as a JSONL file with enough
detail to replay the entire build:

```json
{"ts":"2026-03-17T10:30:00.123Z","outcome":"hit","compiler":"/usr/bin/clang++","args":["-c","foo.cpp","-o","foo.o"],"cwd":"/project/build","env":[["CC","clang"]],"exit_code":0,"session_id":"uuid","latency_ns":1234567}
```

Fields: `ts` (ISO 8601 UTC), `outcome` (`hit`/`miss`/`error`/`link_hit`/`link_miss`),
`compiler` (full path), `args` (full argument list), `cwd`, `env` (omitted when
inheriting daemon env), `exit_code`, `session_id` (null for ephemeral),
`latency_ns` (wall-clock nanoseconds). One JSON object per line — pipe through
`jq` to filter, or replay builds by extracting compiler + args + cwd.

**Per-session compile journal:** Pass `--journal <path>` to `session-start` to
write a dedicated JSONL log containing only the commands from that session.
The path must end in `.jsonl`:

```bash
result=$(zccache session-start --journal build.jsonl)
session_id=$(echo "$result" | jq -r .session_id)
export ZCCACHE_SESSION_ID=$session_id

# ... build runs ...

# Inspect this session's commands only (no noise from other sessions)
jq . build.jsonl

zccache session-end $session_id
```

The session journal uses the same JSONL schema as the global journal. Entries
are written to both the global and session journals simultaneously. The session
file handle is released when `session-end` is called.

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

## Tool Compatibility

zccache works as a drop-in wrapper for these compilers and tools:

**Clang Toolchain:**
[![clang](https://img.shields.io/badge/clang-supported-brightgreen?logo=llvm)](https://clang.llvm.org/)
[![clang-tidy](https://img.shields.io/badge/clang--tidy-supported-brightgreen?logo=llvm)](https://clang.llvm.org/extra/clang-tidy/)
[![IWYU](https://img.shields.io/badge/IWYU-supported-brightgreen?logo=llvm)](https://include-what-you-use.org/)

**Emscripten / WebAssembly:**
[![emcc](https://img.shields.io/badge/emcc-supported-brightgreen?logo=webassembly)](https://emscripten.org/)
[![wasm-ld](https://img.shields.io/badge/wasm--ld-supported-brightgreen?logo=webassembly)](https://lld.llvm.org/WebAssembly.html)

**Rust Toolchain:**
[![rustc](https://img.shields.io/badge/rustc-supported-brightgreen?logo=rust)](https://www.rust-lang.org/)
[![rustfmt](https://img.shields.io/badge/rustfmt-supported-brightgreen?logo=rust)](https://github.com/rust-lang/rustfmt)
[![clippy](https://img.shields.io/badge/clippy-supported-brightgreen?logo=rust)](https://github.com/rust-lang/rust-clippy)

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
