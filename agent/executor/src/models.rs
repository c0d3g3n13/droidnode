use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// Re-export k8s types used throughout
pub use k8s_openapi::api::core::v1::Pod;

// ─── OCI types ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ImageRef {
    pub registry: String,
    pub repository: String,
    pub reference: String, // tag or digest
}

impl ImageRef {
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        let s = s.trim();

        // Split off digest or tag from the rightmost ':' or '@'
        let (name_part, reference) = if let Some(at) = s.find('@') {
            (&s[..at], s[at + 1..].to_string())
        } else {
            // Find the last colon that isn't part of a host:port
            let slash_pos = s.find('/').unwrap_or(0);
            if let Some(colon) = s[slash_pos..].rfind(':') {
                let abs = slash_pos + colon;
                (&s[..abs], s[abs + 1..].to_string())
            } else {
                (s, "latest".to_string())
            }
        };

        // Determine registry vs repository
        let first_slash = name_part.find('/');
        let (registry, repository) = match first_slash {
            Some(idx) => {
                let host = &name_part[..idx];
                // A host contains a dot or colon (e.g. "docker.io", "localhost:5000")
                if host.contains('.') || host.contains(':') {
                    (host.to_string(), name_part[idx + 1..].to_string())
                } else {
                    // Docker Hub short form: "library/ubuntu" → docker.io
                    (
                        "registry-1.docker.io".to_string(),
                        name_part.to_string(),
                    )
                }
            }
            None => {
                // No slash → official image like "ubuntu"
                (
                    "registry-1.docker.io".to_string(),
                    format!("library/{}", name_part),
                )
            }
        };

        Ok(ImageRef {
            registry,
            repository,
            reference,
        })
    }

    pub fn registry_url(&self) -> String {
        format!("https://{}", self.registry)
    }
}

// ─── Digest ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest(pub String);

impl Digest {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn hex(&self) -> &str {
        self.0.strip_prefix("sha256:").unwrap_or(&self.0)
    }

    // Safe filesystem name: replace ':' with '-'
    pub fn as_fs_name(&self) -> String {
        self.0.replace(':', "-")
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ─── OCI Manifest ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImageConfig {
    pub architecture: Option<String>,
    pub os: Option<String>,
    pub config: Option<ContainerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContainerConfig {
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(rename = "User")]
    pub user: Option<String>,
}

// ─── Runtime types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

// ─── Node profile ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProfile {
    pub node_id: String,
    pub cpu: CpuInfo,
    pub memory: MemoryInfo,
    pub storage: StorageInfo,
    pub runtime: RuntimeInfo,
    pub battery: BatteryInfo,
    pub network: NetworkInfo,
    pub conditions: NodeConditions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    pub arch: String,
    pub cores: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageInfo {
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BatteryInfo {
    pub percent: u8,
    pub charging: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkInfo {
    pub network_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConditions {
    pub ready: bool,
    pub battery_pressure: bool,
    pub memory_pressure: bool,
    pub network_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub node_id: String,
    pub conditions: NodeConditions,
    pub memory: MemoryInfo,
    pub storage: StorageInfo,
    pub battery: BatteryInfo,
    pub network: NetworkInfo,
}

// ─── Events ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEvent {
    pub node_id: String,
    pub event_type: EventType,
    pub reason: String,
    pub message: String,
    pub pod_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    Normal,
    Warning,
}

// ─── Pod run status ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodRunStatus {
    pub pod_uid: String,
    pub pod_name: String,
    pub namespace: String,
    pub phase: PodPhase,
    pub message: Option<String>,
    pub containers: Vec<ContainerRunStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PodPhase {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

impl PodPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Succeeded => "Succeeded",
            Self::Failed => "Failed",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRunStatus {
    pub name: String,
    pub ready: bool,
    pub running: bool,
    pub exit_code: Option<i32>,
}

// ─── Health probes ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ProbeType {
    Startup,
    Liveness,
    Readiness,
}

#[derive(Debug, Clone)]
pub struct HttpProbe {
    pub host: String,
    pub port: u16,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub probe_type: ProbeType,
    pub exec_command: Option<Vec<String>>,
    pub http_get: Option<HttpProbe>,
    pub initial_delay_seconds: u32,
    pub period_seconds: u32,
    pub failure_threshold: u32,
    pub success_threshold: u32,
    pub timeout_seconds: u32,
}
