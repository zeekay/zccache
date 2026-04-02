# Depgraph Integration & Stress Tests

Integration and stress tests for the dependency graph crate.

## Test Files

- **`stress_test.rs`** — Stress tests for concurrent depgraph operations.
  Run with: `./test --full` or `uv run cargo test -p zccache-depgraph --test stress_test -- --ignored`

- **`depfile_integration_test.rs`** — Integration tests that exercise the full depfile pipeline: compile with `-MD -MF`, parse the resulting `.d` file, and verify resolved includes match expectations. Requires GCC or Clang installed.
  Run with: `uv run cargo test -p zccache-depgraph --test depfile_integration_test -- --ignored`
