use async_trait::async_trait;
use std::sync::Arc;
use tracing::instrument;

use crate::brokers::ControlPlaneBroker;
use crate::error::Result;
use crate::models::{EventType, NodeEvent};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait EventRecordingService: Send + Sync {
    async fn record_normal(&self, reason: &str, message: &str, pod_id: Option<String>) -> Result<()>;
    async fn record_warning(&self, reason: &str, message: &str, pod_id: Option<String>) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct EventRecordingServiceImpl {
    cp_broker: Arc<dyn ControlPlaneBroker>,
    node_id: String,
}

impl EventRecordingServiceImpl {
    pub fn new(cp_broker: Arc<dyn ControlPlaneBroker>, node_id: String) -> Self {
        Self { cp_broker, node_id }
    }

    async fn record(
        &self,
        event_type: EventType,
        reason: &str,
        message: &str,
        pod_id: Option<String>,
    ) -> Result<()> {
        let event = NodeEvent {
            node_id: self.node_id.clone(),
            event_type,
            reason: reason.to_string(),
            message: message.to_string(),
            pod_id,
        };
        self.cp_broker.record_event(&event).await
    }
}

#[async_trait]
impl EventRecordingService for EventRecordingServiceImpl {
    #[instrument(skip(self), fields(reason, message))]
    async fn record_normal(&self, reason: &str, message: &str, pod_id: Option<String>) -> Result<()> {
        self.record(EventType::Normal, reason, message, pod_id).await
    }

    #[instrument(skip(self), fields(reason, message))]
    async fn record_warning(&self, reason: &str, message: &str, pod_id: Option<String>) -> Result<()> {
        self.record(EventType::Warning, reason, message, pod_id).await
    }
}
