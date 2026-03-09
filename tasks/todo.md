# Stats System Implementation (TDD)

## Overview
Add global daemon stats and opt-in per-session stats tracking.

## Phase 1: Protocol types
- [x] Add `SessionStats` struct to protocol
- [x] Expand `DaemonStatus` with new fields
- [x] Add `track_stats: bool` to `Request::SessionStart`
- [x] Change `Response::SessionEnded` to carry `Option<SessionStats>`
- [x] Serialization roundtrip tests for all new/changed types (7 tests)

## Phase 2: StatsCollector (daemon-side global stats)
- [x] Create `stats.rs` module in `zccache-daemon`
- [x] `StatsCollector` struct with atomic counters
- [x] Methods: `record_compilation`, `record_hit`, `record_miss`, `record_non_cacheable`, `record_error`, `record_session`
- [x] `snapshot()` → returns current values, `time_saved_ms()` derived
- [x] Unit tests (8 tests: zeros, hit, miss, non-cacheable, error, session, time_saved, concurrent)

## Phase 3: Per-session stats tracking
- [x] Add `SessionStatsTracker` to `Session` (optional, only when opted in via `track_stats`)
- [x] Track: compilations, hits, misses, non_cacheable, errors, time_saved, unique sources, bytes
- [x] `finalize()` → returns `FinalizedSessionStats` with duration computed
- [x] `SessionManager::mutate()` for in-place stat recording
- [x] Unit tests (7 tests: zeros, hit, miss, non_cacheable+error, finalize, with/without tracking)

## Phase 4: Wire up in daemon server
- [x] Add `StatsCollector` to `SharedState`
- [x] Instrument `handle_compile` to record hits/misses/non-cacheable/errors with timing
- [x] Populate `DaemonStatus` from real data (StatsCollector + DepGraph + MetadataCache + artifacts)
- [x] Pass `track_stats` through `SessionStart` → `SessionConfig`
- [x] Return `SessionStats` in `SessionEnd` response via finalize
- [x] `record_session_stat()` helper for per-session stat mutations

## Phase 5: CLI output formatting
- [x] Pretty-print expanded `DaemonStatus` in `cmd_status`
- [x] Print session stats on `session-end` when present
- [x] Format helpers: `format_uptime`, `format_duration_ms`, `format_bytes`

## Verification
- Workspace compiles clean (cargo check + clippy -D warnings)
- 191 tests pass, 1 pre-existing failure (cli_binary_session_round_trip — needs compiled binary)
