# Claude Code Hooks

Hook scripts that enforce project standards automatically.

- **rust-guard.sh** — PreToolUse: blocks bare Rust commands (must use `./run`)
- **lint.sh** — PostToolUse: auto-formats + runs clippy on edited .rs files
- **readme-guard.sh** — PostToolUse: ensures every directory has a README.md
