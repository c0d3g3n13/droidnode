use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Child;
use tracing::{info, instrument};

use crate::brokers::ProotBroker;
use crate::error::{DroidError, Result};
use crate::models::{ContainerConfig, Mount, Pod};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait WorkloadExecutionService: Send + Sync {
    /// Launch the first container in `pod` using the merged rootfs at `rootfs_path`.
    /// Returns a handle to the running child process.
    async fn execute_workload(&self, pod: &Pod, rootfs_path: &Path) -> Result<Child>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct WorkloadExecutionServiceImpl {
    proot_broker: Arc<dyn ProotBroker>,
}

impl WorkloadExecutionServiceImpl {
    pub fn new(proot_broker: Arc<dyn ProotBroker>) -> Self {
        Self { proot_broker }
    }

    /// Build the command vector from the pod spec and image config defaults.
    fn resolve_command(
        container: &k8s_openapi::api::core::v1::Container,
        image_config: Option<&ContainerConfig>,
    ) -> Vec<String> {
        // Pod spec command overrides image ENTRYPOINT.
        // Pod spec args overrides image CMD.
        let entrypoint = container
            .command
            .as_deref()
            .or_else(|| image_config.and_then(|c| c.entrypoint.as_deref()))
            .unwrap_or(&[]);

        let args = container
            .args
            .as_deref()
            .or_else(|| image_config.and_then(|c| c.cmd.as_deref()))
            .unwrap_or(&[]);

        entrypoint.iter().chain(args.iter()).cloned().collect()
    }

    fn resolve_env(
        container: &k8s_openapi::api::core::v1::Container,
        image_config: Option<&ContainerConfig>,
    ) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = image_config
            .and_then(|c| c.env.as_ref())
            .map(|vars| {
                vars.iter()
                    .filter_map(|e| {
                        let mut parts = e.splitn(2, '=');
                        let k = parts.next()?.to_string();
                        let v = parts.next().unwrap_or("").to_string();
                        Some((k, v))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Pod-level env vars override image defaults
        if let Some(pod_env) = &container.env {
            for e in pod_env {
                if let Some(val) = &e.value {
                    env.push((e.name.clone(), val.clone()));
                }
            }
        }

        env
    }

    fn resolve_mounts(container: &k8s_openapi::api::core::v1::Container) -> Vec<Mount> {
        container
            .volume_mounts
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|vm| Mount {
                source: PathBuf::from(&vm.mount_path), // source is pod volume; simplified
                target: PathBuf::from(&vm.mount_path),
                read_only: vm.read_only.unwrap_or(false),
            })
            .collect()
    }
}

#[async_trait]
impl WorkloadExecutionService for WorkloadExecutionServiceImpl {
    #[instrument(skip(self, pod), fields(
        pod = pod.metadata.name.as_deref().unwrap_or("?"),
        rootfs = %rootfs_path.display()
    ))]
    async fn execute_workload(&self, pod: &Pod, rootfs_path: &Path) -> Result<Child> {
        let containers = pod
            .spec
            .as_ref()
            .and_then(|s| s.containers.first())
            .ok_or_else(|| DroidError::Workload("pod has no containers".into()))?;

        // Load the image's ContainerConfig written by ImageOrchestrationService.
        // This gives us ENTRYPOINT/CMD/ENV from the image when the pod spec omits them.
        let image_config: Option<ContainerConfig> = {
            let cfg_path = rootfs_path.join(".droidnode_image_config.json");
            match tokio::fs::read(&cfg_path).await {
                Ok(bytes) => serde_json::from_slice(&bytes).ok(),
                Err(_) => None,
            }
        };

        let command = Self::resolve_command(containers, image_config.as_ref());
        if command.is_empty() {
            return Err(DroidError::Workload(format!(
                "pod {}: no command or entrypoint defined",
                pod.metadata.name.as_deref().unwrap_or("?")
            )));
        }

        let env = Self::resolve_env(containers, image_config.as_ref());
        let mounts = Self::resolve_mounts(containers);

        info!(
            pod = pod.metadata.name.as_deref().unwrap_or("?"),
            command = ?command,
            "executing workload"
        );

        self.proot_broker
            .execute(rootfs_path, &command, &env, &mounts)
            .await
    }
}
