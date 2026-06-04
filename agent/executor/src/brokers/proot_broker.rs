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

        let guest_tmp = rootfs.join("tmp");
        tokio::fs::create_dir_all(&guest_tmp).await?;
        ensure_workload_command_exists(rootfs, command)?;
        let command = resolve_proot_command(rootfs, command)?;

        let mut cmd = Command::new(&self.proot_path);

        // Disable proot's seccomp acceleration — conflicts with host and Android kernel
        // seccomp filters. Pure ptrace mode is slower but works everywhere.
        cmd.env("PROOT_NO_SECCOMP", "1");
        cmd.env("PROOT_TMP_DIR", &guest_tmp);
        cmd.env("TMPDIR", &guest_tmp);

        // Root filesystem
        cmd.args(["-r", rootfs.to_str().unwrap_or("/")]);
        cmd.args(["-w", "/"]);

        // Default Linux pseudo-filesystems
        cmd.args(["-b", "/dev"]);
        cmd.args(["-b", "/proc"]);
        cmd.args(["-b", "/sys"]);

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
        cmd.args(&command);

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

fn resolve_proot_command(rootfs: &Path, command: &[String]) -> Result<Vec<String>> {
    let program = Path::new(&command[0]);

    if !program.is_absolute() {
        return Ok(command.to_vec());
    }

    let host_program = resolve_guest_path_to_host(rootfs, program)?;
    let Some(interpreter) = read_elf_interpreter(&host_program)? else {
        return Ok(command.to_vec());
    };

    let guest_interpreter = PathBuf::from(&interpreter);
    let resolved_interpreter = resolve_guest_path(rootfs, &guest_interpreter)?;
    let host_interpreter = guest_path_to_host(rootfs, &resolved_interpreter);

    if !host_interpreter.exists() {
        return Err(DroidError::Workload(format!(
            "ELF interpreter {interpreter} for {} is missing at {}",
            program.display(),
            host_interpreter.display()
        )));
    }

    let mut rewritten = Vec::with_capacity(command.len() + 1);
    rewritten.push(resolved_interpreter.display().to_string());
    rewritten.extend(command.iter().cloned());

    info!(
        program = %program.display(),
        interpreter = %resolved_interpreter.display(),
        host_interpreter = %host_interpreter.display(),
        "running workload through guest ELF interpreter"
    );

    Ok(rewritten)
}

fn guest_path_to_host(rootfs: &Path, guest_path: &Path) -> PathBuf {
    guest_path
        .strip_prefix("/")
        .map(|relative| rootfs.join(relative))
        .unwrap_or_else(|_| rootfs.join(guest_path))
}

fn resolve_guest_path(rootfs: &Path, guest_path: &Path) -> Result<PathBuf> {
    let mut current_guest = guest_path.to_path_buf();

    for _ in 0..16 {
        let host_path = guest_path_to_host(rootfs, &current_guest);
        let metadata = std::fs::symlink_metadata(&host_path)?;

        if !metadata.file_type().is_symlink() {
            return Ok(current_guest);
        }

        let target = std::fs::read_link(&host_path)?;
        current_guest = if target.is_absolute() {
            target
        } else {
            current_guest
                .parent()
                .unwrap_or_else(|| Path::new("/"))
                .join(target)
        };
    }

    Err(DroidError::Workload(format!(
        "too many symlinks while resolving {}",
        guest_path.display()
    )))
}

fn resolve_guest_path_to_host(rootfs: &Path, guest_path: &Path) -> Result<PathBuf> {
    let resolved_guest_path = resolve_guest_path(rootfs, guest_path)?;
    Ok(guest_path_to_host(rootfs, &resolved_guest_path))
}

fn read_elf_interpreter(path: &Path) -> Result<Option<String>> {
    let bytes = std::fs::read(path)?;

    if bytes.len() < 64 || &bytes[0..4] != b"\x7FELF" {
        return Ok(None);
    }

    let class = bytes[4];
    let endian = bytes[5];

    if endian != 1 {
        return Ok(None);
    }

    match class {
        1 => read_elf32_interpreter(&bytes),
        2 => read_elf64_interpreter(&bytes),
        _ => Ok(None),
    }
}

fn read_elf64_interpreter(bytes: &[u8]) -> Result<Option<String>> {
    let phoff = read_u64_le(bytes, 32)? as usize;
    let phentsize = read_u16_le(bytes, 54)? as usize;
    let phnum = read_u16_le(bytes, 56)? as usize;

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        if offset + 56 > bytes.len() {
            break;
        }

        let p_type = read_u32_le(bytes, offset)?;
        if p_type != 3 {
            continue;
        }

        let p_offset = read_u64_le(bytes, offset + 8)? as usize;
        let p_filesz = read_u64_le(bytes, offset + 32)? as usize;
        return read_interpreter_string(bytes, p_offset, p_filesz);
    }

    Ok(None)
}

fn read_elf32_interpreter(bytes: &[u8]) -> Result<Option<String>> {
    let phoff = read_u32_le(bytes, 28)? as usize;
    let phentsize = read_u16_le(bytes, 42)? as usize;
    let phnum = read_u16_le(bytes, 44)? as usize;

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        if offset + 32 > bytes.len() {
            break;
        }

        let p_type = read_u32_le(bytes, offset)?;
        if p_type != 3 {
            continue;
        }

        let p_offset = read_u32_le(bytes, offset + 4)? as usize;
        let p_filesz = read_u32_le(bytes, offset + 16)? as usize;
        return read_interpreter_string(bytes, p_offset, p_filesz);
    }

    Ok(None)
}

fn read_interpreter_string(bytes: &[u8], offset: usize, len: usize) -> Result<Option<String>> {
    if offset >= bytes.len() {
        return Ok(None);
    }

    let end = offset.saturating_add(len).min(bytes.len());
    let raw = &bytes[offset..end];
    let nul = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());

    String::from_utf8(raw[..nul].to_vec())
        .map(Some)
        .map_err(|e| DroidError::Workload(format!("invalid ELF interpreter string: {e}")))
}

fn read_u16_le(bytes: &[u8], offset: usize) -> Result<u16> {
    let Some(slice) = bytes.get(offset..offset + 2) else {
        return Err(DroidError::Workload("truncated ELF header".into()));
    };
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32> {
    let Some(slice) = bytes.get(offset..offset + 4) else {
        return Err(DroidError::Workload("truncated ELF header".into()));
    };
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Result<u64> {
    let Some(slice) = bytes.get(offset..offset + 8) else {
        return Err(DroidError::Workload("truncated ELF header".into()));
    };
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
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
        // Use full path + shell to avoid PATH resolution issues inside the guest rootfs.
        let cmd = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "echo hello from proot".to_string(),
        ];
        let child = broker.execute(&rootfs, &cmd, &[], &[]).await.unwrap();

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
