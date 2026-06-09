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

impl Default for KubeletState {
    fn default() -> Self {
        Self::new()
    }
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
/// Generates a persistent CA on first run and saves it to
/// `$DROIDNODE_DATA_DIR/kubelet-ca.{crt,key}` (default: ~/.droidnode/).
/// The kubelet serving cert is signed by that CA on every startup.
///
/// On first run the agent prints the CA cert path; add it to k3s once:
///   kube-apiserver-arg:
///     - "kubelet-certificate-authority=<path>"
/// After that one restart k3s trusts every cert we sign.
fn make_tls_config(local_ip: &str) -> Result<tokio_rustls::rustls::ServerConfig> {
    use base64::Engine;
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    use tokio_rustls::rustls;

    // Resolve data dir (same logic as Config::from_env in main.rs)
    let data_dir = std::env::var("DROIDNODE_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"))
                .join(".droidnode")
        });
    let _ = std::fs::create_dir_all(&data_dir);

    let ca_cert_path = data_dir.join("kubelet-ca.crt");
    let ca_key_path = data_dir.join("kubelet-ca.key");

    // CA params are identical every run — same Subject DN → same Issuer field
    // in every cert we sign → k3s can match against the persisted CA cert.
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

    // Load or generate the CA key (persisted across restarts so the public key
    // stored in kubelet-ca.crt never changes after the first run).
    let ca_key = if ca_key_path.exists() {
        let pem = std::fs::read_to_string(&ca_key_path)
            .map_err(|e| crate::error::DroidError::Config(format!("CA key read: {e}")))?;
        KeyPair::from_pem(&pem)
            .map_err(|e| crate::error::DroidError::Config(format!("CA key parse: {e}")))?
    } else {
        KeyPair::generate()
            .map_err(|e| crate::error::DroidError::Config(format!("CA key gen: {e}")))?
    };

    // Derive (or re-derive) the CA Certificate object from the stable params+key.
    // rcgen 0.13: self_signed / signed_by return Certificate (not CertifiedKey).
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|e| crate::error::DroidError::Config(format!("CA self-sign: {e}")))?;

    // Persist on first run only
    if !ca_key_path.exists() {
        std::fs::write(&ca_key_path, ca_key.serialize_pem())
            .map_err(|e| crate::error::DroidError::Config(format!("CA key write: {e}")))?;
    }
    if !ca_cert_path.exists() {
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(ca_cert.der())
        );
        std::fs::write(&ca_cert_path, &pem)
            .map_err(|e| crate::error::DroidError::Config(format!("CA cert write: {e}")))?;
        tracing::warn!(
            "kubelet CA written to {path}. One-time k3s setup:\n  \
            Add to /etc/rancher/k3s/config.yaml:\n    \
            kube-apiserver-arg:\n      \
            - \"kubelet-certificate-authority={path}\"\n  \
            Then: sudo systemctl restart k3s",
            path = ca_cert_path.display()
        );
    }

    // Sign the kubelet serving cert with our CA.
    // The IP must be a SAN — k3s connects to the InternalIP we advertise.
    let server_params =
        CertificateParams::new(vec!["droidnode".to_string(), local_ip.to_string()])
            .map_err(|e| crate::error::DroidError::Config(format!("cert params: {e}")))?;
    let server_key = KeyPair::generate()
        .map_err(|e| crate::error::DroidError::Config(format!("server key gen: {e}")))?;
    let signed = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .map_err(|e| crate::error::DroidError::Config(format!("sign: {e}")))?;

    // rcgen 0.13: signed is Certificate; key stays as the separate KeyPair.
    let cert_der = rustls::pki_types::CertificateDer::from(signed.der().to_vec());
    let key_der =
        rustls::pki_types::PrivateKeyDer::Pkcs8(server_key.serialize_der().into());

    tracing::info!(ip = %local_ip, ca = %ca_cert_path.display(), "kubelet TLS ready");

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
