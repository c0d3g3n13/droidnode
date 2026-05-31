use thiserror::Error;

#[derive(Error, Debug)]
pub enum DroidError {
    #[error("OCI registry error: {0}")]
    OciRegistry(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Kubernetes API error: {0}")]
    KubeApi(#[from] kube::Error),
    #[error("Process execution error: {0}")]
    Process(String),
    #[error("Image not found: {0}")]
    ImageNotFound(String),
    #[error("Layer digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("Workload error: {0}")]
    Workload(String),
    #[error("Health probe failed: {0}")]
    ProbeFailed(String),
    #[error("Node registration error: {0}")]
    NodeRegistration(String),
    #[error("Configuration error: {0}")]
    Config(String),
    #[error("Unsupported operation: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, DroidError>;
