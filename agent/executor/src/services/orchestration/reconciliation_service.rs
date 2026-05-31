use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{error, info, instrument, warn};

use crate::brokers::ControlPlaneBroker;
use crate::error::Result;
use crate::models::PodPhase;
use crate::services::foundation::event_recording_service::EventRecordingService;
use crate::services::orchestration::{
    image_orchestration_service::ImageOrchestrationService,
    workload_lifecycle_service::{RunningWorkload, WorkloadLifecycleService},
};

const RECONCILE_INTERVAL_SECS: u64 = 15;

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ReconciliationService: Send + Sync {
    /// Run the reconciliation loop forever. Call this on a dedicated task.
    async fn run_loop(&self) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ReconciliationServiceImpl {
    cp_broker: Arc<dyn ControlPlaneBroker>,
    image_orch: Arc<dyn ImageOrchestrationService>,
    workload_lifecycle: Arc<dyn WorkloadLifecycleService>,
    event_service: Arc<dyn EventRecordingService>,
    node_name: String,
    // pod_uid → running workload
    running: Mutex<HashMap<String, Arc<RunningWorkload>>>,
}

impl ReconciliationServiceImpl {
    pub fn new(
        cp_broker: Arc<dyn ControlPlaneBroker>,
        image_orch: Arc<dyn ImageOrchestrationService>,
        workload_lifecycle: Arc<dyn WorkloadLifecycleService>,
        event_service: Arc<dyn EventRecordingService>,
        node_name: String,
    ) -> Self {
        Self {
            cp_broker,
            image_orch,
            workload_lifecycle,
            event_service,
            node_name,
            running: Mutex::new(HashMap::new()),
        }
    }

    #[instrument(skip(self))]
    async fn reconcile_once(&self) -> Result<()> {
        // 1. Fetch desired state
        let desired_pods = self.cp_broker.get_assigned_pods().await?;

        // 2. Fetch actual state
        let mut running = self.running.lock().await;

        let desired_uids: std::collections::HashSet<String> = desired_pods
            .iter()
            .filter_map(|p| p.metadata.uid.clone())
            .collect();

        // 3a. Pod in desired but not in actual → start it
        for pod in &desired_pods {
            let uid = match &pod.metadata.uid {
                Some(u) => u.clone(),
                None => continue,
            };

            if running.contains_key(&uid) {
                // Already running; update status
                let workload = running.get(&uid).unwrap();
                let status = self.workload_lifecycle.status(workload).await?;
                self.cp_broker
                    .report_pod_status(&workload.pod_name, &workload.namespace, &status)
                    .await?;
                continue;
            }

            // New pod: pull image and start
            let image_name = pod
                .spec
                .as_ref()
                .and_then(|s| s.containers.first())
                .map(|c| c.image.clone().unwrap_or_default())
                .unwrap_or_default();

            if image_name.is_empty() {
                warn!(pod = ?pod.metadata.name, "pod has no image; skipping");
                continue;
            }

            info!(
                pod = ?pod.metadata.name,
                image = %image_name,
                "new pod assigned — preparing image"
            );

            let image_ref = match crate::models::ImageRef::parse(&image_name) {
                Ok(r) => r,
                Err(e) => {
                    error!(pod = ?pod.metadata.name, error = %e, "invalid image ref");
                    continue;
                }
            };

            let rootfs = match self.image_orch.prepare_image(&image_ref).await {
                Ok(p) => p,
                Err(e) => {
                    error!(pod = ?pod.metadata.name, error = %e, "image preparation failed");
                    let _ = self
                        .event_service
                        .record_warning("ImagePullFailed", &e.to_string(), pod.metadata.uid.clone())
                        .await;
                    continue;
                }
            };

            let workload = match self.workload_lifecycle.start(pod, rootfs).await {
                Ok(w) => w,
                Err(e) => {
                    error!(pod = ?pod.metadata.name, error = %e, "workload start failed");
                    let _ = self
                        .event_service
                        .record_warning("WorkloadFailed", &e.to_string(), pod.metadata.uid.clone())
                        .await;
                    continue;
                }
            };

            let _ = self
                .event_service
                .record_normal("Started", "workload started successfully", pod.metadata.uid.clone())
                .await;

            running.insert(uid, workload);
        }

        // 3b. Pod in actual but not desired → stop it
        let to_stop: Vec<String> = running
            .keys()
            .filter(|uid| !desired_uids.contains(*uid))
            .cloned()
            .collect();

        for uid in to_stop {
            if let Some(workload) = running.remove(&uid) {
                info!(pod = %workload.pod_name, "pod no longer desired — stopping");
                if let Err(e) = self.workload_lifecycle.stop(&workload).await {
                    warn!(pod = %workload.pod_name, error = %e, "stop failed");
                }
            }
        }

        // 3c. Check for failed workloads and apply restart policy
        let mut to_restart = Vec::new();
        for (uid, workload) in running.iter() {
            let status = self.workload_lifecycle.status(workload).await?;
            if status.phase == PodPhase::Failed || status.phase == PodPhase::Succeeded {
                info!(pod = %workload.pod_name, phase = ?status.phase, "workload finished");
                to_restart.push(uid.clone());
            }
            self.cp_broker
                .report_pod_status(&workload.pod_name, &workload.namespace, &status)
                .await?;
        }

        // Remove finished workloads (restart logic belongs in WorkloadLifecycle per spec)
        for uid in to_restart {
            running.remove(&uid);
        }

        Ok(())
    }
}

#[async_trait]
impl ReconciliationService for ReconciliationServiceImpl {
    async fn run_loop(&self) -> Result<()> {
        info!(node = %self.node_name, "reconciliation loop started");
        loop {
            if let Err(e) = self.reconcile_once().await {
                error!(error = %e, "reconcile error — will retry");
            }
            tokio::time::sleep(Duration::from_secs(RECONCILE_INTERVAL_SECS)).await;
        }
    }
}
