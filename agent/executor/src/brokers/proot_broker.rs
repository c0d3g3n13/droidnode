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

        // PROOT_TMP_DIR must be outside the rootfs (proot must not translate its own
        // loader path) and on an executable filesystem. On Android, code_cache is exec-
        // able while filesDir is noexec on API 29+. The Kotlin layer sets TMPDIR to
        // code_cache/tmp, so inherit that; fall back to the OS temp dir on desktop Linux.
        let proot_tmp = std::env::var("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        tokio::fs::create_dir_all(&proot_tmp).await?;

        ensure_workload_command_exists(rootfs, command)?;

        // For dynamically-linked ELF binaries the kernel loads PT_INTERP from the HOST
        // filesystem during execve, before proot's ptrace interception is active. On a
        // glibc host (Android/Kali) the guest interpreter (musl, ld-linux-aarch64) does
        // not exist at its expected host path → ENOENT. We avoid this by prepending the
        // symlink-resolved interpreter guest path as the first proot argument. proot execs
        // the interpreter directly (it has no PT_INTERP of its own), and from that point
        // every file access goes through proot's syscall translation which correctly handles
        // rootfs-relative symlinks. The original argv is kept intact so argv[0] is preserved.
        let proot_argv = build_proot_argv(rootfs, command);

        info!(
            proot = %self.proot_path.display(),
            rootfs = %rootfs.display(),
            proot_tmp = %proot_tmp.display(),
            argv = ?proot_argv,
            "spawning proot"
        );

        let mut cmd = Command::new(&self.proot_path);

        // Disable proot's seccomp acceleration — conflicts with host and Android kernel
        // seccomp filters. Pure ptrace mode is slower but works everywhere.
        cmd.env("PROOT_NO_SECCOMP", "1");
        cmd.env("PROOT_TMP_DIR", &proot_tmp);
        // TMPDIR must be the guest path (/tmp), not the host path — proot remaps it.
        cmd.env("TMPDIR", "/tmp");

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

        cmd.args(&proot_argv);

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

// Build the argv vector passed to proot.
//
// For dynamically-linked ELF binaries proot must exec the interpreter (musl ld.so)
// directly so that the kernel never looks for PT_INTERP on the HOST filesystem.
// We prepend the symlink-resolved interpreter guest path; proot translates it to
// a host path inside the rootfs and execs it. The interpreter then opens all
// subsequent files via proot's syscall translation. The original argv is kept so
// that argv[0] inside the guest program is correct (e.g. busybox honours argv[0]
// to decide which applet to run).
fn build_proot_argv(rootfs: &Path, command: &[String]) -> Vec<String> {
    let program = Path::new(&command[0]);
    if !program.is_absolute() {
        return command.to_vec();
    }

    let resolved_program = match resolve_guest_path(rootfs, program) {
        Ok(p) => p,
        Err(_) => return command.to_vec(),
    };
    let host_program = guest_path_to_host(rootfs, &resolved_program);

    let interpreter = match read_elf_interpreter(&host_program) {
        Ok(Some(i)) => i,
        _ => return command.to_vec(),
    };

    let resolved_interp = match resolve_guest_path(rootfs, Path::new(&interpreter)) {
        Ok(p) => p,
        Err(_) => return command.to_vec(),
    };
    if !guest_path_to_host(rootfs, &resolved_interp).exists() {
        return command.to_vec();
    }

    let mut argv = Vec::with_capacity(command.len() + 1);
    argv.push(resolved_interp.display().to_string());
    argv.extend_from_slice(command);

    info!(
        program = %program.display(),
        interpreter = %resolved_interp.display(),
        "prepending ELF interpreter to proot argv"
    );
    argv
}

fn guest_path_to_host(rootfs: &Path, guest_path: &Path) -> PathBuf {
    guest_path
        .strip_prefix("/")
        .map(|rel| rootfs.join(rel))
        .unwrap_or_else(|_| rootfs.join(guest_path))
}

fn resolve_guest_path(rootfs: &Path, guest_path: &Path) -> Result<PathBuf> {
    let mut current = guest_path.to_path_buf();
    for _ in 0..16 {
        let host = guest_path_to_host(rootfs, &current);
        let meta = std::fs::symlink_metadata(&host)?;
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let target = std::fs::read_link(&host)?;
        current = if target.is_absolute() {
            target
        } else {
            current.parent().unwrap_or(Path::new("/")).join(target)
        };
    }
    Err(DroidError::Workload(format!(
        "too many symlinks resolving {}",
        guest_path.display()
    )))
}

fn read_elf_interpreter(path: &Path) -> Result<Option<String>> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 64 || &bytes[0..4] != b"\x7FELF" || bytes[5] != 1 {
        return Ok(None);
    }
    match bytes[4] {
        1 => read_elf_interp_phdr(&bytes, false),
        2 => read_elf_interp_phdr(&bytes, true),
        _ => Ok(None),
    }
}

fn read_elf_interp_phdr(bytes: &[u8], is64: bool) -> Result<Option<String>> {
    let (phoff, phentsize, phnum) = if is64 {
        (
            read_u64(bytes, 32)? as usize,
            read_u16(bytes, 54)? as usize,
            read_u16(bytes, 56)? as usize,
        )
    } else {
        (
            read_u32(bytes, 28)? as usize,
            read_u16(bytes, 42)? as usize,
            read_u16(bytes, 44)? as usize,
        )
    };

    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if read_u32(bytes, off)? != 3 {
            continue;
        }
        let (p_offset, p_filesz) = if is64 {
            (read_u64(bytes, off + 8)? as usize, read_u64(bytes, off + 32)? as usize)
        } else {
            (read_u32(bytes, off + 4)? as usize, read_u32(bytes, off + 16)? as usize)
        };
        let end = p_offset.saturating_add(p_filesz).min(bytes.len());
        if p_offset >= bytes.len() {
            return Ok(None);
        }
        let raw = &bytes[p_offset..end];
        let nul = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        return String::from_utf8(raw[..nul].to_vec())
            .map(Some)
            .map_err(|e| DroidError::Workload(format!("bad ELF interp string: {e}")));
    }
    Ok(None)
}

fn read_u16(bytes: &[u8], off: usize) -> Result<u16> {
    bytes.get(off..off + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| DroidError::Workload("truncated ELF".into()))
}

fn read_u32(bytes: &[u8], off: usize) -> Result<u32> {
    bytes.get(off..off + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| DroidError::Workload("truncated ELF".into()))
}

fn read_u64(bytes: &[u8], off: usize) -> Result<u64> {
    bytes.get(off..off + 8)
        .map(|s| u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        .ok_or_else(|| DroidError::Workload("truncated ELF".into()))
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
