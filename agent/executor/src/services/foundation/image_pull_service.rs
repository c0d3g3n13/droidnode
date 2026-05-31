use async_trait::async_trait;
use std::sync::Arc;
use tracing::{info, instrument};

use crate::brokers::{FilesystemBroker, OciRegistryBroker};
use crate::error::Result;
use crate::models::{Digest, ImageConfig, ImageRef, Manifest};

// A pulled image: manifest, config, and the digest of each downloaded layer blob.
#[derive(Debug)]
pub struct PulledImage {
    pub manifest: Manifest,
    pub config: ImageConfig,
    pub layer_digests: Vec<Digest>,
}

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ImagePullService: Send + Sync {
    async fn pull_image(&self, image_ref: &ImageRef) -> Result<PulledImage>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ImagePullServiceImpl {
    oci_broker: Arc<dyn OciRegistryBroker>,
    fs_broker: Arc<dyn FilesystemBroker>,
}

impl ImagePullServiceImpl {
    pub fn new(
        oci_broker: Arc<dyn OciRegistryBroker>,
        fs_broker: Arc<dyn FilesystemBroker>,
    ) -> Self {
        Self { oci_broker, fs_broker }
    }
}

#[async_trait]
impl ImagePullService for ImagePullServiceImpl {
    #[instrument(skip(self), fields(image = %image_ref.repository))]
    async fn pull_image(&self, image_ref: &ImageRef) -> Result<PulledImage> {
        info!(image = %image_ref.repository, reference = %image_ref.reference, "pulling image");

        let manifest = self.oci_broker.fetch_manifest(image_ref).await?;
        let config_digest = Digest::new(&manifest.config.digest);
        let config = self.oci_broker.fetch_config(image_ref, &config_digest).await?;

        let mut layer_digests = Vec::with_capacity(manifest.layers.len());

        for layer_desc in &manifest.layers {
            let digest = Digest::new(&layer_desc.digest);

            if self.fs_broker.layer_exists(&digest).await? {
                info!(%digest, "layer already cached, skipping fetch");
                layer_digests.push(digest);
                continue;
            }

            info!(%digest, bytes = layer_desc.size, "fetching layer");
            let blob_path = self.fs_broker.create_layer_dir(&digest).await?;
            // Write the compressed tar to <layer_dir>/layer.tar.gz
            let tar_path = blob_path.join("layer.tar.gz");
            let data = self.oci_broker.fetch_layer(image_ref, &digest).await?;
            self.fs_broker.write_layer(&tar_path, data).await?;

            layer_digests.push(digest);
        }

        Ok(PulledImage {
            manifest,
            config,
            layer_digests,
        })
    }
}
