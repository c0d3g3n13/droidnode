use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, instrument};

use crate::brokers::ControlPlaneBroker;
use crate::error::Result;
use crate::services::foundation::node_capability_service::NodeCapabilityService;

const HEARTBEAT_INTERVAL_SECS: u64 = 30;

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait NodeRegistrationService: Send + Sync {
    /// Register this device as a node and start the heartbeat loop.
    async fn run(&self) -> Result<()>;
    /// Gracefully deregister.
    async fn deregister(&self) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct NodeRegistrationServiceImpl {
    cp_broker: Arc<dyn ControlPlaneBroker>,
    capability_service: Arc<dyn NodeCapabilityService>,
}

impl NodeRegistrationServiceImpl {
    pub fn new(
        cp_broker: Arc<dyn ControlPlaneBroker>,
        capability_service: Arc<dyn NodeCapabilityService>,
    ) -> Self {
        Self { cp_broker, capability_service }
    }
}

#[async_trait]
impl NodeRegistrationService for NodeRegistrationServiceImpl {
    #[instrument(skip(self))]
    async fn run(&self) -> Result<()> {
        // 1. Register
        let profile = self.capability_service.get_profile().await?;
        info!(node_id = %profile.node_id, "registering node with control plane");
        self.cp_broker.register_node(&profile).await?;
        info!(node_id = %profile.node_id, "node registered");

        // 2. Heartbeat loop
        loop {
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;

            let profile = match self.capability_service.get_profile().await {
                Ok(p) => p,
                Err(e) => {
                    error!(error = %e, "failed to read node profile for heartbeat");
                    continue;
                }
            };

            let status = crate::models::NodeStatus {
                node_id: profile.node_id.clone(),
                conditions: profile.conditions,
                memory: profile.memory,
                storage: profile.storage,
                battery: profile.battery,
                network: profile.network,
            };

            if let Err(e) = self.cp_broker.send_heartbeat(&status).await {
                error!(error = %e, "heartbeat failed");
            }
        }
    }

    #[instrument(skip(self))]
    async fn deregister(&self) -> Result<()> {
        info!("deregistering node from control plane");
        self.cp_broker.deregister_node().await
    }
}
