Read [CLAUDE.md](./CLAUDE.md) first. The following conventions are mandatory in this repo.

## Rust Commands

- Do not run bare `cargo`, `rustc`, or `rustfmt`.
- Always use `soldr <tool>` directly. soldr resolves repo-local `.cargo` / `.rustup` homes and the rustup-managed toolchain pinned by `rust-toolchain.toml`.
- In this Windows environment, the repo is using the rustup-managed `x86_64-pc-windows-msvc` toolchain pinned by `rust-toolchain.toml`. Do not assume GNU or try to find `gcc`. soldr also enforces MSVC on Windows by default.
- zccache self-builds should set `SOLDR_RUSTC_WRAPPER=none` when invoking `soldr cargo ...` so soldr does not wrap the current zccache build in a previous zccache release.

Examples:

```bash
SOLDR_RUSTC_WRAPPER=none soldr cargo check --workspace --all-targets
SOLDR_RUSTC_WRAPPER=none soldr cargo test -p zccache-download
SOLDR_RUSTC_WRAPPER=none soldr cargo build -p zccache-cli
soldr cargo fmt --all
```

PowerShell example:

```powershell
$env:SOLDR_RUSTC_WRAPPER = "none"; soldr cargo check -p zccache-download-client
$env:SOLDR_RUSTC_WRAPPER = "none"; soldr cargo test -p zccache-download
```

## Python Commands

- Do not run bare `python` or `pip`.
- Always use `uv run ...` or `uv pip ...`.
- Python is only for CI scripts, packaging, and hooks. Runtime logic, tests, and benchmarks belong in Rust unless there is an explicit exception.

## Test Entrypoints

- Prefer `./test` for repo-standard test execution.
- Use `./test --integration` for ignored integration tests.
- Use `./test --full` for the larger suite.
- Use `SOLDR_RUSTC_WRAPPER=none soldr cargo test ...` when you need targeted crate/test execution.

## Practical Rule

- When working in this repo, prefer repo entrypoints over generic tool invocations. If there is a local script, use it.
