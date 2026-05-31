use async_trait::async_trait;
use tracing::instrument;

use crate::error::Result;
use crate::models::{
    BatteryInfo, CpuInfo, MemoryInfo, NetworkInfo, NodeConditions, NodeProfile, RuntimeInfo,
    StorageInfo,
};

const RUNTIME_NAME: &str = "proot-oci-runner";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOW_BATTERY_THRESHOLD: u8 = 20;
const LOW_MEMORY_THRESHOLD_BYTES: u64 = 512 * 1024 * 1024; // 512 MB

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait NodeCapabilityService: Send + Sync {
    async fn get_profile(&self) -> Result<NodeProfile>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct NodeCapabilityServiceImpl {
    node_id: String,
    layers_root: std::path::PathBuf,
}

impl NodeCapabilityServiceImpl {
    pub fn new(node_id: String, layers_root: std::path::PathBuf) -> Self {
        Self { node_id, layers_root }
    }
}

#[async_trait]
impl NodeCapabilityService for NodeCapabilityServiceImpl {
    #[instrument(skip(self))]
    async fn get_profile(&self) -> Result<NodeProfile> {
        let cpu = read_cpu_info();
        let memory = read_memory_info();
        let storage = read_storage_info(&self.layers_root);
        let battery = read_battery_info();
        let network = read_network_info();

        let conditions = NodeConditions {
            ready: is_ready(&memory, &battery, &network),
            battery_pressure: battery_pressure(&battery),
            memory_pressure: memory.available_bytes < LOW_MEMORY_THRESHOLD_BYTES,
            network_available: network.network_type != "none",
        };

        Ok(NodeProfile {
            node_id: self.node_id.clone(),
            cpu,
            memory,
            storage,
            runtime: RuntimeInfo {
                name: RUNTIME_NAME.into(),
                version: RUNTIME_VERSION.into(),
            },
            battery,
            network,
            conditions,
        })
    }
}

// ─── Platform-specific readers ────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_cpu_info() -> CpuInfo {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);

    // Detect architecture via uname
    let arch = {
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        unsafe { libc::uname(&mut u) };
        let machine = unsafe {
            std::ffi::CStr::from_ptr(u.machine.as_ptr())
                .to_string_lossy()
                .to_string()
        };
        machine
    };

    CpuInfo { arch, cores }
}

#[cfg(not(target_os = "linux"))]
fn read_cpu_info() -> CpuInfo {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    CpuInfo {
        arch: std::env::consts::ARCH.to_string(),
        cores,
    }
}

#[cfg(target_os = "linux")]
fn read_memory_info() -> MemoryInfo {
    let content = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = 0u64;
    let mut available = 0u64;

    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = parse_meminfo_kb(line);
        } else if line.starts_with("MemAvailable:") {
            available = parse_meminfo_kb(line);
        }
    }

    MemoryInfo {
        total_bytes: total * 1024,
        available_bytes: available * 1024,
    }
}

#[cfg(not(target_os = "linux"))]
fn read_memory_info() -> MemoryInfo {
    MemoryInfo {
        total_bytes: 4 * 1024 * 1024 * 1024,
        available_bytes: 2 * 1024 * 1024 * 1024,
    }
}

fn read_storage_info(path: &std::path::Path) -> StorageInfo {
    // Use std::fs for statvfs equivalent (available on all platforms via statvfs crate or libc)
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        let cpath = CString::new(path.to_str().unwrap_or("/")).unwrap();
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
        StorageInfo {
            available_bytes: stat.f_bavail * stat.f_bsize,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        StorageInfo {
            available_bytes: 64 * 1024 * 1024 * 1024,
        }
    }
}

fn read_battery_info() -> BatteryInfo {
    // On Android this is populated by the Kotlin layer via IPC.
    // For Linux development, report a full always-charging battery.
    BatteryInfo {
        percent: 100,
        charging: true,
    }
}

fn read_network_info() -> NetworkInfo {
    // On Android this comes from the Kotlin layer.
    // Stub: assume WiFi for dev builds.
    NetworkInfo {
        network_type: "wifi".into(),
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn parse_meminfo_kb(line: &str) -> u64 {
    // Format: "MemTotal:       8192000 kB"
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn is_ready(memory: &MemoryInfo, battery: &BatteryInfo, network: &NetworkInfo) -> bool {
    let memory_ok = memory.available_bytes >= LOW_MEMORY_THRESHOLD_BYTES;
    let battery_ok = !battery_pressure(battery);
    let network_ok = network.network_type != "none";
    memory_ok && battery_ok && network_ok
}

fn battery_pressure(battery: &BatteryInfo) -> bool {
    battery.percent < LOW_BATTERY_THRESHOLD && !battery.charging
}
