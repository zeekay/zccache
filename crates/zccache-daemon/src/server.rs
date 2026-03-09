//! Daemon server — accepts IPC connections and handles requests.

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use zccache_depgraph::{SessionManager, SystemIncludeCache};
use zccache_ipc::{IpcConnection, IpcListener};
use zccache_protocol::{Request, Response};

/// Shared state accessible by all connection handlers.
struct SharedState {
    sessions: SessionManager,
    system_includes: Mutex<SystemIncludeCache>,
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
            } => handle_session_start(&state, client_pid, &working_dir, &compiler).await,
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
