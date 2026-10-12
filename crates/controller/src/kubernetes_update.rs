use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::Database;
use crate::node_client::NodeClients;

/// Rolling update orchestrator for Kubernetes control plane
pub struct KubernetesUpdateOrchestrator {
    db: Database,
    node_clients: Arc<std::collections::HashMap<String, NodeClients>>,
}

/// Update phase for tracking progress
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Planning,
    CordoningNode,
    DrainingNode,
    UpdatingBinaries,
    RestartingServices,
    WaitingForHealth,
    UncordoningNode,
    Complete,
    Failed,
}

impl UpdatePhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::CordoningNode => "cordoning-node",
            Self::DrainingNode => "draining-node",
            Self::UpdatingBinaries => "updating-binaries",
            Self::RestartingServices => "restarting-services",
            Self::WaitingForHealth => "waiting-for-health",
            Self::UncordoningNode => "uncordoning-node",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateSpec {
    pub cluster_name: String,
    pub target_version: String,
    pub kubelet_binary_path: String,
    pub apiserver_binary_path: Option<String>,
    pub controller_manager_binary_path: Option<String>,
    pub scheduler_binary_path: Option<String>,
    pub etcd_binary_path: Option<String>,
}

#[derive(Debug, Clone)]
struct NodeUpdateStatus {
    node_id: String,
    hostname: String,
    phase: UpdatePhase,
    error: Option<String>,
}

impl KubernetesUpdateOrchestrator {
    pub fn new(
        db: Database,
        node_clients: Arc<std::collections::HashMap<String, NodeClients>>,
    ) -> Self {
        Self { db, node_clients }
    }

    /// Perform rolling update of control plane nodes one at a time
    pub async fn rolling_update(&self, update_spec: UpdateSpec) -> Result<()> {
        info!(
            cluster = update_spec.cluster_name,
            version = update_spec.target_version,
            "starting rolling update of Kubernetes control plane"
        );

        // Get list of control plane nodes
        let nodes = self
            .get_control_plane_nodes(&update_spec.cluster_name)
            .await?;

        if nodes.is_empty() {
            anyhow::bail!(
                "no control plane nodes found for cluster {}",
                update_spec.cluster_name
            );
        }

        info!(
            cluster = update_spec.cluster_name,
            node_count = nodes.len(),
            "updating {} control plane nodes one at a time",
            nodes.len()
        );

        // Update each node sequentially
        for (idx, node) in nodes.iter().enumerate() {
            info!(
                cluster = update_spec.cluster_name,
                node_id = node.node_id,
                progress = format!("{}/{}", idx + 1, nodes.len()),
                "starting update for node"
            );

            match self.update_single_node(node, &update_spec).await {
                Ok(_) => {
                    info!(
                        cluster = update_spec.cluster_name,
                        node_id = node.node_id,
                        "node update completed successfully"
                    );
                }
                Err(e) => {
                    error!(
                        cluster = update_spec.cluster_name,
                        node_id = node.node_id,
                        error = %e,
                        "node update failed"
                    );
                    return Err(e).context(format!("failed to update node {}", node.node_id));
                }
            }

            // Brief pause between nodes
            if idx < nodes.len() - 1 {
                info!("waiting 30 seconds before next node update");
                sleep(Duration::from_secs(30)).await;
            }
        }

        info!(
            cluster = update_spec.cluster_name,
            version = update_spec.target_version,
            "rolling update completed successfully"
        );

        Ok(())
    }

    /// Update a single node following the cordon -> drain -> update -> uncordon pattern
    async fn update_single_node(&self, node: &ClusterNode, update_spec: &UpdateSpec) -> Result<()> {
        // Phase 1: Cordon node (mark as unschedulable)
        self.update_node_phase(&node.node_id, UpdatePhase::CordoningNode)
            .await?;
        self.cordon_node(&node.node_id).await?;

        // Phase 2: Drain workloads from node
        self.update_node_phase(&node.node_id, UpdatePhase::DrainingNode)
            .await?;
        self.drain_node(&node.node_id).await?;

        // Phase 3: Update binaries
        self.update_node_phase(&node.node_id, UpdatePhase::UpdatingBinaries)
            .await?;
        self.update_binaries(&node.node_id, update_spec).await?;

        // Phase 4: Restart services
        self.update_node_phase(&node.node_id, UpdatePhase::RestartingServices)
            .await?;
        self.restart_services(&node.node_id).await?;

        // Phase 5: Wait for node to be healthy
        self.update_node_phase(&node.node_id, UpdatePhase::WaitingForHealth)
            .await?;
        self.wait_for_node_healthy(&node.node_id, 300).await?;

        // Phase 6: Uncordon node (mark as schedulable again)
        self.update_node_phase(&node.node_id, UpdatePhase::UncordoningNode)
            .await?;
        self.uncordon_node(&node.node_id).await?;

        self.update_node_phase(&node.node_id, UpdatePhase::Complete)
            .await?;

        Ok(())
    }

    async fn cordon_node(&self, node_id: &str) -> Result<()> {
        info!(node_id = node_id, "cordoning node");

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        // Execute kubectl cordon
        let _output = client
            .execute_command(&format!("kubectl cordon {}", node_id))
            .await?;

        info!(node_id = node_id, "node cordoned successfully");
        Ok(())
    }

    async fn drain_node(&self, node_id: &str) -> Result<()> {
        info!(node_id = node_id, "draining node");

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        // Execute kubectl drain with common flags
        let _output = client
            .execute_command(&format!(
                "kubectl drain {} --ignore-daemonsets --delete-emptydir-data --force --timeout=300s",
                node_id
            ))
            .await?;

        info!(node_id = node_id, "node drained successfully");
        Ok(())
    }

    async fn update_binaries(&self, node_id: &str, update_spec: &UpdateSpec) -> Result<()> {
        info!(
            node_id = node_id,
            version = update_spec.target_version,
            "updating Kubernetes binaries"
        );

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        // Update kubelet
        self.update_binary(client, "kubelet", &update_spec.kubelet_binary_path)
            .await?;

        // Update control plane components if specified
        if let Some(ref apiserver_path) = update_spec.apiserver_binary_path {
            self.update_binary(client, "kube-apiserver", apiserver_path)
                .await?;
        }
        if let Some(ref cm_path) = update_spec.controller_manager_binary_path {
            self.update_binary(client, "kube-controller-manager", cm_path)
                .await?;
        }
        if let Some(ref sched_path) = update_spec.scheduler_binary_path {
            self.update_binary(client, "kube-scheduler", sched_path)
                .await?;
        }
        if let Some(ref etcd_path) = update_spec.etcd_binary_path {
            self.update_binary(client, "etcd", etcd_path).await?;
        }

        info!(node_id = node_id, "binaries updated successfully");
        Ok(())
    }

    async fn update_binary(
        &self,
        client: &NodeClients,
        binary_name: &str,
        source_path: &str,
    ) -> Result<()> {
        info!(binary = binary_name, "updating binary");

        // Copy binary to /usr/local/bin
        let _output = client
            .execute_command(&format!(
                "cp {} /usr/local/bin/{}",
                source_path, binary_name
            ))
            .await?;

        // Make executable
        let _output = client
            .execute_command(&format!("chmod +x /usr/local/bin/{}", binary_name))
            .await?;

        Ok(())
    }

    async fn restart_services(&self, node_id: &str) -> Result<()> {
        info!(node_id = node_id, "restarting Kubernetes services");

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        // Restart services in proper order
        let services = vec![
            "kubelet",
            "kube-apiserver",
            "kube-controller-manager",
            "kube-scheduler",
        ];

        for service in services {
            info!(node_id = node_id, service = service, "restarting service");

            let _output = client
                .execute_command(&format!("systemctl restart {}", service))
                .await?;

            // Brief delay between service restarts
            sleep(Duration::from_secs(5)).await;
        }

        info!(node_id = node_id, "services restarted");
        Ok(())
    }

    async fn wait_for_node_healthy(&self, node_id: &str, timeout_secs: u64) -> Result<()> {
        info!(
            node_id = node_id,
            timeout = timeout_secs,
            "waiting for node to become healthy"
        );

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        let start = std::time::Instant::now();
        loop {
            // Check if node is ready via kubectl
            let output = client
                .execute_command(&format!("kubectl get node {} -o jsonpath='{{.status.conditions[?(@.type==\"Ready\")].status}}'", node_id))
                .await?;

            if output.trim() == "True" {
                info!(node_id = node_id, "node is healthy");
                return Ok(());
            }

            if start.elapsed().as_secs() > timeout_secs {
                anyhow::bail!("timeout waiting for node {} to become healthy", node_id);
            }

            info!(node_id = node_id, "node not ready yet, waiting...");
            sleep(Duration::from_secs(10)).await;
        }
    }

    async fn uncordon_node(&self, node_id: &str) -> Result<()> {
        info!(node_id = node_id, "uncordoning node");

        let client = self
            .node_clients
            .get(node_id)
            .context("node client not found")?;

        // Execute kubectl uncordon
        let _output = client
            .execute_command(&format!("kubectl uncordon {}", node_id))
            .await?;

        info!(node_id = node_id, "node uncordoned successfully");
        Ok(())
    }

    async fn get_control_plane_nodes(&self, cluster_name: &str) -> Result<Vec<ClusterNode>> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let mut nodes = Vec::new();

        for row in rows {
            let node = self.db.get_node(&row.node_id)?.context("node not found")?;

            nodes.push(ClusterNode {
                node_id: row.node_id,
                hostname: node.hostname,
                ip_address: node.address.split(':').next().unwrap_or("").to_string(),
            });
        }

        Ok(nodes)
    }

    async fn update_node_phase(&self, node_id: &str, phase: UpdatePhase) -> Result<()> {
        info!(
            node_id = node_id,
            phase = phase.as_str(),
            "updating node phase"
        );
        // TODO: Store phase in database for tracking
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ClusterNode {
    node_id: String,
    hostname: String,
    ip_address: String,
}
