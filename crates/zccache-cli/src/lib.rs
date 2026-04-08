#![allow(clippy::missing_errors_doc)]

use std::path::Path;
use zccache_core::NormalizedPath;

#[cfg(feature = "python")]
mod python;

#[derive(Debug, Clone)]
pub struct InoConvertOptions {
    pub clang_args: Vec<String>,
    pub inject_arduino_include: bool,
}

impl Default for InoConvertOptions {
    fn default() -> Self {
        Self {
            clang_args: Vec::new(),
            inject_arduino_include: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InoConvertResult {
    pub cache_hit: bool,
    pub skipped_write: bool,
}

pub fn run_ino_convert_cached(
    input: &Path,
    output: &Path,
    options: &InoConvertOptions,
) -> Result<InoConvertResult, Box<dyn std::error::Error>> {
    let input_hash = zccache_hash::hash_file(input)?;
    let mut hasher = zccache_hash::StreamHasher::new();
    hasher.update(b"zccache-ino-convert-v1");
    hasher.update(input_hash.as_bytes());
    hasher.update(input.as_os_str().to_string_lossy().as_bytes());
    hasher.update(if options.inject_arduino_include {
        b"include-arduino-h"
    } else {
        b"no-arduino-h"
    });
    if let Some(libclang_hash) = zccache_compiler::arduino::libclang_hash() {
        hasher.update(libclang_hash.as_bytes());
    }
    for arg in &options.clang_args {
        hasher.update(arg.as_bytes());
        hasher.update(b"\0");
    }
    let cache_key = hasher.finalize().to_hex();

    let cache_dir = zccache_core::config::default_cache_dir().join("ino");
    std::fs::create_dir_all(&cache_dir)?;
    let cached_cpp = cache_dir.join(format!("{cache_key}.ino.cpp"));

    if cached_cpp.exists() {
        return restore_cached_ino_output(&cached_cpp, output);
    }

    let generated = zccache_compiler::arduino::generate_ino_cpp(
        input,
        &zccache_compiler::arduino::ArduinoConversionOptions {
            clang_args: options.clang_args.clone(),
            inject_arduino_include: options.inject_arduino_include,
        },
    )?;

    write_file_atomically(&cached_cpp, generated.cpp.as_bytes())?;
    restore_cached_ino_output(&cached_cpp, output).map(|_| InoConvertResult {
        cache_hit: false,
        skipped_write: false,
    })
}

fn restore_cached_ino_output(
    cached_cpp: &Path,
    output: &Path,
) -> Result<InoConvertResult, Box<dyn std::error::Error>> {
    if output.exists() {
        let output_hash = zccache_hash::hash_file(output)?;
        let cached_hash = zccache_hash::hash_file(cached_cpp)?;
        if output_hash == cached_hash {
            return Ok(InoConvertResult {
                cache_hit: true,
                skipped_write: true,
            });
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(cached_cpp, output)?;
    Ok(InoConvertResult {
        cache_hit: true,
        skipped_write: false,
    })
}

fn write_file_atomically(path: &Path, data: &[u8]) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), data)?;
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.error),
    }
}

fn resolve_endpoint(explicit: Option<&str>) -> String {
    if let Some(ep) = explicit {
        return ep.to_string();
    }
    if let Ok(ep) = std::env::var("ZCCACHE_ENDPOINT") {
        return ep;
    }
    zccache_ipc::default_endpoint()
}

fn run_async<T>(future: impl std::future::Future<Output = Result<T, String>>) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create tokio runtime: {e}"))?
        .block_on(future)
}

#[derive(Debug)]
enum VersionCheck {
    Ok,
    Unreachable,
    DaemonOlder { daemon_ver: String },
    DaemonNewer,
    CommError,
}

#[cfg(unix)]
async fn connect_client(
    endpoint: &str,
) -> Result<zccache_ipc::IpcConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

#[cfg(windows)]
async fn connect_client(
    endpoint: &str,
) -> Result<zccache_ipc::IpcClientConnection, zccache_ipc::IpcError> {
    zccache_ipc::connect(endpoint).await
}

async fn check_daemon_version(endpoint: &str) -> VersionCheck {
    let mut conn = match connect_client(endpoint).await {
        Ok(c) => c,
        Err(_) => return VersionCheck::Unreachable,
    };
    if conn.send(&zccache_protocol::Request::Status).await.is_err() {
        return VersionCheck::CommError;
    }
    match conn.recv::<zccache_protocol::Response>().await {
        Ok(Some(zccache_protocol::Response::Status(s))) => {
            if s.version == zccache_core::VERSION {
                return VersionCheck::Ok;
            }
            let client_ver = zccache_core::version::current();
            match zccache_core::version::Version::parse(&s.version) {
                Some(daemon_ver) => match daemon_ver.cmp(&client_ver) {
                    std::cmp::Ordering::Equal => VersionCheck::Ok,
                    std::cmp::Ordering::Greater => VersionCheck::DaemonNewer,
                    std::cmp::Ordering::Less => VersionCheck::DaemonOlder {
                        daemon_ver: s.version,
                    },
                },
                None => VersionCheck::DaemonOlder {
                    daemon_ver: s.version,
                },
            }
        }
        _ => VersionCheck::CommError,
    }
}

async fn spawn_and_wait(endpoint: &str) -> Result<(), String> {
    let daemon_bin = find_daemon_binary().ok_or("cannot find zccache-daemon binary")?;
    spawn_daemon(&daemon_bin, endpoint)?;

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if connect_client(endpoint).await.is_ok() {
            return Ok(());
        }
    }
    Err("daemon started but not accepting connections after 10s".to_string())
}

async fn ensure_daemon(endpoint: &str) -> Result<(), String> {
    match check_daemon_version(endpoint).await {
        VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
        VersionCheck::DaemonOlder { daemon_ver } => {
            return Err(format!(
                "daemon v{daemon_ver} is older than client v{}. Run `zccache stop` first.",
                zccache_core::VERSION,
            ));
        }
        VersionCheck::CommError => {
            return Err(
                "cannot communicate with daemon (possible protocol mismatch). Run `zccache stop` first."
                    .to_string(),
            );
        }
        VersionCheck::Unreachable => {}
    }

    if let Some(pid) = zccache_ipc::check_running_daemon() {
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            match check_daemon_version(endpoint).await {
                VersionCheck::Ok | VersionCheck::DaemonNewer => return Ok(()),
                VersionCheck::DaemonOlder { daemon_ver } => {
                    return Err(format!(
                        "daemon v{daemon_ver} is older than client v{}. Run `zccache stop` first.",
                        zccache_core::VERSION,
                    ));
                }
                VersionCheck::CommError => {
                    return Err(
                        "cannot communicate with daemon (possible protocol mismatch). Run `zccache stop` first."
                            .to_string(),
                    );
                }
                VersionCheck::Unreachable => continue,
            }
        }
        return Err(format!(
            "daemon process {pid} exists but not accepting connections"
        ));
    }

    spawn_and_wait(endpoint).await
}

fn find_daemon_binary() -> Option<NormalizedPath> {
    let name = if cfg!(windows) {
        "zccache-daemon.exe"
    } else {
        "zccache-daemon"
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate.into());
            }
        }
    }

    which_on_path(name)
}

fn which_on_path(name: &str) -> Option<NormalizedPath> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.into());
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe.into());
            }
        }
    }
    None
}

fn spawn_daemon(bin: &Path, endpoint: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(["--foreground", "--endpoint", endpoint]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
        disable_handle_inheritance();
    }

    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    #[cfg(windows)]
    restore_handle_inheritance();

    Ok(())
}

#[cfg(windows)]
fn disable_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout().as_raw_handle();
        let stderr = std::io::stderr().as_raw_handle();
        let _ = SetHandleInformation(stdout.cast(), HANDLE_FLAG_INHERIT, 0);
        let _ = SetHandleInformation(stderr.cast(), HANDLE_FLAG_INHERIT, 0);
    }
}

#[cfg(windows)]
fn restore_handle_inheritance() {
    use std::os::windows::io::AsRawHandle;

    extern "system" {
        fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;

    unsafe {
        let stdout = std::io::stdout().as_raw_handle();
        let stderr = std::io::stderr().as_raw_handle();
        let _ = SetHandleInformation(stdout.cast(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
        let _ = SetHandleInformation(stderr.cast(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
    }
}

#[derive(Debug, Clone)]
pub struct SessionStartResponse {
    pub session_id: String,
    pub journal_path: Option<String>,
}

pub fn client_start(endpoint: Option<&str>) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move { ensure_daemon(&endpoint).await })
}

pub fn client_stop(endpoint: Option<&str>) -> Result<bool, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = match connect_client(&endpoint).await {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        conn.send(&zccache_protocol::Request::Shutdown)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::ShuttingDown)) => Ok(true),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_status(endpoint: Option<&str>) -> Result<zccache_protocol::DaemonStatus, String> {
    let endpoint = resolve_endpoint(endpoint);
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("daemon not running at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::Status)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::Status(status))) => Ok(status),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_start(
    endpoint: Option<&str>,
    cwd: &Path,
    log_file: Option<&Path>,
    track_stats: bool,
    journal_path: Option<&Path>,
) -> Result<SessionStartResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cwd = cwd.to_path_buf();
    let log_file = log_file.map(NormalizedPath::from);
    let journal_path = journal_path.map(NormalizedPath::from);

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::SessionStart {
            client_pid: std::process::id(),
            working_dir: cwd.into(),
            log_file,
            track_stats,
            journal_path,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionStarted {
                session_id,
                journal_path,
            })) => Ok(SessionStartResponse {
                session_id,
                journal_path: journal_path.map(|p| p.display().to_string()),
            }),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_end(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<zccache_protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::SessionEnd {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionEnded { stats })) => Ok(stats),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn client_session_stats(
    endpoint: Option<&str>,
    session_id: &str,
) -> Result<Option<zccache_protocol::SessionStats>, String> {
    let endpoint = resolve_endpoint(endpoint);
    let session_id = session_id.to_string();
    run_async(async move {
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::SessionStats {
            session_id: session_id.clone(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::SessionStatsResult { stats })) => Ok(stats),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

#[derive(Debug, Clone)]
pub struct FingerprintCheckResponse {
    pub decision: String,
    pub reason: Option<String>,
    pub changed_files: Vec<String>,
}

pub fn fingerprint_check(
    endpoint: Option<&str>,
    cache_file: &Path,
    cache_type: &str,
    root: &Path,
    extensions: &[String],
    include_globs: &[String],
    exclude: &[String],
) -> Result<FingerprintCheckResponse, String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    let cache_type = cache_type.to_string();
    let root = root.to_path_buf();
    let extensions = extensions.to_vec();
    let include_globs = include_globs.to_vec();
    let exclude = exclude.to_vec();

    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;

        conn.send(&zccache_protocol::Request::FingerprintCheck {
            cache_file: cache_file.into(),
            cache_type,
            root: root.into(),
            extensions,
            include_globs,
            exclude,
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;

        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintCheckResult {
                decision,
                reason,
                changed_files,
            })) => Ok(FingerprintCheckResponse {
                decision,
                reason,
                changed_files,
            }),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_mark_success(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, true)
}

pub fn fingerprint_mark_failure(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    fingerprint_mark(endpoint, cache_file, false)
}

fn fingerprint_mark(
    endpoint: Option<&str>,
    cache_file: &Path,
    success: bool,
) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        let request = if success {
            zccache_protocol::Request::FingerprintMarkSuccess {
                cache_file: cache_file.into(),
            }
        } else {
            zccache_protocol::Request::FingerprintMarkFailure {
                cache_file: cache_file.into(),
            }
        };
        conn.send(&request)
            .await
            .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}

pub fn fingerprint_invalidate(endpoint: Option<&str>, cache_file: &Path) -> Result<(), String> {
    let endpoint = resolve_endpoint(endpoint);
    let cache_file = cache_file.to_path_buf();
    run_async(async move {
        ensure_daemon(&endpoint).await?;
        let mut conn = connect_client(&endpoint)
            .await
            .map_err(|e| format!("cannot connect to daemon at {endpoint}: {e}"))?;
        conn.send(&zccache_protocol::Request::FingerprintInvalidate {
            cache_file: cache_file.into(),
        })
        .await
        .map_err(|e| format!("failed to send to daemon: {e}"))?;
        match conn.recv::<zccache_protocol::Response>().await {
            Ok(Some(zccache_protocol::Response::FingerprintAck)) => Ok(()),
            Ok(Some(zccache_protocol::Response::Error { message })) => Err(message),
            Ok(None) => Err("lost connection to daemon (no response received)".to_string()),
            Ok(Some(other)) => Err(format!("unexpected response from daemon: {other:?}")),
            Err(e) => Err(format!("broken connection to daemon: {e}")),
        }
    })
}
