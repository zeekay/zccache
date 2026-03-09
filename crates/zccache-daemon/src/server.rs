//! Daemon server — accepts IPC connections and handles requests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use zccache_depgraph::{SessionId, SessionManager, SystemIncludeCache};
use zccache_ipc::{IpcConnection, IpcListener};
use zccache_protocol::{ArtifactData, ArtifactOutput, Request, Response};

/// Cached compilation artifact.
#[derive(Debug, Clone)]
struct CachedArtifact {
    artifact: ArtifactData,
}

/// Shared state accessible by all connection handlers.
struct SharedState {
    sessions: SessionManager,
    system_includes: Mutex<SystemIncludeCache>,
    /// In-memory artifact cache: cache_key_hex → artifact data.
    artifacts: Mutex<HashMap<String, CachedArtifact>>,
}

/// The daemon server that listens for IPC connections.
pub struct DaemonServer {
    listener: IpcListener,
    shutdown: Arc<Notify>,
    state: Arc<SharedState>,
}

impl DaemonServer {
    /// Create a new daemon server bound to the given endpoint.
    pub fn bind(endpoint: &str) -> Result<Self, zccache_ipc::IpcError> {
        let listener = IpcListener::bind(endpoint)?;
        Ok(Self {
            listener,
            shutdown: Arc::new(Notify::new()),
            state: Arc::new(SharedState {
                sessions: SessionManager::new(std::time::Duration::from_secs(300)),
                system_includes: Mutex::new(SystemIncludeCache::new()),
                artifacts: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Get a handle to signal shutdown.
    #[must_use]
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.shutdown)
    }

    /// Run the server, accepting connections until shutdown is signaled.
    pub async fn run(&mut self) -> Result<(), zccache_ipc::IpcError> {
        tracing::info!("daemon server running");

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
                    return Ok(());
                }
            }
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

        let response = match request {
            Request::Ping => Response::Pong,
            Request::Shutdown => {
                conn.send(&Response::ShuttingDown).await?;
                return Ok(());
            }
            Request::Status => Response::Status(zccache_protocol::DaemonStatus {
                artifact_count: 0,
                cache_size_bytes: 0,
                metadata_entries: 0,
                uptime_secs: 0,
                cache_hits: 0,
                cache_misses: 0,
            }),
            Request::Lookup { .. } => Response::LookupResult(zccache_protocol::LookupResult::Miss),
            Request::Store { .. } => Response::StoreResult(zccache_protocol::StoreResult::Stored),
            Request::SessionStart {
                client_pid,
                working_dir,
                compiler,
                log_file,
            } => handle_session_start(&state, client_pid, &working_dir, &compiler, log_file).await,
            Request::Compile {
                session_id,
                args,
                cwd,
            } => handle_compile(&state, session_id, &args, &cwd).await,
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
    };

    let session_id = state.sessions.create(session_config);

    Response::SessionStarted {
        session_id: session_id.value(),
        system_includes: system_includes
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    }
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

/// Handle a Compile request: parse args, check cache, run compiler or return cached.
async fn handle_compile(
    state: &SharedState,
    session_id: u64,
    args: &[String],
    cwd: &str,
) -> Response {
    let sid = SessionId::from_raw(session_id);

    // Look up session
    let compiler = match state.sessions.compiler(&sid) {
        Some(c) => c,
        None => {
            return Response::Error {
                message: format!("unknown session: {session_id}"),
            };
        }
    };

    state.sessions.touch(&sid);

    // Parse the args to find source file, output file, and compute cache key
    let parsed = zccache_compiler::parse_invocation(compiler.to_str().unwrap_or(""), args);
    let compilation = match parsed {
        zccache_compiler::ParsedInvocation::Cacheable(c) => c,
        zccache_compiler::ParsedInvocation::NonCacheable { reason } => {
            write_session_log(&state.sessions, &sid, &format!("non-cacheable: {reason}"));
            // Fall through to direct compilation
            return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid).await;
        }
    };

    // Compute cache key: hash(compiler_binary + source_content + cache_relevant_args)
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

    let cache_key =
        match compute_cache_key(&compiler, &source_path, &compilation.cache_relevant_args) {
            Ok(k) => k,
            Err(e) => {
                write_session_log(
                    &state.sessions,
                    &sid,
                    &format!("cache key error: {e}, falling back to direct compile"),
                );
                return run_compiler_direct(&compiler, args, cwd, &state.sessions, &sid).await;
            }
        };

    // Check cache
    {
        let artifacts = state.artifacts.lock().await;
        if let Some(cached) = artifacts.get(&cache_key) {
            // Cache hit! Write the primary output to the requested output_path.
            // Secondary outputs (if any) go to cwd.
            for (i, output) in cached.artifact.outputs.iter().enumerate() {
                let out_path = if i == 0 {
                    // Primary output always goes to the requested path
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

        let mut artifacts = state.artifacts.lock().await;
        artifacts.insert(cache_key, CachedArtifact { artifact });
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

/// Compute a cache key from compiler binary hash + source hash + args.
fn compute_cache_key(
    compiler: &std::path::Path,
    source: &std::path::Path,
    args: &[String],
) -> Result<String, String> {
    let compiler_hash =
        zccache_hash::hash_file(compiler).map_err(|e| format!("hash compiler: {e}"))?;
    let source_hash = zccache_hash::hash_file(source).map_err(|e| format!("hash source: {e}"))?;

    let mut builder = zccache_hash::cache_key::CacheKeyBuilder::new()
        .compiler(compiler_hash)
        .source(source_hash);
    for a in args {
        builder = builder.arg(a);
    }
    let key = builder.build();

    Ok(key.to_hex())
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
            server.run().await.unwrap();
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
            server.run().await.unwrap();
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
            server.run().await.unwrap();
        });

        let mut client = zccache_ipc::connect(&endpoint).await.unwrap();
        client.send(&Request::Status).await.unwrap();
        let resp: Option<Response> = client.recv().await.unwrap();
        assert!(matches!(resp, Some(Response::Status(_))));

        shutdown.notify_one();
        server_task.await.unwrap();
    }
}
