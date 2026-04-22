# zccache GitHub Action

Unified Rust compilation + dependency caching for GitHub Actions. Replaces both `mozilla-actions/sccache-action` and `Swatinem/rust-cache` with a single action.

## Usage

```yaml
steps:
  - uses: actions/checkout@v4

  - uses: zackees/zccache@v1
    with:
      shared-key: ${{ matrix.target }}  # optional, for matrix isolation

  # ... your build/test steps ...

  # REQUIRED: cleanup at end of every job
  - if: always()
    uses: zackees/zccache/action/cleanup@v1
```

## What it replaces

| Before (2 actions) | After (1 action) |
|---|---|
| `mozilla-actions/sccache-action@v0.0.9` | `zackees/zccache@v1` |
| `Swatinem/rust-cache@v2` | (included) |

## Inputs

| Input | Default | Description |
|---|---|---|
| `cache-cargo-registry` | `true` | Cache `~/.cargo/registry/` and `~/.cargo/git/` |
| `cache-compilation` | `true` | Cache compilation units via zccache daemon |
| `cache-target` | `true` | Cache target snapshot + run `zccache warm` |
| `compilation-restore-fallback` | `true` | Allow prefix fallback for compilation cache restores |
| `target-restore-fallback` | `false` | Allow prefix fallback for target snapshot restores |
| `target-dir` | `target` | Path to the cargo target directory |
| `shared-key` | `""` | Extra cache key for matrix isolation |
| `zccache-version` | `latest` | PyPI version to install |
| `save-cache` | `true` | Set `false` for PR builds (restore-only) |

## Outputs

| Output | Description |
|---|---|
| `cache-hit-compilation` | Whether zccache cache was restored |
| `cache-hit-registry` | Whether cargo registry cache was restored |
| `cache-hit-target` | Whether target snapshot cache was restored |

## Architecture

The action has two parts because composite actions lack `post` steps:

1. **`zackees/zccache`** (setup) restores caches, installs zccache, restores the target snapshot, runs `zccache warm`, and starts the daemon.
2. **`zackees/zccache/action/cleanup`** (teardown) stops the daemon and saves caches.

The cleanup action must be called with `if: always()` to ensure caches are saved even on failure.

### Cache layers

| Layer | What | Replaces |
|---|---|---|
| zccache compilation | Per-unit `.o`/`.rlib` files (~1ms hit) | sccache (~170ms hit) |
| Cargo registry | `~/.cargo/registry/` + `~/.cargo/git/` | Swatinem/rust-cache |
| Target snapshot | `target/` tarball excluding `incremental/` | Cargo fingerprint recomputation |

State is passed from setup to cleanup via `~/.zccache-action-state/`.

### Restore policy

- `compilation-restore-fallback: true` keeps the speed-first behavior for incremental CI.
- `target-restore-fallback: false` is the default because stale Cargo target snapshots are not safe to prefix-restore across different source trees.
- For release-hardened builds, prefer `cache-target: false` and exact-key-only restores.
