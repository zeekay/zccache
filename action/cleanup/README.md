# zccache cleanup action

Companion to the main `zackees/zccache` action. Stops the daemon and saves caches.

## Usage

Always call with `if: always()`:

```yaml
- if: always()
  uses: zackees/zccache/action/cleanup@v1
```

## What it does

1. Prints zccache stats (hit/miss rates)
2. Stops the zccache daemon
3. Saves compilation cache to GHA cache
4. Saves cargo registry cache to GHA cache
5. Cleans up temporary state files

Cache keys are read from state written by the setup action (`~/.zccache-action-state/`).
