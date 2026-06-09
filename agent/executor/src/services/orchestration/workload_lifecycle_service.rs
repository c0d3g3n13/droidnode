use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, instrument, warn};

use crate::error::{DroidError, Result};
use crate::exposers::virtual_kubelet_exposer::KubeletState;
use crate::models::{ContainerRunStatus, Pod, PodPhase, PodRunStatus, ProbeConfig};
use crate::services::foundation::{
    health_probe_service::HealthProbeService,
    workload_execution_service::WorkloadExecutionService,
};

// ─── Running workload handle ──────────────────────────────────────────────────

pub struct RunningWorkload {
    pub pod_uid: String,
    pub pod_name: String,
    pub namespace: String,
    pub container_name: String,
    pub rootfs: PathBuf,
    pub child: Mutex<Child>,
}

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait WorkloadLifecycleService: Send + Sync {
    async fn start(&self, pod: &Pod, rootfs: PathBuf) -> Result<Arc<RunningWorkload>>;
    async fn stop(&self, workload: &RunningWorkload) -> Result<()>;
    async fn status(&self, workload: &RunningWorkload) -> Result<PodRunStatus>;
    async fn run_liveness_probe(
        &self,
        workload: &RunningWorkload,
        probe: &ProbeConfig,
    ) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct WorkloadLifecycleServiceImpl {
    execution_service: Arc<dyn WorkloadExecutionService>,
    probe_service: Arc<dyn HealthProbeService>,
    kubelet_state: KubeletState,
}

impl WorkloadLifecycleServiceImpl {
    pub fn new(
        execution_service: Arc<dyn WorkloadExecutionService>,
        probe_service: Arc<dyn HealthProbeService>,
        kubelet_state: KubeletState,
    ) -> Self {
        Self { execution_service, probe_service, kubelet_state }
    }
}

#[async_trait]
impl WorkloadLifecycleService for WorkloadLifecycleServiceImpl {
    #[instrument(skip(self, pod), fields(
        pod = pod.metadata.name.as_deref().unwrap_or("?"),
        rootfs = %rootfs.display()
    ))]
    async fn start(&self, pod: &Pod, rootfs: PathBuf) -> Result<Arc<RunningWorkload>> {
        let pod_uid = pod.metadata.uid.clone().unwrap_or_default();
        let pod_name = pod.metadata.name.clone().unwrap_or_default();
        let namespace = pod.metadata.namespace.clone().unwrap_or_else(|| "default".into());
        let container_name = pod
            .spec
            .as_ref()
            .and_then(|s| s.containers.first())
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "container".into());

        let mut child = self.execution_service.execute_workload(pod, &rootfs).await?;

        // Spawn tasks to drain stdout/stderr into the kubelet log ring buffer.
        // The buffer is keyed by pod name — that's what the /containerLogs URL
        // path supplies, so kubectl logs can look it up directly.
        if let Some(stdout) = child.stdout.take() {
            let state = self.kubelet_state.clone();
            let name = pod_name.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    state.append_log(&name, line).await;
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let state = self.kubelet_state.clone();
            let name = pod_name.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    state.append_log(&name, line).await;
                }
            });
        }

        info!(pod = %pod_name, "workload started");
        eprintln!("POD_EVENT started pod={pod_name}");

        Ok(Arc::new(RunningWorkload {
            pod_uid,
            pod_name,
            namespace,
            container_name,
            rootfs,
            child: Mutex::new(child),
        }))
    }

    #[instrument(skip(self, workload), fields(pod = %workload.pod_name))]
    async fn stop(&self, workload: &RunningWorkload) -> Result<()> {
        let mut child = workload.child.lock().await;
        child
            .kill()
            .await
            .map_err(|e| DroidError::Process(format!("kill failed: {e}")))?;
        info!(pod = %workload.pod_name, "workload stopped");
        Ok(())
    }

    #[instrument(skip(self, workload), fields(pod = %workload.pod_name))]
    async fn status(&self, workload: &RunningWorkload) -> Result<PodRunStatus> {
        let mut child = workload.child.lock().await;

        let (phase, exit_code, running) = match child.try_wait() {
            Ok(Some(status)) => {
                let code = status.code();
                let phase = if status.success() {
                    PodPhase::Succeeded
                } else {
                    PodPhase::Failed
                };
                (phase, code, false)
            }
            Ok(None) => (PodPhase::Running, None, true),
            Err(e) => {
                warn!(pod = %workload.pod_name, error = %e, "could not check child status");
                (PodPhase::Unknown, None, false)
            }
        };

        Ok(PodRunStatus {
            pod_uid: workload.pod_uid.clone(),
            pod_name: workload.pod_name.clone(),
            namespace: workload.namespace.clone(),
            phase,
            message: None,
            containers: vec![ContainerRunStatus {
                name: workload.container_name.clone(),
                ready: running,
                running,
                exit_code,
            }],
        })
    }

    #[instrument(skip(self, workload, probe), fields(pod = %workload.pod_name))]
    async fn run_liveness_probe(
        &self,
        workload: &RunningWorkload,
        probe: &ProbeConfig,
    ) -> Result<()> {
        self.probe_service
            .run_probe(probe, &workload.rootfs, None)
            .await
    }
}
