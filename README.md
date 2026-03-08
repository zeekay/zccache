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
