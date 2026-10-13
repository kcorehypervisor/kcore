use anyhow::{anyhow, Context, Result};
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::db::Database;
use crate::kubernetes_pki::{self, KubernetesPki, NodeInfo};
use crate::node_client::NodeClients;

/// Orchestrates Kubernetes cluster bootstrap across multiple nodes
#[derive(Clone)]
pub struct KubernetesOrchestrator {
    db: Database,
    node_clients: NodeClients,
}

/// Bootstrap phase for tracking progress
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapPhase {
    GeneratingPki,
    ConfiguringBootstrapNode,
    WaitingForEtcdLeader,
    JoiningNodes,
    WaitingForQuorum,
    StartingApiServer,
    Ready,
    Failed,
}

impl BootstrapPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GeneratingPki => "generating-pki",
            Self::ConfiguringBootstrapNode => "configuring-bootstrap-node",
            Self::WaitingForEtcdLeader => "waiting-for-etcd-leader",
            Self::JoiningNodes => "joining-nodes",
            Self::WaitingForQuorum => "waiting-for-quorum",
            Self::StartingApiServer => "starting-apiserver",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl KubernetesOrchestrator {
    pub fn new(db: Database, node_clients: NodeClients) -> Self {
        Self { db, node_clients }
    }

    /// Orchestrate complete cluster bootstrap or scale-up
    pub async fn bootstrap_cluster(
        &self,
        cluster_name: &str,
        api_endpoint: &str,
        service_cidr: &str,
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            "starting Kubernetes cluster orchestration"
        );

        let nodes = self.get_cluster_nodes(cluster_name).await?;
        if nodes.is_empty() {
            anyhow::bail!("no nodes configured for cluster {}", cluster_name);
        }

        // Check if this is a new cluster or adding nodes to existing
        let existing_members = self.count_healthy_etcd_members(cluster_name).await?;
        let is_new_cluster = existing_members == 0;

        if is_new_cluster {
            info!(
                cluster = cluster_name,
                node_count = nodes.len(),
                "bootstrapping new cluster"
            );
            self.bootstrap_new_cluster(cluster_name, api_endpoint, service_cidr, &nodes)
                .await?;
        } else {
            info!(
                cluster = cluster_name,
                existing_members = existing_members,
                new_nodes = nodes.len(),
                "scaling existing cluster"
            );
            self.scale_existing_cluster(cluster_name, &nodes, existing_members)
                .await?;
        }

        Ok(())
    }

    /// Bootstrap a brand new cluster from scratch
    async fn bootstrap_new_cluster(
        &self,
        cluster_name: &str,
        api_endpoint: &str,
        service_cidr: &str,
        nodes: &[ClusterNode],
    ) -> Result<()> {
        // Fetch cluster config to get cluster_cidr
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let cluster_cidr = &cluster.cluster_cidr;

        // Phase 1: Generate PKI
        self.update_phase(cluster_name, BootstrapPhase::GeneratingPki)
            .await?;
        let pki = self
            .generate_and_store_pki(cluster_name, api_endpoint, service_cidr, nodes)
            .await?;

        // Phase 2: Deploy all nodes in parallel
        // For static bootstrap, all nodes need --initial-cluster-state=new
        // and must be deployed before etcd can form quorum
        self.update_phase(cluster_name, BootstrapPhase::ConfiguringBootstrapNode)
            .await?;

        info!(
            cluster = cluster_name,
            node_count = nodes.len(),
            "deploying all nodes in parallel for static bootstrap"
        );

        // Deploy all nodes concurrently
        let mut deploy_tasks = Vec::new();
        for node in nodes {
            let cluster_name = cluster_name.to_string();
            let node = node.clone();
            let pki = pki.clone();
            let cluster_cidr = cluster_cidr.to_string();
            let service_cidr = service_cidr.to_string();
            let all_nodes = nodes.to_vec();
            let orchestrator = self.clone();

            let task = tokio::spawn(async move {
                info!(
                    cluster = %cluster_name,
                    node_id = %node.node_id,
                    "deploying node configuration"
                );
                orchestrator
                    .configure_node(
                        &cluster_name,
                        &node,
                        &pki,
                        &cluster_cidr,
                        &service_cidr,
                        &all_nodes,
                    )
                    .await
            });
            deploy_tasks.push(task);
        }

        // Wait for all deployments to complete
        let mut deploy_results = Vec::new();
        for task in deploy_tasks {
            deploy_results.push(task.await.context("task join failed")?);
        }

        // Check if any deployments failed
        let failed_count = deploy_results.iter().filter(|r| r.is_err()).count();
        if failed_count > 0 {
            error!(
                cluster = cluster_name,
                failed = failed_count,
                total = nodes.len(),
                "some node deployments failed"
            );
            return Err(anyhow!(
                "{} of {} node deployments failed",
                failed_count,
                nodes.len()
            ));
        }

        info!(
            cluster = cluster_name,
            node_count = nodes.len(),
            "all nodes deployed successfully, waiting for etcd quorum"
        );

        // Phase 3: Wait for etcd quorum (majority of nodes)
        self.update_phase(cluster_name, BootstrapPhase::WaitingForQuorum)
            .await?;
        let required_quorum = (nodes.len() / 2) + 1;
        self.wait_for_etcd_quorum(cluster_name, required_quorum, 300)
            .await?;

        // Phase 4: Wait for API server
        self.update_phase(cluster_name, BootstrapPhase::StartingApiServer)
            .await?;
        self.wait_for_apiserver(cluster_name, 180).await?;

        // Phase 5: Mark ready
        self.update_phase(cluster_name, BootstrapPhase::Ready)
            .await?;

        info!(
            cluster = cluster_name,
            nodes = nodes.len(),
            "cluster bootstrap completed successfully"
        );
        Ok(())
    }

    /// Scale an existing cluster by adding new nodes
    async fn scale_existing_cluster(
        &self,
        cluster_name: &str,
        all_nodes: &[ClusterNode],
        existing_member_count: usize,
    ) -> Result<()> {
        // Fetch cluster config
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let cluster_cidr = &cluster.cluster_cidr;
        let service_cidr = &cluster.service_cidr;

        // Load existing PKI from database
        let pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found for existing cluster")?;

        info!(cluster = cluster_name, "scaling cluster: joining new nodes");

        // Find nodes that aren't part of etcd yet
        let new_nodes: Vec<&ClusterNode> = all_nodes
            .iter()
            .filter(|node| {
                // Check if this node is already in etcd
                !self.is_node_in_etcd(cluster_name, &node.node_id)
            })
            .collect();

        if new_nodes.is_empty() {
            info!(cluster = cluster_name, "no new nodes to add");
            return Ok(());
        }

        info!(
            cluster = cluster_name,
            new_node_count = new_nodes.len(),
            "adding nodes to existing cluster"
        );

        // Add each new node sequentially
        for node in &new_nodes {
            info!(
                cluster = cluster_name,
                node_id = node.node_id,
                "adding node to cluster"
            );

            // All nodes get the same etcd initial cluster config
            self.configure_node(
                cluster_name,
                node,
                &pki,
                cluster_cidr,
                service_cidr,
                all_nodes,
            )
            .await?;

            // Wait between additions
            sleep(Duration::from_secs(15)).await;
        }

        // Wait for new quorum
        let expected_members = existing_member_count + new_nodes.len();
        let required_quorum = (expected_members / 2) + 1;

        info!(
            cluster = cluster_name,
            expected_members = expected_members,
            required_quorum = required_quorum,
            "waiting for expanded quorum"
        );

        self.wait_for_etcd_quorum(cluster_name, required_quorum, 180)
            .await?;

        info!(
            cluster = cluster_name,
            new_size = expected_members,
            "cluster scale-up completed"
        );
        Ok(())
    }

    fn is_node_in_etcd(&self, _cluster_name: &str, _node_id: &str) -> bool {
        // TODO: Query etcd member list to check if node is already a member
        // For now, assume it's not (will be implemented with etcd client)
        false
    }

    async fn get_cluster_nodes(&self, cluster_name: &str) -> Result<Vec<ClusterNode>> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let mut nodes = Vec::new();

        for row in rows {
            let node = self.db.get_node(&row.node_id)?.context("node not found")?;

            nodes.push(ClusterNode {
                node_id: row.node_id,
                hostname: node.hostname,
                ip_address: node.address.split(':').next().unwrap_or("").to_string(),
                initial_cluster: row.initial_cluster,
            });
        }

        Ok(nodes)
    }

    async fn generate_and_store_pki(
        &self,
        cluster_name: &str,
        api_endpoint: &str,
        service_cidr: &str,
        nodes: &[ClusterNode],
    ) -> Result<KubernetesPki> {
        info!(cluster = cluster_name, "generating Kubernetes PKI");

        let node_infos: Vec<NodeInfo> = nodes
            .iter()
            .map(|n| NodeInfo {
                node_id: n.node_id.clone(),
                hostname: n.hostname.clone(),
                ip_address: n.ip_address.clone(),
            })
            .collect();

        let pki = kubernetes_pki::generate_kubernetes_pki(
            cluster_name,
            api_endpoint,
            service_cidr,
            &node_infos,
        )?;

        // TODO: Store PKI in database for future reference
        // Skipping for now until kubernetes_pki table migration is working
        // self.db
        //     .store_kubernetes_pki(cluster_name, &pki)
        //     .context("failed to store PKI")?;

        info!(cluster = cluster_name, "PKI generated");
        Ok(pki)
    }

    async fn configure_node(
        &self,
        cluster_name: &str,
        node: &ClusterNode,
        pki: &KubernetesPki,
        cluster_cidr: &str,
        service_cidr: &str,
        all_nodes: &[ClusterNode],
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            "configuring Kubernetes on node"
        );

        let address = format!("{}:9091", node.ip_address);
        let mut admin_client = self
            .node_clients
            .get_admin(&address)
            .context("node client not found")?;

        // Get node-specific certs
        let (etcd_server_cert, etcd_server_key) = pki
            .etcd_node_certs
            .get(&node.node_id)
            .context("etcd server cert not found")?;
        let (etcd_peer_cert, etcd_peer_key) = pki
            .etcd_node_peer_certs
            .get(&node.node_id)
            .context("etcd peer cert not found")?;

        // Build complete Kubernetes Nix config with embedded PKI
        let k8s_config = crate::nixgen::generate_kubernetes_config(
            cluster_name,
            node,
            pki,
            cluster_cidr,
            service_cidr,
            etcd_server_cert,
            etcd_server_key,
            etcd_peer_cert,
            etcd_peer_key,
            all_nodes,
        )?;

        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            config_len = k8s_config.len(),
            "generated Kubernetes configuration with embedded PKI"
        );

        // Apply configuration via node agent RPC
        use crate::node_proto::ApplyRawKubernetesConfigRequest;

        let request = ApplyRawKubernetesConfigRequest {
            nix_config_content: k8s_config,
            kubeconfig_data: pki.admin_kubeconfig.clone(),
        };

        let response = admin_client
            .apply_raw_kubernetes_config(request)
            .await
            .context("apply_raw_kubernetes_config RPC failed")?
            .into_inner();

        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            apply_id = response.apply_id,
            status = response.status,
            "Kubernetes configuration deployment started on node"
        );

        Ok(())
    }

    async fn wait_for_etcd_leader(&self, node_id: &str, timeout_secs: u64) -> Result<()> {
        info!(node_id = node_id, "waiting for etcd leader election");

        // Get node address from database
        let node = self.db.get_node(node_id)?.context("node not found")?;
        let address = &node.address;

        let _client = self
            .node_clients
            .get_admin(address)
            .context("node client not found")?;

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            // TODO: Implement actual etcd status check via node agent RPC
            // For now, just wait and assume etcd is starting up
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            info!(node_id = node_id, "waiting for etcd...");
        }

        // Assume etcd is ready after timeout period
        info!(node_id = node_id, "assuming etcd is ready");
        Ok(())
    }

    async fn wait_for_etcd_quorum(
        &self,
        cluster_name: &str,
        expected_nodes: usize,
        timeout_secs: u64,
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            expected_nodes = expected_nodes,
            "waiting for etcd quorum"
        );

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            let healthy_count = self.count_healthy_etcd_members(cluster_name).await?;

            if healthy_count >= expected_nodes {
                info!(
                    cluster = cluster_name,
                    healthy_nodes = healthy_count,
                    "etcd quorum achieved"
                );
                return Ok(());
            }

            info!(
                cluster = cluster_name,
                healthy_nodes = healthy_count,
                expected_nodes = expected_nodes,
                "waiting for more etcd members"
            );

            sleep(Duration::from_secs(10)).await;
        }

        anyhow::bail!("timeout waiting for etcd quorum in {}", cluster_name)
    }

    async fn count_healthy_etcd_members(&self, cluster_name: &str) -> Result<usize> {
        let _nodes = self.get_cluster_nodes(cluster_name).await?;

        // TODO: Implement actual etcd status check via node agent RPC
        // For now, always return 0 to force fresh bootstrap
        // This ensures clusters are always bootstrapped from scratch until
        // we implement proper etcd health checking
        Ok(0)
    }

    async fn wait_for_apiserver(&self, cluster_name: &str, timeout_secs: u64) -> Result<()> {
        info!(cluster = cluster_name, "waiting for API server");

        let nodes = self.get_cluster_nodes(cluster_name).await?;
        let start = std::time::Instant::now();

        while start.elapsed().as_secs() < timeout_secs {
            let mut healthy_count = 0;

            for node in &nodes {
                let address = format!("{}:9091", node.ip_address);
                if self.node_clients.get_admin(&address).is_some() {
                    // TODO: Implement actual apiserver status check via node agent RPC
                    // For now, assume healthy if node is reachable
                    healthy_count += 1;
                }
            }

            if healthy_count > (nodes.len() / 2) {
                info!(
                    cluster = cluster_name,
                    healthy_apiservers = healthy_count,
                    "API server quorum achieved"
                );
                return Ok(());
            }

            sleep(Duration::from_secs(10)).await;
        }

        anyhow::bail!("timeout waiting for API server in {}", cluster_name)
    }

    async fn update_phase(&self, cluster_name: &str, phase: BootstrapPhase) -> Result<()> {
        info!(
            cluster = cluster_name,
            phase = phase.as_str(),
            "updating bootstrap phase"
        );

        self.db
            .update_kubernetes_cluster_phase(cluster_name, phase.as_str())?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ClusterNode {
    pub node_id: String,
    pub hostname: String,
    pub ip_address: String,
    pub initial_cluster: bool,
}

/// Background task that orchestrates Kubernetes cluster bootstraps
pub async fn start_kubernetes_orchestrator(db: Database, node_clients: NodeClients) {
    let orchestrator = KubernetesOrchestrator::new(db.clone(), node_clients);

    loop {
        // Check for clusters in "bootstrapping" phase that need bootstrap
        match db.list_kubernetes_clusters_by_phase("bootstrapping") {
            Ok(clusters) => {
                for cluster in clusters {
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    let api_endpoint = cluster.api_server_endpoint.clone();
                    let service_cidr = cluster.service_cidr.clone();

                    // Spawn bootstrap task for each pending cluster
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator
                            .bootstrap_cluster(&cluster_name, &api_endpoint, &service_cidr)
                            .await
                        {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes bootstrap failed"
                            );
                            let _ = orchestrator
                                .update_phase(&cluster_name, BootstrapPhase::Failed)
                                .await;
                        }
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list bootstrapping Kubernetes clusters");
            }
        }

        // Check every 30 seconds
        sleep(Duration::from_secs(30)).await;
    }
}
