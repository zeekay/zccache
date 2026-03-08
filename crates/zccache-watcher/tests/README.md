# Integration & Stress Tests

Adversarial and stress tests for the file watcher subsystem (fscache + watcher).

**Not run by default.** Use `--ignored` to run:

```bash
# Run all stress/adversarial tests
uv run cargo test -p zccache-watcher --test stress_test -- --ignored

# Run a specific test
uv run cargo test -p zccache-watcher --test stress_test -- --ignored stress_concurrent_lookups
```
