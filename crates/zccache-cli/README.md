# zccache-cli

CLI binary (`zccache`). Subcommands: start, stop (`kill` alias), status, clear, wrap, inspect, session-start, session-end, session-stats, crashes, download.

## Top-Level Flags

sccache-compatible flags that work without a subcommand:

```bash
zccache --clear       # Clear the entire artifact cache (same as `zccache clear`)
zccache --show-stats  # Show daemon and cache statistics (same as `zccache status`)
```

## Session Commands

Sessions group compiler invocations into a build and provide per-session hit/miss statistics.

```bash
# Start a session with stats tracking and a log file
zccache session-start --stats --log build/session.log
# stdout: {"session_id":"<uuid>","started_at":1710000000}

# Compilations use ZCCACHE_SESSION_ID env var
export ZCCACHE_SESSION_ID=<uuid>
zccache clang++ -c foo.cpp -o foo.o

# Query stats mid-build (non-destructive, session stays active)
zccache session-stats <session_id>
# stderr: Session <id> (active, 12.3s)
#           45 compilations: 30 hits, 12 misses, 3 non-cacheable
#           Hit rate: 71.4%

# End session (returns final stats if --stats was used)
zccache session-end <session_id>
```

### Flags

| Command | Flag | Description |
|---------|------|-------------|
| `session-start` | `--stats` | Enable per-session hit/miss tracking |
| `session-start` | `--log <path>` | Write per-compilation diagnostics to a log file |
| `session-start` | `--cwd <path>` | Override working directory (default: current dir) |
| `session-start` | `--endpoint <ep>` | IPC endpoint override |
| `session-stats` | `--endpoint <ep>` | IPC endpoint override |
| `session-end` | `--endpoint <ep>` | IPC endpoint override |

## Download Command

High-level artifact fetch and optional unarchive flow. The dedicated download daemon stays internal.

```bash
zccache download https://example.com/toolchain.tar.gz
zccache download https://example.com/toolchain.tar.gz archive/toolchain.tar.gz
zccache download https://example.com/toolchain.tar.gz --unarchive toolchain/
zccache download https://example.com/toolchain.tar.gz archive/toolchain.tar.gz --unarchive toolchain/
```

If the archive path is omitted, `zccache` chooses a deterministic cache path under its own cache directory.
