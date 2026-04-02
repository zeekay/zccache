# CI & Development Tools

Python scripts for development tooling. `.env` prepends `.cargo/bin` to PATH so `uv run cargo` finds the rustup toolchain.

## Top-level scripts

- **`uv run cargo`** — Execute cargo with the correct Rust toolchain (via `.env` PATH)
- **`./lint`** — Workspace linting (rustfmt + clippy), supports single-file mode
- **`./test`** — Workspace tests, supports per-crate filtering
- **`./perf`** — Performance benchmarks (zccache vs sccache vs bare clang)

## Hooks (`ci/hooks/`)

Claude Code hooks that enforce project conventions:

- **tool_guard.py** — PreToolUse: blocks bare `cargo`/`rustc` (must use `uv run` so `.env` PATH applies) and bare `python`/`pip` (must use `uv`)
- **lint.py** — PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** — PostToolUse: ensures every directory has a `README.md`
