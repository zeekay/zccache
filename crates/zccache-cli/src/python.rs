use std::path::PathBuf;

use pyo3::exceptions::{PyOSError, PyRuntimeError};
use pyo3::prelude::*;

use crate::{
    client_session_end, client_session_start, client_session_stats, client_start, client_status,
    client_stop, fingerprint_check, fingerprint_invalidate, fingerprint_mark_failure,
    fingerprint_mark_success, run_ino_convert_cached, InoConvertOptions,
};

fn runtime_to_py_err(message: String) -> PyErr {
    PyErr::new::<PyRuntimeError, _>(message)
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeDaemonStatus {
    #[pyo3(get)]
    version: String,
    #[pyo3(get)]
    artifact_count: u64,
    #[pyo3(get)]
    cache_size_bytes: u64,
    #[pyo3(get)]
    metadata_entries: u64,
    #[pyo3(get)]
    uptime_secs: u64,
    #[pyo3(get)]
    cache_hits: u64,
    #[pyo3(get)]
    cache_misses: u64,
    #[pyo3(get)]
    total_compilations: u64,
    #[pyo3(get)]
    non_cacheable: u64,
    #[pyo3(get)]
    compile_errors: u64,
    #[pyo3(get)]
    time_saved_ms: u64,
    #[pyo3(get)]
    total_links: u64,
    #[pyo3(get)]
    link_hits: u64,
    #[pyo3(get)]
    link_misses: u64,
    #[pyo3(get)]
    link_non_cacheable: u64,
    #[pyo3(get)]
    dep_graph_contexts: u64,
    #[pyo3(get)]
    dep_graph_files: u64,
    #[pyo3(get)]
    sessions_total: u64,
    #[pyo3(get)]
    sessions_active: u64,
    #[pyo3(get)]
    cache_dir: String,
    #[pyo3(get)]
    dep_graph_version: u32,
    #[pyo3(get)]
    dep_graph_disk_size: u64,
}

impl From<zccache_protocol::DaemonStatus> for NativeDaemonStatus {
    fn from(value: zccache_protocol::DaemonStatus) -> Self {
        Self {
            version: value.version,
            artifact_count: value.artifact_count,
            cache_size_bytes: value.cache_size_bytes,
            metadata_entries: value.metadata_entries,
            uptime_secs: value.uptime_secs,
            cache_hits: value.cache_hits,
            cache_misses: value.cache_misses,
            total_compilations: value.total_compilations,
            non_cacheable: value.non_cacheable,
            compile_errors: value.compile_errors,
            time_saved_ms: value.time_saved_ms,
            total_links: value.total_links,
            link_hits: value.link_hits,
            link_misses: value.link_misses,
            link_non_cacheable: value.link_non_cacheable,
            dep_graph_contexts: value.dep_graph_contexts,
            dep_graph_files: value.dep_graph_files,
            sessions_total: value.sessions_total,
            sessions_active: value.sessions_active,
            cache_dir: value.cache_dir.display().to_string(),
            dep_graph_version: value.dep_graph_version,
            dep_graph_disk_size: value.dep_graph_disk_size,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeSessionStart {
    #[pyo3(get)]
    session_id: String,
    #[pyo3(get)]
    journal_path: Option<String>,
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeSessionStats {
    #[pyo3(get)]
    duration_ms: u64,
    #[pyo3(get)]
    compilations: u64,
    #[pyo3(get)]
    hits: u64,
    #[pyo3(get)]
    misses: u64,
    #[pyo3(get)]
    non_cacheable: u64,
    #[pyo3(get)]
    errors: u64,
    #[pyo3(get)]
    time_saved_ms: u64,
    #[pyo3(get)]
    unique_sources: u64,
    #[pyo3(get)]
    bytes_read: u64,
    #[pyo3(get)]
    bytes_written: u64,
}

impl From<zccache_protocol::SessionStats> for NativeSessionStats {
    fn from(value: zccache_protocol::SessionStats) -> Self {
        Self {
            duration_ms: value.duration_ms,
            compilations: value.compilations,
            hits: value.hits,
            misses: value.misses,
            non_cacheable: value.non_cacheable,
            errors: value.errors,
            time_saved_ms: value.time_saved_ms,
            unique_sources: value.unique_sources,
            bytes_read: value.bytes_read,
            bytes_written: value.bytes_written,
        }
    }
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeFingerprintCheck {
    #[pyo3(get)]
    decision: String,
    #[pyo3(get)]
    reason: Option<String>,
    #[pyo3(get)]
    changed_files: Vec<String>,
}

#[pyclass(module = "zccache._native")]
#[derive(Clone)]
pub struct NativeInoConvertResult {
    #[pyo3(get)]
    cache_hit: bool,
    #[pyo3(get)]
    skipped_write: bool,
}

#[pyclass(module = "zccache._native")]
pub struct NativeClient {
    endpoint: Option<String>,
}

#[pymethods]
impl NativeClient {
    #[new]
    #[pyo3(signature = (endpoint=None))]
    fn new(endpoint: Option<String>) -> Self {
        Self { endpoint }
    }

    fn start(&self) -> PyResult<()> {
        client_start(self.endpoint.as_deref()).map_err(runtime_to_py_err)
    }

    fn stop(&self) -> PyResult<bool> {
        client_stop(self.endpoint.as_deref()).map_err(runtime_to_py_err)
    }

    fn status(&self) -> PyResult<NativeDaemonStatus> {
        client_status(self.endpoint.as_deref())
            .map(NativeDaemonStatus::from)
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (cwd, log_file=None, track_stats=false, journal_path=None))]
    fn session_start(
        &self,
        cwd: String,
        log_file: Option<String>,
        track_stats: bool,
        journal_path: Option<String>,
    ) -> PyResult<NativeSessionStart> {
        let cwd = PathBuf::from(cwd);
        let log_file = log_file.map(PathBuf::from);
        let journal_path = journal_path.map(PathBuf::from);
        client_session_start(
            self.endpoint.as_deref(),
            cwd.as_path(),
            log_file.as_deref(),
            track_stats,
            journal_path.as_deref(),
        )
        .map(|result| NativeSessionStart {
            session_id: result.session_id,
            journal_path: result.journal_path,
        })
        .map_err(runtime_to_py_err)
    }

    fn session_end(&self, session_id: String) -> PyResult<Option<NativeSessionStats>> {
        client_session_end(self.endpoint.as_deref(), &session_id)
            .map(|stats| stats.map(NativeSessionStats::from))
            .map_err(runtime_to_py_err)
    }

    fn session_stats(&self, session_id: String) -> PyResult<Option<NativeSessionStats>> {
        client_session_stats(self.endpoint.as_deref(), &session_id)
            .map(|stats| stats.map(NativeSessionStats::from))
            .map_err(runtime_to_py_err)
    }

    #[pyo3(signature = (
        cache_file,
        cache_type="two-layer".to_string(),
        root=".".to_string(),
        extensions=vec![],
        include_globs=vec![],
        exclude=vec![]
    ))]
    fn fingerprint_check(
        &self,
        cache_file: String,
        cache_type: String,
        root: String,
        extensions: Vec<String>,
        include_globs: Vec<String>,
        exclude: Vec<String>,
    ) -> PyResult<NativeFingerprintCheck> {
        fingerprint_check(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
            &cache_type,
            PathBuf::from(root).as_path(),
            &extensions,
            &include_globs,
            &exclude,
        )
        .map(|result| NativeFingerprintCheck {
            decision: result.decision,
            reason: result.reason,
            changed_files: result.changed_files,
        })
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_mark_success(&self, cache_file: String) -> PyResult<()> {
        fingerprint_mark_success(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_mark_failure(&self, cache_file: String) -> PyResult<()> {
        fingerprint_mark_failure(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }

    fn fingerprint_invalidate(&self, cache_file: String) -> PyResult<()> {
        fingerprint_invalidate(
            self.endpoint.as_deref(),
            PathBuf::from(cache_file).as_path(),
        )
        .map_err(runtime_to_py_err)
    }
}

#[pyfunction]
#[pyo3(signature = (
    input,
    output,
    clang_args=vec![],
    inject_arduino_include=true
))]
fn convert_ino(
    input: String,
    output: String,
    clang_args: Vec<String>,
    inject_arduino_include: bool,
) -> PyResult<NativeInoConvertResult> {
    run_ino_convert_cached(
        PathBuf::from(input).as_path(),
        PathBuf::from(output).as_path(),
        &InoConvertOptions {
            clang_args,
            inject_arduino_include,
        },
    )
    .map(|result| NativeInoConvertResult {
        cache_hit: result.cache_hit,
        skipped_write: result.skipped_write,
    })
    .map_err(|e| PyErr::new::<PyOSError, _>(e.to_string()))
}

#[pyfunction]
fn default_endpoint() -> String {
    crate::resolve_endpoint(None)
}

#[pyfunction]
fn check_running_daemon() -> Option<u32> {
    zccache_ipc::check_running_daemon()
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeClient>()?;
    m.add_class::<NativeDaemonStatus>()?;
    m.add_class::<NativeSessionStart>()?;
    m.add_class::<NativeSessionStats>()?;
    m.add_class::<NativeFingerprintCheck>()?;
    m.add_class::<NativeInoConvertResult>()?;
    m.add_function(wrap_pyfunction!(convert_ino, m)?)?;
    m.add_function(wrap_pyfunction!(default_endpoint, m)?)?;
    m.add_function(wrap_pyfunction!(check_running_daemon, m)?)?;
    Ok(())
}
