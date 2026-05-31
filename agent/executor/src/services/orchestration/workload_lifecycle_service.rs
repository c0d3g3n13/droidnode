use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, instrument, warn};

use crate::error::{DroidError, Result};
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
}

impl WorkloadLifecycleServiceImpl {
    pub fn new(
        execution_service: Arc<dyn WorkloadExecutionService>,
        probe_service: Arc<dyn HealthProbeService>,
    ) -> Self {
        Self { execution_service, probe_service }
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

        let child = self.execution_service.execute_workload(pod, &rootfs).await?;
        info!(pod = %pod_name, "workload started");

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
