# crates/zccache-daemon/src/bin

Daemon-crate test fixtures published as `[[bin]]` targets so integration tests
can spawn them via `env!("CARGO_BIN_EXE_<name>")`.

- **`crash_trigger.rs`** — installs the daemon's panic + signal crash handlers
  then deliberately triggers a crash kind chosen on the CLI (`panic`,
  `sigsegv`, `sigabrt`, `stack-overflow`, `illegal-instruction`). Driven by
  `tests/crash_minidump_test.rs`, which asserts a crash dump file appears on
  disk after the child exits.

Production daemon binary lives at `src/main.rs`, not here.
