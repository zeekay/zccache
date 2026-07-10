# Data Flow

Step-by-step traces of the three main execution paths through zccache.

For component details see [overview.md](overview.md). For IPC specifics see [ipc.md](ipc.md).

---

## Cache Hit Path

There are two IPC modes:

- **Ephemeral mode** (drop-in wrapper, no `ZCCACHE_SESSION_ID`): CLI sends a single
  `Request::CompileEphemeral` message. The daemon creates an internal session,
  compiles, ends the session, and returns the result — **1 IPC roundtrip**.
- **Session mode** (`ZCCACHE_SESSION_ID` set by a build system integration):
  CLI sends `Request::Compile` within an existing session — also 1 roundtrip,
  but the session was created separately.

```
User invokes:  zccache clang++ -c foo.c -o foo.o

1. CLI parses argv. Determines: compiler=clang++, source=foo.c, output=foo.o.
   This is a single-source compilation — cacheable.

2. CLI calls connect().
   a. Compute socket path: $XDG_RUNTIME_DIR/zccache/sock (Unix)
      or \\.\pipe\zccache-{username} (Windows).
   b. Attempt connect. If refused or socket missing:
      - Check lock file. If lock file exists and process alive, retry briefly.
      - Otherwise, clean stale socket/lock, fork/spawn daemon, wait for
        socket to appear, connect.

3. CLI sends Request::CompileEphemeral { client_pid, working_dir, compiler,
   args, cwd, env } over IPC. (Single roundtrip — session start, compile,
   and session end are handled internally by the daemon.)

4. Daemon IPC server receives request, spawns a tokio task.

5. Daemon re-parses args server-side to extract canonical info.
   Resolves compiler path to absolute.

6. Daemon computes compiler identity hash:
   a. Check metadata cache for compiler binary. If High confidence and
      content_hash is Some, use it.
   b. Otherwise, stat the compiler binary, update metadata entry, hash
      the file, store in metadata cache at High confidence.

7. Daemon computes source content hash:
   a. Check metadata cache for foo.c. Suppose confidence is Medium
      (watcher says unchanged).
   b. Medium is not High — stat the file. Compare (mtime, size, file_id)
      with cached entry.
   c. Match: promote to High confidence, use cached content_hash if present.
      No match: re-hash file, update entry at High.

8. Daemon computes dependency hash:
   (MVP: run preprocessor to get dependency content hash. Future: use
   cached per-header hashes.)

9. Daemon computes cache key = blake3(compiler_id, sorted_args,
   sorted_env, source_hash, dep_hash).

10. Daemon queries ArtifactStore::lookup(key).
    a. redb index lookup by key — found, returns artifact directory path
       and metadata.
    b. Verify artifact directory exists and manifest is intact.
    c. Update last-access-time in redb index.

11. Daemon copies cached output files to the requested output paths
    (e.g., copies cached object file to foo.o).

12. Daemon sends Response::CacheHit { exit_code: 0, stdout, stderr }
    over IPC.

13. CLI receives response. Writes stdout/stderr to its own stdout/stderr.
    Exits with the cached exit code.
```

## Cache Miss Path

```
Steps 1–9: identical to cache hit path.

10. Daemon queries ArtifactStore::lookup(key) — not found.

11. Daemon calls CompilerManager::run_compiler(compiler, args, cwd, env).
    a. Spawns the real compiler as a child process via tokio::process::Command.
    b. Waits for completion, captures stdout, stderr, exit code.

12. If exit code != 0, daemon sends Response::CacheMiss { exit_code,
    stdout, stderr }. Does NOT cache failed compilations. Done.

13. If exit code == 0, daemon stores the artifact:
    a. Create temp directory under {cache_root}/tmp/{random}.
    b. Copy output files into temp dir.
    c. Write manifest.json into temp dir.
    d. Compute artifact content hash (the cache key).
    e. Rename temp dir to {cache_root}/artifacts/{hash[0..2]}/{hash[2..4]}/{hash}.
       Atomic on same filesystem.
    f. Insert entry into redb index with current timestamp as last-access-time.
    g. If total cache size exceeds max, trigger async eviction.

14. Daemon sends Response::CacheMiss { exit_code: 0, stdout, stderr }.

15. CLI receives response. Output files already exist on disk (the real
    compiler wrote them). CLI writes stdout/stderr, exits with exit code.
```

## Non-Cacheable Invocation Passthrough

```
User invokes:  zccache-cc foo.c bar.c -o program   (linking, multiple sources)

1. CLI parses argv. Determines this is a link invocation or multi-source
   compilation — not cacheable.

2. CLI does NOT contact the daemon.

3. CLI execs the underlying compiler directly:
   a. Determine real compiler path (from PATH, skipping zccache wrappers).
   b. execvp(compiler, original_args).

4. CLI process is replaced by the compiler. Exit code propagates to the
   build system.
```

Non-cacheable patterns detected by the CLI:
- No `-c` flag (linking invocation).
- Multiple source files.
- `-E` / `-M` / `-MM` (preprocessing / dependency generation only).
- `-` as input (stdin source).
- Unrecognized compiler.

## Rustc Cache-Key Semantics

The rustc key is a two-level construction in `zccache-depgraph`:

- **Context key** (`RustcCompileContext::context_key_with_root`): compiler
  identity hash, all cache-relevant flags (`--crate-type`, `--edition`,
  `--emit`, `--cfg`, codegen flags, lints, `--remap-path-prefix`, …),
  extern crate identities, and the `CARGO_*` env subset (minus
  `CARGO_MAKEFLAGS`, `CARGO_INCREMENTAL`, and the volatile path-valued
  `CARGO_MANIFEST_DIR` / `CARGO_MANIFEST_PATH` / `CARGO_TARGET_DIR`).
- **Artifact key** (`compute_rustc_artifact_key_with_root`): context key +
  content hashes of the source, every dep-info file dependency, and every
  `--extern` rlib/rmeta.

**Env-var dependencies (issue #1021).** Non-`CARGO_` env values read via
`env!()` / `option_env!()` — e.g. build-script `cargo:rustc-env=` values from
vergen / shadow-rs / built — are tracked through the `# env-dep:` dep-info
lines the compiler emits. After each compile the daemon records the env-dep
*names* on the context entry (`DepGraph::record_env_deps`) plus a blake3
fingerprint of their values in that compile's client env. Every hit path
(request-level fast path, context-level fast path, depgraph verify, rustc
check/build metadata-compat) gates on `DepGraph::env_deps_match` — a changed
value forces a recompile instead of serving an artifact with the stale value
baked in. Volatile path-valued names (`OUT_DIR`, `CARGO_MANIFEST_DIR`,
`CARGO_MANIFEST_PATH`, `CARGO_TARGET_DIR`) are excluded: their referenced
*content* is hashed as ordinary file deps, and fingerprinting the path string
would cascade misses across checkout moves. Both fields persist in the
depgraph snapshot (format v6). Compiles that emit no dep-info (`--emit=link`
only) record no env-dep names and keep the pre-#1021 behavior for
non-`CARGO_` vars.

**Cacheable crate types.** `lib`, `rlib`, `staticlib`, `proc-macro`, `bin`
(`RUSTC_CACHEABLE_CRATE_TYPES` in `zccache-compiler/src/parse_rustc.rs`).
`dylib` / `cdylib` are deliberately non-cacheable: final shared-library
artifacts recompile every time while their rlib deps still hit.

**Native-library blind spot (recorded decision, issue #1021 item 3).**
Link-step native libraries resolved via `-L`/`-l` are *not* content-hashed
into `bin`/`staticlib` keys — sccache shares this blind spot, and hashing
resolved system libraries on every link costs syscalls on the hot path with
no field reports justifying it. Revisit (flip to hashing) if a real-world
stale-bin report lands or before promoting any shared/remote artifact tier.

**Incremental compilation.** `-C incremental` is excluded from the key and
the compiler may use the incremental dir on a miss. sccache instead refuses
to cache incremental compiles. Caveat accepted: incremental can change CGU
partitioning (internal symbol names), so byte-level artifact identity is not
guaranteed across incremental states; served artifacts always come from a
real compile of the same keyed inputs.
