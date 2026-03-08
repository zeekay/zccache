# Documentation Guide

Architecture docs are split by subsystem. Read only what's relevant to your current work.

## Which doc to read

| Working on crate | Read these |
|---|---|
| `zccache-cli` | [overview.md](architecture/overview.md) (2.1), [data-flow.md](architecture/data-flow.md) |
| `zccache-daemon` | [overview.md](architecture/overview.md) (2.2), [runtime.md](architecture/runtime.md) |
| `zccache-ipc` | [overview.md](architecture/overview.md) (2.3), [ipc.md](architecture/ipc.md) |
| `zccache-protocol` | [overview.md](architecture/overview.md) (2.4), [ipc.md](architecture/ipc.md) |
| `zccache-fscache` | [metadata-cache.md](architecture/metadata-cache.md) |
| `zccache-watcher` | [metadata-cache.md](architecture/metadata-cache.md) (watcher section) |
| `zccache-artifact` | [artifact-store.md](architecture/artifact-store.md) |
| `zccache-hash` | [overview.md](architecture/overview.md) (2.8) |
| `zccache-compiler` | [overview.md](architecture/overview.md) (2.9), [data-flow.md](architecture/data-flow.md) |
| `zccache-core` | [overview.md](architecture/overview.md) |
| Platform-specific issues | [portability.md](architecture/portability.md) |

## Other docs

- **[DESIGN_DECISIONS.md](DESIGN_DECISIONS.md)** — 15 ADR-style decisions with rationale
- **[ROADMAP.md](ROADMAP.md)** — 7 implementation phases
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — Index of all architecture documents
