<!-- managed-by: ci.py -->

# `ci.gates`

Per-gate runners for the unified CI pipeline. Each module exposes a single `run() -> int` function that performs one gate (fmt, clippy, dylint, build, unit, …) and returns its exit code. The top-level `ci.py` at the repo root dispatches to these by name.

## Layout

| File | Gate |
|---|---|
| `fmt.py` | `soldr cargo fmt --all -- --check` |
| `clippy.py` | `soldr cargo clippy --workspace --all-targets -- -D warnings` |
| `dylint.py` | nightly-2026-03-26 driver + `python -m ci.lint --dylint-only` |
| `docs.py` | `soldr cargo doc --workspace --no-deps` with `RUSTDOCFLAGS=-D warnings` |
| `build.py` | `soldr cargo check --workspace --all-targets` |
| `test_compile.py` | `soldr cargo test --workspace --lib --bins --no-run` |
| `unit.py` | `soldr cargo test --workspace --lib --bins --no-fail-fast` |
| `integration.py` | `soldr cargo test --workspace --no-fail-fast` |
| `cargo_registry.py` | hash determinism + save (`--output`) + clean round-trip |
| `gha_cache.py` | `zccache gha-cache status` smoke |
| `wrapper_e2e.py` | `SOLDR_RUSTC_WRAPPER` end-to-end against a fresh cargo lib |
| `action_yaml.py` | `action.yml` + `action/cleanup/action.yml` structural check |
| `action_surface.py` | CLI surface contract — every subcommand `action.yml` calls |
| `_common.py` | shared helpers (`soldr_cargo`, `find_built_binary`, `is_platform`, …) |

## Why a sub-package and not inline functions in `ci.py`

`ci.py` is the entry point — invoked via `uv run --script ci.py <gate>`. PEP 723 inline-metadata isolates it from the parent `pyproject.toml`, so the maturin build never fires for a routine `fmt` invocation. Keeping `ci.py` tight (just the dispatcher) makes the script body easy to audit; the actual logic lives in `ci/gates/*.py` where it can grow without bloating the dispatcher.

The sub-package also lets each gate be:

- **Lintable in isolation** — pyright/ruff understand `def run() -> int` directly.
- **Replaceable from outside** — a downstream consumer can monkey-patch or stub a single gate without touching the rest.
- **Tested in isolation** — a future `tests/test_gates.py` can `import ci.gates.fmt; ci.gates.fmt.run()` against a worktree fixture.

## How CI consumes this

`.github/workflows/ci.yml` per-matrix-entry steps become one-liners:

```yaml
- run: uv run --script ci.py fmt
- run: uv run --script ci.py clippy
- run: uv run --script ci.py build
- run: uv run --script ci.py unit
- ...
```

For interactive use, `uv run --script ci.py all` runs every gate sequentially with the same continue-past-failures semantics CI uses; a final summary lists which gates broke (or "all gates passed").

`uv run --script` reads the inline PEP 723 deps from `ci.py` and provisions an isolated venv — it does NOT load the surrounding `pyproject.toml`, so the maturin build the parent project would otherwise trigger never fires for routine CI gate runs.
