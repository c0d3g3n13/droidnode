use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tracing::{info, instrument};

use crate::error::Result;
use crate::models::ImageRef;
use crate::services::foundation::{
    image_pull_service::ImagePullService,
    image_unpack_service::ImageUnpackService,
};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ImageOrchestrationService: Send + Sync {
    /// Pull → unpack → merge → return path to merged rootfs.
    async fn prepare_image(&self, image_ref: &ImageRef) -> Result<PathBuf>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ImageOrchestrationServiceImpl {
    pull_service: Arc<dyn ImagePullService>,
    unpack_service: Arc<dyn ImageUnpackService>,
    rootfs_base: PathBuf,
}

impl ImageOrchestrationServiceImpl {
    pub fn new(
        pull_service: Arc<dyn ImagePullService>,
        unpack_service: Arc<dyn ImageUnpackService>,
        rootfs_base: PathBuf,
    ) -> Self {
        Self { pull_service, unpack_service, rootfs_base }
    }

    fn image_rootfs_path(&self, image_ref: &ImageRef) -> PathBuf {
        let safe_ref = image_ref.reference.replace(':', "-").replace('@', "_at_");
        self.rootfs_base
            .join(&image_ref.registry)
            .join(&image_ref.repository)
            .join(&safe_ref)
    }
}

#[async_trait]
impl ImageOrchestrationService for ImageOrchestrationServiceImpl {
    #[instrument(skip(self), fields(image = %image_ref.repository, reference = %image_ref.reference))]
    async fn prepare_image(&self, image_ref: &ImageRef) -> Result<PathBuf> {
        let rootfs = self.image_rootfs_path(image_ref);

        if rootfs.join(".droidnode_ready").exists() {
            info!(rootfs = %rootfs.display(), "rootfs already prepared");
            return Ok(rootfs);
        }

        // 1. Pull all layers (downloads only what is not cached)
        let pulled = self.pull_service.pull_image(image_ref).await?;
        info!(layers = pulled.layer_digests.len(), "image pulled");

        // 2. Apply layers in order (base → top) into the merged rootfs directory
        fs::create_dir_all(&rootfs).await?;
        for digest in &pulled.layer_digests {
            info!(%digest, rootfs = %rootfs.display(), "applying layer");
            self.unpack_service.unpack_layer(digest, &rootfs).await?;
        }

        // 3. Persist the image's ContainerConfig (entrypoint, cmd, env) so the
        //    execution service can resolve the command without re-fetching from registry.
        if let Some(container_config) = pulled.config.config.as_ref() {
            let json = serde_json::to_vec(container_config)?;
            fs::write(rootfs.join(".droidnode_image_config.json"), json).await?;
        }

        // 4. Sentinel file marks this rootfs as fully prepared
        fs::write(rootfs.join(".droidnode_ready"), b"1").await?;
        info!(rootfs = %rootfs.display(), "rootfs ready");

        Ok(rootfs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brokers::{FilesystemBroker, FilesystemBrokerImpl, OciRegistryBroker, OciRegistryBrokerImpl};
    use crate::services::foundation::{
        image_pull_service::{ImagePullService, ImagePullServiceImpl},
        image_unpack_service::{ImageUnpackService, ImageUnpackServiceImpl},
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn test_prepare_alpine_rootfs() {
        let tmp = std::env::temp_dir().join("droidnode_orch_test");
        let layers_dir = tmp.join("layers");
        let rootfs_dir = tmp.join("rootfs");

        let oci = Arc::new(OciRegistryBrokerImpl::new());
        let fs = Arc::new(FilesystemBrokerImpl::new(layers_dir));

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
            rootfs_dir.clone(),
        );

        let image_ref = crate::models::ImageRef::parse("alpine:latest").unwrap();
        let rootfs = svc.prepare_image(&image_ref).await.unwrap();

        assert!(rootfs.join(".droidnode_ready").exists(), "sentinel missing");
        assert!(rootfs.join("bin").exists(), "rootfs /bin missing");
        assert!(rootfs.join("etc").exists(), "rootfs /etc missing");

        println!("rootfs at: {}", rootfs.display());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
