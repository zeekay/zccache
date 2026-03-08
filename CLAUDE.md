# CLAUDE.md

zccache is a local-first compiler cache daemon (11 crates). See @docs/CLAUDE.md for which architecture doc to read based on what you're working on.

## Essential Rules

- **Always use `./run` to execute Rust commands.** Bare cargo/rustc are blocked by hook.
- **Always use `uv` for Python.** Bare `python`/`pip` are blocked by hook. Use `uv run python ...` or `uv pip ...`.
- MSRV: 1.75 | Edition: 2021 | Toolchain: stable (clippy + rustfmt)
- CI: Linux, macOS, Windows. All warnings denied (`RUSTFLAGS="-D warnings"`)
- Every directory with files must have a README.md (enforced by hook)

## Commands

```bash
./run cargo check --workspace --all-targets
./run cargo test --workspace
./run cargo test -p <crate> -- <test_name>
./run cargo clippy --workspace --all-targets -- -D warnings
./run cargo fmt --all
RUSTDOCFLAGS="-D warnings" ./run cargo doc --workspace --no-deps
./run cargo bench -p zccache-hash
```

## Hooks (enforced automatically)

All hooks are Python scripts in `ci/hooks/`, invoked via `uv run python`:

- **PreToolUse**: `ci/hooks/tool_guard.py` blocks bare Rust commands (must use `./run`) and bare `python`/`pip` (must use `uv`)
- **PostToolUse**: `ci/hooks/lint.py` auto-formats + runs clippy on edited .rs files
- **PostToolUse**: `ci/hooks/readme_guard.py` errors if directory lacks README.md

## Core Principles

- Simplicity first. Minimal code impact. No over-engineering.
- No laziness. Root causes only. Senior developer standards.
- Verify before done. Run tests, demonstrate correctness.
- Plan non-trivial work in `tasks/todo.md`. Capture lessons in `tasks/lessons.md`.
