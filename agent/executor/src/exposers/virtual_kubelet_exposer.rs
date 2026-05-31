use axum_server;
use rcgen;
use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, instrument};

use crate::error::Result;
use crate::models::{NodeProfile, PodRunStatus};

// ─── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct KubeletState {
    pub node_profile: Arc<RwLock<Option<NodeProfile>>>,
    // pod_uid → log lines ring buffer
    pub pod_logs: Arc<RwLock<HashMap<String, Vec<String>>>>,
    pub pod_statuses: Arc<RwLock<HashMap<String, PodRunStatus>>>,
}

impl KubeletState {
    pub fn new() -> Self {
        Self {
            node_profile: Arc::new(RwLock::new(None)),
            pod_logs: Arc::new(RwLock::new(HashMap::new())),
            pod_statuses: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn append_log(&self, pod_uid: &str, line: String) {
        let mut logs = self.pod_logs.write().await;
        let buf = logs.entry(pod_uid.to_string()).or_insert_with(Vec::new);
        buf.push(line);
        // Keep last 10 000 lines per pod
        if buf.len() > 10_000 {
            buf.drain(..buf.len() - 10_000);
        }
    }

    pub async fn update_status(&self, status: PodRunStatus) {
        let mut statuses = self.pod_statuses.write().await;
        statuses.insert(status.pod_uid.clone(), status);
    }
}

// ─── Exposer ──────────────────────────────────────────────────────────────────

pub struct VirtualKubeletExposer {
    state: KubeletState,
    bind_addr: SocketAddr,
}

impl VirtualKubeletExposer {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            state: KubeletState::new(),
            bind_addr,
        }
    }

    pub fn state(&self) -> KubeletState {
        self.state.clone()
    }

    /// Start the kubelet HTTPS server. This runs until the process exits.
    /// k8s API server always connects to kubelets over HTTPS. We generate a
    /// self-signed cert at startup; configure k3s/k8s with
    /// --kubelet-insecure-skip-tls-verify-backend=true to trust it.
    #[instrument(skip(self), fields(addr = %self.bind_addr))]
    pub async fn serve(self) -> Result<()> {
        use axum_server::tls_rustls::RustlsConfig;

        let state = self.state.clone();
        let app = build_router(state);

        let cert = rcgen::generate_simple_self_signed(vec!["droidnode".to_string()])
            .map_err(|e| crate::error::DroidError::Config(format!("TLS cert generation: {e}")))?;

        let tls_config = RustlsConfig::from_der(
            vec![cert.serialize_der()
                .map_err(|e| crate::error::DroidError::Config(format!("cert serialize: {e}")))?],
            cert.serialize_private_key_der(),
        )
        .await
        .map_err(|e| crate::error::DroidError::Config(format!("TLS config: {e}")))?;

        info!(addr = %self.bind_addr, "kubelet HTTPS server listening");
        axum_server::bind_rustls(self.bind_addr, tls_config)
            .serve(app.into_make_service())
            .await
            .map_err(|e| crate::error::DroidError::Process(format!("kubelet server: {e}")))?;

        Ok(())
    }
}

// ─── Router ───────────────────────────────────────────────────────────────────

fn build_router(state: KubeletState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/pods", get(list_pods))
        .route(
            "/containerLogs/:namespace/:pod_id/:container_name",
            get(container_logs),
        )
        .route("/metrics", get(metrics))
        .with_state(state)
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn list_pods(State(state): State<KubeletState>) -> impl IntoResponse {
    let statuses = state.pod_statuses.read().await;
    let pods: Vec<&PodRunStatus> = statuses.values().collect();
    Json(serde_json::json!({ "pods": pods }))
}

async fn container_logs(
    State(state): State<KubeletState>,
    Path((_namespace, pod_id, _container_name)): Path<(String, String, String)>,
) -> Response {
    // Look up by pod_uid or pod_name — for simplicity match by pod_id
    let logs = state.pod_logs.read().await;
    let output = logs
        .get(&pod_id)
        .map(|lines| lines.join("\n"))
        .unwrap_or_default();

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain")
        .body(Body::from(output))
        .unwrap()
}

async fn metrics(State(state): State<KubeletState>) -> impl IntoResponse {
    let profile_guard = state.node_profile.read().await;
    let mut out = String::new();

    if let Some(profile) = &*profile_guard {
        out.push_str(&format!(
            "# HELP droidnode_memory_available_bytes Available memory in bytes\n\
             # TYPE droidnode_memory_available_bytes gauge\n\
             droidnode_memory_available_bytes {}\n",
            profile.memory.available_bytes
        ));
        out.push_str(&format!(
            "# HELP droidnode_memory_total_bytes Total memory in bytes\n\
             # TYPE droidnode_memory_total_bytes gauge\n\
             droidnode_memory_total_bytes {}\n",
            profile.memory.total_bytes
        ));
        out.push_str(&format!(
            "# HELP droidnode_storage_available_bytes Available storage in bytes\n\
             # TYPE droidnode_storage_available_bytes gauge\n\
             droidnode_storage_available_bytes {}\n",
            profile.storage.available_bytes
        ));
        out.push_str(&format!(
            "# HELP droidnode_battery_percent Battery charge percentage\n\
             # TYPE droidnode_battery_percent gauge\n\
             droidnode_battery_percent {}\n",
            profile.battery.percent
        ));
        out.push_str(&format!(
            "# HELP droidnode_node_ready Node readiness (1=ready)\n\
             # TYPE droidnode_node_ready gauge\n\
             droidnode_node_ready {}\n",
            if profile.conditions.ready { 1 } else { 0 }
        ));
    }

    (
        StatusCode::OK,
        [("Content-Type", "text/plain; version=0.0.4")],
        out,
    )
}
