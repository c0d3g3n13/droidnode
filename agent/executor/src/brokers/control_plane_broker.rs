use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{api::{Api, Patch, PatchParams, PostParams}, Client};
use serde_json::json;
use tracing::{debug, instrument};

use crate::error::{DroidError, Result};
use crate::models::{NodeEvent, NodeProfile, NodeStatus};

// ─── Trait ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ControlPlaneBroker: Send + Sync {
    async fn register_node(&self, profile: &NodeProfile) -> Result<()>;
    async fn send_heartbeat(&self, status: &NodeStatus) -> Result<()>;
    async fn get_assigned_pods(&self) -> Result<Vec<Pod>>;
    async fn report_pod_status(&self, pod_name: &str, namespace: &str, status: &crate::models::PodRunStatus) -> Result<()>;
    async fn record_event(&self, event: &NodeEvent) -> Result<()>;
    async fn deregister_node(&self) -> Result<()>;
}

// ─── Implementation ───────────────────────────────────────────────────────────

pub struct ControlPlaneBrokerImpl {
    client: Client,
    node_name: String,
}

impl ControlPlaneBrokerImpl {
    pub async fn new(node_name: String) -> Result<Self> {
        let client = Client::try_default()
            .await
            .map_err(DroidError::KubeApi)?;
        Ok(Self { client, node_name })
    }

    /// Build a Node object from our capability profile.
    fn build_node_object(&self, profile: &NodeProfile) -> Node {
        let memory_str = format!("{}", profile.memory.total_bytes);
        let cpu_str = format!("{}", profile.cpu.cores);

        let mut node = Node::default();
        node.metadata = ObjectMeta {
            name: Some(self.node_name.clone()),
            labels: Some([
                ("kubernetes.io/hostname".into(), self.node_name.clone()),
                ("kubernetes.io/arch".into(), profile.cpu.arch.clone()),
                ("kubernetes.io/os".into(), "linux".into()),
                ("droidnode/managed".into(), "true".into()),
            ].into()),
            ..Default::default()
        };

        // Status is set via a PATCH after creation
        let _ = memory_str;
        let _ = cpu_str;

        node
    }
}

#[async_trait]
impl ControlPlaneBroker for ControlPlaneBrokerImpl {
    #[instrument(skip(self, profile), fields(node = %self.node_name))]
    async fn register_node(&self, profile: &NodeProfile) -> Result<()> {
        let nodes: Api<Node> = Api::all(self.client.clone());
        let node_obj = self.build_node_object(profile);

        // Create or replace
        match nodes.create(&PostParams::default(), &node_obj).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 409 => {
                // Node already exists — update status on the next heartbeat
                debug!(node = %self.node_name, "node already registered");
            }
            Err(e) => return Err(DroidError::KubeApi(e)),
        }

        // Immediately post the full status so the node appears Ready
        self.send_heartbeat(&NodeStatus {
            node_id: profile.node_id.clone(),
            conditions: profile.conditions.clone(),
            memory: profile.memory.clone(),
            storage: profile.storage.clone(),
            battery: profile.battery.clone(),
            network: profile.network.clone(),
        })
        .await?;

        Ok(())
    }

    #[instrument(skip(self, status), fields(node = %self.node_name))]
    async fn send_heartbeat(&self, status: &NodeStatus) -> Result<()> {
        let nodes: Api<Node> = Api::all(self.client.clone());

        let ready_status = if status.conditions.ready { "True" } else { "False" };
        let mem_pressure = if status.conditions.memory_pressure { "True" } else { "False" };
        let net_avail = if status.conditions.network_available { "True" } else { "False" };

        let patch = json!({
            "status": {
                "conditions": [
                    {
                        "type": "Ready",
                        "status": ready_status,
                        "reason": "DroidNodeReady",
                        "message": "droidnode agent is running"
                    },
                    {
                        "type": "MemoryPressure",
                        "status": mem_pressure,
                        "reason": "DroidNodeMemory"
                    },
                    {
                        "type": "NetworkUnavailable",
                        "status": if net_avail == "True" { "False" } else { "True" },
                        "reason": "DroidNodeNetwork"
                    }
                ],
                "allocatable": {
                    "cpu": format!("{}", 0_u32), // reported dynamically
                    "memory": format!("{}Ki", status.memory.available_bytes / 1024)
                },
                "capacity": {
                    "memory": format!("{}Ki", status.memory.total_bytes / 1024)
                }
            }
        });

        nodes
            .patch_status(
                &self.node_name,
                &PatchParams::apply("droidnode"),
                &Patch::Merge(&patch),
            )
            .await
            .map_err(DroidError::KubeApi)?;

        Ok(())
    }

    #[instrument(skip(self), fields(node = %self.node_name))]
    async fn get_assigned_pods(&self) -> Result<Vec<Pod>> {
        use kube::api::ListParams;
        let pods: Api<Pod> = Api::all(self.client.clone());
        let field_selector = format!("spec.nodeName={}", self.node_name);
        let lp = ListParams::default().fields(&field_selector);
        let pod_list = pods.list(&lp).await.map_err(DroidError::KubeApi)?;
        Ok(pod_list.items)
    }

    #[instrument(skip(self, status), fields(pod = %pod_name, ns = %namespace))]
    async fn report_pod_status(
        &self,
        pod_name: &str,
        namespace: &str,
        status: &crate::models::PodRunStatus,
    ) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);

        let container_statuses: Vec<serde_json::Value> = status
            .containers
            .iter()
            .map(|c| {
                let state = if c.running {
                    json!({ "running": { "startedAt": null } })
                } else {
                    json!({ "terminated": { "exitCode": c.exit_code.unwrap_or(0) } })
                };
                json!({
                    "name": c.name,
                    "ready": c.ready,
                    "state": state
                })
            })
            .collect();

        let patch = json!({
            "status": {
                "phase": status.phase.as_str(),
                "message": status.message,
                "containerStatuses": container_statuses
            }
        });

        pods.patch_status(pod_name, &PatchParams::apply("droidnode"), &Patch::Merge(&patch))
            .await
            .map_err(DroidError::KubeApi)?;

        Ok(())
    }

    #[instrument(skip(self, event), fields(node = %self.node_name))]
    async fn record_event(&self, event: &NodeEvent) -> Result<()> {
        use chrono::Utc;

        let events: Api<Event> = Api::namespaced(self.client.clone(), "default");

        let event_type = match event.event_type {
            crate::models::EventType::Normal => "Normal",
            crate::models::EventType::Warning => "Warning",
        };

        let ev = Event {
            metadata: ObjectMeta {
                generate_name: Some(format!("{}-", self.node_name)),
                namespace: Some("default".into()),
                ..Default::default()
            },
            event_time: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime(Utc::now())),
            action: Some(event.reason.clone()),
            reason: Some(event.reason.clone()),
            message: Some(event.message.clone()),
            type_: Some(event_type.into()),
            reporting_component: Some("droidnode".into()),
            reporting_instance: Some(self.node_name.clone()),
            ..Default::default()
        };

        events
            .create(&PostParams::default(), &ev)
            .await
            .map_err(DroidError::KubeApi)?;

        Ok(())
    }

    #[instrument(skip(self), fields(node = %self.node_name))]
    async fn deregister_node(&self) -> Result<()> {
        use kube::api::DeleteParams;
        let nodes: Api<Node> = Api::all(self.client.clone());
        nodes
            .delete(&self.node_name, &DeleteParams::default())
            .await
            .map_err(DroidError::KubeApi)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        BatteryInfo, CpuInfo, MemoryInfo, NetworkInfo, NodeConditions, NodeEvent, NodeProfile,
        NodeStatus, RuntimeInfo, StorageInfo, EventType,
    };

    fn test_profile(node_id: &str) -> NodeProfile {
        NodeProfile {
            node_id: node_id.into(),
            cpu: CpuInfo { arch: "amd64".into(), cores: 4 },
            memory: MemoryInfo { total_bytes: 8 * 1024 * 1024 * 1024, available_bytes: 4 * 1024 * 1024 * 1024 },
            storage: StorageInfo { available_bytes: 10 * 1024 * 1024 * 1024 },
            runtime: RuntimeInfo { name: "proot-oci-runner".into(), version: "0.1.0".into() },
            battery: BatteryInfo { percent: 100, charging: true },
            network: NetworkInfo { network_type: "wifi".into() },
            conditions: NodeConditions {
                ready: true,
                battery_pressure: false,
                memory_pressure: false,
                network_available: true,
            },
        }
    }

    #[tokio::test]
    async fn test_register_heartbeat_get_pods_deregister() {
        if std::env::var("KUBECONFIG").is_err() && !std::path::Path::new("/etc/rancher/k3s/k3s.yaml").exists() {
            eprintln!("KUBECONFIG not set and k3s not found — skipping control plane test");
            return;
        }

        let node_name = "droidnode-test-broker".to_string();
        let broker = ControlPlaneBrokerImpl::new(node_name.clone()).await.unwrap();
        let profile = test_profile(&node_name);

        // Register
        broker.register_node(&profile).await.unwrap();
        println!("node registered: {node_name}");

        // Heartbeat
        let status = NodeStatus {
            node_id: node_name.clone(),
            conditions: profile.conditions.clone(),
            memory: profile.memory.clone(),
            storage: profile.storage.clone(),
            battery: profile.battery.clone(),
            network: profile.network.clone(),
        };
        broker.send_heartbeat(&status).await.unwrap();
        println!("heartbeat sent");

        // Assigned pods — should be empty for a fresh test node
        let pods = broker.get_assigned_pods().await.unwrap();
        println!("assigned pods: {}", pods.len());
        assert!(pods.is_empty(), "expected no pods assigned to test node");

        // Record an event
        broker.record_event(&NodeEvent {
            node_id: node_name.clone(),
            reason: "TestEvent".into(),
            message: "control plane broker integration test".into(),
            event_type: EventType::Normal,
            pod_id: None,
        }).await.unwrap();
        println!("event recorded");

        // Deregister — Ok(()) return is sufficient; k8s deletion is async
        broker.deregister_node().await.unwrap();
        println!("node deregistered");
    }
}
