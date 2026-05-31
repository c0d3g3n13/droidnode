use async_trait::async_trait;
use bytes::Bytes;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::instrument;

use crate::error::Result;
use crate::models::Digest;

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait FilesystemBroker: Send + Sync {
    async fn create_layer_dir(&self, digest: &Digest) -> Result<PathBuf>;
    async fn write_layer(&self, path: &Path, data: Bytes) -> Result<()>;
    async fn remove_layer(&self, digest: &Digest) -> Result<()>;
    async fn layer_exists(&self, digest: &Digest) -> Result<bool>;
    async fn available_bytes(&self) -> Result<u64>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct FilesystemBrokerImpl {
    layers_root: PathBuf,
}

impl FilesystemBrokerImpl {
    pub fn new(layers_root: PathBuf) -> Self {
        Self { layers_root }
    }

    fn layer_path(&self, digest: &Digest) -> PathBuf {
        self.layers_root.join(digest.as_fs_name())
    }
}

#[async_trait]
impl FilesystemBroker for FilesystemBrokerImpl {
    #[instrument(skip(self), fields(digest = %digest))]
    async fn create_layer_dir(&self, digest: &Digest) -> Result<PathBuf> {
        let path = self.layer_path(digest);
        fs::create_dir_all(&path).await?;
        Ok(path)
    }

    #[instrument(skip(self, data), fields(path = %path.display(), bytes = data.len()))]
    async fn write_layer(&self, path: &Path, data: Bytes) -> Result<()> {
        fs::write(path, data).await?;
        Ok(())
    }

    #[instrument(skip(self), fields(digest = %digest))]
    async fn remove_layer(&self, digest: &Digest) -> Result<()> {
        let path = self.layer_path(digest);
        if path.exists() {
            fs::remove_dir_all(&path).await?;
        }
        Ok(())
    }

    #[instrument(skip(self), fields(digest = %digest))]
    async fn layer_exists(&self, digest: &Digest) -> Result<bool> {
        Ok(self.layer_path(digest).exists())
    }

    #[instrument(skip(self))]
    async fn available_bytes(&self) -> Result<u64> {
        available_space(&self.layers_root)
    }
}

// ─── Platform helpers ─────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn available_space(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    let cpath = CString::new(path.to_str().unwrap_or("/")).map_err(|e| {
        crate::error::DroidError::Filesystem(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            e,
        ))
    })?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: path is a valid null-terminated string
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(crate::error::DroidError::Filesystem(
            std::io::Error::last_os_error(),
        ));
    }
    Ok(stat.f_bavail * stat.f_bsize)
}

#[cfg(not(target_os = "linux"))]
fn available_space(_path: &Path) -> Result<u64> {
    // Stub for non-Linux development builds
    Ok(u64::MAX)
}
