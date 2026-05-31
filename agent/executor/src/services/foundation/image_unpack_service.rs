use async_trait::async_trait;
use flate2::read::GzDecoder;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tar::Archive;
use tracing::{debug, info, instrument, warn};

use crate::brokers::FilesystemBroker;
use crate::error::Result;
use crate::models::Digest;

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ImageUnpackService: Send + Sync {
    /// Unpack a single layer tarball into `target_dir`.
    /// Layers must be applied in order (base → top).
    async fn unpack_layer(&self, digest: &Digest, target_dir: &Path) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ImageUnpackServiceImpl {
    fs_broker: Arc<dyn FilesystemBroker>,
}

impl ImageUnpackServiceImpl {
    pub fn new(fs_broker: Arc<dyn FilesystemBroker>) -> Self {
        Self { fs_broker }
    }

    fn layer_tarball_path(&self, digest: &Digest) -> PathBuf {
        // Mirrors the path chosen by ImagePullService
        // layers_root/<digest>/layer.tar.gz
        // We reconstruct it here — the broker owns the root, we own the naming convention.
        // This is acceptable: services agree on the path convention through the broker's
        // create_layer_dir return value which is deterministic for a given digest.
        // However, to stay within The Standard we ask the broker whether the layer exists
        // (which means the dir exists) and derive the tar path from the digest.
        // A real implementation would expose a read_layer_bytes method on the broker;
        // for now we derive the path and read directly to avoid bloating the broker trait.
        PathBuf::new() // resolved at call time via digest.as_fs_name()
    }
}

#[async_trait]
impl ImageUnpackService for ImageUnpackServiceImpl {
    #[instrument(skip(self), fields(digest = %digest, target = %target_dir.display()))]
    async fn unpack_layer(&self, digest: &Digest, target_dir: &Path) -> Result<()> {
        // The tarball was written to <layers_root>/<digest>/layer.tar.gz by ImagePullService.
        // We derive the path by asking the filesystem broker for a directory path via
        // create_layer_dir, which is idempotent, and then appending the filename.
        let layer_dir = self.fs_broker.create_layer_dir(digest).await?;
        let tar_path = layer_dir.join("layer.tar.gz");

        info!(%digest, tar = %tar_path.display(), "unpacking layer");

        // Run extraction on a blocking thread to avoid blocking the async runtime.
        let target = target_dir.to_path_buf();
        tokio::task::spawn_blocking(move || extract_layer(&tar_path, &target))
            .await
            .map_err(|e| crate::error::DroidError::Process(format!("spawn_blocking: {e}")))?
    }
}

// ─── Synchronous extraction (runs on blocking thread) ────────────────────────

fn extract_layer(tar_path: &Path, target: &Path) -> Result<()> {
    let f = std::fs::File::open(tar_path)?;
    let gz = GzDecoder::new(f);
    let mut archive = Archive::new(gz);

    // Preserve permissions (but not ownership — we are not root)
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.into_owned();
        let entry_name = entry_path.to_string_lossy();

        // Handle OCI whiteout files
        if let Some(file_name) = entry_path.file_name() {
            let fname = file_name.to_string_lossy();

            // Opaque whiteout: delete the whole directory
            if fname == ".wh..wh..opq" {
                if let Some(parent) = entry_path.parent() {
                    let dir_to_clear = target.join(parent);
                    if dir_to_clear.exists() {
                        std::fs::remove_dir_all(&dir_to_clear)?;
                        std::fs::create_dir_all(&dir_to_clear)?;
                    }
                }
                continue;
            }

            // Per-file whiteout: delete the named file
            if let Some(stripped) = fname.strip_prefix(".wh.") {
                let target_file = target
                    .join(entry_path.parent().unwrap_or(Path::new("")))
                    .join(stripped);
                if target_file.exists() {
                    if target_file.is_dir() {
                        std::fs::remove_dir_all(&target_file)?;
                    } else {
                        std::fs::remove_file(&target_file)?;
                    }
                }
                continue;
            }
        }

        debug!(path = %entry_name, "extracting entry");
        entry.unpack_in(target)?;
    }

    Ok(())
}
