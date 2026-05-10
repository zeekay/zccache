# zccache-daemon

Tokio async runtime, IPC server, orchestrates all subsystems.

## How the daemon avoids file-locking the install path on Windows

A running Windows process file-locks its own executable, which means a
naive `pip install --upgrade zccache` would fail to overwrite
`Scripts/zccache-daemon.exe` while a daemon is alive. zccache solves this
in two layers:

1. **CLI relocates the daemon binary before spawning** (primary path).
   `zccache_cli::prepare_daemon_exe` copies the daemon binary from its
   install path into `<global_cache_dir>/runtime-binaries/zccache-daemon.<rand>.<ext>`,
   and the CLI spawns from that copy. The install path is never executed
   in place, so it stays overwritable. Stale copies are cleaned up by
   `zccache_cli::gc_runtime_binaries` on every spawn — locked files
   (currently-running daemons) are silently skipped by the OS.

2. **Daemon's `unlock_exe()` is the fallback for direct invocations.**
   When a user manually runs `zccache-daemon.exe` (rare), the binary is
   still on the install path. `unlock_exe()` (in `src/trampoline.rs`)
   renames it to `<install>.exe.old.<rand>` and copies a fresh unlocked
   file back. The running process keeps executing from the renamed file.
   When the daemon is launched normally via the CLI, `current_exe()`
   already lives under `runtime-binaries/`, so `unlock_exe()` short-circuits
   to a no-op.

`release_cwd()` (also in `src/trampoline.rs`) chdir's to `std::env::temp_dir()`
so the daemon stops pinning whatever directory the launcher happened to be
in (e.g. a project's `.venv`). Both run as the very first lines of `main()`,
before tracing init or any IPC bind.

### Opt-out

Set `ZCCACHE_NO_UNLOCK=1` to skip the in-place rename/copy in `unlock_exe()`.
The CLI's relocation step always runs (no opt-out) since it's now the
primary mechanism. The cwd release runs unconditionally.

See issue #134 for background; PR #135 ported `unlock_exe()`/`release_cwd()`
from [clud](https://github.com/zackees/clud)'s `clud-bin/src/trampoline.rs`.
