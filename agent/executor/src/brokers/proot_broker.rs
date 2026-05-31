use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};
use tracing::instrument;

use crate::error::{DroidError, Result};
use crate::models::Mount;

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ProotBroker: Send + Sync {
    async fn execute(
        &self,
        rootfs: &Path,
        command: &[String],
        env: &[(String, String)],
        mounts: &[Mount],
    ) -> Result<Child>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ProotBrokerImpl {
    proot_path: PathBuf,
}

impl ProotBrokerImpl {
    pub fn new(proot_path: PathBuf) -> Self {
        Self { proot_path }
    }
}

#[async_trait]
impl ProotBroker for ProotBrokerImpl {
    #[instrument(skip(self, env, mounts), fields(
        rootfs = %rootfs.display(),
        command = ?command
    ))]
    async fn execute(
        &self,
        rootfs: &Path,
        command: &[String],
        env: &[(String, String)],
        mounts: &[Mount],
    ) -> Result<Child> {
        if command.is_empty() {
            return Err(DroidError::Process("command is empty".into()));
        }

        let mut cmd = Command::new(&self.proot_path);

        // Root filesystem
        cmd.args(["-r", rootfs.to_str().unwrap_or("/")]);

        // Default Linux pseudo-filesystems
        cmd.args(["-b", "/dev"]);
        cmd.args(["-b", "/proc"]);
        cmd.args(["-b", "/sys"]);

        // Additional mounts from the pod spec
        for mount in mounts {
            let bind = if mount.read_only {
                format!(
                    "{}:{}",
                    mount.source.display(),
                    mount.target.display()
                )
            } else {
                format!(
                    "{}:{}",
                    mount.source.display(),
                    mount.target.display()
                )
            };
            cmd.args(["-b", &bind]);
        }

        // Environment variables
        for (key, val) in env {
            cmd.env(key, val);
        }

        // Workload command
        cmd.args(command);

        // Capture stdout/stderr for log streaming
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd
            .spawn()
            .map_err(|e| DroidError::Process(format!("proot spawn failed: {e}")))?;

        Ok(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brokers::{FilesystemBroker, FilesystemBrokerImpl, OciRegistryBroker, OciRegistryBrokerImpl};
    use crate::services::foundation::image_pull_service::{ImagePullService, ImagePullServiceImpl};
    use crate::services::foundation::image_unpack_service::{ImageUnpackService, ImageUnpackServiceImpl};
    use crate::services::orchestration::image_orchestration_service::{
        ImageOrchestrationService, ImageOrchestrationServiceImpl,
    };
    use std::sync::Arc;

    async fn prepare_alpine(tmp: &std::path::Path) -> std::path::PathBuf {
        let oci = Arc::new(OciRegistryBrokerImpl::new());
        let fs = Arc::new(FilesystemBrokerImpl::new(tmp.join("layers")));
        let pull = Arc::new(ImagePullServiceImpl::new(
            Arc::clone(&oci) as Arc<dyn OciRegistryBroker>,
            Arc::clone(&fs) as Arc<dyn FilesystemBroker>,
        ));
        let unpack = Arc::new(ImageUnpackServiceImpl::new(
            Arc::clone(&fs) as Arc<dyn FilesystemBroker>,
        ));
        let svc = ImageOrchestrationServiceImpl::new(
            pull as Arc<dyn ImagePullService>,
            unpack as Arc<dyn ImageUnpackService>,
            tmp.join("rootfs"),
        );
        let image_ref = crate::models::ImageRef::parse("alpine:latest").unwrap();
        svc.prepare_image(&image_ref).await.unwrap()
    }

    #[tokio::test]
    async fn test_proot_echo() {
        let proot_path = std::path::PathBuf::from(
            std::env::var("PROOT_PATH").unwrap_or_else(|_| "/usr/bin/proot".into()),
        );
        if !proot_path.exists() {
            eprintln!("proot not found at {}, skipping", proot_path.display());
            return;
        }

        let tmp = std::env::temp_dir().join("droidnode_proot_test");
        let rootfs = prepare_alpine(&tmp).await;

        let broker = ProotBrokerImpl::new(proot_path);
        let cmd = vec!["echo".to_string(), "hello from proot".to_string()];
        let mut child = broker.execute(&rootfs, &cmd, &[], &[]).await.unwrap();

        let output = child.wait_with_output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        println!("stdout: {stdout}");
        assert!(output.status.success(), "proot exited with {:?}", output.status.code());
        assert!(stdout.contains("hello from proot"), "unexpected stdout: {stdout}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
