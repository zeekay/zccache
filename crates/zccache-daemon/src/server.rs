//! Daemon server — accepts IPC connections and handles requests.

use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use zccache_depgraph::{
    CompileContext, ContextKey, DepGraph, DepfileStrategy, SessionId, SessionManager,
    SystemIncludeCache, UserDepFlags,
};
use zccache_fscache::{CacheSystem, Clock};
use zccache_hash::ContentHash;
use zccache_ipc::{IpcConnection, IpcListener};
use zccache_protocol::{ArtifactData, ArtifactOutput, Request, Response};
use zccache_watcher::{NotifyWatcher, SettleBuffer, SettledEvent};

use crate::stats::{HitPhases, MissPhases, PhaseProfiler, StatsCollector};

/// Cached result of a verified cache hit, enabling zero-hash fast path.
///
/// When the journal clock hasn't advanced since the last verified hit for a
/// context, we can skip all stat/hash/depgraph work and jump straight to
/// artifact lookup.
struct FastHitEntry {
    clock: Clock,
    artifact_key_hex: String,
    cached_at: std::time::Instant,
}

/// Maximum age for fast-hit cache entries. Matches the High→Medium confidence
/// decay in the metadata cache. Without watcher events, entries expire and
/// fall through to the stat-verify slow path. Set to 60s because the watcher
/// + journal provide real invalidation — this timer is just a safety net.
const FAST_HIT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Cached compilation artifact.
#[derive(Debug, Clone)]
struct CachedArtifact {
    artifact: ArtifactData,
}

/// Shared state accessible by all connection handlers.
struct SharedState {
    sessions: SessionManager,
    system_includes: Mutex<SystemIncludeCache>,
    /// Dependency graph: tracks include relationships and cache verdicts.
    dep_graph: DepGraph,
    /// In-memory artifact cache: artifact_key_hex → artifact data.
    artifacts: DashMap<String, CachedArtifact>,
    /// Metadata cache + change journal. The watcher feeds file-change events
    /// into this, which downgrades confidence so `lookup()` re-hashes on
    /// next access. Without the watcher, stat-verify on every `lookup()` is
    /// the fallback (correct but slower).
    cache_system: CacheSystem,
    /// File watcher for proactive metadata invalidation.
    watcher: Mutex<Option<NotifyWatcher>>,
    /// Directories currently being watched (avoid duplicate watches).
    watched_dirs: Mutex<HashSet<PathBuf>>,
    /// Shutdown signal — shared so request handlers can trigger shutdown.
    shutdown: Arc<Notify>,
    /// Epoch seconds of last client activity (for idle timeout).
    last_activity: AtomicU64,
    /// Daemon start time (epoch seconds).
    start_time: u64,
    /// Global stats collector.
    stats: StatsCollector,
    /// Phase-level profiler for hot-path breakdown.
    profiler: PhaseProfiler,
    /// On-disk artifact cache for hardlink optimization on cache hits.
    artifact_dir: PathBuf,
    /// Temporary directory for injected depfiles.
    depfile_tmpdir: PathBuf,
    /// Ultra-fast hit cache: context_key → (clock, artifact_key_hex, timestamp).
    /// When the journal clock hasn't advanced since the last verified hit,
    /// we skip all stat/hash/depgraph work and jump straight to artifact lookup.
    fast_hit_cache: DashMap<ContextKey, FastHitEntry>,
    /// Whether the file watcher is active. Fast-hit cache is only used when
    /// the watcher is running, since we rely on it for change detection.
    watcher_active: AtomicBool,
}

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Monotonic counter ensuring each `DaemonServer` instance gets unique
/// artifact and depfile directories, even within the same process.
static SERVER_INSTANCE: AtomicU64 = AtomicU64::new(0);

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        let instance = SERVER_INSTANCE.fetch_add(1, Ordering::Relaxed);
        let artifact_dir = zccache_core::config::default_cache_dir().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).ok();

        let artifacts: DashMap<String, CachedArtifact> = DashMap::new();

        // Reload persisted artifacts from .meta sidecars so cache survives
        // daemon restarts.
        if let Ok(entries) = std::fs::read_dir(&artifact_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                    if let Ok(data) = std::fs::read(&path) {
                        if let Ok(artifact) = bincode::deserialize::<ArtifactData>(&data) {
                            let stem = path
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                            artifacts.insert(stem, CachedArtifact { artifact });
                        }
                    }
                }
            }
        }
        let loaded = artifacts.len();
        if loaded > 0 {
            tracing::info!(loaded, "restored persisted artifacts");
        }

        Ok(Self {
            listener,
            shutdown: Arc::clone(&shutdown),
            state: Arc::new(SharedState {
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                dep_graph: DepGraph::new(),
                artifacts,
                cache_system: CacheSystem::new(),
                watcher: Mutex::new(None),
                watched_dirs: Mutex::new(HashSet::new()),
                shutdown,
                last_activity: AtomicU64::new(now),
                start_time: now,
                stats: StatsCollector::new(),
                profiler: PhaseProfiler::new(),
                artifact_dir,
                depfile_tmpdir: {
                    let dir = std::env::temp_dir().join(format!(
                        "zccache-depfiles-{}-{instance}",
                        std::process::id()
                    ));
                    std::fs::create_dir_all(&dir).ok();
                    dir
                },
                fast_hit_cache: DashMap::new(),
                watcher_active: AtomicBool::new(false),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Get a snapshot of the phase profiler (for benchmarks).
    #[must_use]
    pub fn profile_snapshot(&self) -> crate::stats::ProfileSnapshot {
        self.state.profiler.snapshot()
    }

    /// Run the server, accepting connections until shutdown is signaled.
    ///
    /// `idle_timeout_secs`: if non-zero, the daemon shuts down after this many
    /// seconds with no client activity. Pass 0 to disable.
    pub async fn run(&mut self, idle_timeout_secs: u64) -> Result<(), zccache_ipc::IpcError> {
        tracing::info!("daemon server running");

        self.start_watcher_pipeline().await;

        // Start idle watchdog if timeout is configured.
        if idle_timeout_secs > 0 {
            let state = Arc::clone(&self.state);
            let timeout = idle_timeout_secs;
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let last = state.last_activity.load(Ordering::Relaxed);
                    let idle = now_secs().saturating_sub(last);
                    if idle >= timeout {
                        tracing::info!(idle_secs = idle, "idle timeout — shutting down");
                        state.shutdown.notify_one();
                        break;
                    }
                }
            });
        }

        loop {
            tokio::select! {
                result = self.listener.accept() => {
                    let conn = result?;
                    let state = Arc::clone(&self.state);
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(conn, state).await {
                            tracing::warn!("connection error: {e}");
                        }
                    });
                }
                () = self.shutdown.notified() => {
                    tracing::info!("daemon server shutting down");
                    // Drop the watcher to stop the OS thread and close channels.
                    // The settle buffer and consumer tasks will exit when their
                    // input channels close.
                    *self.state.watcher.lock().await = None;
                    return Ok(());
                }
            }
        }
    }

    /// Initialize the file watcher pipeline:
    /// `NotifyWatcher (OS thread) → SettleBuffer (tokio task) → CacheSystem consumer (tokio task)`
    async fn start_watcher_pipeline(&self) {
        let ignore = Arc::new(zccache_watcher::IgnoreFilter::default());
        let (watcher, raw_rx) = match NotifyWatcher::new(ignore) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("failed to start file watcher: {e} — running without watcher");
                return;
            }
        };

        *self.state.watcher.lock().await = Some(watcher);
        self.state.watcher_active.store(true, Ordering::Release);

        // Settle buffer: coalesces raw events into batches after a quiet period.
        let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
        let settle = SettleBuffer::default_window();
        tokio::spawn(async move {
            settle.run(raw_rx, settled_tx).await;
        });

        // Consumer: feeds settled events into CacheSystem for metadata invalidation.
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            while let Some(event) = settled_rx.recv().await {
                match event {
                    SettledEvent::Batch { changed, removed } => {
                        let count = changed.len() + removed.len();
                        if count > 0 {
                            tracing::debug!(
                                changed = changed.len(),
                                removed = removed.len(),
                                "watcher batch applied"
                            );
                            state
                                .cache_system
                                .apply_changes_with_removals(changed, removed);
                        }
                    }
                    SettledEvent::Overflow => {
                        tracing::warn!("watcher overflow — downgrading all metadata");
                        state.cache_system.apply_overflow();
                    }
                }
            }
            tracing::debug!("watcher consumer task exiting");
        });

        tracing::info!("file watcher pipeline started");
    }
}

/// Watch a directory for file changes, if not already watched.
async fn watch_directory(state: &SharedState, dir: &Path) {
    watch_directories(state, &[dir.to_path_buf()]).await;
}

/// Watch multiple directories in a single batch, acquiring locks once.
///
/// Canonicalizes all paths up front, deduplicates against already-watched set,
/// then registers all new watches in one lock acquisition.
async fn watch_directories(state: &SharedState, dirs: &[PathBuf]) {
    if dirs.is_empty() {
        return;
    }

    // Canonicalize all paths (filesystem work, no lock needed).
    let canonical: Vec<PathBuf> = dirs
        .iter()
        .filter_map(|dir| match dir.canonicalize() {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::debug!("cannot canonicalize {}: {e}", dir.display());
                None
            }
        })
        .collect();

    if canonical.is_empty() {
        return;
    }

    // Single lock acquisition: filter already-watched and register new ones.
    let mut watched = state.watched_dirs.lock().await;
    let new_dirs: Vec<PathBuf> = canonical
        .into_iter()
        .filter(|p| !watched.contains(p))
        .collect();

    if new_dirs.is_empty() {
        return;
    }

    let mut watcher_guard = state.watcher.lock().await;
    if let Some(ref mut w) = *watcher_guard {
        for dir in new_dirs {
            if let Err(e) = w.watch(&dir) {
                tracing::warn!("failed to watch {}: {e}", dir.display());
                continue;
            }
            tracing::info!("watching directory: {}", dir.display());
            watched.insert(dir);
        }
    }
}

/// Handle a single client connection.
async fn handle_connection(
    mut conn: IpcConnection,
    state: Arc<SharedState>,
) -> Result<(), zccache_ipc::IpcError> {
    loop {
        let request: Option<Request> = conn.recv().await?;
        let Some(request) = request else {
            tracing::debug!("client disconnected");
            return Ok(());
        };

        tracing::debug!(?request, "received request");
        state.last_activity.store(now_secs(), Ordering::Relaxed);

        let response = match request {
            Request::Ping => Response::Pong,
            Request::Shutdown => {
                conn.send(&Response::ShuttingDown).await?;
                state.shutdown.notify_one();
                return Ok(());
            }
            Request::Status => {
                let snap = state.stats.snapshot();
                let dg = state.dep_graph.stats();
                let artifact_count = state.artifacts.len() as u64;
                let cache_size_bytes: u64 = state
                    .artifacts
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .value()
                            .artifact
                            .outputs
                            .iter()
                            .map(|o| o.data.len() as u64)
                            .collect::<Vec<_>>()
                    })
                    .sum();
                let metadata_entries = state.cache_system.metadata().len() as u64;
                Response::Status(zccache_protocol::DaemonStatus {
                    artifact_count,
                    cache_size_bytes,
                    metadata_entries,
                    uptime_secs: now_secs().saturating_sub(state.start_time),
                    cache_hits: snap.hits,
                    cache_misses: snap.misses,
                    total_compilations: snap.compilations,
                    non_cacheable: snap.non_cacheable,
                    compile_errors: snap.compile_errors,
                    time_saved_ms: snap.time_saved_ms(),
                    total_links: snap.link_total,
                    link_hits: snap.link_hits,
                    link_misses: snap.link_misses,
                    link_non_cacheable: snap.link_non_cacheable,
                    dep_graph_contexts: dg.context_count as u64,
                    dep_graph_files: dg.file_count as u64,
                    sessions_total: snap.sessions_total,
                    sessions_active: state.sessions.active_count() as u64,
                    cache_dir: zccache_core::config::default_cache_dir()
                        .to_string_lossy()
                        .into_owned(),
                })
            }
            Request::Lookup { .. } => Response::LookupResult(zccache_protocol::LookupResult::Miss),
            Request::Store { .. } => Response::StoreResult(zccache_protocol::StoreResult::Stored),
            Request::Clear => handle_clear(&state).await,
            Request::SessionStart {
                client_pid,
                working_dir,
                compiler,
                log_file,
                track_stats,
            } => {
                state.stats.record_session();
                handle_session_start(
                    &state,
                    client_pid,
                    &working_dir,
                    &compiler,
                    log_file,
                    track_stats,
                )
                .await
            }
            Request::Compile {
                session_id,
                args,
                cwd,
                compiler,
                env,
            } => handle_compile(&state, session_id, &args, &cwd, compiler.as_deref(), env).await,
            Request::CompileEphemeral {
                client_pid,
                working_dir,
                compiler,
                args,
                cwd,
                env,
            } => {
                handle_compile_ephemeral(
                    &state,
                    client_pid,
                    &working_dir,
                    &compiler,
                    &args,
                    &cwd,
                    env,
                )
                .await
            }
            Request::SessionEnd { session_id } => {
                let sid = SessionId::from_raw(session_id);
                if let Some(session) = state.sessions.end(&sid) {
                    let stats = session.stats_tracker.map(|tracker| {
                        let f = tracker.finalize(session.created_at);
                        zccache_protocol::SessionStats {
                            duration_ms: f.duration_ms,
                            compilations: f.compilations,
                            hits: f.hits,
                            misses: f.misses,
                            non_cacheable: f.non_cacheable,
                            errors: f.errors,
                            time_saved_ms: f.time_saved_ms,
                            unique_sources: f.unique_sources,
                            bytes_read: f.bytes_read,
                            bytes_written: f.bytes_written,
                        }
                    });
                    Response::SessionEnded { stats }
                } else {
                    Response::Error {
                        message: format!("unknown session: {session_id}"),
                    }
                }
            }
            Request::LinkEphemeral {
                client_pid,
                working_dir,
                tool,
                args,
                cwd,
                env,
            } => {
                handle_link_ephemeral(&state, client_pid, &working_dir, &tool, &args, &cwd, env)
                    .await
            }
        };

        conn.send(&response).await?;
    }
}

/// Handle a Clear request: wipe all caches and reset stats.
async fn handle_clear(state: &SharedState) -> Response {
    // Snapshot counts before clearing.
    let artifacts_removed = {
        let count = state.artifacts.len() as u64;
        state.artifacts.clear();
        count
    };
    let metadata_cleared = state.cache_system.metadata().len() as u64;
    let dep_graph_contexts_cleared = state.dep_graph.stats().context_count as u64;

    // Calculate on-disk artifact size before deleting.
    let on_disk_bytes_freed = match std::fs::read_dir(&state.artifact_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.metadata().ok().map(|m| m.len()))
            .sum(),
        Err(_) => 0,
    };

    // Clear all subsystems.
    state.dep_graph.clear();
    state.cache_system.clear();
    state.fast_hit_cache.clear();
    state.system_includes.lock().await.clear();
    state.watched_dirs.lock().await.clear();

    // Reset stats and profiler.
    state.stats.reset();
    state.profiler.reset();

    // Delete on-disk artifact files.
    if let Ok(entries) = std::fs::read_dir(&state.artifact_dir) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }

    tracing::info!(
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
        "cache cleared"
    );

    Response::Cleared {
        artifacts_removed,
        metadata_cleared,
        dep_graph_contexts_cleared,
        on_disk_bytes_freed,
    }
}

/// Handle a single-roundtrip ephemeral compile: session start + compile + session end.
/// Avoids 3 IPC roundtrips for drop-in wrapper mode.
async fn handle_compile_ephemeral(
    state: &Arc<SharedState>,
    client_pid: u32,
    working_dir: &str,
    compiler: &str,
    args: &[String],
    cwd: &str,
    env: Option<Vec<(String, String)>>,
) -> Response {
    // 1. Start ephemeral session (inline, no IPC roundtrip)
    state.stats.record_session();
    let session_resp =
        handle_session_start(state, client_pid, working_dir, compiler, None, false).await;
    let session_id = match session_resp {
        Response::SessionStarted { session_id, .. } => session_id,
        Response::Error { message } => return Response::Error { message },
        other => {
            return Response::Error {
                message: format!("unexpected session start response: {other:?}"),
            };
        }
    };

    // 2. Compile (no compiler override — session compiler IS the wrapped compiler)
    let result = handle_compile(state, session_id, args, cwd, None, env).await;

    // 3. End session (best-effort, no response needed)
    let sid = SessionId::from_raw(session_id);
    state.sessions.end(&sid);

    result
}

/// Handle a single-roundtrip ephemeral link/archive request.
///
/// Parses the tool invocation, computes a cache key from the tool binary and
/// all input file hashes, and returns a cached result or runs the real tool.
async fn handle_link_ephemeral(
    state: &Arc<SharedState>,
    _client_pid: u32,
    _working_dir: &str,
    tool: &str,
    args: &[String],
    cwd: &str,
    env: Option<Vec<(String, String)>>,
) -> Response {
    use zccache_compiler::parse_archiver::{parse_archive_invocation, ParsedArchiveInvocation};

    state.stats.record_link();

    // 1. Parse the archiver invocation
    let parsed = parse_archive_invocation(tool, args);
    let archive = match parsed {
        ParsedArchiveInvocation::Cacheable(c) => c,
        ParsedArchiveInvocation::NonCacheable { reason } => {
            tracing::debug!(%reason, "link non-cacheable, passing through");
            state.stats.record_link_non_cacheable();
            return run_tool_passthrough(tool, args, cwd, env).await;
        }
    };

    // 2. Non-determinism check: warn and pass through
    if archive.non_deterministic {
        let warning = format!(
            "non-deterministic archiver invocation (missing {} flag) — skipping cache",
            match archive.family {
                zccache_compiler::parse_archiver::ArchiverFamily::MsvcLib => "/BREPRO",
                _ => "D",
            }
        );
        tracing::warn!(%warning);
        state.stats.record_link_non_cacheable();
        let result = run_tool_passthrough(tool, args, cwd, env).await;
        return match result {
            Response::LinkResult {
                exit_code,
                stdout,
                stderr,
                cached,
                ..
            } => Response::LinkResult {
                exit_code,
                stdout,
                stderr,
                cached,
                warning: Some(warning),
            },
            other => other,
        };
    }

    // 3. Hash the tool binary
    let tool_path = std::path::Path::new(tool);
    let tool_hash = match hash_file_via_cache(state, tool_path) {
        Some(h) => h,
        None => {
            tracing::warn!("cannot hash tool {tool}");
            return run_tool_passthrough(tool, args, cwd, env).await;
        }
    };

    // 4. Hash all input files
    let cwd_path = std::path::Path::new(cwd);
    let mut key_builder = zccache_hash::link_cache_key::LinkCacheKeyBuilder::new().tool(tool_hash);

    for flag in &archive.cache_relevant_flags {
        key_builder = key_builder.flag(flag);
    }

    for input in &archive.input_files {
        let input_path = if input.is_absolute() {
            input.clone()
        } else {
            cwd_path.join(input)
        };
        let input_hash = match hash_file_via_cache(state, &input_path) {
            Some(h) => h,
            None => {
                tracing::warn!(
                    "cannot hash input file {}: skipping cache",
                    input_path.display()
                );
                return run_tool_passthrough(tool, args, cwd, env).await;
            }
        };
        key_builder = key_builder.input(input_hash);
    }

    let cache_key = key_builder.build();
    let key_hex = cache_key.to_hex();

    // 5. Cache lookup
    if let Some(entry) = state.artifacts.get(&key_hex) {
        tracing::debug!(%key_hex, "link cache hit");
        state.stats.record_link_hit();

        // Write cached output to disk
        let output_path = if archive.output_file.is_absolute() {
            archive.output_file.clone()
        } else {
            cwd_path.join(&archive.output_file)
        };
        for out in &entry.artifact.outputs {
            let target = if entry.artifact.outputs.len() == 1 {
                output_path.clone()
            } else {
                output_path.parent().unwrap_or(cwd_path).join(&out.name)
            };
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Err(e) = std::fs::write(&target, &out.data) {
                tracing::warn!("failed to write cached output {}: {e}", target.display());
                return run_tool_passthrough(tool, args, cwd, env).await;
            }
        }

        return Response::LinkResult {
            exit_code: entry.artifact.exit_code,
            stdout: entry.artifact.stdout.clone(),
            stderr: entry.artifact.stderr.clone(),
            cached: true,
            warning: None,
        };
    }

    // 6. Cache miss — run the real tool
    tracing::debug!(%key_hex, "link cache miss");
    state.stats.record_link_miss();

    let result = run_tool_passthrough(tool, args, cwd, env).await;

    // 7. If successful, cache the output
    if let Response::LinkResult {
        exit_code: 0,
        ref stdout,
        ref stderr,
        ..
    } = result
    {
        let output_path = if archive.output_file.is_absolute() {
            archive.output_file.clone()
        } else {
            cwd_path.join(&archive.output_file)
        };
        if let Ok(data) = std::fs::read(&output_path) {
            let artifact = ArtifactData {
                outputs: vec![ArtifactOutput {
                    name: archive
                        .output_file
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    data,
                }],
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                exit_code: 0,
            };

            // Persist to disk
            let meta_path = state.artifact_dir.join(format!("{key_hex}.meta"));
            if let Ok(encoded) = bincode::serialize(&artifact) {
                std::fs::write(&meta_path, &encoded).ok();
            }

            state
                .artifacts
                .insert(key_hex.clone(), CachedArtifact { artifact });
            tracing::debug!(%key_hex, "link artifact cached");
        }
    }

    result
}

/// Hash a file using the metadata cache (with watcher-assisted confidence).
fn hash_file_via_cache(state: &SharedState, path: &Path) -> Option<ContentHash> {
    // Try metadata cache first (stat-verified hash)
    if let Ok(hash) = state.cache_system.metadata().lookup(path) {
        return Some(hash);
    }
    // Fall back to direct hash
    zccache_hash::hash_file(path).ok()
}

/// Run a tool directly (passthrough) and return a LinkResult response.
async fn run_tool_passthrough(
    tool: &str,
    args: &[String],
    cwd: &str,
    env: Option<Vec<(String, String)>>,
) -> Response {
    let mut cmd = std::process::Command::new(tool);
    cmd.args(args);
    cmd.current_dir(cwd);

    if let Some(env_vars) = env {
        cmd.env_clear();
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }
    }

    match cmd.output() {
        Ok(output) => Response::LinkResult {
            exit_code: output.status.code().unwrap_or(1),
            stdout: output.stdout,
            stderr: output.stderr,
            cached: false,
            warning: None,
        },
        Err(e) => Response::Error {
            message: format!("failed to run {tool}: {e}"),
        },
    }
}

/// Handle a SessionStart request: discover system includes, create session.
async fn handle_session_start(
    state: &SharedState,
    client_pid: u32,
    working_dir: &str,
    compiler: &str,
    log_file: Option<String>,
    track_stats: bool,
) -> Response {
    let compiler_path = PathBuf::from(compiler);

    // Check if compiler exists
    if !compiler_path.exists() {
        return Response::Error {
            message: format!("compiler not found: {compiler}"),
        };
    }

    // Discover system includes (cached per compiler path)
    let system_includes = {
        let mut cache = state.system_includes.lock().await;
        cache
            .get_or_discover(&compiler_path, |compiler| {
                let args = zccache_depgraph::discovery_args();
                let output = std::process::Command::new(compiler).args(&args).output();
                match output {
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        zccache_depgraph::parse_system_include_output(&stderr)
                    }
                    Err(e) => {
                        tracing::warn!("failed to run compiler for include discovery: {e}");
                        Vec::new()
                    }
                }
            })
            .to_vec()
    };

    let session_config = zccache_depgraph::SessionConfig {
        client_pid,
        working_dir: PathBuf::from(working_dir),
        compiler: compiler_path,
        system_includes: system_includes.clone(),
        log_file: log_file.map(PathBuf::from),
        track_stats,
    };

    let session_id = state.sessions.create(session_config);

    // Watch the working directory for file changes.
    watch_directory(state, &PathBuf::from(working_dir)).await;

    // Watch system include directories so the journal can track header changes
    // and enable the zero-syscall fast path for system headers.
    watch_directories(state, &system_includes).await;

    Response::SessionStarted {
        session_id: session_id.value(),
        system_includes: system_includes
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    }
}

/// Apply a mutation to the session's stats tracker (if tracking is enabled).
fn record_session_stat(
    sessions: &SessionManager,
    session_id: &SessionId,
    f: impl FnOnce(&mut zccache_depgraph::SessionStatsTracker),
) {
    sessions.mutate(session_id, |session| {
        if let Some(ref mut tracker) = session.stats_tracker {
            f(tracker);
        }
    });
}

/// Write a log line to the session's log file (if configured).
fn write_session_log(sessions: &SessionManager, session_id: &SessionId, message: &str) {
    if let Some(session) = sessions.get(session_id) {
        if let Some(ref log_path) = session.log_file {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(f, "{message}");
            }
        }
    }
}

/// Hash a file using the CacheSystem's metadata cache.
///
/// This stat-verifies the file, hashes if needed (with TOCTOU protection),
/// and caches the result. The file watcher proactively downgrades confidence
/// on changes, ensuring stale hashes are re-computed.
///
/// `clock` should be snapped once at the start of each compile request so all
/// files in a single compilation see a consistent journal clock.
fn hash_file(cache_system: &CacheSystem, path: &Path, clock: Clock) -> Result<ContentHash, String> {
    cache_system
        .lookup_since(path, clock)
        .map(|r| r.hash)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Build a CompileContext and UserDepFlags from a CacheableCompilation and session info.
fn build_compile_context(
    compilation: &zccache_compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[PathBuf],
) -> (CompileContext, UserDepFlags) {
    // Parse the original args through depgraph's parser to get structured search paths
    let parsed = zccache_depgraph::args::parse_compile_args(&compilation.original_args, cwd);
    let dep_flags = parsed.dep_flags.clone();
    let mut ctx = CompileContext::from_parsed_args(&parsed);

    // Inject session's system includes
    for path in system_includes {
        if !ctx.include_search.system.contains(path) {
            ctx.include_search.system.push(path.clone());
        }
    }

    (ctx, dep_flags)
}

/// Write cached output to disk. Optimized syscall sequence:
/// 1. Try hardlink directly (1 syscall — common case when output doesn't exist)
/// 2. If that fails, remove existing output and retry hardlink (2 syscalls)
/// 3. Fall back to fs::write from memory (1 syscall)
fn write_cached_output(out_path: &Path, cache_file: &Path, data: &[u8]) -> std::io::Result<()> {
    // Fast path: hardlink directly (works when out_path doesn't exist yet)
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        return Ok(());
    }
    // Output exists or cache file missing — remove and retry
    let _ = std::fs::remove_file(out_path);
    if std::fs::hard_link(cache_file, out_path).is_ok() {
        return Ok(());
    }
    // Hardlink failed entirely (cross-device, no cache file) — copy from memory
    std::fs::write(out_path, data)
}

/// Handle a Compile request: parse args, check depgraph, run compiler or return cached.
async fn handle_compile(
    state_arc: &Arc<SharedState>,
    session_id: u64,
    args: &[String],
    cwd: &str,
    compiler_override: Option<&str>,
    client_env: Option<Vec<(String, String)>>,
) -> Response {
    let state = state_arc.as_ref();
    let compile_start = std::time::Instant::now();
    let sid = SessionId::from_raw(session_id);
    // Snap the journal clock once so all file hashes in this request see a
    // consistent view (avoids per-file current_clock() syscalls).
    let snap_clock = state.cache_system.current_clock();

    state.stats.record_compilation();

    // Look up session
    let (session_compiler, system_includes) = match (
        state.sessions.compiler(&sid),
        state.sessions.system_includes(&sid),
    ) {
        (Some(c), Some(si)) => (c, si),
        _ => {
            return Response::Error {
                message: format!("unknown session: {session_id}"),
            };
        }
    };

    // Use per-request compiler override if provided (e.g., `wrap gcc` on a g++ session),
    // otherwise fall back to the session compiler.
    let compiler = match compiler_override {
        Some(path) => PathBuf::from(path),
        None => session_compiler,
    };

    state.sessions.touch(&sid);

    // ── Phase: parse args ────────────────────────────────────────────
    let t0 = std::time::Instant::now();
    let parsed = zccache_compiler::parse_invocation(compiler.to_str().unwrap_or(""), args);
    let compilation = match parsed {
        zccache_compiler::ParsedInvocation::Cacheable(c) => c,
        zccache_compiler::ParsedInvocation::NonCacheable { reason } => {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, &sid, |t| t.record_non_cacheable());
            write_session_log(&state.sessions, &sid, &format!("non-cacheable: {reason}"));
            return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid, &client_env)
                .await;
        }
        zccache_compiler::ParsedInvocation::MultiFile {
            compilations,
            original_args: _,
        } => {
            return handle_compile_multi(
                Arc::clone(state_arc),
                sid,
                compiler,
                compilations,
                cwd.to_string(),
                system_includes,
                client_env,
            )
            .await;
        }
    };
    let parse_args_us = t0.elapsed().as_micros() as u64;

    let cwd_path = PathBuf::from(cwd);
    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file)
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file)
    };

    // ── Phase: build context + register ──────────────────────────────
    let t1 = std::time::Instant::now();
    let (ctx, dep_flags) = build_compile_context(&compilation, &cwd_path, &system_includes);
    let context_key = state.dep_graph.register(ctx.clone());
    let build_context_us = t1.elapsed().as_micros() as u64;

    // ── Ultra-fast path: clock-based skip ────────────────────────────
    // If the watcher is active and the journal clock hasn't advanced since
    // our last verified hit for this context, skip ALL hash/depgraph work
    // and reuse the stored artifact key directly.
    if state.watcher_active.load(Ordering::Acquire) {
        if let Some(entry) = state.fast_hit_cache.get(&context_key) {
            let current_clock = state.cache_system.current_clock();
            if entry.clock == current_clock && entry.cached_at.elapsed() < FAST_HIT_MAX_AGE {
                let artifact_key_hex = &entry.artifact_key_hex;
                let t5 = std::time::Instant::now();
                let cached = state.artifacts.get(artifact_key_hex).map(|r| r.clone());
                let artifact_lookup_us = t5.elapsed().as_micros() as u64;

                if let Some(cached) = cached {
                    let t6 = std::time::Instant::now();
                    for (i, output) in cached.artifact.outputs.iter().enumerate() {
                        let out_path = if i == 0 {
                            output_path.clone()
                        } else {
                            cwd_path.join(&output.name)
                        };
                        let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                        if let Err(e) = write_cached_output(&out_path, &cache_file, &output.data) {
                            return Response::Error {
                                message: format!(
                                    "failed to write cached output {}: {e}",
                                    out_path.display()
                                ),
                            };
                        }
                    }
                    let write_output_us = t6.elapsed().as_micros() as u64;

                    let t7 = std::time::Instant::now();
                    let latency_us = compile_start.elapsed().as_micros() as u64;
                    let artifact_bytes: u64 = cached
                        .artifact
                        .outputs
                        .iter()
                        .map(|o| o.data.len() as u64)
                        .sum();
                    state.stats.record_hit(latency_us, artifact_bytes);
                    let src = source_path.clone();
                    record_session_stat(&state.sessions, &sid, move |t| {
                        t.record_hit(src, latency_us, artifact_bytes);
                    });
                    write_session_log(
                        &state.sessions,
                        &sid,
                        &format!(
                            "cache hit (fast): {} -> {}",
                            source_path.display(),
                            output_path.display()
                        ),
                    );
                    let bookkeeping_us = t7.elapsed().as_micros() as u64;

                    let total_us = compile_start.elapsed().as_micros() as u64;
                    state.profiler.record_hit(&HitPhases {
                        parse_args_us,
                        build_context_us,
                        hash_source_us: 0,
                        hash_headers_us: 0,
                        depgraph_check_us: 0,
                        artifact_lookup_us,
                        write_output_us,
                        bookkeeping_us,
                        total_us,
                    });

                    return Response::CompileResult {
                        exit_code: cached.artifact.exit_code,
                        stdout: cached.artifact.stdout.clone(),
                        stderr: cached.artifact.stderr.clone(),
                        cached: true,
                    };
                }
            }
        }
    }

    // ── Slow path: hash + depgraph verify ────────────────────────────

    // ── Phase: hash source ───────────────────────────────────────────
    let t2 = std::time::Instant::now();
    let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();
    match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => {
            hash_map.insert(source_path.clone(), h);
        }
        Err(e) => {
            write_session_log(
                &state.sessions,
                &sid,
                &format!("cache key error: {e}, falling back to direct compile"),
            );
            return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid, &client_env)
                .await;
        }
    }
    let hash_source_us = t2.elapsed().as_micros() as u64;

    // ── Phase: hash headers ──────────────────────────────────────────
    let t3 = std::time::Instant::now();
    if let Some(includes) = state.dep_graph.get_includes(&context_key) {
        for header in &includes {
            match hash_file(&state.cache_system, header, snap_clock) {
                Ok(h) => {
                    hash_map.insert(header.clone(), h);
                }
                Err(e) => {
                    write_session_log(
                        &state.sessions,
                        &sid,
                        &format!("[DIAG] header_hash_fail: {} error={e}", header.display()),
                    );
                }
            }
        }
    }
    let hash_headers_us = t3.elapsed().as_micros() as u64;

    // ── Phase: depgraph check ────────────────────────────────────────
    let t4 = std::time::Instant::now();
    let (verdict, diag_reason) = {
        let is_fresh = |_: &Path| true;
        let get_hash = |p: &Path| hash_map.get(p).copied();
        state
            .dep_graph
            .check_diagnostic(&context_key, is_fresh, get_hash)
    };
    let depgraph_check_us = t4.elapsed().as_micros() as u64;
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "[DIAG] depgraph_check: {} -> {} verdict={} reason={}",
            source_path.display(),
            output_path.display(),
            match &verdict {
                zccache_depgraph::CacheVerdict::Hit { .. } => "Hit",
                zccache_depgraph::CacheVerdict::SourceChanged { .. } => "SourceChanged",
                zccache_depgraph::CacheVerdict::HeadersChanged { .. } => "HeadersChanged",
                zccache_depgraph::CacheVerdict::Cold => "Cold",
                zccache_depgraph::CacheVerdict::NeedsPreprocessor => "NeedsPreprocessor",
            },
            diag_reason,
        ),
    );
    match verdict {
        zccache_depgraph::CacheVerdict::Hit { artifact_key }
        | zccache_depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            // ── Phase: artifact lookup ────────────────────────────────
            let t5 = std::time::Instant::now();
            let artifact_key_hex = artifact_key.hash().to_hex();
            let cached = state.artifacts.get(&artifact_key_hex).map(|r| r.clone());
            let artifact_lookup_us = t5.elapsed().as_micros() as u64;

            if let Some(cached) = cached {
                // ── Phase: write output ──────────────────────────────
                let t6 = std::time::Instant::now();
                for (i, output) in cached.artifact.outputs.iter().enumerate() {
                    let out_path = if i == 0 {
                        output_path.clone()
                    } else {
                        cwd_path.join(&output.name)
                    };
                    let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                    if let Err(e) = write_cached_output(&out_path, &cache_file, &output.data) {
                        return Response::Error {
                            message: format!(
                                "failed to write cached output {}: {e}",
                                out_path.display()
                            ),
                        };
                    }
                }
                let write_output_us = t6.elapsed().as_micros() as u64;

                // ── Phase: bookkeeping ───────────────────────────────
                let t7 = std::time::Instant::now();
                let latency_us = compile_start.elapsed().as_micros() as u64;
                let artifact_bytes: u64 = cached
                    .artifact
                    .outputs
                    .iter()
                    .map(|o| o.data.len() as u64)
                    .sum();
                state.stats.record_hit(latency_us, artifact_bytes);
                let src = source_path.clone();
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_hit(src, latency_us, artifact_bytes);
                });
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!(
                        "cache hit: {} -> {}",
                        source_path.display(),
                        output_path.display()
                    ),
                );
                let bookkeeping_us = t7.elapsed().as_micros() as u64;

                // Populate fast-hit cache for future requests
                let current_clock = state.cache_system.current_clock();
                state.fast_hit_cache.insert(
                    context_key,
                    FastHitEntry {
                        clock: current_clock,
                        artifact_key_hex: artifact_key_hex.clone(),
                        cached_at: std::time::Instant::now(),
                    },
                );

                // Record phase profile
                let total_us = compile_start.elapsed().as_micros() as u64;
                state.profiler.record_hit(&HitPhases {
                    parse_args_us,
                    build_context_us,
                    hash_source_us,
                    hash_headers_us,
                    depgraph_check_us,
                    artifact_lookup_us,
                    write_output_us,
                    bookkeeping_us,
                    total_us,
                });

                return Response::CompileResult {
                    exit_code: cached.artifact.exit_code,
                    stdout: cached.artifact.stdout.clone(),
                    stderr: cached.artifact.stderr.clone(),
                    cached: true,
                };
            }
            // Artifact key computed but no artifact stored yet — fall through to compile
            write_session_log(
                &state.sessions,
                &sid,
                &format!("[DIAG] artifact_not_found: key={artifact_key_hex}"),
            );
        }
        zccache_depgraph::CacheVerdict::Cold
        | zccache_depgraph::CacheVerdict::HeadersChanged { .. }
        | zccache_depgraph::CacheVerdict::NeedsPreprocessor => {
            // Need to compile and scan includes
        }
    }

    // Cache miss — invalidate fast-hit cache for this context
    state.fast_hit_cache.remove(&context_key);

    // Cache miss — run the compiler
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "cache miss: {} -> {} (reason: {diag_reason})",
            source_path.display(),
            output_path.display()
        ),
    );

    // ── Phase: compiler exec (with depfile injection) ────────────────
    let t_exec = std::time::Instant::now();
    let supports_depfile = compilation.family.supports_depfile();
    let (extra_args, depfile_strategy) = zccache_depgraph::depfile::prepare_depfile(
        supports_depfile,
        &dep_flags,
        &output_path,
        &state.depfile_tmpdir,
    );

    let mut cmd = tokio::process::Command::new(&compiler);
    cmd.args(args).current_dir(cwd);
    if !extra_args.is_empty() {
        cmd.args(&extra_args);
    }
    apply_client_env(&mut cmd, &client_env);
    let result = cmd.output().await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };
    let compiler_exec_us = t_exec.elapsed().as_micros() as u64;

    let exit_code = output.status.code().unwrap_or(-1);

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
    }

    // Only cache successful compilations
    if exit_code == 0 {
        // Read the output file
        let output_data = match std::fs::read(&output_path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("failed to read output file {}: {e}", output_path.display());
                return Response::CompileResult {
                    exit_code,
                    stdout: output.stdout,
                    stderr: output.stderr,
                    cached: false,
                };
            }
        };

        // ── Phase: include scan (depfile or fallback) ────────────────
        let t_scan = std::time::Instant::now();
        let scan_result = match &depfile_strategy {
            DepfileStrategy::Injected { path }
            | DepfileStrategy::UserSpecified { path }
            | DepfileStrategy::UserDefault { path } => {
                let cwd_path = PathBuf::from(cwd);
                match zccache_depgraph::depfile::parse_depfile_path(path, &source_path, &cwd_path) {
                    Ok(result) => {
                        if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                            let _ = std::fs::remove_file(path);
                        }
                        result
                    }
                    Err(e) => {
                        tracing::warn!("depfile parse failed, falling back to scanner: {e}");
                        write_session_log(
                            &state.sessions,
                            &sid,
                            &format!(
                                "[DIAG] depfile_parse_fail: path={} error={e}",
                                path.display()
                            ),
                        );
                        if matches!(depfile_strategy, DepfileStrategy::Injected { .. }) {
                            let _ = std::fs::remove_file(path);
                        }
                        zccache_depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
                    }
                }
            }
            DepfileStrategy::Unsupported => {
                zccache_depgraph::scanner::scan_recursive(&source_path, &ctx.include_search)
            }
        };
        let include_scan_us = t_scan.elapsed().as_micros() as u64;

        // Register scanned paths for zero-syscall fast path on future hits.
        let tracked_paths: Vec<PathBuf> = std::iter::once(source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .collect();
        state.cache_system.register_tracked(&tracked_paths);

        // Watch parent directories of discovered headers so watcher events
        // keep the journal accurate and enable the zero-syscall fast path.
        {
            let header_dirs: Vec<PathBuf> = {
                let mut dirs = HashSet::new();
                for header in &scan_result.resolved {
                    if let Some(parent) = header.parent() {
                        dirs.insert(parent.to_path_buf());
                    }
                }
                dirs.into_iter().collect()
            };
            watch_directories(state, &header_dirs).await;
        }

        // ── Phase: hash all files ────────────────────────────────────
        let t_hash = std::time::Instant::now();
        let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();
        if let Ok(h) = hash_file(&state.cache_system, &source_path, snap_clock) {
            hash_map.insert(source_path.clone(), h);
        }
        for header in &scan_result.resolved {
            if let Ok(h) = hash_file(&state.cache_system, header, snap_clock) {
                hash_map.insert(header.clone(), h);
            }
        }
        let hash_all_us = t_hash.elapsed().as_micros() as u64;

        // ── Phase: store artifact ────────────────────────────────────
        let t_store = std::time::Instant::now();
        let get_hash = |p: &Path| hash_map.get(p).copied();
        let include_count = scan_result.resolved.len();
        if let Some(artifact_key) = state.dep_graph.update(&context_key, scan_result, get_hash) {
            let artifact_key_hex = artifact_key.hash().to_hex();
            write_session_log(
                &state.sessions,
                &sid,
                &format!(
                    "[DIAG] update: {} artifact_key={} includes={include_count}",
                    source_path.display(),
                    &artifact_key_hex[..8],
                ),
            );
            let artifact = ArtifactData {
                outputs: vec![ArtifactOutput {
                    name: output_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    data: output_data,
                }],
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                exit_code,
            };

            // Persist outputs to on-disk cache for hardlink optimization.
            for (i, out) in artifact.outputs.iter().enumerate() {
                let cache_path = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                let _ = std::fs::write(&cache_path, &out.data);
            }

            // Persist .meta sidecar so artifacts survive daemon restarts.
            let meta_path = state.artifact_dir.join(format!("{artifact_key_hex}.meta"));
            if let Ok(meta_bytes) = bincode::serialize(&artifact) {
                let _ = std::fs::write(&meta_path, &meta_bytes);
            }

            let artifact_bytes: u64 = artifact.outputs.iter().map(|o| o.data.len() as u64).sum();
            state
                .artifacts
                .insert(artifact_key_hex, CachedArtifact { artifact });

            let latency_us = compile_start.elapsed().as_micros() as u64;
            state.stats.record_miss(latency_us, artifact_bytes);
            let src = source_path.clone();
            record_session_stat(&state.sessions, &sid, move |t| {
                t.record_miss(src, artifact_bytes);
            });
        }
        let artifact_store_us = t_store.elapsed().as_micros() as u64;

        // Record miss phase profile
        let total_us = compile_start.elapsed().as_micros() as u64;
        state.profiler.record_miss(&MissPhases {
            compiler_exec_us,
            include_scan_us,
            hash_all_us,
            artifact_store_us,
            total_us,
        });
    }

    Response::CompileResult {
        exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
        cached: false,
    }
}

/// Apply client environment variables to a compiler command.
/// If `client_env` is `Some`, clears the inherited env and sets only the client's vars.
fn apply_client_env(cmd: &mut tokio::process::Command, client_env: &Option<Vec<(String, String)>>) {
    if let Some(vars) = client_env {
        cmd.env_clear();
        for (key, val) in vars {
            cmd.env(key, val);
        }
    }
}

/// A deferred output write for a cache hit.
struct PendingWrite {
    out_path: PathBuf,
    cache_file: PathBuf,
    data: Vec<u8>,
}

/// Result of a per-unit cache check in multi-file compile.
enum UnitCacheResult {
    /// Cache hit — output write is deferred for batching.
    Hit {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        artifact_bytes: u64,
        source_path: PathBuf,
        pending_writes: Vec<PendingWrite>,
    },
    /// Cache miss — needs compilation.
    Miss {
        source_path: PathBuf,
        output_path: PathBuf,
        context_key: ContextKey,
        ctx: Box<CompileContext>,
    },
}

/// Check cache for a single compilation unit. Returns Hit (output written) or Miss.
fn check_unit_cache(
    state: &SharedState,
    compilation: &zccache_compiler::CacheableCompilation,
    cwd_path: &Path,
    system_includes: &[PathBuf],
) -> UnitCacheResult {
    let t0 = std::time::Instant::now();
    let snap_clock = state.cache_system.current_clock();
    state.stats.record_compilation();

    let source_path = if compilation.source_file.is_absolute() {
        compilation.source_file.clone()
    } else {
        cwd_path.join(&compilation.source_file)
    };
    let output_path = if compilation.output_file.is_absolute() {
        compilation.output_file.clone()
    } else {
        cwd_path.join(&compilation.output_file)
    };

    let (ctx, _dep_flags) = build_compile_context(compilation, cwd_path, system_includes);
    let t_ctx = t0.elapsed();
    let context_key = state.dep_graph.register(ctx.clone());
    let t_register = t0.elapsed();

    // Hash source
    let source_hash = match hash_file(&state.cache_system, &source_path, snap_clock) {
        Ok(h) => h,
        Err(_) => {
            return UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx: Box::new(ctx),
            };
        }
    };
    let t_hash_source = t0.elapsed();

    // Hash known headers
    let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();
    hash_map.insert(source_path.clone(), source_hash);
    if let Some(includes) = state.dep_graph.get_includes(&context_key) {
        for header in &includes {
            if let Ok(h) = hash_file(&state.cache_system, header, snap_clock) {
                hash_map.insert(header.clone(), h);
            }
        }
    }
    let t_hash_headers = t0.elapsed();

    // Depgraph check
    let verdict = {
        let is_fresh = |_: &Path| true;
        let get_hash = |p: &Path| hash_map.get(p).copied();
        state.dep_graph.check(&context_key, is_fresh, get_hash)
    };
    let t_depgraph = t0.elapsed();

    // Try to serve from cache
    if let zccache_depgraph::CacheVerdict::Hit { artifact_key }
    | zccache_depgraph::CacheVerdict::SourceChanged { artifact_key } = verdict
    {
        let artifact_key_hex = artifact_key.hash().to_hex();
        if let Some(cached) = state.artifacts.get(&artifact_key_hex).map(|r| r.clone()) {
            let t_lookup = t0.elapsed();
            // Build deferred writes instead of writing immediately
            let mut pending = Vec::with_capacity(cached.artifact.outputs.len());
            for (i, output) in cached.artifact.outputs.iter().enumerate() {
                let out_path = if i == 0 {
                    output_path.clone()
                } else {
                    cwd_path.join(&output.name)
                };
                let cache_file = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                pending.push(PendingWrite {
                    out_path,
                    cache_file,
                    data: output.data.clone(),
                });
            }

            let artifact_bytes: u64 = cached
                .artifact
                .outputs
                .iter()
                .map(|o| o.data.len() as u64)
                .sum();
            state.stats.record_hit(0, artifact_bytes);

            // Profile output (write_output is now deferred, so 0)
            let total_us = t0.elapsed().as_micros() as u64;
            state.profiler.record_hit(&HitPhases {
                parse_args_us: 0,
                build_context_us: t_ctx.as_micros() as u64,
                hash_source_us: (t_hash_source - t_register).as_micros() as u64,
                hash_headers_us: (t_hash_headers - t_hash_source).as_micros() as u64,
                depgraph_check_us: (t_depgraph - t_hash_headers).as_micros() as u64,
                artifact_lookup_us: (t_lookup - t_depgraph).as_micros() as u64,
                write_output_us: 0,
                bookkeeping_us: 0,
                total_us,
            });

            return UnitCacheResult::Hit {
                stdout: cached.artifact.stdout.clone(),
                stderr: cached.artifact.stderr.clone(),
                artifact_bytes,
                source_path,
                pending_writes: pending,
            };
        }
    }

    state.fast_hit_cache.remove(&context_key);
    UnitCacheResult::Miss {
        source_path,
        output_path,
        context_key,
        ctx: Box::new(ctx),
    }
}

/// Handle a multi-file compile: check cache per-unit in parallel, serve hits, batch misses.
#[allow(clippy::too_many_arguments)]
async fn handle_compile_multi(
    state: Arc<SharedState>,
    sid: SessionId,
    compiler: PathBuf,
    compilations: Vec<zccache_compiler::CacheableCompilation>,
    cwd: String,
    system_includes: Vec<PathBuf>,
    client_env: Option<Vec<(String, String)>>,
) -> Response {
    let cwd_path = PathBuf::from(&cwd);
    let snap_clock = state.cache_system.current_clock();
    let mut all_stdout = Vec::new();
    let mut all_stderr = Vec::new();

    // ── Phase 1: Check cache for each unit (parallel, as-completed) ──
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, compilation) in compilations.iter().enumerate() {
        let state = Arc::clone(&state);
        let cwd_path = cwd_path.clone();
        let system_includes = system_includes.clone();
        let compilation = compilation.clone();
        join_set.spawn_blocking(move || {
            (
                idx,
                check_unit_cache(&state, &compilation, &cwd_path, &system_includes),
            )
        });
    }

    // Collect results in original order
    let mut indexed_results: Vec<(usize, UnitCacheResult)> = Vec::with_capacity(compilations.len());
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(pair) => indexed_results.push(pair),
            Err(e) => {
                return Response::Error {
                    message: format!("cache check task panicked: {e}"),
                };
            }
        }
    }
    indexed_results.sort_by_key(|(idx, _)| *idx);

    let mut unit_results: Vec<UnitCacheResult> = Vec::with_capacity(indexed_results.len());
    let mut all_pending_writes: Vec<PendingWrite> = Vec::new();
    for (_, mut result) in indexed_results {
        match &result {
            UnitCacheResult::Hit {
                stdout,
                stderr,
                artifact_bytes,
                source_path,
                ..
            } => {
                all_stdout.extend_from_slice(stdout);
                all_stderr.extend_from_slice(stderr);
                let src = source_path.clone();
                let bytes = *artifact_bytes;
                record_session_stat(&state.sessions, &sid, move |t| {
                    t.record_hit(src, 0, bytes);
                });
            }
            UnitCacheResult::Miss { source_path, .. } => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("multi-file cache miss: {}", source_path.display()),
                );
            }
        }
        // Drain pending writes from hits for batched parallel execution
        if let UnitCacheResult::Hit {
            ref mut pending_writes,
            ..
        } = result
        {
            all_pending_writes.append(pending_writes);
        }
        unit_results.push(result);
    }

    // ── Phase 1b: Execute all output writes in parallel ─────────────
    if !all_pending_writes.is_empty() {
        let mut write_set = tokio::task::JoinSet::new();
        for pw in all_pending_writes {
            write_set.spawn_blocking(move || {
                let _ = write_cached_output(&pw.out_path, &pw.cache_file, &pw.data);
            });
        }
        while write_set.join_next().await.is_some() {}
    }

    let miss_sources: Vec<&PathBuf> = unit_results
        .iter()
        .filter_map(|r| match r {
            UnitCacheResult::Miss { source_path, .. } => Some(source_path),
            UnitCacheResult::Hit { .. } => None,
        })
        .collect();

    if miss_sources.is_empty() {
        return Response::CompileResult {
            exit_code: 0,
            stdout: all_stdout,
            stderr: all_stderr,
            cached: true,
        };
    }

    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "multi-file: compiling {} of {} files",
            miss_sources.len(),
            compilations.len()
        ),
    );

    // Build compiler args: -c <miss sources...> <shared flags>
    // Inject -MD for depfile generation if compiler supports it.
    let supports_depfile = compilations[0].family.supports_depfile();
    let shared_flags = &compilations[0].cache_relevant_args;
    let mut compiler_args: Vec<String> = vec!["-c".to_string()];
    for src in &miss_sources {
        compiler_args.push(src.to_string_lossy().into_owned());
    }
    compiler_args.extend(shared_flags.iter().cloned());
    if supports_depfile {
        compiler_args.push("-MD".to_string());
    }

    let mut cmd = tokio::process::Command::new(&compiler);
    cmd.args(&compiler_args).current_dir(&cwd);
    apply_client_env(&mut cmd, &client_env);
    let result = cmd.output().await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };

    let exit_code = output.status.code().unwrap_or(-1);
    all_stdout.extend_from_slice(&output.stdout);
    all_stderr.extend_from_slice(&output.stderr);

    if exit_code != 0 {
        state.stats.record_error();
        record_session_stat(&state.sessions, &sid, |t| t.record_error());
        return Response::CompileResult {
            exit_code,
            stdout: all_stdout,
            stderr: all_stderr,
            cached: false,
        };
    }

    // ── Phase 3: Cache each miss result individually ─────────────────
    for unit in &unit_results {
        let (source_path, output_path, context_key, ctx) = match unit {
            UnitCacheResult::Miss {
                source_path,
                output_path,
                context_key,
                ctx,
            } => (source_path, output_path, context_key, ctx),
            UnitCacheResult::Hit { .. } => continue,
        };

        let output_data = match std::fs::read(output_path) {
            Ok(data) => data,
            Err(_) => continue,
        };

        // Scan includes: use depfile if available, fall back to scanner.
        let scan_result = if supports_depfile {
            let d_path = source_path.with_extension("d");
            // Multi-file -MD places .d files relative to the source
            let cwd_d_path = cwd_path.join(
                d_path
                    .file_name()
                    .unwrap_or_else(|| std::ffi::OsStr::new("deps.d")),
            );
            let depfile_path = if d_path.exists() {
                d_path
            } else if cwd_d_path.exists() {
                cwd_d_path
            } else {
                // Try deriving from source file stem in cwd
                let stem = source_path
                    .file_stem()
                    .unwrap_or_else(|| std::ffi::OsStr::new("out"));
                cwd_path.join(stem).with_extension("d")
            };
            match zccache_depgraph::depfile::parse_depfile_path(
                &depfile_path,
                source_path,
                &cwd_path,
            ) {
                Ok(result) => {
                    let _ = std::fs::remove_file(&depfile_path);
                    result
                }
                Err(e) => {
                    tracing::warn!(
                        "multi-file depfile parse failed for {}: {e}",
                        source_path.display()
                    );
                    zccache_depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
                }
            }
        } else {
            zccache_depgraph::scanner::scan_recursive(source_path, &ctx.include_search)
        };

        let tracked_paths: Vec<PathBuf> = std::iter::once(source_path.clone())
            .chain(scan_result.resolved.iter().cloned())
            .collect();
        state.cache_system.register_tracked(&tracked_paths);

        // Watch parent directories of discovered headers.
        {
            let header_dirs: Vec<PathBuf> = {
                let mut dirs = HashSet::new();
                for header in &scan_result.resolved {
                    if let Some(parent) = header.parent() {
                        dirs.insert(parent.to_path_buf());
                    }
                }
                dirs.into_iter().collect()
            };
            watch_directories(&state, &header_dirs).await;
        }

        // Hash all files
        let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();
        if let Ok(h) = hash_file(&state.cache_system, source_path, snap_clock) {
            hash_map.insert(source_path.clone(), h);
        }
        for header in &scan_result.resolved {
            if let Ok(h) = hash_file(&state.cache_system, header, snap_clock) {
                hash_map.insert(header.clone(), h);
            }
        }

        // Store artifact
        let get_hash = |p: &Path| hash_map.get(p).copied();
        let update_result = state.dep_graph.update(context_key, scan_result, get_hash);
        if let Some(artifact_key) = update_result {
            let artifact = ArtifactData {
                outputs: vec![ArtifactOutput {
                    name: output_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    data: output_data,
                }],
                stdout: Vec::new(),
                stderr: Vec::new(),
                exit_code: 0,
            };

            let artifact_key_hex = artifact_key.hash().to_hex();
            for (i, out) in artifact.outputs.iter().enumerate() {
                let cache_path = state.artifact_dir.join(format!("{artifact_key_hex}_{i}"));
                let _ = std::fs::write(&cache_path, &out.data);
            }

            // Persist .meta sidecar so artifacts survive daemon restarts.
            let meta_path = state.artifact_dir.join(format!("{artifact_key_hex}.meta"));
            if let Ok(meta_bytes) = bincode::serialize(&artifact) {
                let _ = std::fs::write(&meta_path, &meta_bytes);
            }

            let artifact_bytes: u64 = artifact.outputs.iter().map(|o| o.data.len() as u64).sum();
            state
                .artifacts
                .insert(artifact_key_hex, CachedArtifact { artifact });

            state.stats.record_miss(0, artifact_bytes);
            let src = source_path.clone();
            record_session_stat(&state.sessions, &sid, move |t| {
                t.record_miss(src, artifact_bytes);
            });
        }
    }

    Response::CompileResult {
        exit_code: 0,
        stdout: all_stdout,
        stderr: all_stderr,
        cached: false,
    }
}

/// Run the compiler directly without caching.
async fn run_compiler_direct(
    compiler: &PathBuf,
    args: &[String],
    cwd: &str,
    sessions: &SessionManager,
    sid: &SessionId,
    client_env: &Option<Vec<(String, String)>>,
) -> Response {
    let mut cmd = tokio::process::Command::new(compiler);
    cmd.args(args).current_dir(cwd);
    apply_client_env(&mut cmd, client_env);
    let result = cmd.output().await;

    match result {
        Ok(output) => {
            let exit_code = output.status.code().unwrap_or(-1);
            write_session_log(
                sessions,
                sid,
                &format!("direct compile: exit_code={exit_code}"),
            );
            Response::CompileResult {
                exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                cached: false,
            }
        }
        Err(e) => Response::Error {
            message: format!("failed to run compiler: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_ping_pong() {
        let endpoint = zccache_ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let shutdown = server.shutdown_handle();

        let server_task = tokio::spawn(async move {
            server.run(0).await.unwrap();
        });

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Ping).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::Pong));

        shutdown.notify_one();
        server_task.await.unwrap();
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_shutdown_request() {
        let endpoint = zccache_ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let shutdown = server.shutdown_handle();

        let server_task = tokio::spawn(async move {
            server.run(0).await.unwrap();
        });

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Shutdown).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert_eq!(resp, Some(Response::ShuttingDown));

        shutdown.notify_one();
        server_task.await.unwrap();
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_clear_empty() {
        let endpoint = zccache_ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let shutdown = server.shutdown_handle();

        let server_task = tokio::spawn(async move {
            server.run(0).await.unwrap();
        });

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Clear).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        match resp {
            Some(Response::Cleared {
                metadata_cleared,
                dep_graph_contexts_cleared,
                ..
            }) => {
                // artifacts_removed may be >0 if persistent cache has entries
                // from a prior run. Metadata and dep graph are always fresh.
                assert_eq!(metadata_cleared, 0);
                assert_eq!(dep_graph_contexts_cleared, 0);
            }
            other => panic!("expected Cleared, got: {other:?}"),
        }

        shutdown.notify_one();
        server_task.await.unwrap();
    }

    #[tokio::test]
    #[ignore] // integration-level: starts real daemon with IPC + file watcher
    async fn test_server_status() {
        let endpoint = zccache_ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind(&endpoint).unwrap();
        let shutdown = server.shutdown_handle();

        let server_task = tokio::spawn(async move {
            server.run(0).await.unwrap();
        });

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert!(matches!(resp, Some(Response::Status(_))));

        shutdown.notify_one();
        server_task.await.unwrap();
    }
}
