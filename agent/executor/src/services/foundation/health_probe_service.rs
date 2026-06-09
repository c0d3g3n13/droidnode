use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, instrument};

use crate::brokers::ProotBroker;
use crate::error::{DroidError, Result};
use crate::models::{HttpProbe, ProbeConfig};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait HealthProbeService: Send + Sync {
    /// Run a single probe execution. Returns Ok(()) when the probe passes.
    async fn run_probe(
        &self,
        config: &ProbeConfig,
        rootfs: &Path,
        pid_namespace: Option<u32>,
    ) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct HealthProbeServiceImpl {
    proot_broker: Arc<dyn ProotBroker>,
    http_client: reqwest::Client,
}

impl HealthProbeServiceImpl {
    pub fn new(proot_broker: Arc<dyn ProotBroker>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build probe HTTP client");
        Self { proot_broker, http_client }
    }
}

#[async_trait]
impl HealthProbeService for HealthProbeServiceImpl {
    #[instrument(skip(self, config), fields(probe_type = ?std::mem::discriminant(&config.probe_type)))]
    async fn run_probe(
        &self,
        config: &ProbeConfig,
        rootfs: &Path,
        _pid_namespace: Option<u32>,
    ) -> Result<()> {
        let timeout = Duration::from_secs(config.timeout_seconds as u64);

        let result = tokio::time::timeout(timeout, self.execute_probe(config, rootfs)).await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(DroidError::ProbeFailed(format!(
                "probe timed out after {}s",
                config.timeout_seconds
            ))),
        }
    }
}

impl HealthProbeServiceImpl {
    async fn execute_probe(&self, config: &ProbeConfig, rootfs: &Path) -> Result<()> {
        if let Some(cmd) = &config.exec_command {
            return self.exec_probe(cmd, rootfs).await;
        }
        if let Some(http) = &config.http_get {
            return self.http_probe(http).await;
        }
        Err(DroidError::Config("probe has no exec or httpGet configured".into()))
    }

    async fn exec_probe(&self, command: &[String], rootfs: &Path) -> Result<()> {
        debug!(command = ?command, "running exec probe");

        let mut child = self
            .proot_broker
            .execute(rootfs, command, &[], &[], None)
            .await?;

        let status = child
            .wait()
            .await
            .map_err(|e| DroidError::Process(format!("probe wait: {e}")))?;

        if status.success() {
            Ok(())
        } else {
            Err(DroidError::ProbeFailed(format!(
                "exec probe exited with status {}",
                status.code().unwrap_or(-1)
            )))
        }
    }

    async fn http_probe(&self, probe: &HttpProbe) -> Result<()> {
        let url = format!("http://{}:{}{}", probe.host, probe.port, probe.path);
        debug!(%url, "running HTTP GET probe");

        let resp = self
            .http_client
            .get(&url)
            .send()
            .await
            .map_err(|e| DroidError::ProbeFailed(format!("HTTP probe error: {e}")))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(DroidError::ProbeFailed(format!(
                "HTTP probe returned status {}",
                resp.status()
            )))
        }
    }
}
