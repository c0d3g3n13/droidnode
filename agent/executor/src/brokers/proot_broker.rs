use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::process::{Child, Command};
use tracing::{info, instrument, warn};

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
        cwd: Option<&str>,
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
        cwd: Option<&str>,
    ) -> Result<Child> {
        if command.is_empty() {
            return Err(DroidError::Process("command is empty".into()));
        }

        let guest_tmp = rootfs.join("tmp");
        tokio::fs::create_dir_all(&guest_tmp).await?;

        // PROOT_TMP_DIR must be outside the rootfs (proot must not translate its own
        // loader path) and on an executable filesystem. On Android, code_cache is exec-
        // able while filesDir is noexec on API 29+. The Kotlin layer sets TMPDIR to
        // code_cache/tmp, so inherit that; fall back to the OS temp dir on desktop Linux.
        let proot_tmp = std::env::var("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        tokio::fs::create_dir_all(&proot_tmp).await?;

        ensure_workload_command_exists(rootfs, command)?;

        let mut cmd = Command::new(&self.proot_path);

        // Disable proot's seccomp acceleration — conflicts with host and Android kernel
        // seccomp filters. Pure ptrace mode is slower but works everywhere.
        cmd.env("PROOT_NO_SECCOMP", "1");
        cmd.env("PROOT_TMP_DIR", &proot_tmp);
        // TMPDIR must be the guest path (/tmp), not the host path — proot remaps it.
        cmd.env("TMPDIR", "/tmp");
        // Default PATH for the guest — Android's inherited PATH only has /system/bin etc.
        // Pod-specified env vars applied below will override this if the pod sets its own PATH.
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

        // On Android, untrusted_app cannot execve files from code_cache/tmp
        // (dalvikcache_data_file type lacks `execute` SELinux permission on most vendor
        // kernels). proot's default behaviour — extract its embedded loader to PROOT_TMP_DIR
        // and then execve it — therefore silently fails; proot falls back to direct execve
        // which fails for musl ELFs (ld-musl-aarch64.so.1 is not on the Android host).
        //
        // Fix: ship the loader as libproot_loader.so in jniLibs. That directory has
        // nativelib_data_file SELinux type, which untrusted_app CAN execve. Tell proot
        // where the loader is via BOTH the env-var (PROOT_LOADER) and the CLI flag
        // (--loader) so that whichever mechanism the running proot version honours is used.
        let loader = self.proot_path.with_file_name("libproot_loader.so");
        let loader_found = loader.exists();
        if loader_found {
            // Termux proot does not accept a --loader CLI flag; use the env var instead.
            cmd.env("PROOT_LOADER", &loader);
        } else {
            warn!(
                loader = %loader.display(),
                "libproot_loader.so missing from nativeLibDir — musl ELF exec will fail on Android"
            );
        }

        info!(
            proot       = %self.proot_path.display(),
            rootfs      = %rootfs.display(),
            proot_tmp   = %proot_tmp.display(),
            loader      = %loader.display(),
            loader_found,
            command     = ?command,
            "spawning proot"
        );

        // Root filesystem
        cmd.args(["-r", rootfs.to_str().unwrap_or("/")]);
        cmd.args(["-w", cwd.unwrap_or("/")]);

        // Default Linux pseudo-filesystems
        cmd.args(["-b", "/dev"]);
        cmd.args(["-b", "/proc"]);
        cmd.args(["-b", "/sys"]);

        // Bind host DNS config so containers can resolve names (needed for apk, curl, etc.).
        // /etc/hosts gives localhost and the device hostname; resolv.conf gives nameservers.
        for host_file in ["/etc/resolv.conf", "/etc/hosts"] {
            if std::path::Path::new(host_file).exists() {
                cmd.args(["-b", host_file]);
            }
        }

        // Additional mounts from the pod spec
        for mount in mounts {
            if !mount.source.exists() {
                warn!(
                    source = %mount.source.display(),
                    target = %mount.target.display(),
                    "skipping bind mount because host source does not exist"
                );
                continue;
            }

            let bind = format!(
                "{}:{}",
                mount.source.display(),
                mount.target.display()
            );
            cmd.args(["-b", &bind]);
        }

        // Environment variables
        for (key, val) in env {
            cmd.env(key, val);
        }

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

fn ensure_workload_command_exists(rootfs: &Path, command: &[String]) -> Result<()> {
    let program = Path::new(&command[0]);

    if !program.is_absolute() {
        return Ok(());
    }

    let guest_program = program
        .strip_prefix("/")
        .map(|relative| rootfs.join(relative))
        .unwrap_or_else(|_| rootfs.join(program));

    let metadata = std::fs::symlink_metadata(&guest_program);
    match metadata {
        Ok(meta) => {
            let file_type = meta.file_type();
            info!(
                guest = %program.display(),
                host = %guest_program.display(),
                is_file = file_type.is_file(),
                is_symlink = file_type.is_symlink(),
                "workload command found in rootfs"
            );
            Ok(())
        }
        Err(e) => {
            let bin_entries = std::fs::read_dir(rootfs.join("bin"))
                .map(|entries| {
                    entries
                        .filter_map(|entry| entry.ok())
                        .take(20)
                        .filter_map(|entry| entry.file_name().into_string().ok())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_else(|_| "<missing bin dir>".to_string());

            Err(DroidError::Workload(format!(
                "workload command {} not found at {}: {e}; rootfs bin entries: {bin_entries}",
                program.display(),
                guest_program.display()
            )))
        }
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
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hello from proot".to_string(),
        ];
        let child = broker.execute(&rootfs, &cmd, &[], &[], None).await.unwrap();

        let output = child.wait_with_output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        println!("stdout: {stdout}");
        println!("stderr: {stderr}");
        assert!(
            output.status.success(),
            "proot exited with {:?}\nstderr: {stderr}",
            output.status.code()
        );
        assert!(stdout.contains("hello from proot"), "unexpected stdout: {stdout}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
