//! Per-IPC-connection request dispatch loop.

use super::*;

/// Handle a single client connection.
pub(super) async fn handle_connection(
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

        // Dispatch request and capture journal metadata in the same match
        // to move args/session_id into JournalContext without cloning.
        // Only env needs cloning because handlers consume it.
        let journal_start = std::time::Instant::now();
        let (response, journal_ctx): (Response, Option<JournalContext>) = match request {
            Request::Ping => (Response::Pong, None),
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
                    .map(|entry| entry.value().meta.total_size)
                    .sum();
                let metadata_entries = state.cache_system.metadata().len() as u64;
                (
                    Response::Status(zccache_protocol::DaemonStatus {
                        version: zccache_core::VERSION.to_string(),
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
                        cache_dir: zccache_core::config::default_cache_dir(),
                        dep_graph_version: zccache_depgraph::DEPGRAPH_VERSION,
                        dep_graph_disk_size: zccache_depgraph::depgraph_file_path()
                            .metadata()
                            .map(|m| m.len())
                            .unwrap_or(0),
                        dep_graph_persisted: state.dep_graph_persisted.load(Ordering::Acquire),
                    }),
                    None,
                )
            }
            Request::Lookup { .. } => (
                Response::LookupResult(zccache_protocol::LookupResult::Miss),
                None,
            ),
            Request::Store { .. } => (
                Response::StoreResult(zccache_protocol::StoreResult::Stored),
                None,
            ),
            Request::Clear => (handle_clear(&state).await, None),
            Request::SessionStart {
                client_pid,
                working_dir,
                log_file,
                track_stats,
                journal_path,
            } => {
                state.stats.record_session();
                (
                    handle_session_start(
                        &state,
                        client_pid,
                        &working_dir,
                        log_file,
                        track_stats,
                        journal_path,
                    )
                    .await,
                    None,
                )
            }
            Request::Compile {
                session_id,
                args,
                cwd,
                compiler,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: compiler.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: Some(session_id),
                };
                let resp = handle_compile(
                    &state,
                    ctx.session_id.as_deref().unwrap(),
                    &ctx.args,
                    &cwd,
                    &compiler,
                    env,
                )
                .await;
                (resp, Some(ctx))
            }
            Request::CompileEphemeral {
                client_pid,
                working_dir,
                compiler,
                args,
                cwd,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: compiler.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp = handle_compile_ephemeral(
                    &state,
                    client_pid,
                    &working_dir,
                    &compiler,
                    &ctx.args,
                    &cwd,
                    env,
                )
                .await;
                (resp, Some(ctx))
            }
            Request::SessionStats { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        if let Some(session) = state.sessions.get(&sid) {
                            let stats = session.stats_tracker.as_ref().map(|tracker| {
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
                            Response::SessionStatsResult { stats }
                        } else {
                            Response::Error {
                                message: format!("unknown session: {session_id}"),
                            }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::SessionEnd { session_id } => (
                match session_id.parse::<SessionId>() {
                    Ok(sid) => {
                        state.session_worktree_roots.remove(&sid);
                        if let Some(session) = state.sessions.end(&sid) {
                            // Close the session journal file handle if one was open.
                            if let Some(ref path) = session.journal_path {
                                state.journal.close_session(path);
                            }
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
                            // Idempotent: session-end on an unknown session is a
                            // no-op success. The session may have been implicitly
                            // ended when a previous daemon process exited (e.g.
                            // killed by zccache-ci to unlock target binaries on
                            // Windows). Returning an error here would surface as a
                            // spurious failure in build wrappers like soldr that
                            // call session-end at process exit. No stats are
                            // returned because the session state is gone.
                            Response::SessionEnded { stats: None }
                        }
                    }
                    Err(_) => Response::Error {
                        message: format!("invalid session ID: {session_id}"),
                    },
                },
                None,
            ),
            Request::LinkEphemeral {
                client_pid,
                tool,
                args,
                cwd,
                env,
            } => {
                let ctx = JournalContext {
                    compiler: tool.to_string_lossy().into_owned(),
                    args,
                    cwd: cwd.to_string_lossy().into_owned(),
                    env: env.clone(),
                    session_id: None,
                };
                let resp =
                    handle_link_ephemeral(&state, client_pid, &tool, &ctx.args, &cwd, env).await;
                (resp, Some(ctx))
            }
            Request::FingerprintCheck {
                cache_file,
                cache_type,
                root,
                extensions,
                include_globs,
                exclude,
            } => {
                // Register watcher BEFORE check so events arriving during
                // the scan are not lost.
                watch_directory(&state, &root).await;
                let result = state.fingerprint.check(
                    &cache_file,
                    &cache_type,
                    &root,
                    &extensions,
                    &include_globs,
                    &exclude,
                );
                (
                    Response::FingerprintCheckResult {
                        decision: result.decision,
                        reason: result.reason,
                        changed_files: result.changed_files,
                    },
                    None,
                )
            }
            Request::FingerprintMarkSuccess { cache_file } => {
                state.fingerprint.mark_success(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintMarkFailure { cache_file } => {
                state.fingerprint.mark_failure(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::FingerprintInvalidate { cache_file } => {
                state.fingerprint.invalidate(&cache_file);
                (Response::FingerprintAck, None)
            }
            Request::ListRustArtifacts => {
                let mut artifacts = Vec::new();
                for entry in state.artifacts.iter() {
                    let key = entry.key().clone();
                    let cached = entry.value();
                    // Only include artifacts that look like Rust outputs
                    // (.rlib, .rmeta, .d files).
                    let names: Vec<String> = cached.meta.output_names.to_vec();
                    let is_rust = names.iter().any(|n| {
                        n.ends_with(".rlib")
                            || n.ends_with(".rmeta")
                            || n.ends_with(".d")
                            || n.ends_with(".so")
                            || n.ends_with(".dylib")
                            || n.ends_with(".dll")
                    });
                    if is_rust {
                        artifacts.push(zccache_protocol::RustArtifactInfo {
                            cache_key: key,
                            output_names: names.clone(),
                            payload_count: names.len(),
                        });
                    }
                }
                (Response::RustArtifactList { artifacts }, None)
            }
        };

        // Log to compile journal for journalable requests.
        if let Some(ctx) = journal_ctx {
            if let Some((outcome, exit_code)) = extract_outcome(&response) {
                let latency_ns = journal_start.elapsed().as_nanos();
                // Look up session journal path for per-session logging.
                let session_journal_path = ctx.session_id.as_ref().and_then(|sid| {
                    sid.parse::<SessionId>().ok().and_then(|parsed| {
                        state
                            .sessions
                            .get(&parsed)
                            .and_then(|s| s.journal_path.clone())
                    })
                });
                state.journal.log(
                    &JournalEntry::new(ctx, outcome, exit_code, latency_ns),
                    session_journal_path.as_deref(),
                );
            }
        }

        conn.send(&response).await?;
    }
}
