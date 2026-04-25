# zccache-artifact integration tests

Integration tests for `zccache-artifact`. Inline unit tests live next to
their modules under `src/`. This directory is reserved for tests that:

- exercise behavior across multiple types in the public API
- run multi-threaded stress / concurrency scenarios
- validate platform-specific code paths

## Files

- **`kv_stress.rs`** — Concurrency, durability, platform-compliance, and
  input-edge tests for `KvStore`. Counterpart to the inline functional
  tests in `src/kv.rs`.
