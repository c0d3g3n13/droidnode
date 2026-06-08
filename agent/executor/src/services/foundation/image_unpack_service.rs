use async_trait::async_trait;
use flate2::read::GzDecoder;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tar::{Archive, EntryType};
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

fn other_err(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::Other, msg)
}

fn extract_layer(tar_path: &Path, target: &Path) -> Result<()> {
    let f = std::fs::File::open(tar_path)?;
    let gz = GzDecoder::new(f);
    let mut archive = Archive::new(gz);

    // Preserve permissions (but not ownership — we are not root)
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);

    // Hardlinks whose link target hadn't been extracted yet when first encountered.
    // Resolved by copy in a second pass after the full archive is read.
    let mut deferred_hardlinks: Vec<(PathBuf, PathBuf)> = Vec::new();

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.into_owned();
        let entry_name = entry_path.to_string_lossy();
        let entry_type = entry.header().entry_type();

        // Read link name from header before consuming the entry (hardlinks only).
        let link_name: Option<PathBuf> = if entry_type == EntryType::Link {
            entry.link_name().ok().flatten().map(|p| p.into_owned())
        } else {
            None
        };

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

        match entry.unpack_in(target) {
            Ok(_) => {}
            Err(_) if entry_type == EntryType::Link => {
                // Hardlink creation can fail on Android (protected_hardlinks kernel
                // setting, or the link target appears later in the stream). Fall back
                // to a file copy; defer if the source isn't extracted yet.
                if let Some(src_rel) = &link_name {
                    let src = target.join(src_rel);
                    let dst = target.join(&entry_path);
                    if src.exists() {
                        hardlink_copy(&src, &dst)?;
                    } else {
                        deferred_hardlinks.push((src_rel.clone(), entry_path.clone()));
                    }
                } else {
                    return Err(other_err(format!(
                        "failed to unpack `{}`",
                        target.join(&entry_path).display()
                    ))
                    .into());
                }
            }
            Err(_) => {
                return Err(other_err(format!(
                    "failed to unpack `{}`",
                    target.join(&entry_path).display()
                ))
                .into());
            }
        }
    }

    // Second pass: resolve deferred hardlinks whose target appeared later in the stream.
    for (src_rel, dst_rel) in &deferred_hardlinks {
        let src = target.join(src_rel);
        let dst = target.join(dst_rel);
        if src.exists() {
            warn!(src = %src.display(), dst = %dst.display(), "resolving deferred hardlink via copy");
            hardlink_copy(&src, &dst)?;
        } else {
            return Err(other_err(format!(
                "failed to unpack `{}` (hardlink target `{}` not found)",
                dst.display(),
                src.display()
            ))
            .into());
        }
    }

    Ok(())
}

fn hardlink_copy(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst).map_err(|e| {
        other_err(format!(
            "failed to copy hardlink `{}` -> `{}`: {e}",
            src.display(),
            dst.display()
        ))
    })?;
    Ok(())
}
