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

    #[instrument(skip(self), fields(addr = %self.bind_addr))]
    pub async fn serve(self) -> Result<()> {
        use hyper::service::service_fn;
        use hyper_util::rt::{TokioExecutor, TokioIo};
        use hyper_util::server::conn::auto::Builder as ConnBuilder;
        use std::sync::Arc;
        use tokio_rustls::TlsAcceptor;

        let app = build_router(self.state.clone());

        // Detect outbound IP so it can be added as a SAN in the cert.
        // The k8s API server connects to us at the InternalIP we advertise,
        // so that IP must appear in the cert's Subject Alternative Names.
        let local_ip = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| { s.connect("8.8.8.8:80")?; s.local_addr() })
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());

        let tls_cfg = make_tls_config(&local_ip)?;
        let acceptor = TlsAcceptor::from(Arc::new(tls_cfg));
        let listener = tokio::net::TcpListener::bind(self.bind_addr)
            .await
            .map_err(crate::error::DroidError::Filesystem)?;

        info!(addr = %self.bind_addr, "kubelet HTTPS server listening");

        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => { tracing::warn!("accept error: {e}"); continue; }
            };
            let acceptor = acceptor.clone();
            let app = app.clone();

            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else { return; };
                let io = TokioIo::new(tls);
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let mut app = app.clone();
                    async move {
                        use tower::Service;
                        let req = req.map(axum::body::Body::new);
                        app.call(req).await
                    }
                });
                ConnBuilder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await
                    .ok();
            });
        }
    }
}

// ─── TLS config ───────────────────────────────────────────────────────────────

/// Build a rustls ServerConfig for the kubelet HTTPS endpoint.
///
/// Prefers signing the cert with the k3s cluster CA so the API server trusts it
/// without any k3s configuration changes. Falls back to a self-signed cert when
/// the CA files are not present (e.g. vanilla k8s or Android; in that case you
/// must configure the API server to trust or skip the cert).
///
/// CA paths can be overridden via DROIDNODE_CA_CERT / DROIDNODE_CA_KEY env vars.
fn make_tls_config(local_ip: &str) -> Result<tokio_rustls::rustls::ServerConfig> {
    use rcgen::{CertificateParams, KeyPair};
    use tokio_rustls::rustls;

    // SANs: DNS hostname + actual IP the API server will dial
    let sans = vec!["droidnode".to_string(), local_ip.to_string()];

    let ca_cert_path = std::env::var("DROIDNODE_CA_CERT")
        .unwrap_or_else(|_| "/var/lib/rancher/k3s/server/tls/server-ca.crt".into());
    let ca_key_path = std::env::var("DROIDNODE_CA_KEY")
        .unwrap_or_else(|_| "/var/lib/rancher/k3s/server/tls/server-ca.key".into());

    if let (Ok(ca_cert_pem), Ok(ca_key_pem)) = (
        std::fs::read_to_string(&ca_cert_path),
        std::fs::read_to_string(&ca_key_path),
    ) {
        let ca_key = KeyPair::from_pem(&ca_key_pem)
            .map_err(|e| crate::error::DroidError::Config(format!("CA key: {e}")))?;
        let ca_params = CertificateParams::from_ca_cert_pem(&ca_cert_pem)
            .map_err(|e| crate::error::DroidError::Config(format!("CA cert: {e}")))?;
        // Reconstruct the CA Certificate object so rcgen can use it as issuer.
        // The API server verifies our cert against its copy of the CA cert; using
        // the same key is what matters, not the reconstructed CA cert's metadata.
        let ca_cert_obj = ca_params
            .self_signed(&ca_key)
            .map_err(|e| crate::error::DroidError::Config(format!("CA self-sign: {e}")))?;

        let params = CertificateParams::new(sans.clone())
            .map_err(|e| crate::error::DroidError::Config(format!("cert params: {e}")))?;
        let key = KeyPair::generate()
            .map_err(|e| crate::error::DroidError::Config(format!("key gen: {e}")))?;
        let signed = params
            .signed_by(&key, &ca_cert_obj.cert, &ca_key)
            .map_err(|e| crate::error::DroidError::Config(format!("sign: {e}")))?;

        let cert_der =
            rustls::pki_types::CertificateDer::from(signed.cert.der().to_vec());
        let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
            signed.key_pair.serialize_der().into(),
        );

        tracing::info!(ca = %ca_cert_path, ip = %local_ip, "kubelet cert signed by k3s CA");
        return rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .map_err(|e| crate::error::DroidError::Config(format!("tls: {e}")));
    }

    tracing::warn!(
        ca = %ca_cert_path,
        "k3s CA not found — using self-signed cert (set DROIDNODE_CA_CERT/KEY to fix kubectl logs)"
    );
    let certified = rcgen::generate_simple_self_signed(sans)
        .map_err(|e| crate::error::DroidError::Config(format!("cert: {e}")))?;
    let cert_der =
        rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(
        certified.key_pair.serialize_der().into(),
    );
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| crate::error::DroidError::Config(format!("tls: {e}")))
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
