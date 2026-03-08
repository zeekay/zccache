# CI & Development Tools

Python scripts for development tooling, invoked via `uv run`.

## Hooks (`ci/hooks/`)

Claude Code hooks that enforce project conventions:

- **tool_guard.py** — PreToolUse: blocks bare `cargo`/`rustc` (must use `./run`) and bare `python`/`pip` (must use `uv`)
- **lint.py** — PostToolUse: auto-formats + runs clippy on edited `.rs` files
- **readme_guard.py** — PostToolUse: ensures every directory has a `README.md`
