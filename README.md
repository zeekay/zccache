# zccache

A high-performance, portable compiler-cache daemon for fast local development workflows.

Inspired by [sccache](https://github.com/mozilla/sccache), but optimized for local-first use
with aggressive file metadata caching and filesystem watching.

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

## Performance

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
