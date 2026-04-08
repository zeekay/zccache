# CI & Development Tools

Python scripts for development tooling. Project-root trampolines (`_cargo`, `_rustc`, `_rustfmt`) prepend the shared rustup proxy location to PATH so the pinned toolchain is always used.

## Top-level scripts

- **`./_cargo`** — Execute cargo with the correct Rust toolchain (via trampoline PATH normalization)
- **`./_rustc`** — Execute rustc with the correct toolchain
- **`./_rustfmt`** — Execute rustfmt with the correct toolchain
- **`./lint`** — Workspace linting (rustfmt + clippy), supports single-file mode
- **`./test`** — Workspace tests, supports per-crate filtering
- **`./perf`** — Performance benchmarks (zccache vs sccache vs bare clang)

## Hooks (`ci/hooks/`)

Claude Code hooks that enforce project conventions:

- **tool_guard.py** — PreToolUse: blocks bare `cargo`/`rustc` and `uv run cargo`/`uv run rustc` (must use trampolines) and bare `python`/`pip` (must use `uv`)
- **lint.py** — PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** — PostToolUse: ensures every directory has a `README.md`
