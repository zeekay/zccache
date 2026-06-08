# zccache-protocol

Wire protocol: `Request`/`Response` enums over a length-prefixed daemon frame.
The active compatibility path is v15 bincode. The v16 prost schema is generated
from `proto/zccache_v1.proto` and scaffolded in `wire_prost.rs` so the daemon can
later dispatch both v15 bincode and v16 prost frames during migration.

## Module Layout

`messages/mod.rs` owns the append-only `Request` and `Response` enum order.
Domain payloads live next to it:

- `messages/status.rs`: daemon status, session stats, phase timing.
- `messages/artifact.rs`: artifact cache payloads and Rust artifact listings.
- `messages/exec.rs`: generic tool execution request options.
- `messages/compat.rs`: bincode roundtrip and variant-index guards.
- `wire_prost.rs`: generated protobuf module, v16 frame helpers, and
  `ZCCACHE_DAEMON_WIRE` parsing.

New protocol payload structs should land in the closest domain module. New
enum variants must still be appended in `messages/mod.rs` and require a
`PROTOCOL_VERSION` bump.

## Wire Migration

`PROTOCOL_VERSION` remains `15` while the public `encode_message` and
`decode_message` helpers emit and accept bincode bodies. `PROST_PROTOCOL_VERSION`
is `16` for the planned prost path. `ZCCACHE_DAEMON_WIRE=prost` is reserved for
the future v16 client default, and `ZCCACHE_DAEMON_WIRE=bincode` is the fallback
spelling that will keep old v15 behavior available during the transition.

## Request Variants

| Variant | Description |
|---------|-------------|
| `Ping` | Health check |
| `Shutdown` | Graceful daemon shutdown |
| `Status` | Global daemon statistics (`DaemonStatus`) |
| `SessionStart` | Create a session (`client_pid`, `working_dir`, `log_file`, `track_stats`) |
| `SessionEnd` | End a session, returns final `SessionStats` if tracking was enabled |
| `SessionStats` | Query mid-session stats without ending the session |
| `Compile` | Compile within an existing session |
| `CompileEphemeral` | Single-roundtrip compile (session start + compile + session end) |
| `LinkEphemeral` | Single-roundtrip link/archive |
| `Lookup` / `Store` | Direct artifact cache access |
| `Clear` | Wipe all caches |

## Response Variants

| Variant | Description |
|---------|-------------|
| `Pong` | Reply to `Ping` |
| `ShuttingDown` | Ack for `Shutdown` |
| `Status(DaemonStatus)` | Global stats snapshot |
| `SessionStarted { session_id }` | UUID session identifier |
| `SessionEnded { stats }` | Final `Option<SessionStats>` |
| `SessionStatsResult { stats }` | Mid-session `Option<SessionStats>` snapshot |
| `CompileResult` | `exit_code`, `stdout`, `stderr`, `cached` flag |
| `LinkResult` | Same as `CompileResult` plus optional `warning` |
| `Error { message }` | Error string |
| `Cleared` | Counts of artifacts/metadata/contexts removed |
