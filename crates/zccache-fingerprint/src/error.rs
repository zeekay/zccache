use std::path::PathBuf;

/// Errors that can occur during fingerprint operations.
#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("scan error in {path}: {message}")]
    Scan { path: PathBuf, message: String },
}

pub type Result<T> = std::result::Result<T, FingerprintError>;
