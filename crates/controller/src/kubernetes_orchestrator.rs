use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Context, Result};
use tokio::sync::Mutex;
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
    /// Serializes Nix renders for one node so two cluster bootstraps cannot
    /// overwrite each other's file with a stale single-cluster config.
    node_apply_locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
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
        Self {
            db,
            node_clients,
            node_apply_locks: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn node_apply_lock(&self, node_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .node_apply_locks
            .lock()
            .expect("kubernetes node apply locks");
        locks
            .entry(node_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
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
                ip_address: if row.advertise_address.is_empty() {
                    node.address.split(':').next().unwrap_or("").to_string()
                } else {
                    row.advertise_address
                },
                iface: row.iface,
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

        self.db
            .store_kubernetes_pki(cluster_name, &pki)
            .context("failed to store PKI")?;

        info!(cluster = cluster_name, "PKI generated");
        Ok(pki)
    }

    /// Push node Nix again without advancing the cluster phase.
    ///
    /// Cluster create used to mark the cluster ready from a second bootstrap
    /// that never embedded certificates. A controller restart then ignored the
    /// cluster because the orchestrator only selects phase `bootstrapping`.
    async fn redeploy_node_certificates(
        &self,
        cluster_name: &str,
        api_endpoint: &str,
        service_cidr: &str,
    ) -> Result<()> {
        let nodes = self.get_cluster_nodes(cluster_name).await?;
        if nodes.is_empty() {
            anyhow::bail!("no nodes configured for cluster {}", cluster_name);
        }
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;

        let stored = self.db.load_kubernetes_pki(cluster_name)?;
        let pki = match stored {
            Some(pki)
                if nodes.iter().all(|node| {
                    pki.etcd_node_certs.contains_key(&node.node_id)
                        && pki.etcd_node_peer_certs.contains_key(&node.node_id)
                }) =>
            {
                pki
            }
            _ => {
                info!(
                    cluster = cluster_name,
                    "stored PKI is missing node certificates; generating"
                );
                self.generate_and_store_pki(cluster_name, api_endpoint, service_cidr, &nodes)
                    .await?
            }
        };

        for node in &nodes {
            self.configure_node(
                cluster_name,
                node,
                &pki,
                &cluster.cluster_cidr,
                service_cidr,
                &nodes,
            )
            .await?;
        }
        info!(
            cluster = cluster_name,
            nodes = nodes.len(),
            "redeployed kubernetes node configuration"
        );
        Ok(())
    }

    async fn configure_node(
        &self,
        cluster_name: &str,
        node: &ClusterNode,
        pki: &KubernetesPki,
        _cluster_cidr: &str,
        _service_cidr: &str,
        _all_nodes: &[ClusterNode],
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            "configuring Kubernetes on node"
        );

        let address = format!("{}:9091", node.ip_address);
        // Startup redeploy runs before the controller has dialed every node.
        if self.node_clients.get_admin(&address).is_none() {
            self.node_clients
                .connect(&address)
                .await
                .with_context(|| format!("connect node agent {address}"))?;
        }
        let mut admin_client = self
            .node_clients
            .get_admin(&address)
            .context("node client not found")?;

        // Fail before the node rebuild if this cluster has no etcd certs yet.
        pki.etcd_node_certs
            .get(&node.node_id)
            .context("etcd server cert not found")?;
        pki.etcd_node_peer_certs
            .get(&node.node_id)
            .context("etcd peer cert not found")?;

        let apply_lock = self.node_apply_lock(&node.node_id);
        let _guard = apply_lock.lock().await;
        let rendered = self
            .render_node_nix(&node.node_id, cluster_name, pki)
            .await?;

        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            config_len = rendered.nix.len(),
            clusters = rendered.cluster_count,
            "generated Kubernetes configuration for every cluster on the node"
        );

        // Apply configuration via node agent RPC
        use crate::node_proto::ApplyRawKubernetesConfigRequest;

        let request = ApplyRawKubernetesConfigRequest {
            nix_config_content: rendered.nix,
            kubeconfig_data: rendered.primary_kubeconfig,
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

    /// Nix for every cluster that already has PKI and includes this node.
    /// `current` overrides the database copy so a cluster that just stored
    /// certificates is included even if the read races the write.
    async fn render_node_nix(
        &self,
        node_id: &str,
        current_cluster: &str,
        current_pki: &KubernetesPki,
    ) -> Result<RenderedNodeNix> {
        let clusters = self.db.list_kubernetes_clusters()?;
        let mut members = Vec::new();
        let mut primary_kubeconfig = String::new();
        for cluster in clusters {
            let nodes = self.get_cluster_nodes(&cluster.name).await?;
            let Some(node) = nodes.iter().find(|n| n.node_id == node_id).cloned() else {
                continue;
            };
            let pki = if cluster.name == current_cluster {
                current_pki.clone()
            } else if let Some(stored) = self.db.load_kubernetes_pki(&cluster.name)? {
                stored
            } else {
                info!(
                    cluster = %cluster.name,
                    node_id,
                    "skipping cluster with no stored PKI"
                );
                continue;
            };
            let (etcd_server_cert, etcd_server_key) = pki
                .etcd_node_certs
                .get(&node.node_id)
                .context("etcd server cert not found")?
                .clone();
            let (etcd_peer_cert, etcd_peer_key) = pki
                .etcd_node_peer_certs
                .get(&node.node_id)
                .context("etcd peer cert not found")?
                .clone();
            let cfg = self
                .db
                .get_kubernetes_cluster_config(&cluster.name)?
                .context("cluster config not found")?;
            let settings = crate::nixgen::KubernetesRenderSettings {
                api_server_endpoint: cluster.api_server_endpoint.clone(),
                cluster_dns: cluster.cluster_dns.clone(),
                ha_enabled: cfg.ha_enabled,
                vip_address: cfg.vip_address,
                vip_interface: cfg.vip_interface,
                cilium_mode: cfg.cilium_mode,
                cilium_mtu: cfg.cilium_mtu,
                cilium_tunnel: cfg.cilium_tunnel,
                cilium_auto_direct_routes: cfg.cilium_auto_direct_routes,
                cilium_lb_ip_pools: cfg
                    .cilium_lb_ip_pools
                    .split(',')
                    .filter(|pool| !pool.is_empty())
                    .map(|pool| pool.to_string())
                    .collect(),
            };
            let ports = crate::nixgen::kubernetes_listen_ports(&settings.api_server_endpoint);
            if ports.api_server == 6443 {
                primary_kubeconfig = pki.admin_kubeconfig.clone();
            }
            let mut ordered = nodes.clone();
            ordered.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            let index = ordered
                .iter()
                .position(|n| n.node_id == node_id)
                .unwrap_or(0);
            let pod_subnet = crate::nixgen::node_pod_subnet(&cluster.cluster_cidr, index);
            members.push(OwnedMember {
                cluster_name: cluster.name,
                node,
                pki,
                cluster_cidr: cluster.cluster_cidr,
                service_cidr: cluster.service_cidr,
                etcd_server_cert,
                etcd_server_key,
                etcd_peer_cert,
                etcd_peer_key,
                all_nodes: nodes,
                settings,
                pod_subnet,
            });
        }
        if members.is_empty() {
            anyhow::bail!("no kubernetes clusters with PKI include node {node_id}");
        }
        let renders: Vec<crate::nixgen::KubernetesMemberRender<'_>> = members
            .iter()
            .map(|member| crate::nixgen::KubernetesMemberRender {
                cluster_name: &member.cluster_name,
                node: &member.node,
                pki: &member.pki,
                cluster_cidr: &member.cluster_cidr,
                service_cidr: &member.service_cidr,
                etcd_server_cert: &member.etcd_server_cert,
                etcd_server_key: &member.etcd_server_key,
                etcd_peer_cert: &member.etcd_peer_cert,
                etcd_peer_key: &member.etcd_peer_key,
                all_nodes: &member.all_nodes,
                settings: &member.settings,
                pod_subnet: member.pod_subnet.clone(),
            })
            .collect();
        let nix = crate::nixgen::generate_colocated_kubernetes_config(&renders)?;
        if primary_kubeconfig.is_empty() {
            primary_kubeconfig = current_pki.admin_kubeconfig.clone();
        }
        Ok(RenderedNodeNix {
            nix,
            primary_kubeconfig,
            cluster_count: members.len(),
        })
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
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(0);
        };
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(0);
        };
        let etcd_port =
            crate::nixgen::kubernetes_listen_ports(&cluster.api_server_endpoint).etcd_client;
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let mut healthy = 0usize;
        for row in rows.iter().filter(|row| row.role == "control-plane") {
            let ip = row.advertise_address.as_str();
            if ip.is_empty() {
                continue;
            }
            match etcd_member_healthy(
                ip,
                etcd_port,
                &pki.etcd_ca_cert,
                &pki.apiserver_etcd_client_cert,
                &pki.apiserver_etcd_client_key,
            )
            .await
            {
                Ok(true) => healthy += 1,
                Ok(false) => {}
                Err(e) => {
                    info!(
                        cluster = cluster_name,
                        node_id = %row.node_id,
                        error = %e,
                        "etcd health probe failed"
                    );
                }
            }
        }
        Ok(healthy)
    }

    async fn control_plane_quorum(&self, cluster_name: &str) -> Result<usize> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let control_planes = rows
            .iter()
            .filter(|row| row.role == "control-plane")
            .count();
        Ok((control_planes / 2) + 1)
    }

    async fn apiserver_is_ready(&self, cluster_name: &str) -> Result<bool> {
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(false);
        };
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(false);
        };
        match apiserver_ready(&cluster.api_server_endpoint, &pki.ca_cert).await {
            Ok(ready) => Ok(ready),
            Err(e) => {
                info!(
                    cluster = cluster_name,
                    error = %e,
                    "API server readiness probe failed"
                );
                Ok(false)
            }
        }
    }

    /// A cluster left in `failed` by the old stub health check can already
    /// have etcd and the API server up. Promote it without minting new PKI.
    async fn promote_if_healthy(&self, cluster_name: &str) -> Result<()> {
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(());
        };
        if cluster.phase == "ready" || cluster.phase == "bootstrapping" {
            return Ok(());
        }
        let need = self.control_plane_quorum(cluster_name).await?;
        let healthy = self.count_healthy_etcd_members(cluster_name).await?;
        if healthy < need {
            return Ok(());
        }
        if !self.apiserver_is_ready(cluster_name).await? {
            return Ok(());
        }
        let control_planes = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .iter()
            .filter(|row| row.role == "control-plane")
            .count() as i32;
        self.db
            .mark_kubernetes_cluster_ready(cluster_name, control_planes)?;
        info!(
            cluster = cluster_name,
            healthy_etcd = healthy,
            control_planes,
            "recorded kubernetes cluster ready"
        );
        Ok(())
    }

    async fn wait_for_apiserver(&self, cluster_name: &str, timeout_secs: u64) -> Result<()> {
        info!(cluster = cluster_name, "waiting for API server");

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            if self.apiserver_is_ready(cluster_name).await? {
                info!(cluster = cluster_name, "API server ready");
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

fn endpoint_host_port(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .to_string()
}

fn response_reports_ready(path: &str, status: u16, body: &str) -> bool {
    if status != 200 {
        return false;
    }
    if path == "/health" {
        body.replace([' ', '\n'], "")
            .contains("\"health\":\"true\"")
    } else {
        body.contains("ok")
    }
}

async fn etcd_member_healthy(
    ip: &str,
    port: u16,
    ca_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> Result<bool> {
    let (status, body) = tls_get(
        &format!("{ip}:{port}"),
        ip,
        "/health",
        ca_pem,
        Some((client_cert_pem, client_key_pem)),
    )
    .await?;
    Ok(response_reports_ready("/health", status, &body))
}

async fn apiserver_ready(api_endpoint: &str, ca_pem: &str) -> Result<bool> {
    let endpoint = endpoint_host_port(api_endpoint);
    let host = endpoint.split(':').next().unwrap_or("").to_string();
    if host.is_empty() {
        return Ok(false);
    }
    let (status, body) = tls_get(&endpoint, &host, "/readyz", ca_pem, None).await?;
    Ok(response_reports_ready("/readyz", status, &body))
}

async fn tls_get(
    addr: &str,
    server_name: &str,
    path: &str,
    ca_pem: &str,
    client_auth: Option<(&str, &str)>,
) -> Result<(u16, String)> {
    use rustls::pki_types::ServerName;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(ca_pem)? {
        roots.add(cert).context("etcd or kubernetes CA")?;
    }
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let config = if let Some((cert_pem, key_pem)) = client_auth {
        builder
            .with_client_auth_cert(pem_certs(cert_pem)?, pem_key(key_pem)?)
            .context("etcd client certificate")?
    } else {
        builder.with_no_client_auth()
    };
    let name = ServerName::try_from(server_name.to_string()).context("probe server name")?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = tokio::time::timeout(Duration::from_secs(3), tokio::net::TcpStream::connect(addr))
        .await
        .context("connect timed out")?
        .with_context(|| format!("connect {addr}"))?;
    let mut tls = connector
        .connect(name, stream)
        .await
        .context("tls handshake")?;
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {server_name}\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes())
        .await
        .context("write probe")?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), tls.read_to_end(&mut buf))
        .await
        .context("read timed out")?
        .context("read probe")?;
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

fn pem_certs(pem_str: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::CertificateDer;
    let mut certs = Vec::new();
    for block in pem::parse_many(pem_str).context("parsing certificate pem")? {
        if block.tag() == "CERTIFICATE" {
            certs.push(CertificateDer::from(block.contents().to_vec()));
        }
    }
    if certs.is_empty() {
        anyhow::bail!("no certificate in pem");
    }
    Ok(certs)
}

fn pem_key(pem_str: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    use rustls::pki_types::{
        PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    };
    let block = pem::parse(pem_str).context("parsing private key pem")?;
    let bytes = block.contents().to_vec();
    match block.tag() {
        "PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(bytes))),
        "EC PRIVATE KEY" => Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(bytes))),
        "RSA PRIVATE KEY" => Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(bytes))),
        other => anyhow::bail!("unsupported private key type {other}"),
    }
}

struct RenderedNodeNix {
    nix: String,
    primary_kubeconfig: String,
    cluster_count: usize,
}

struct OwnedMember {
    cluster_name: String,
    node: ClusterNode,
    pki: KubernetesPki,
    cluster_cidr: String,
    service_cidr: String,
    etcd_server_cert: String,
    etcd_server_key: String,
    etcd_peer_cert: String,
    etcd_peer_key: String,
    all_nodes: Vec<ClusterNode>,
    settings: crate::nixgen::KubernetesRenderSettings,
    pod_subnet: String,
}

#[derive(Debug, Clone)]
pub struct ClusterNode {
    pub node_id: String,
    pub hostname: String,
    pub ip_address: String,
    pub iface: String,
    pub initial_cluster: bool,
}

/// Background task that orchestrates Kubernetes cluster bootstraps
pub async fn start_kubernetes_orchestrator(db: Database, node_clients: NodeClients) {
    let orchestrator = KubernetesOrchestrator::new(db.clone(), node_clients);

    // Clusters that already left `bootstrapping` never get another render.
    // Push certificates once per controller start. The 30s loop below still
    // owns clusters that are waiting to bootstrap.
    match db.list_kubernetes_clusters() {
        Ok(clusters) => {
            for cluster in clusters {
                if cluster.phase == "bootstrapping" {
                    continue;
                }
                let orchestrator = orchestrator.clone();
                let cluster_name = cluster.name.clone();
                let api_endpoint = cluster.api_server_endpoint.clone();
                let service_cidr = cluster.service_cidr.clone();
                let already_up = orchestrator
                    .count_healthy_etcd_members(&cluster_name)
                    .await
                    .unwrap_or(0);
                let need = orchestrator
                    .control_plane_quorum(&cluster_name)
                    .await
                    .unwrap_or(1);
                if already_up >= need {
                    info!(
                        cluster = %cluster_name,
                        healthy_etcd = already_up,
                        "etcd already has quorum; skipping certificate redeploy"
                    );
                    continue;
                }
                tokio::spawn(async move {
                    // Node agents may still be restarting when the controller
                    // comes up. This path runs once per process, so retry.
                    for attempt in 1..=6 {
                        match orchestrator
                            .redeploy_node_certificates(&cluster_name, &api_endpoint, &service_cidr)
                            .await
                        {
                            Ok(()) => return,
                            Err(e) => {
                                error!(
                                    cluster = cluster_name,
                                    attempt,
                                    error = %e,
                                    "kubernetes certificate redeploy failed"
                                );
                                sleep(Duration::from_secs(5)).await;
                            }
                        }
                    }
                });
            }
        }
        Err(e) => {
            error!(error = %e, "failed to list Kubernetes clusters for certificate redeploy");
        }
    }

    loop {
        if let Ok(clusters) = db.list_kubernetes_clusters() {
            for cluster in clusters {
                if let Err(e) = orchestrator.promote_if_healthy(&cluster.name).await {
                    error!(
                        cluster = %cluster.name,
                        error = %e,
                        "kubernetes health promotion failed"
                    );
                }
            }
        }

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

#[cfg(test)]
mod health_tests {
    use super::response_reports_ready;

    #[test]
    fn etcd_health_body_requires_true() {
        assert!(response_reports_ready(
            "/health",
            200,
            r#"{"health":"true"}"#
        ));
        assert!(response_reports_ready(
            "/health",
            200,
            "{\n  \"health\": \"true\"\n}"
        ));
        assert!(!response_reports_ready(
            "/health",
            200,
            r#"{"health":"false"}"#
        ));
        assert!(!response_reports_ready(
            "/health",
            503,
            r#"{"health":"true"}"#
        ));
    }

    #[test]
    fn apiserver_readyz_accepts_ok() {
        assert!(response_reports_ready("/readyz", 200, "ok"));
        assert!(!response_reports_ready("/readyz", 500, "ok"));
    }
}
