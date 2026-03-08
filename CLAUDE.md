# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Environment Setup

**Always use `./run` to execute Rust commands.** This wrapper ensures the rustup-managed toolchain is used, regardless of what else is on the system PATH. It is the equivalent of `uv run` for Python.

```bash
./run cargo check --workspace   # correct — uses rustup toolchain
cargo check --workspace          # BLOCKED by hook — might use wrong Rust
```

A PreToolUse hook (`.claude/hooks/rust-guard.sh`) enforces this: bare `cargo`, `rustc`, `rustfmt`, and `clippy` commands are denied. If the toolchain is missing entirely, run `./install` first.

## Build & Development Commands

```bash
# Build entire workspace
./run cargo build --workspace

# Check (faster than build, no codegen)
./run cargo check --workspace --all-targets

# Run all tests
./run cargo test --workspace

# Run tests for a single crate
./run cargo test -p zccache-hash
./run cargo test -p zccache-fscache

# Run a specific test
./run cargo test -p zccache-hash -- cache_key_deterministic

# Lint
./run cargo clippy --workspace --all-targets -- -D warnings

# Format check
./run cargo fmt --all -- --check

# Format fix
./run cargo fmt --all

# Build docs
RUSTDOCFLAGS="-D warnings" ./run cargo doc --workspace --no-deps

# Run benchmarks (zccache-hash only currently)
./run cargo bench -p zccache-hash
```

**MSRV:** 1.75 | **Edition:** 2021 | **Toolchain:** stable (with clippy + rustfmt)

**CI runs on:** Linux, macOS, and Windows. All warnings are denied (`RUSTFLAGS="-D warnings"`).

## Architecture

zccache is a local-first compiler cache daemon. The CLI intercepts compiler invocations, sends them to a long-running per-user daemon over IPC, and the daemon returns cached artifacts on hit or executes the real compiler on miss.

### Crate Dependency Graph

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

### Crate Responsibilities

- **zccache-core** — Shared error types (`Error`/`Result`), `Config`, `NormalizedPath` for cross-platform path handling
- **zccache-hash** — `ContentHash` (blake3), `CacheKeyBuilder` with domain-separated deterministic hashing
- **zccache-protocol** — `Request`/`Response` enums, `ArtifactData`, length-prefixed bincode framing (`encode_message`/`decode_message`)
- **zccache-ipc** — Platform IPC endpoint discovery (`default_endpoint()`: Unix sockets vs named pipes)
- **zccache-fscache** — `MetadataCache` (DashMap-backed) with `Confidence` levels (High/Medium/Low) and time-based decay
- **zccache-artifact** — Content-addressed disk store with 2-level hex sharding, redb index for LRU eviction
- **zccache-watcher** — `FileWatcher` trait over notify crate; dedicated OS thread, events via tokio channel
- **zccache-compiler** — `CompilerFamily` detection, `ParsedInvocation` for cacheability checks
- **zccache-daemon** — Tokio async runtime, IPC server, orchestrates all subsystems
- **zccache-cli** — Subcommands: start, stop, status, clear, wrap, inspect

### Key Design Patterns

**Correctness model (layered invalidation):** Watcher events set confidence to Medium, never High. All cache lookups stat-verify before returning a hit. Content hashing is ground truth. A wrong cache hit is catastrophic; an extra stat is cheap.

**IPC:** Unix domain sockets on Linux/macOS, named pipes on Windows, behind a transport trait. Messages are length-prefixed bincode. Daemon is lazily started by CLI if not running.

**File identity:** Tracked as (path, file_id) where file_id = inode on Unix, nFileIndex on Windows. Catches file replacement even when mtime is unchanged.

**Cache keys:** blake3 hash of: compiler identity + sorted args + sorted env vars + source content hash + dependency hashes. Domain separation tag "zccache-cache-key-v1".

**Concurrency:** Tokio tasks for IPC, DashMap for metadata cache (sharded lock-free reads), redb MVCC for artifact index, file watcher on dedicated OS thread.

### Current Status

Phase 0 (scaffolding) is complete. All 11 crates are stubbed with real types, traits, and tests. Phase 1 (daemon + CLI + IPC) is next. See `docs/ROADMAP.md` for the full phased plan.

### Key Documentation

- `docs/ARCHITECTURE.md` — Full system design (component diagram, data flow, correctness model, crash recovery, portability)
- `docs/DESIGN_DECISIONS.md` — 15 ADR-style decisions with rationale and alternatives
- `docs/ROADMAP.md` — 7 implementation phases with deliverables, tests, and risks

## Workflow Orchestration

### 1. Plan Mode Default
- Enter plan mode for ANY non-trivial task (3+ steps or architectural decisions)
- If something goes sideways, STOP and re-plan immediately — don't keep pushing
- Use plan mode for verification steps, not just building
- Write detailed specs upfront to reduce ambiguity

### 2. Subagent Strategy
- Use subagents liberally to keep main context window clean
- Offload research, exploration, and parallel analysis to subagents
- For complex problems, throw more compute at it via subagents
- One task per subagent for focused execution

### 3. Self-Improvement Loop
- After ANY correction from the user: update `tasks/lessons.md` with the pattern
- Write rules for yourself that prevent the same mistake
- Ruthlessly iterate on these lessons until mistake rate drops
- Review lessons at session start for relevant project

### 4. Verification Before Done
- Never mark a task complete without proving it works
- Diff behavior between main and your changes when relevant
- Ask yourself: "Would a staff engineer approve this?"
- Run tests, check logs, demonstrate correctness

### 5. Demand Elegance (Balanced)
- For non-trivial changes: pause and ask "is there a more elegant way?"
- If a fix feels hacky: "Knowing everything I know now, implement the elegant solution"
- Skip this for simple, obvious fixes — don't over-engineer
- Challenge your own work before presenting it

### 6. Autonomous Bug Fixing
- When given a bug report: just fix it. Don't ask for hand-holding
- Point at logs, errors, failing tests — then resolve them
- Zero context switching required from the user
- Go fix failing CI tests without being told how

## Task Management

1. **Plan First**: Write plan to `tasks/todo.md` with checkable items
2. **Verify Plan**: Check in before starting implementation
3. **Track Progress**: Mark items complete as you go
4. **Explain Changes**: High-level summary at each step
5. **Document Results**: Add review section to `tasks/todo.md`
6. **Capture Lessons**: Update `tasks/lessons.md` after corrections

## Core Principles

- **Simplicity First**: Make every change as simple as possible. Impact minimal code.
- **No Laziness**: Find root causes. No temporary fixes. Senior developer standards.
- **Minimal Impact**: Changes should only touch what's necessary. Avoid introducing bugs.
