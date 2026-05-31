use anyhow::Context;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use executor::{
    brokers::{
        ControlPlaneBrokerImpl, FilesystemBrokerImpl, OciRegistryBrokerImpl, ProotBrokerImpl,
    },
    exposers::VirtualKubeletExposer,
    services::{
        foundation::{
            EventRecordingServiceImpl, HealthProbeServiceImpl, ImagePullServiceImpl,
            ImageUnpackServiceImpl, NodeCapabilityServiceImpl, WorkloadExecutionServiceImpl,
        },
        orchestration::{
            ImageOrchestrationServiceImpl, NodeRegistrationService, NodeRegistrationServiceImpl,
            ReconciliationService, ReconciliationServiceImpl, WorkloadLifecycleServiceImpl,
        },
    },
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let config = Config::from_env();
    info!(
        node_id = %config.node_id,
        proot_path = %config.proot_path.display(),
        layers_root = %config.layers_root.display(),
        kubelet_addr = %config.kubelet_addr,
        "droidnode agent starting"
    );

    // ─── Brokers ─────────────────────────────────────────────────────────────

    let oci_broker = Arc::new(OciRegistryBrokerImpl::new());

    let fs_broker = Arc::new(FilesystemBrokerImpl::new(config.layers_root.clone()));

    let proot_broker = Arc::new(ProotBrokerImpl::new(config.proot_path.clone()));

    let cp_broker = Arc::new(
        ControlPlaneBrokerImpl::new(config.node_id.clone())
            .await
            .context("failed to create k8s client — is KUBECONFIG set?")?,
    );

    // ─── Foundation services ─────────────────────────────────────────────────

    let pull_service = Arc::new(ImagePullServiceImpl::new(
        Arc::clone(&oci_broker) as Arc<dyn executor::brokers::OciRegistryBroker>,
        Arc::clone(&fs_broker) as Arc<dyn executor::brokers::FilesystemBroker>,
    ));

    let unpack_service = Arc::new(ImageUnpackServiceImpl::new(
        Arc::clone(&fs_broker) as Arc<dyn executor::brokers::FilesystemBroker>,
    ));

    let execution_service = Arc::new(WorkloadExecutionServiceImpl::new(
        Arc::clone(&proot_broker) as Arc<dyn executor::brokers::ProotBroker>,
    ));

    let probe_service = Arc::new(HealthProbeServiceImpl::new(
        Arc::clone(&proot_broker) as Arc<dyn executor::brokers::ProotBroker>,
    ));

    let capability_service = Arc::new(NodeCapabilityServiceImpl::new(
        config.node_id.clone(),
        config.layers_root.clone(),
    ));

    let event_service = Arc::new(EventRecordingServiceImpl::new(
        Arc::clone(&cp_broker) as Arc<dyn executor::brokers::ControlPlaneBroker>,
        config.node_id.clone(),
    ));

    // ─── Orchestration services ───────────────────────────────────────────────

    let image_orch = Arc::new(ImageOrchestrationServiceImpl::new(
        Arc::clone(&pull_service) as Arc<dyn executor::services::foundation::ImagePullService>,
        Arc::clone(&unpack_service) as Arc<dyn executor::services::foundation::ImageUnpackService>,
        config.rootfs_base.clone(),
    ));

    let workload_lifecycle = Arc::new(WorkloadLifecycleServiceImpl::new(
        Arc::clone(&execution_service)
            as Arc<dyn executor::services::foundation::WorkloadExecutionService>,
        Arc::clone(&probe_service) as Arc<dyn executor::services::foundation::HealthProbeService>,
    ));

    let reconciler = Arc::new(ReconciliationServiceImpl::new(
        Arc::clone(&cp_broker) as Arc<dyn executor::brokers::ControlPlaneBroker>,
        Arc::clone(&image_orch)
            as Arc<dyn executor::services::orchestration::ImageOrchestrationService>,
        Arc::clone(&workload_lifecycle)
            as Arc<dyn executor::services::orchestration::WorkloadLifecycleService>,
        Arc::clone(&event_service)
            as Arc<dyn executor::services::foundation::EventRecordingService>,
        config.node_id.clone(),
    ));

    let registration = Arc::new(NodeRegistrationServiceImpl::new(
        Arc::clone(&cp_broker) as Arc<dyn executor::brokers::ControlPlaneBroker>,
        Arc::clone(&capability_service)
            as Arc<dyn executor::services::foundation::NodeCapabilityService>,
    ));

    // ─── Exposer ──────────────────────────────────────────────────────────────

    let exposer = VirtualKubeletExposer::new(config.kubelet_addr);

    // ─── Launch all tasks ────────────────────────────────────────────────────

    let reg_handle = {
        let reg = Arc::clone(&registration);
        tokio::spawn(async move {
            if let Err(e) = reg.run().await {
                error!(error = %e, "node registration task failed");
            }
        })
    };

    let reconcile_handle = {
        let rec = Arc::clone(&reconciler);
        tokio::spawn(async move {
            if let Err(e) = rec.run_loop().await {
                error!(error = %e, "reconciliation loop failed");
            }
        })
    };

    let kubelet_handle = tokio::spawn(async move {
        if let Err(e) = exposer.serve().await {
            error!(error = %e, "kubelet HTTP server failed");
        }
    });

    // Await shutdown signal
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT received — shutting down");
        }
        _ = reg_handle => {}
        _ = reconcile_handle => {}
        _ = kubelet_handle => {}
    }

    // Deregister node on clean shutdown
    if let Err(e) = registration.deregister().await {
        error!(error = %e, "deregistration failed");
    }

    info!("droidnode agent stopped");
    Ok(())
}

// ─── Configuration ────────────────────────────────────────────────────────────

struct Config {
    node_id: String,
    proot_path: PathBuf,
    layers_root: PathBuf,
    rootfs_base: PathBuf,
    kubelet_addr: SocketAddr,
}

impl Config {
    fn from_env() -> Self {
        let base_dir = std::env::var("DROIDNODE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_or_home().join(".droidnode")
            });

        let node_id = std::env::var("DROIDNODE_NODE_ID").unwrap_or_else(|_| {
            format!("droidnode-{}", hostname_or_uuid())
        });

        let proot_path = std::env::var("DROIDNODE_PROOT_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| base_dir.join("proot"));

        let layers_root = std::env::var("DROIDNODE_LAYERS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| base_dir.join("layers"));

        let rootfs_base = std::env::var("DROIDNODE_ROOTFS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| base_dir.join("rootfs"));

        let kubelet_port: u16 = std::env::var("DROIDNODE_KUBELET_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(10250);

        let kubelet_addr: SocketAddr = format!("0.0.0.0:{kubelet_port}").parse().unwrap();

        Self {
            node_id,
            proot_path,
            layers_root,
            rootfs_base,
            kubelet_addr,
        }
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn hostname_or_uuid() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}
