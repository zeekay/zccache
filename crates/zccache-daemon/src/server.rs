//! Daemon server — accepts IPC connections and handles requests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use zccache_depgraph::{CompileContext, DepGraph, SessionId, SessionManager, SystemIncludeCache};
use zccache_fscache::CacheSystem;
use zccache_hash::ContentHash;
use zccache_ipc::{IpcConnection, IpcListener};
use zccache_protocol::{ArtifactData, ArtifactOutput, Request, Response};
use zccache_watcher::{NotifyWatcher, SettleBuffer, SettledEvent};

use crate::stats::StatsCollector;

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
    artifacts: Mutex<HashMap<String, CachedArtifact>>,
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

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        let shutdown = Arc::new(Notify::new());
        let now = now_secs();
        Ok(Self {
            listener,
            shutdown: Arc::clone(&shutdown),
            state: Arc::new(SharedState {
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                dep_graph: DepGraph::new(),
                artifacts: Mutex::new(HashMap::new()),
                cache_system: CacheSystem::new(),
                watcher: Mutex::new(None),
                watched_dirs: Mutex::new(HashSet::new()),
                shutdown,
                last_activity: AtomicU64::new(now),
                start_time: now,
                stats: StatsCollector::new(),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
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
    let canonical = match dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!("cannot canonicalize {}: {e}", dir.display());
            return;
        }
    };

    let mut watched = state.watched_dirs.lock().await;
    if watched.contains(&canonical) {
        return;
    }

    let mut watcher_guard = state.watcher.lock().await;
    if let Some(ref mut w) = *watcher_guard {
        if let Err(e) = w.watch(&canonical) {
            tracing::warn!("failed to watch {}: {e}", canonical.display());
            return;
        }
        tracing::info!("watching directory: {}", canonical.display());
        watched.insert(canonical);
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
                let artifacts = state.artifacts.lock().await;
                let artifact_count = artifacts.len() as u64;
                let cache_size_bytes: u64 = artifacts
                    .values()
                    .flat_map(|c| &c.artifact.outputs)
                    .map(|o| o.data.len() as u64)
                    .sum();
                drop(artifacts);
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
            } => handle_compile(&state, session_id, &args, &cwd).await,
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
        };

        conn.send(&response).await?;
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
fn hash_file(cache_system: &CacheSystem, path: &Path) -> Result<ContentHash, String> {
    cache_system
        .metadata()
        .lookup(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Build a CompileContext from a CacheableCompilation and session info.
fn build_compile_context(
    compilation: &zccache_compiler::CacheableCompilation,
    cwd: &Path,
    system_includes: &[PathBuf],
) -> CompileContext {
    // Parse the original args through depgraph's parser to get structured search paths
    let parsed = zccache_depgraph::args::parse_compile_args(&compilation.original_args, cwd);
    let mut ctx = CompileContext::from_parsed_args(&parsed);

    // Inject session's system includes
    for path in system_includes {
        if !ctx.include_search.system.contains(path) {
            ctx.include_search.system.push(path.clone());
        }
    }

    ctx
}

/// Handle a Compile request: parse args, check depgraph, run compiler or return cached.
async fn handle_compile(
    state: &SharedState,
    session_id: u64,
    args: &[String],
    cwd: &str,
) -> Response {
    let compile_start = std::time::Instant::now();
    let sid = SessionId::from_raw(session_id);

    state.stats.record_compilation();

    // Look up session
    let (compiler, system_includes) = match (
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

    state.sessions.touch(&sid);

    // Parse the args to find source file, output file, and cache-relevant args
    let parsed = zccache_compiler::parse_invocation(compiler.to_str().unwrap_or(""), args);
    let compilation = match parsed {
        zccache_compiler::ParsedInvocation::Cacheable(c) => c,
        zccache_compiler::ParsedInvocation::NonCacheable { reason } => {
            state.stats.record_non_cacheable();
            record_session_stat(&state.sessions, &sid, |t| t.record_non_cacheable());
            write_session_log(&state.sessions, &sid, &format!("non-cacheable: {reason}"));
            return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid).await;
        }
    };

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

    // Build CompileContext and register with depgraph
    let ctx = build_compile_context(&compilation, &cwd_path, &system_includes);
    let context_key = state.dep_graph.register(ctx.clone());

    // Check depgraph for cache verdict.
    // is_fresh always returns true: content hashing via CacheSystem is ground truth.
    // The watcher helps CacheSystem know when to re-hash (by downgrading confidence),
    // but correctness doesn't depend on is_fresh — the artifact key comparison is
    // what determines hit/miss.
    let verdict = {
        let is_fresh = |_: &Path| true;

        let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();

        // Hash source file
        match hash_file(&state.cache_system, &source_path) {
            Ok(h) => {
                hash_map.insert(source_path.clone(), h);
            }
            Err(e) => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("cache key error: {e}, falling back to direct compile"),
                );
                return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid).await;
            }
        }

        // Hash all known headers (from previous scan, if any)
        if let Some(includes) = state.dep_graph.get_includes(&context_key) {
            for header in &includes {
                match hash_file(&state.cache_system, header) {
                    Ok(h) => {
                        hash_map.insert(header.clone(), h);
                    }
                    Err(_) => {
                        // Header disappeared — force rescan
                    }
                }
            }
        }

        let get_hash = |p: &Path| hash_map.get(p).copied();
        state.dep_graph.check(&context_key, is_fresh, get_hash)
    };

    // Process verdict
    match verdict {
        zccache_depgraph::CacheVerdict::Hit { artifact_key }
        | zccache_depgraph::CacheVerdict::SourceChanged { artifact_key } => {
            let artifact_key_hex = artifact_key.hash().to_hex();
            let artifacts = state.artifacts.lock().await;
            if let Some(cached) = artifacts.get(&artifact_key_hex) {
                // Cache hit! Write outputs to disk.
                for (i, output) in cached.artifact.outputs.iter().enumerate() {
                    let out_path = if i == 0 {
                        output_path.clone()
                    } else {
                        cwd_path.join(&output.name)
                    };
                    if let Err(e) = std::fs::write(&out_path, &output.data) {
                        return Response::Error {
                            message: format!(
                                "failed to write cached output {}: {e}",
                                out_path.display()
                            ),
                        };
                    }
                }

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

                return Response::CompileResult {
                    exit_code: cached.artifact.exit_code,
                    stdout: cached.artifact.stdout.clone(),
                    stderr: cached.artifact.stderr.clone(),
                    cached: true,
                };
            }
            // Artifact key computed but no artifact stored yet — fall through to compile
        }
        zccache_depgraph::CacheVerdict::Cold
        | zccache_depgraph::CacheVerdict::HeadersChanged { .. }
        | zccache_depgraph::CacheVerdict::NeedsPreprocessor => {
            // Need to compile and scan includes
        }
    }

    // Cache miss — run the compiler
    write_session_log(
        &state.sessions,
        &sid,
        &format!(
            "cache miss: {} -> {}",
            source_path.display(),
            output_path.display()
        ),
    );

    let result = tokio::process::Command::new(&compiler)
        .args(args)
        .current_dir(cwd)
        .output()
        .await;

    let output = match result {
        Ok(o) => o,
        Err(e) => {
            return Response::Error {
                message: format!("failed to run compiler: {e}"),
            };
        }
    };

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

        // Scan includes and update depgraph
        let scan_result =
            zccache_depgraph::scanner::scan_recursive(&source_path, &ctx.include_search);

        // Hash all files for the artifact key
        let mut hash_map: HashMap<PathBuf, ContentHash> = HashMap::new();

        if let Ok(h) = hash_file(&state.cache_system, &source_path) {
            hash_map.insert(source_path.clone(), h);
        }
        for header in &scan_result.resolved {
            if let Ok(h) = hash_file(&state.cache_system, header) {
                hash_map.insert(header.clone(), h);
            }
        }

        let get_hash = |p: &Path| hash_map.get(p).copied();
        if let Some(artifact_key) = state.dep_graph.update(&context_key, scan_result, get_hash) {
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

            let artifact_bytes: u64 = artifact.outputs.iter().map(|o| o.data.len() as u64).sum();
            let artifact_key_hex = artifact_key.hash().to_hex();
            let mut artifacts = state.artifacts.lock().await;
            artifacts.insert(artifact_key_hex, CachedArtifact { artifact });

            let latency_us = compile_start.elapsed().as_micros() as u64;
            state.stats.record_miss(latency_us, artifact_bytes);
            let src = source_path.clone();
            record_session_stat(&state.sessions, &sid, move |t| {
                t.record_miss(src, artifact_bytes);
            });
        }
    }

    Response::CompileResult {
        exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
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
) -> Response {
    let result = tokio::process::Command::new(compiler)
        .args(args)
        .current_dir(cwd)
        .output()
        .await;

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
