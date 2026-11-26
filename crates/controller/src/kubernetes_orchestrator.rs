use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Context, Result};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::db::{Database, KubernetesClusterNodeRow, KubernetesNodeUpgradeRow};
use crate::kubernetes_lifecycle::{self, MemberSnapshot, ScaleStep};
use crate::kubernetes_nodes::{self, GrpcNodes, KubernetesNodes, SecretFile};
use crate::kubernetes_pki::{self, KubernetesPki, NodeInfo};
use crate::kubernetes_upgrade::{
    self, apply_failure_summary, ApplyProgress, PendingApply, UpgradeMember, UpgradeStep,
    UpgradeView,
};
use crate::kubernetes_versions;
use crate::node_client::NodeClients;

pub mod ops;

/// Orchestrates Kubernetes cluster bootstrap across multiple nodes
#[derive(Clone)]
pub struct KubernetesOrchestrator {
    db: Database,
    node_clients: NodeClients,
    /// Node-agent / etcd / API I/O. Tests pass a [`kubernetes_nodes::Fake`].
    nodes: Arc<dyn KubernetesNodes>,
    /// Serializes Nix renders for one node so two cluster bootstraps cannot
    /// overwrite each other's file with a stale single-cluster config.
    node_apply_locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// One bootstrap or scale task per cluster. The loop ticks every 30s.
    inflight: Arc<StdMutex<HashSet<String>>>,
    /// When each control plane was first seen unhealthy ("cluster/node" ->
    /// unix seconds). Lost on restart, which only restarts the repair clock.
    unhealthy: Arc<StdMutex<HashMap<String, i64>>>,
    /// Where scheduled and manual etcd snapshots are written.
    backup_root: std::path::PathBuf,
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
    WaitingForNodes,
    Ready,
    Failed,
}

const NODES_READY_TIMEOUT_SECS: u64 = 600;
pub const NODES_NOT_READY: &str = "nodes not Ready";
/// A cluster whose nodes are being released by DeleteKubernetesCluster.
/// No reconcile loop acts on it, and node renders leave it out.
pub const PHASE_DELETING: &str = "deleting";
pub const PHASE_RECONFIGURING: &str = "reconfiguring";
pub const PHASE_RESTORING: &str = "restoring";
pub const PHASE_ROLLING_BACK: &str = "rolling-back";
const APPLY_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const ETCD_MEMBERS_TIMEOUT_SECS: u64 = 300;
const KUBERNETES_DISABLED_NIX: &str = "{\n  kcore.kubernetes.enable = false;\n}\n";

pub struct MemberHealth {
    pub node_id: String,
    pub role: String,
    /// None when the API could not list nodes.
    pub ready: Option<bool>,
    pub etcd_healthy: bool,
    pub api_healthy: bool,
}

pub struct ClusterHealth {
    pub api_reachable: bool,
    pub nodes: Vec<MemberHealth>,
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
            Self::WaitingForNodes => "waiting-for-nodes",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

impl KubernetesOrchestrator {
    pub fn new(db: Database, node_clients: NodeClients) -> Self {
        let nodes: Arc<dyn KubernetesNodes> = Arc::new(GrpcNodes::new(node_clients.clone()));
        Self::with_nodes(db, node_clients, nodes)
    }

    pub fn with_nodes(
        db: Database,
        node_clients: NodeClients,
        nodes: Arc<dyn KubernetesNodes>,
    ) -> Self {
        Self {
            db,
            node_clients,
            nodes,
            node_apply_locks: Arc::new(StdMutex::new(HashMap::new())),
            inflight: Arc::new(StdMutex::new(HashSet::new())),
            unhealthy: Arc::new(StdMutex::new(HashMap::new())),
            backup_root: std::path::PathBuf::from(ops::DEFAULT_BACKUP_ROOT),
        }
    }

    pub fn with_backup_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.backup_root = root.into();
        self
    }

    fn claim(&self, cluster_name: &str) -> bool {
        self.inflight
            .lock()
            .expect("kubernetes inflight")
            .insert(cluster_name.to_string())
    }

    fn is_claimed(&self, cluster_name: &str) -> bool {
        self.inflight
            .lock()
            .expect("kubernetes inflight")
            .contains(cluster_name)
    }

    fn release(&self, cluster_name: &str) {
        self.inflight
            .lock()
            .expect("kubernetes inflight")
            .remove(cluster_name);
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

        info!(
            cluster = cluster_name,
            node_count = nodes.len(),
            "bootstrapping new cluster"
        );
        self.bootstrap_new_cluster(cluster_name, api_endpoint, service_cidr, &nodes)
            .await?;

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

        let mut applies = Vec::new();
        let mut failures = Vec::new();
        for (node, task) in nodes.iter().zip(deploy_tasks) {
            match task.await.context("task join failed")? {
                Ok(apply_id) => applies.push(PendingApply {
                    node_id: node.node_id.clone(),
                    address: format!("{}:9091", node.ip_address),
                    apply_id,
                }),
                Err(e) => failures.push(format!("{}: {e:#}", node.node_id)),
            }
        }
        if !failures.is_empty() {
            error!(
                cluster = cluster_name,
                failed = failures.len(),
                total = nodes.len(),
                "some node deployments failed"
            );
            return Err(anyhow!(
                "{} of {} node deployments failed: {}",
                failures.len(),
                nodes.len(),
                failures.join("; ")
            ));
        }

        info!(
            cluster = cluster_name,
            node_count = nodes.len(),
            "all nodes deployed successfully, waiting for etcd quorum"
        );

        // Phase 3: Wait for etcd quorum. Workers are kubelet members and do
        // not count toward that majority.
        self.update_phase(cluster_name, BootstrapPhase::WaitingForEtcdLeader)
            .await?;
        let first_cp = nodes
            .iter()
            .find(|n| n.role != "worker")
            .context("cluster has no control-plane node")?;
        self.wait_for_etcd_leader(cluster_name, &first_cp.node_id, 180)
            .await?;

        self.update_phase(cluster_name, BootstrapPhase::WaitingForQuorum)
            .await?;
        let control_planes = nodes.iter().filter(|node| node.role != "worker").count();
        if control_planes == 0 {
            anyhow::bail!("cluster {cluster_name} has no control-plane node");
        }
        let required_quorum = (control_planes / 2) + 1;
        self.wait_for_etcd_quorum(cluster_name, required_quorum, &mut applies, 300)
            .await?;

        // Phase 4: Wait for API server
        self.update_phase(cluster_name, BootstrapPhase::StartingApiServer)
            .await?;
        self.wait_for_apiserver(cluster_name, 180).await?;

        // Phase 5: every member has registered and reports Ready
        self.update_phase(cluster_name, BootstrapPhase::WaitingForNodes)
            .await?;
        self.wait_for_nodes_ready(
            cluster_name,
            control_planes as i32,
            NODES_READY_TIMEOUT_SECS,
        )
        .await?;

        info!(
            cluster = cluster_name,
            nodes = nodes.len(),
            "cluster bootstrap completed"
        );
        Ok(())
    }

    /// Ready once every member is Ready in the API. After the timeout the
    /// cluster is degraded and names the missing nodes; the health loop
    /// promotes it when they come up.
    async fn wait_for_nodes_ready(
        &self,
        cluster_name: &str,
        control_planes: i32,
        timeout_secs: u64,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        let mut not_ready = Vec::new();
        while start.elapsed().as_secs() < timeout_secs {
            match self.refresh_node_readiness(cluster_name).await {
                Ok(missing) if missing.is_empty() => {
                    if let Err(err) = self.label_workers(cluster_name).await {
                        warn!(cluster = cluster_name, error = %err, "worker role labels not applied");
                    }
                    self.db
                        .mark_kubernetes_cluster_ready(cluster_name, control_planes)?;
                    return Ok(());
                }
                Ok(missing) => not_ready = missing,
                Err(e) => info!(cluster = cluster_name, error = %e, "node readiness check failed"),
            }
            sleep(Duration::from_secs(10)).await;
        }
        let message = if not_ready.is_empty() {
            format!("{NODES_NOT_READY}: the API did not list the nodes")
        } else {
            format!("{NODES_NOT_READY}: {}", not_ready.join(", "))
        };
        self.db
            .set_kubernetes_cluster_status(cluster_name, "degraded", &message)?;
        Ok(())
    }

    /// Store each member's Ready condition and the ready counts from the API
    /// node list. Returns the members that are not Ready.
    async fn refresh_node_readiness(&self, cluster_name: &str) -> Result<Vec<String>> {
        let api_nodes = self.list_api_nodes(cluster_name, None).await?;
        let mut not_ready = Vec::new();
        let mut ready_control_planes = 0;
        let mut ready_workers = 0;
        for mut row in self.db.list_kubernetes_cluster_nodes(cluster_name)? {
            if row.pending_join || row.pending_removal {
                continue;
            }
            let hostname = self
                .db
                .get_node(&row.node_id)?
                .map(|node| node.hostname)
                .unwrap_or_default();
            let ip = self.member_ip(&row).unwrap_or_default();
            let listed = kubernetes_lifecycle::match_registered_node(
                &api_nodes,
                &row.k8s_node_name,
                &hostname,
                &ip,
            );
            let ready = listed.is_some_and(|node| node.ready);
            if !ready {
                not_ready.push(row.node_id.clone());
            } else if row.role == "worker" {
                ready_workers += 1;
            } else {
                ready_control_planes += 1;
            }
            let name = listed
                .map(|node| node.name.clone())
                .unwrap_or_else(|| row.k8s_node_name.clone());
            if row.kubelet_ready != ready || row.k8s_node_name != name {
                row.kubelet_ready = ready;
                row.k8s_node_name = name;
                self.db.upsert_kubernetes_cluster_node(&row)?;
            }
        }
        self.db
            .set_kubernetes_ready_counts(cluster_name, ready_control_planes, ready_workers)?;
        Ok(not_ready)
    }

    /// Live etcd, API server, and node readiness for every member.
    pub async fn observe_health(&self, cluster_name: &str) -> Result<ClusterHealth> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let pki = self.db.load_kubernetes_pki(cluster_name)?;
        let api_reachable = self.apiserver_is_ready(cluster_name).await?;
        let api_nodes = if api_reachable {
            match self.list_api_nodes(cluster_name, None).await {
                Ok(nodes) => Some(nodes),
                Err(e) => {
                    info!(cluster = cluster_name, error = %e, "listing API nodes failed");
                    None
                }
            }
        } else {
            None
        };
        let endpoint = endpoint_host_port(&cluster.api_server_endpoint);
        let api_host = endpoint.split(':').next().unwrap_or("").to_string();
        let api_port =
            crate::nixgen::kubernetes_listen_ports(&cluster.api_server_endpoint).api_server;

        let mut nodes = Vec::new();
        for row in self.db.list_kubernetes_cluster_nodes(cluster_name)? {
            let control_plane = row.role != "worker";
            let hostname = self
                .db
                .get_node(&row.node_id)?
                .map(|node| node.hostname)
                .unwrap_or_default();
            let ip = self.member_ip(&row).unwrap_or_default();
            let ready = api_nodes.as_ref().map(|listed| {
                kubernetes_lifecycle::match_registered_node(
                    listed,
                    &row.k8s_node_name,
                    &hostname,
                    &ip,
                )
                .is_some_and(|node| node.ready)
            });
            let etcd_healthy = control_plane
                && self
                    .probe_etcd_member(cluster_name, &ip)
                    .await
                    .unwrap_or(false);
            // Every control plane serves the same certificate, so check the
            // node's address against the cluster endpoint's name.
            let api_healthy = control_plane
                && !ip.is_empty()
                && match &pki {
                    Some(pki) => self
                        .nodes
                        .apiserver_ready(
                            &format!("{ip}:{api_port}"),
                            &api_host,
                            &kubernetes_pki::ca_trust_bundle(pki),
                        )
                        .await
                        .unwrap_or(false),
                    None => false,
                };
            nodes.push(MemberHealth {
                node_id: row.node_id,
                role: row.role,
                ready,
                etcd_healthy,
                api_healthy,
            });
        }
        Ok(ClusterHealth {
            api_reachable,
            nodes,
        })
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
                role: row.role,
                kubelet_kubeconfig: String::new(),
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
    pub async fn redeploy_node_certificates(
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
            Some(mut pki) => {
                let mut issued = false;
                for node in &nodes {
                    if pki.etcd_node_certs.contains_key(&node.node_id)
                        && pki.etcd_node_peer_certs.contains_key(&node.node_id)
                    {
                        continue;
                    }
                    let _kubelet = kubernetes_pki::issue_node_certificates(
                        &mut pki,
                        cluster_name,
                        api_endpoint,
                        &NodeInfo {
                            node_id: node.node_id.clone(),
                            hostname: node.hostname.clone(),
                            ip_address: node.ip_address.clone(),
                        },
                    )?;
                    issued = true;
                }
                if issued {
                    self.db.store_kubernetes_pki(cluster_name, &pki)?;
                }
                pki
            }
            None => {
                info!(cluster = cluster_name, "stored PKI is missing; generating");
                self.generate_and_store_pki(cluster_name, api_endpoint, service_cidr, &nodes)
                    .await?
            }
        };

        // Joining nodes are configured by the scaling steps, and a leaving
        // node no longer renders this cluster.
        let not_serving: HashSet<String> = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .filter(|row| row.pending_join || row.pending_removal)
            .map(|row| row.node_id)
            .collect();
        for node in &nodes {
            if not_serving.contains(&node.node_id) {
                continue;
            }
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
    ) -> Result<String> {
        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            "configuring Kubernetes on node"
        );

        let address = format!("{}:9091", node.ip_address);

        // Fail before the node rebuild if this cluster has no etcd certs yet.
        pki.etcd_node_certs
            .get(&node.node_id)
            .context("etcd server cert not found")?;
        pki.etcd_node_peer_certs
            .get(&node.node_id)
            .context("etcd peer cert not found")?;

        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let kubelet_kubeconfig = if node.kubelet_kubeconfig.is_empty() {
            let mut issued = pki.clone();
            let kubeconfig = kubernetes_pki::issue_node_certificates(
                &mut issued,
                cluster_name,
                &cluster.api_server_endpoint,
                &NodeInfo {
                    node_id: node.node_id.clone(),
                    hostname: if node.hostname.is_empty() {
                        node.node_id.clone()
                    } else {
                        node.hostname.clone()
                    },
                    ip_address: node.ip_address.clone(),
                },
            )?;
            // Persist any etcd certs issue_node_certificates may have filled.
            if issued.etcd_node_certs.len() != pki.etcd_node_certs.len()
                || issued.etcd_node_peer_certs.len() != pki.etcd_node_peer_certs.len()
            {
                self.db.store_kubernetes_pki(cluster_name, &issued)?;
            }
            kubeconfig
        } else {
            node.kubelet_kubeconfig.clone()
        };

        let apply_lock = self.node_apply_lock(&node.node_id);
        let _guard = apply_lock.lock().await;
        // Kubelet credentials are delivered as a secret file; the Nix module
        // only gets an empty string so the private key never enters /nix/store.
        let rendered = self
            .render_node_nix(&node.node_id, cluster_name, pki, "")
            .await?;

        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            config_len = rendered.nix.len(),
            clusters = rendered.cluster_count,
            "generated Kubernetes configuration for every cluster on the node"
        );

        let encryption = self
            .db
            .get_kubernetes_cluster_ops(cluster_name)
            .ok()
            .flatten()
            .and_then(|ops| {
                crate::kubernetes_hardening::EncryptionKeys::from_json(&ops.encryption_json)
            });
        let mut secrets = pki_secret_files(cluster_name, &node.node_id, pki, encryption.as_ref());
        if !pki.admin_kubeconfig.is_empty() {
            secrets.push(SecretFile {
                path: format!(
                    "{}/admin.conf",
                    crate::nixgen::kubernetes_pki_key_dir(cluster_name)
                ),
                mode: 0o600,
                content: pki.admin_kubeconfig.as_bytes().to_vec(),
            });
        }
        if !kubelet_kubeconfig.is_empty() {
            secrets.push(SecretFile {
                path: format!(
                    "{}/kubelet.conf",
                    crate::nixgen::kubernetes_pki_key_dir(cluster_name)
                ),
                mode: 0o600,
                content: kubelet_kubeconfig.into_bytes(),
            });
        }
        if let Some(ops) = self
            .db
            .get_kubernetes_cluster_ops(cluster_name)
            .ok()
            .flatten()
        {
            let oidc = crate::kubernetes_hardening::OidcSpec::from_json(&ops.oidc_json);
            if !oidc.ca_pem.is_empty() {
                secrets.push(SecretFile {
                    path: format!(
                        "{}/oidc-ca.crt",
                        crate::nixgen::kubernetes_pki_key_dir(cluster_name)
                    ),
                    mode: 0o600,
                    content: oidc.ca_pem.into_bytes(),
                });
            }
        }
        let apply_id = self
            .nodes
            .apply_config(
                &address,
                &rendered.nix,
                &rendered.primary_kubeconfig,
                &secrets,
            )
            .await
            .map_err(|e| anyhow!(e))?;
        self.record_rendered_hash(cluster_name, &node.node_id, pki)
            .await;

        info!(
            cluster = cluster_name,
            node_id = node.node_id,
            apply_id = %apply_id,
            "Kubernetes configuration deployment started on node"
        );

        Ok(apply_id)
    }

    /// Nix for every cluster that already has PKI and includes this node.
    /// `current` overrides the database copy so a cluster that just stored
    /// certificates is included even if the read races the write.
    async fn render_node_nix(
        &self,
        node_id: &str,
        current_cluster: &str,
        current_pki: &KubernetesPki,
        kubelet_kubeconfig: &str,
    ) -> Result<RenderedNodeNix> {
        let clusters = self.db.list_kubernetes_clusters()?;
        let mut members = Vec::new();
        let mut primary_kubeconfig = String::new();
        for cluster in clusters {
            if cluster.phase == PHASE_DELETING {
                continue;
            }
            let membership = self.db.list_kubernetes_cluster_nodes(&cluster.name)?;
            if membership
                .iter()
                .any(|row| row.node_id == node_id && row.pending_removal)
            {
                continue;
            }
            let nodes = self.get_cluster_nodes(&cluster.name).await?;
            let Some(mut node) = nodes.iter().find(|n| n.node_id == node_id).cloned() else {
                continue;
            };
            if cluster.name == current_cluster && !kubelet_kubeconfig.is_empty() {
                node.kubelet_kubeconfig = kubelet_kubeconfig.to_string();
            }
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
                schedule_on_control_plane: control_planes_take_workloads(
                    cfg.schedule_on_control_plane,
                    &membership,
                ),
                provisioner_manifest: applied_provisioner_manifest(
                    &cfg.local_provisioner_json,
                    &cluster.api_server_endpoint,
                ),
                ceph_csi_manifest: self.ceph_csi_manifest_for(&cluster.name),
                metrics_server_manifest: crate::kubernetes_csi::metrics_server_manifest(),
                // The node's own version, not the cluster's: a rollout moves
                // one node at a time, and any other redeploy must not jump ahead.
                version: crate::kubernetes_versions::normalize(
                    &self
                        .db
                        .kubernetes_node_version(&cluster.name, node_id)?
                        .unwrap_or_default(),
                ),
                oidc: self
                    .db
                    .get_kubernetes_cluster_ops(&cluster.name)?
                    .map(|ops| crate::kubernetes_hardening::OidcSpec::from_json(&ops.oidc_json))
                    .unwrap_or_default(),
                default_deny_label: self
                    .db
                    .get_kubernetes_cluster_ops(&cluster.name)?
                    .map(|ops| ops.default_deny_label)
                    .unwrap_or_default(),
                reserve_for_vms: self
                    .db
                    .list_vms_for_node(node_id)
                    .map(|vms| !vms.is_empty())
                    .unwrap_or(false),
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

    /// Probe `/health` on the member's etcd client port until it answers healthy
    /// or `timeout_secs` elapses.
    async fn wait_for_etcd_leader(
        &self,
        cluster_name: &str,
        node_id: &str,
        timeout_secs: u64,
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            node_id = node_id,
            "waiting for etcd leader election"
        );
        let pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found")?;
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let row = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .find(|r| r.node_id == node_id)
            .with_context(|| format!("{node_id} is not a member of {cluster_name}"))?;
        let ip = if row.advertise_address.is_empty() {
            self.db
                .get_node(node_id)?
                .context("node not found")?
                .address
                .split(':')
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            row.advertise_address.clone()
        };
        if ip.is_empty() {
            anyhow::bail!("no advertise address for {node_id}");
        }
        let etcd_port =
            crate::nixgen::kubernetes_listen_ports(&cluster.api_server_endpoint).etcd_client;
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            match self
                .nodes
                .etcd_health(
                    &ip,
                    etcd_port,
                    &kubernetes_pki::etcd_ca_trust_bundle(&pki),
                    &pki.apiserver_etcd_client_cert,
                    &pki.apiserver_etcd_client_key,
                )
                .await
            {
                Ok(true) => {
                    info!(cluster = cluster_name, node_id, "etcd is healthy");
                    return Ok(());
                }
                Ok(false) => {
                    info!(cluster = cluster_name, node_id, "etcd not healthy yet");
                }
                Err(e) => {
                    info!(
                        cluster = cluster_name,
                        node_id,
                        error = %e,
                        "etcd health probe failed"
                    );
                }
            }
            sleep(Duration::from_secs(2)).await;
        }
        anyhow::bail!("etcd on {node_id} did not become healthy within {timeout_secs}s")
    }

    async fn wait_for_etcd_quorum(
        &self,
        cluster_name: &str,
        expected_nodes: usize,
        applies: &mut Vec<PendingApply>,
        timeout_secs: u64,
    ) -> Result<()> {
        info!(
            cluster = cluster_name,
            expected_nodes = expected_nodes,
            "waiting for etcd quorum"
        );

        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            self.check_pending_applies(applies).await?;
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

        let rebuilding: Vec<&str> = applies.iter().map(|a| a.node_id.as_str()).collect();
        if rebuilding.is_empty() {
            anyhow::bail!("timeout waiting for etcd quorum in {cluster_name}")
        }
        anyhow::bail!(
            "timeout waiting for etcd quorum in {cluster_name}; still rebuilding: {}",
            rebuilding.join(", ")
        )
    }

    /// Drops finished rebuilds and fails on the first one that failed, so a
    /// broken node configuration is reported instead of a quorum timeout.
    async fn check_pending_applies(&self, applies: &mut Vec<PendingApply>) -> Result<()> {
        let mut still_running = Vec::new();
        for apply in applies.drain(..) {
            let (progress, message) = self.apply_progress(&apply.address, &apply.apply_id).await;
            match progress {
                ApplyProgress::Failed => anyhow::bail!(
                    "nixos-rebuild failed on {}: {}",
                    apply.node_id,
                    apply_failure_summary(&message)
                ),
                ApplyProgress::Succeeded => {}
                _ => still_running.push(apply),
            }
        }
        *applies = still_running;
        Ok(())
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
            match self
                .nodes
                .etcd_health(
                    ip,
                    etcd_port,
                    &kubernetes_pki::etcd_ca_trust_bundle(&pki),
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

    /// True when enough etcd members answer that one control plane can
    /// restart without taking the cluster down.
    pub async fn etcd_has_quorum(&self, cluster_name: &str) -> bool {
        let healthy = self
            .count_healthy_etcd_members(cluster_name)
            .await
            .unwrap_or(0);
        let need = self.control_plane_quorum(cluster_name).await.unwrap_or(1);
        healthy >= need
    }

    /// Push the stored configuration to `members` one at a time, control
    /// planes first. Each rebuild has to finish, and a control plane's etcd
    /// has to be back, before the next node starts, so a change that restarts
    /// etcd or the API server never takes them down on two nodes at once.
    async fn roll_config(
        &self,
        cluster_name: &str,
        mut members: Vec<ClusterNode>,
        phase: &str,
    ) -> Result<()> {
        members.sort_by_key(|member| member.role == "worker");
        let total = members.len();
        for (done, member) in members.iter().enumerate() {
            self.db.set_kubernetes_cluster_status(
                cluster_name,
                phase,
                &format!(
                    "applying configuration to {} ({done}/{total} nodes, control planes first)",
                    member.node_id
                ),
            )?;
            let apply_id = self
                .push_member_config(cluster_name, &member.node_id)
                .await
                .with_context(|| format!("push configuration to {}", member.node_id))?;
            self.wait_for_apply(
                &member.node_id,
                &format!("{}:9091", member.ip_address),
                &apply_id,
            )
            .await?;
            if member.role != "worker" {
                self.wait_for_etcd_members(cluster_name, &member.node_id)
                    .await?;
            }
        }
        Ok(())
    }

    async fn wait_for_apply(&self, node_id: &str, address: &str, apply_id: &str) -> Result<()> {
        if apply_id.is_empty() {
            return Ok(());
        }
        let start = std::time::Instant::now();
        loop {
            let (progress, message) = self.apply_progress(address, apply_id).await;
            match progress {
                ApplyProgress::Succeeded => return Ok(()),
                ApplyProgress::Failed => anyhow::bail!(
                    "nixos-rebuild failed on {node_id}: {}",
                    apply_failure_summary(&message)
                ),
                // The agent lost the record; etcd health decides for a
                // control plane.
                ApplyProgress::Unknown | ApplyProgress::NotRecorded => return Ok(()),
                ApplyProgress::Running => {}
            }
            if start.elapsed() > APPLY_TIMEOUT {
                anyhow::bail!(
                    "nixos-rebuild on {node_id} did not finish in {} minutes",
                    APPLY_TIMEOUT.as_secs() / 60
                );
            }
            sleep(Duration::from_secs(5)).await;
        }
    }

    /// Waits until every control plane that is already in etcd answers.
    async fn wait_for_etcd_members(&self, cluster_name: &str, after: &str) -> Result<()> {
        let expected = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .iter()
            .filter(|row| row.role == "control-plane" && !row.pending_join)
            .count();
        let start = std::time::Instant::now();
        let mut healthy = 0;
        while start.elapsed().as_secs() < ETCD_MEMBERS_TIMEOUT_SECS {
            healthy = self.count_healthy_etcd_members(cluster_name).await?;
            if healthy >= expected {
                return Ok(());
            }
            sleep(Duration::from_secs(5)).await;
        }
        anyhow::bail!(
            "etcd did not recover after {after} was reconfigured: {healthy} of {expected} members healthy"
        )
    }

    fn member_role(&self, cluster_name: &str, node_id: &str) -> Result<String> {
        Ok(self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .find(|row| row.node_id == node_id)
            .map(|row| row.role)
            .unwrap_or_default())
    }

    /// Re-render the control planes already serving after the control-plane
    /// set changed. Their keepalived peers come from the member list: a
    /// joining node they do not list never hears the VIP holder and claims
    /// the VIP too.
    async fn refresh_serving_control_planes(&self, cluster_name: &str, skip: &str) -> Result<()> {
        let serving: HashSet<String> = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .filter(|row| {
                row.role == "control-plane"
                    && !row.pending_join
                    && !row.pending_removal
                    && row.node_id != skip
            })
            .map(|row| row.node_id)
            .collect();
        let members = self
            .get_cluster_nodes(cluster_name)
            .await?
            .into_iter()
            .filter(|member| serving.contains(&member.node_id))
            .collect();
        self.roll_config(cluster_name, members, "scaling").await
    }

    /// Roll the configuration the operator last applied out to every member.
    async fn reconcile_reconfigure(&self, cluster_name: &str) -> Result<()> {
        let generation = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?
            .generation;
        let members = self.get_cluster_nodes(cluster_name).await?;
        self.roll_config(cluster_name, members, PHASE_RECONFIGURING)
            .await?;
        let current = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        // Applied again mid-rollout: leave the phase so the next pass rolls
        // the newer spec out.
        if current.generation != generation {
            return Ok(());
        }
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            "ready",
            "cluster configuration deployed",
        )?;
        Ok(())
    }

    async fn apiserver_is_ready(&self, cluster_name: &str) -> Result<bool> {
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(false);
        };
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(false);
        };
        let endpoint = endpoint_host_port(&cluster.api_server_endpoint);
        let host = endpoint.split(':').next().unwrap_or("").to_string();
        match self
            .nodes
            .apiserver_ready(&endpoint, &host, &kubernetes_pki::ca_trust_bundle(&pki))
            .await
        {
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
    /// A ready cluster only has its node readiness and counts refreshed.
    async fn promote_if_healthy(&self, cluster_name: &str) -> Result<()> {
        if self.is_claimed(cluster_name) {
            return Ok(());
        }
        let Some(cluster) = self.db.get_kubernetes_cluster(cluster_name)? else {
            return Ok(());
        };
        if cluster.phase == "ready" {
            if let Err(e) = self.refresh_node_readiness(cluster_name).await {
                info!(cluster = cluster_name, error = %e, "node readiness refresh failed");
            }
            if let Err(e) = self.ensure_ceph_csi_secrets(cluster_name).await {
                info!(cluster = cluster_name, error = %e, "ceph CSI secret apply failed");
            }
            return Ok(());
        }
        if crate::kubernetes_lifecycle::promotion_blocked(&cluster.phase, &cluster.status_message) {
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
        let not_ready = self.refresh_node_readiness(cluster_name).await?;
        if !not_ready.is_empty() {
            if cluster.phase == "degraded" && cluster.status_message.starts_with(NODES_NOT_READY) {
                self.db.set_kubernetes_cluster_status(
                    cluster_name,
                    "degraded",
                    &format!("{NODES_NOT_READY}: {}", not_ready.join(", ")),
                )?;
            }
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
        if let Err(e) = self.ensure_ceph_csi_secrets(cluster_name).await {
            info!(cluster = cluster_name, error = %e, "ceph CSI secret apply failed");
        }
        if let Err(e) = self.ensure_storage_ceph(cluster_name).await {
            info!(cluster = cluster_name, error = %e, "storage.ceph ensure failed");
        }
        Ok(())
    }

    /// When `storage.ceph` is enabled: require every member has a Ceph public
    /// address, ensure `client.k8s-<cluster>` exists, and store its key.
    async fn ensure_storage_ceph(&self, cluster_name: &str) -> Result<()> {
        let Some(ops) = self.db.get_kubernetes_cluster_ops(cluster_name)? else {
            return Ok(());
        };
        if ops.storage_json.trim().is_empty() {
            return Ok(());
        }
        let mut storage: crate::kubernetes_csi::CephStorage =
            serde_json::from_str(&ops.storage_json).context("storage_json")?;
        if !storage.enabled {
            return Ok(());
        }
        let members = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        for member in &members {
            let node = self
                .db
                .get_node(&member.node_id)?
                .with_context(|| format!("member {} missing", member.node_id))?;
            let addr = node.address.split(':').next().unwrap_or("").trim();
            if addr.is_empty() {
                anyhow::bail!(
                    "member {} has no address for Ceph CSI preflight",
                    member.node_id
                );
            }
        }
        let ceph_row = self
            .db
            .get_ceph_cluster(&storage.cluster)?
            .with_context(|| format!("CephCluster {} missing", storage.cluster))?;
        let ceph_spec = crate::ceph_cluster_spec::spec_from_json(&ceph_row.spec_json)
            .context("decode CephCluster")?;
        let admin_member = ceph_spec
            .nodes
            .first()
            .context("CephCluster has no members")?;
        let mut admin = crate::ceph_cluster_reconciler::admin_for(
            &self.db,
            &self.node_clients,
            &admin_member.node_id,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let client = format!("client.{}", storage.client_name(cluster_name));
        let key_resp = admin
            .ensure_ceph_client_key(tonic::Request::new(
                crate::node_proto::EnsureCephClientKeyRequest {
                    client_name: client,
                    pool: storage.pool.clone(),
                },
            ))
            .await
            .map_err(|e| anyhow::anyhow!("EnsureCephClientKey: {e}"))?
            .into_inner();
        let user_key = crate::kubernetes_local::cephx_key_from_keyring(&key_resp.keyring)
            .context("ceph CSI keyring has no key=")?;
        if storage.user_key != user_key {
            storage.user_key = user_key;
            let json = serde_json::to_string(&storage)?;
            self.db
                .set_kubernetes_cluster_storage_json(cluster_name, &json)?;
        }
        Ok(())
    }

    /// Fetch client.kcore and apply CSI Secrets via the kube API (not Nix).
    async fn ensure_ceph_csi_secrets(&self, cluster_name: &str) -> Result<()> {
        let Some(cfg) = self.db.get_kubernetes_cluster_config(cluster_name)? else {
            return Ok(());
        };
        let provisioner = crate::kubernetes_local::decode(&cfg.local_provisioner_json)
            .unwrap_or_else(|_| crate::kubernetes_local::LocalProvisioner::disabled());
        if !provisioner.has_ceph() {
            return Ok(());
        }
        let ceph_name = provisioner.ceph_cluster.clone();
        let ceph_row = self
            .db
            .get_ceph_cluster(&ceph_name)?
            .context("CephCluster for CSI missing")?;
        let ceph_spec = crate::ceph_cluster_spec::spec_from_json(&ceph_row.spec_json)
            .context("decode CephCluster")?;
        let member = ceph_spec
            .nodes
            .first()
            .context("CephCluster has no members")?;
        let mut admin = crate::ceph_cluster_reconciler::admin_for(
            &self.db,
            &self.node_clients,
            &member.node_id,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let key_resp = admin
            .ensure_ceph_client_key(tonic::Request::new(
                crate::node_proto::EnsureCephClientKeyRequest {
                    client_name: "client.kcore".into(),
                    pool: if provisioner.ceph_pool.is_empty() {
                        "kcore-vms".into()
                    } else {
                        provisioner.ceph_pool.clone()
                    },
                },
            ))
            .await
            .map_err(|e| anyhow::anyhow!("EnsureCephClientKey: {e}"))?
            .into_inner();
        let user_key = crate::kubernetes_local::cephx_key_from_keyring(&key_resp.keyring)
            .context("client.kcore keyring has no key=")?;
        let cp = self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .find(|n| n.role == "control-plane" && !n.pending_removal)
            .context("no control-plane for CSI secret")?;
        let node = self
            .db
            .get_node(&cp.node_id)?
            .context("control-plane node missing")?;
        crate::kubernetes_admin::upsert_ceph_csi_secret(
            &self.node_clients,
            &node.address,
            "csi-rbd-secret",
            "kcore",
            &user_key,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        if provisioner.has_cephfs() {
            crate::kubernetes_admin::upsert_ceph_csi_secret(
                &self.node_clients,
                &node.address,
                "csi-cephfs-secret",
                "kcore",
                &user_key,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        }
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

    async fn reconcile_scaling(&self, cluster_name: &str) -> Result<()> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        if kubernetes_lifecycle::scaling_blocks_ready(&cluster.status_message) {
            let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
            let pending = rows
                .iter()
                .any(|row| row.pending_join || row.pending_removal);
            if !pending {
                return Ok(());
            }
        }

        let mut drained = Vec::new();
        let mut refusal: Option<String> = None;
        for _ in 0..16 {
            let members = self.observe_members(cluster_name).await?;
            let step = kubernetes_lifecycle::next_scale_step(&members, &drained);
            match step {
                ScaleStep::Ready => {
                    if let Some(message) = refusal {
                        self.db
                            .set_kubernetes_cluster_status(cluster_name, "failed", &message)?;
                    } else {
                        let control_planes = members
                            .iter()
                            .filter(|member| member.role != "worker")
                            .count() as i32;
                        self.db
                            .mark_kubernetes_cluster_ready(cluster_name, control_planes)?;
                    }
                    return Ok(());
                }
                ScaleStep::Wait { message, .. } => {
                    self.db
                        .set_kubernetes_cluster_status(cluster_name, "scaling", &message)?;
                    return Ok(());
                }
                ScaleStep::WaitReady { .. } => {
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        "scaling",
                        "waiting for the node to become Ready",
                    )?;
                    return Ok(());
                }
                ScaleStep::Refuse { node_id, message } => {
                    self.clear_pending_removal(cluster_name, &node_id)?;
                    refusal = Some(message);
                }
                ScaleStep::AddEtcd { node_id } => {
                    self.etcd_member_add(cluster_name, &node_id).await?;
                }
                ScaleStep::Configure { node_id } => {
                    if self.member_role(cluster_name, &node_id)? == "control-plane" {
                        self.refresh_serving_control_planes(cluster_name, &node_id)
                            .await?;
                    }
                    self.configure_joining_node(cluster_name, &node_id).await?;
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        "scaling",
                        "waiting for the node to become Ready",
                    )?;
                    return Ok(());
                }
                ScaleStep::MarkJoined {
                    node_id,
                    registered_name,
                } => {
                    self.mark_member_joined(cluster_name, &node_id, &registered_name)?;
                    if let Err(err) = self.label_workers(cluster_name).await {
                        warn!(cluster = cluster_name, error = %err, "worker role labels not applied");
                    }
                }
                ScaleStep::CordonDrain { node_id } => {
                    self.drain_member(cluster_name, &node_id).await?;
                    drained.push(node_id);
                }
                ScaleStep::RemoveEtcd { node_id } => {
                    if self.member_role(cluster_name, &node_id)? == "control-plane" {
                        if let Err(e) = self.take_snapshot(cluster_name, "pre-scale-down").await {
                            warn!(
                                cluster = cluster_name,
                                node = %node_id,
                                error = %format!("{e:#}"),
                                "pre-scale-down snapshot skipped"
                            );
                        }
                    }
                    self.etcd_member_remove(cluster_name, &node_id).await?;
                }
                ScaleStep::DeleteApiNode { node_id } => {
                    self.delete_api_member(cluster_name, &node_id).await?;
                }
                ScaleStep::DeleteMembership { node_id } => {
                    let role = self.member_role(cluster_name, &node_id)?;
                    self.clear_member_nix(cluster_name, &node_id).await?;
                    self.db
                        .delete_kubernetes_cluster_node(cluster_name, &node_id)?;
                    if role == "control-plane" {
                        self.refresh_serving_control_planes(cluster_name, &node_id)
                            .await?;
                    }
                }
            }
        }
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            "scaling",
            "cluster scaling in progress",
        )?;
        Ok(())
    }

    /// Advance a rolling version upgrade until a step has to wait.
    async fn reconcile_upgrade(&self, cluster_name: &str) -> Result<()> {
        let start = self.observe_upgrade(cluster_name).await?;
        self.record_upgrade_start(cluster_name, &start).await;
        for _ in 0..16 {
            let view = self.observe_upgrade(cluster_name).await?;
            let desired = kubernetes_versions::effective(&view.desired);
            let (done, total) = kubernetes_upgrade::progress(&view);
            let step = kubernetes_upgrade::next_upgrade_step(&view);
            info!(
                cluster = cluster_name,
                ?step,
                done,
                total,
                "kubernetes upgrade step"
            );
            match step {
                UpgradeStep::Done => {
                    let control_planes = view
                        .members
                        .iter()
                        .filter(|member| member.role != "worker")
                        .count() as i32;
                    self.db
                        .mark_kubernetes_cluster_ready(cluster_name, control_planes)?;
                    self.db.set_kubernetes_cluster_status(
                        cluster_name,
                        "ready",
                        &format!("Kubernetes {desired} on all {total} nodes"),
                    )?;
                    return Ok(());
                }
                UpgradeStep::Wait { message, .. } => {
                    self.set_upgrade_status(cluster_name, &desired, done, total, &message)?;
                    return Ok(());
                }
                UpgradeStep::Drain { node_id } => {
                    self.set_upgrade_status(
                        cluster_name,
                        &desired,
                        done,
                        total,
                        &format!("draining {node_id}"),
                    )?;
                    let mut row = self.upgrade_row(cluster_name, &node_id)?;
                    // Recorded first so a crash mid-drain still uncordons later.
                    row.upgrade_cordoned = true;
                    self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                    if let Err(err) = self.drain_member(cluster_name, &node_id).await {
                        self.fail_upgrade(
                            cluster_name,
                            &node_id,
                            &format!("drain of {node_id} failed: {err:#}"),
                        )
                        .await?;
                        return Ok(());
                    }
                    row.upgrade_state = kubernetes_upgrade::STATE_DRAINED.to_string();
                    self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                }
                UpgradeStep::Apply { node_id } => {
                    let previous = self.upgrade_row(cluster_name, &node_id)?;
                    let mut row = previous.clone();
                    row.kubernetes_version = view.desired.clone();
                    row.upgrade_state = kubernetes_upgrade::STATE_APPLYING.to_string();
                    row.upgrade_apply_id = String::new();
                    self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                    match self.push_member_config(cluster_name, &node_id).await {
                        Ok(apply_id) => {
                            // An agent without apply tracking returns no id;
                            // the kubelet version alone then shows progress.
                            row.upgrade_apply_id = if apply_id.is_empty() {
                                "untracked".to_string()
                            } else {
                                apply_id
                            };
                            self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                        }
                        Err(err) => {
                            self.db
                                .set_kubernetes_node_upgrade(cluster_name, &previous)?;
                            return Err(
                                err.context(format!("push Kubernetes {desired} to {node_id}"))
                            );
                        }
                    }
                    self.set_upgrade_status(
                        cluster_name,
                        &desired,
                        done,
                        total,
                        &format!("rebuilding {node_id} with Kubernetes {desired}"),
                    )?;
                    return Ok(());
                }
                UpgradeStep::Uncordon { node_id } => {
                    self.uncordon_member(cluster_name, &node_id).await?;
                    let mut row = self.upgrade_row(cluster_name, &node_id)?;
                    row.upgrade_cordoned = false;
                    self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                }
                UpgradeStep::Finish { node_id } => {
                    let mut row = self.upgrade_row(cluster_name, &node_id)?;
                    row.upgrade_state = kubernetes_upgrade::STATE_IDLE.to_string();
                    row.upgrade_apply_id = String::new();
                    self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
                    info!(cluster = cluster_name, node_id, version = %desired, "node upgraded");
                }
                UpgradeStep::Fail { node_id, message } => {
                    self.fail_upgrade(cluster_name, &node_id, &message).await?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn set_upgrade_status(
        &self,
        cluster_name: &str,
        desired: &str,
        done: usize,
        total: usize,
        message: &str,
    ) -> Result<()> {
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            "upgrading",
            &format!("upgrading to Kubernetes {desired} ({done}/{total} nodes): {message}"),
        )?;
        Ok(())
    }

    /// Stop the rollout on `node_id`. The node is uncordoned and stays on
    /// whatever its last rebuild activated; re-applying the manifest resumes.
    async fn fail_upgrade(&self, cluster_name: &str, node_id: &str, message: &str) -> Result<()> {
        error!(
            cluster = cluster_name,
            node_id, message, "kubernetes upgrade stopped"
        );
        let mut row = self.upgrade_row(cluster_name, node_id)?;
        if row.upgrade_cordoned {
            if let Err(err) = self.uncordon_member(cluster_name, node_id).await {
                error!(cluster = cluster_name, node_id, error = %err, "uncordon after a failed upgrade");
            } else {
                row.upgrade_cordoned = false;
            }
        }
        row.upgrade_state = kubernetes_upgrade::STATE_FAILED.to_string();
        row.upgrade_apply_id = String::new();
        self.db.set_kubernetes_node_upgrade(cluster_name, &row)?;
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            "degraded",
            &format!(
                "{}: {message}. Fix the node and re-apply the manifest to resume, \
                 or run `kctl kubernetes upgrade rollback` to return to the previous version.",
                kubernetes_upgrade::UPGRADE_FAILED
            ),
        )?;
        Ok(())
    }

    fn upgrade_row(&self, cluster_name: &str, node_id: &str) -> Result<KubernetesNodeUpgradeRow> {
        self.db
            .list_kubernetes_node_upgrades(cluster_name)?
            .into_iter()
            .find(|row| row.node_id == node_id)
            .with_context(|| format!("{node_id} is not a member of {cluster_name}"))
    }

    async fn uncordon_member(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let (address, _) = self.api_peer(cluster_name, None).await?;
        let name = self
            .registered_name(cluster_name, node_id, &address)
            .await?;
        kubernetes_nodes::cordon(self.nodes.as_ref(), &address, &name, false)
            .await
            .map_err(|err| anyhow!(err))
    }

    /// Render and push one member's Nix; returns the node's apply id.
    async fn push_member_config(&self, cluster_name: &str, node_id: &str) -> Result<String> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found")?;
        let nodes = self.get_cluster_nodes(cluster_name).await?;
        let node = nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .context("member not found")?;
        self.configure_node(
            cluster_name,
            node,
            &pki,
            &cluster.cluster_cidr,
            &cluster.service_cidr,
            &nodes,
        )
        .await
    }

    async fn apply_progress(&self, address: &str, apply_id: &str) -> (ApplyProgress, String) {
        self.nodes.apply_status(address, apply_id).await
    }

    async fn observe_upgrade(&self, cluster_name: &str) -> Result<UpgradeView> {
        let cfg = self
            .db
            .get_kubernetes_cluster_config(cluster_name)?
            .context("cluster config not found")?;
        let membership = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let api_ready = self.apiserver_is_ready(cluster_name).await?;
        let api_nodes = self
            .list_api_nodes(cluster_name, None)
            .await
            .unwrap_or_default();
        let mut members = Vec::new();
        for row in self.db.list_kubernetes_node_upgrades(cluster_name)? {
            if row.pending_join || row.pending_removal {
                continue;
            }
            let member = membership
                .iter()
                .find(|m| m.node_id == row.node_id)
                .context("membership row not found")?;
            let node = self.db.get_node(&row.node_id)?;
            let hostname = node
                .as_ref()
                .map(|n| n.hostname.clone())
                .unwrap_or_default();
            let address = node.as_ref().map(|n| n.address.clone()).unwrap_or_default();
            let ip = self.member_ip(member).unwrap_or_default();
            let listed = kubernetes_lifecycle::match_registered_node(
                &api_nodes,
                &member.k8s_node_name,
                &hostname,
                &ip,
            );
            let etcd_healthy = row.role != "worker"
                && self
                    .probe_etcd_member(cluster_name, &ip)
                    .await
                    .unwrap_or(false);
            let (apply, apply_message) = if row.upgrade_state != kubernetes_upgrade::STATE_APPLYING
            {
                (ApplyProgress::Unknown, String::new())
            } else if row.upgrade_apply_id.is_empty() {
                (ApplyProgress::NotRecorded, String::new())
            } else if address.is_empty() {
                (ApplyProgress::Unknown, String::new())
            } else {
                self.apply_progress(&address, &row.upgrade_apply_id).await
            };
            members.push(UpgradeMember {
                node_id: row.node_id,
                role: row.role,
                version: row.kubernetes_version,
                state: row.upgrade_state,
                cordoned: row.upgrade_cordoned,
                apply,
                apply_message,
                kubelet_version: listed
                    .map(|n| n.kubelet_version.clone())
                    .unwrap_or_default(),
                node_ready: listed.is_some_and(|n| n.ready),
                etcd_healthy,
            });
        }
        Ok(UpgradeView {
            desired: kubernetes_versions::normalize(&cfg.kubernetes_version),
            api_ready,
            members,
        })
    }

    async fn observe_members(&self, cluster_name: &str) -> Result<Vec<MemberSnapshot>> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let needs_etcd = rows
            .iter()
            .any(|row| row.role != "worker" && (row.pending_join || row.pending_removal));
        let etcd_members = match self.etcd_member_list(cluster_name).await {
            Ok(members) => members,
            Err(err) if needs_etcd => return Err(err),
            Err(_) => Vec::new(),
        };
        let api_nodes = if rows
            .iter()
            .any(|row| row.pending_join || row.pending_removal)
        {
            self.list_api_nodes(cluster_name, None).await?
        } else {
            Vec::new()
        };
        let mut members = Vec::new();
        for row in rows {
            let kcore = self.db.get_node(&row.node_id)?;
            let (exists, ready, hostname) = match &kcore {
                Some(node) => (
                    true,
                    node.status == "ready" && node.approval_status == "approved",
                    node.hostname.clone(),
                ),
                None => (false, false, String::new()),
            };
            let ip = if row.advertise_address.is_empty() {
                kcore
                    .as_ref()
                    .map(|node| node.address.split(':').next().unwrap_or("").to_string())
                    .unwrap_or_default()
            } else {
                row.advertise_address.clone()
            };
            let in_etcd = kubernetes_lifecycle::member_for_ip(&etcd_members, &ip).is_some();
            let etcd_healthy = if row.role == "worker" {
                false
            } else {
                self.probe_etcd_member(cluster_name, &ip)
                    .await
                    .unwrap_or(false)
            };
            let matched = kubernetes_lifecycle::match_registered_node(
                &api_nodes,
                &row.k8s_node_name,
                &hostname,
                &ip,
            );
            members.push(MemberSnapshot {
                node_id: row.node_id,
                role: row.role,
                pending_join: row.pending_join,
                pending_removal: row.pending_removal,
                kubelet_ready: row.kubelet_ready,
                kcore_exists: exists,
                kcore_ready: ready,
                in_etcd,
                etcd_healthy,
                in_api: matched.is_some(),
                api_ready: matched.is_some_and(|node| node.ready),
                registered_name: matched
                    .map(|node| node.name.clone())
                    .unwrap_or_else(|| row.k8s_node_name.clone()),
            });
        }
        Ok(members)
    }

    async fn configure_joining_node(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        let mut pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found for existing cluster")?;
        let row = self
            .member_row(cluster_name, node_id)?
            .context("membership row not found")?;
        let node = self.db.get_node(node_id)?.context("node not found")?;
        let ip = if row.advertise_address.is_empty() {
            node.address.split(':').next().unwrap_or("").to_string()
        } else {
            row.advertise_address.clone()
        };
        let hostname = if node.hostname.is_empty() {
            node_id.to_string()
        } else {
            node.hostname.clone()
        };
        let kubelet = kubernetes_pki::issue_node_certificates(
            &mut pki,
            cluster_name,
            &cluster.api_server_endpoint,
            &NodeInfo {
                node_id: node_id.to_string(),
                hostname: hostname.clone(),
                ip_address: ip.clone(),
            },
        )?;
        self.db.store_kubernetes_pki(cluster_name, &pki)?;
        let member = ClusterNode {
            node_id: node_id.to_string(),
            hostname,
            ip_address: ip,
            iface: row.iface,
            initial_cluster: false,
            role: row.role,
            kubelet_kubeconfig: kubelet,
        };
        let apply_id = self
            .configure_node(
                cluster_name,
                &member,
                &pki,
                &cluster.cluster_cidr,
                &cluster.service_cidr,
                &[],
            )
            .await?;
        // The next pass sees the node outside the API until kubelet registers.
        // Returning before the rebuild ends would start another one.
        self.wait_for_apply(node_id, &format!("{}:9091", member.ip_address), &apply_id)
            .await
    }

    fn mark_member_joined(
        &self,
        cluster_name: &str,
        node_id: &str,
        registered_name: &str,
    ) -> Result<()> {
        let mut row = self
            .member_row(cluster_name, node_id)?
            .context("membership row not found")?;
        row.pending_join = false;
        row.kubelet_ready = true;
        row.k8s_node_name = registered_name.to_string();
        self.db.upsert_kubernetes_cluster_node(&row)?;
        Ok(())
    }

    fn clear_pending_removal(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let Some(mut row) = self.member_row(cluster_name, node_id)? else {
            return Ok(());
        };
        row.pending_removal = false;
        self.db.upsert_kubernetes_cluster_node(&row)?;
        Ok(())
    }

    fn member_row(
        &self,
        cluster_name: &str,
        node_id: &str,
    ) -> Result<Option<KubernetesClusterNodeRow>> {
        Ok(self
            .db
            .list_kubernetes_cluster_nodes(cluster_name)?
            .into_iter()
            .find(|row| row.node_id == node_id))
    }

    async fn drain_member(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let (address, _single) = self.api_peer(cluster_name, Some(node_id)).await?;
        let name = self
            .registered_name(cluster_name, node_id, &address)
            .await?;
        kubernetes_nodes::drain_node(self.nodes.as_ref(), &address, &name)
            .await
            .map_err(|err| anyhow!(err))
    }

    async fn label_workers(&self, cluster_name: &str) -> Result<()> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let workers: Vec<_> = rows
            .iter()
            .filter(|row| row.role == "worker" && !row.pending_removal)
            .collect();
        if workers.is_empty() {
            return Ok(());
        }
        let (address, _) = self.api_peer(cluster_name, None).await?;
        for row in workers {
            let name = self
                .registered_name(cluster_name, &row.node_id, &address)
                .await?;
            kubernetes_nodes::label_worker(self.nodes.as_ref(), &address, &name)
                .await
                .map_err(|err| anyhow!(err))?;
        }
        Ok(())
    }

    async fn delete_api_member(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let (address, _) = self.api_peer(cluster_name, Some(node_id)).await?;
        let name = self
            .registered_name(cluster_name, node_id, &address)
            .await?;
        kubernetes_nodes::delete_node(self.nodes.as_ref(), &address, &name)
            .await
            .map_err(|err| anyhow!(err))?;
        Ok(())
    }

    async fn registered_name(
        &self,
        cluster_name: &str,
        node_id: &str,
        peer: &str,
    ) -> Result<String> {
        let row = self
            .member_row(cluster_name, node_id)?
            .context("membership row not found")?;
        let node = self.db.get_node(node_id)?.context("node not found")?;
        let nodes = self.list_api_nodes(cluster_name, Some(peer)).await?;
        let ip = if row.advertise_address.is_empty() {
            node.address.split(':').next().unwrap_or("").to_string()
        } else {
            row.advertise_address.clone()
        };
        let name =
            kubernetes_nodes::registered_node_name(&nodes, &row.k8s_node_name, &node.hostname, &ip)
                .map_err(|err| anyhow!(err))?;
        if row.k8s_node_name != name {
            let mut stored = row;
            stored.k8s_node_name = name.clone();
            self.db.upsert_kubernetes_cluster_node(&stored)?;
        }
        Ok(name)
    }

    async fn list_api_nodes(
        &self,
        cluster_name: &str,
        peer: Option<&str>,
    ) -> Result<Vec<kubernetes_lifecycle::ListedNode>> {
        let address = if let Some(peer) = peer {
            peer.to_string()
        } else {
            self.api_peer(cluster_name, None).await?.0
        };
        kubernetes_nodes::list_nodes(self.nodes.as_ref(), &address)
            .await
            .map_err(|err| anyhow!(err))
    }

    /// Agent address of a control-plane node, and whether it is the only one.
    /// `avoid` is the node being drained or deleted. The only control plane is
    /// used when there is no other API.
    async fn api_peer(&self, cluster_name: &str, avoid: Option<&str>) -> Result<(String, bool)> {
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let control_planes: Vec<_> = rows
            .iter()
            .filter(|row| row.role != "worker")
            .cloned()
            .collect();
        let single = control_planes.len() <= 1;
        let chosen = control_planes
            .iter()
            .find(|row| avoid != Some(row.node_id.as_str()))
            .or_else(|| control_planes.first())
            .context("cluster has no control-plane node")?
            .clone();
        let node = self
            .db
            .get_node(&chosen.node_id)?
            .context("control-plane node not found")?;
        if node.address.is_empty() {
            anyhow::bail!("control-plane {} has no agent address", chosen.node_id);
        }
        Ok((node.address, single))
    }

    /// Push a node's configuration without `cluster_name`: its other clusters
    /// when it has any, otherwise Kubernetes switched off.
    async fn clear_member_nix(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let node = self.db.get_node(node_id)?.context("node not found")?;
        // A render error must not fall back to switching Kubernetes off on a
        // node that still serves another cluster.
        let nix = if self.node_in_other_cluster(cluster_name, node_id)? {
            let pki = self
                .db
                .load_kubernetes_pki(cluster_name)?
                .context("PKI not found")?;
            self.render_node_nix(node_id, cluster_name, &pki, "")
                .await?
                .nix
        } else {
            KUBERNETES_DISABLED_NIX.to_string()
        };
        self.push_nix(&node.address, &nix, "").await
    }

    fn node_in_other_cluster(&self, cluster_name: &str, node_id: &str) -> Result<bool> {
        for cluster in self.db.list_kubernetes_clusters()? {
            if cluster.name == cluster_name || cluster.phase == PHASE_DELETING {
                continue;
            }
            if self
                .db
                .list_kubernetes_cluster_nodes(&cluster.name)?
                .iter()
                .any(|row| row.node_id == node_id && !row.pending_removal)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Take a cluster off every member node before its records are dropped.
    /// The cluster is marked deleting first, so renders for any node leave it
    /// out. Returns the nodes that did not take the new configuration.
    pub async fn release_members(&self, cluster_name: &str) -> Result<Vec<(String, String)>> {
        self.db.set_kubernetes_cluster_status(
            cluster_name,
            PHASE_DELETING,
            "removing Kubernetes from the member nodes",
        )?;
        let mut failed = Vec::new();
        for row in self.db.list_kubernetes_cluster_nodes(cluster_name)? {
            if let Err(err) = self.clear_member_nix(cluster_name, &row.node_id).await {
                warn!(cluster = cluster_name, node_id = %row.node_id, error = %format!("{err:#}"), "could not remove Kubernetes from node");
                failed.push((row.node_id, format!("{err:#}")));
            }
        }
        Ok(failed)
    }

    async fn push_nix(&self, address: &str, nix: &str, kubeconfig: &str) -> Result<()> {
        self.nodes
            .apply_config(address, nix, kubeconfig, &[])
            .await
            .map(|_| ())
            .map_err(|e| anyhow!(e))
    }

    async fn etcd_member_list(
        &self,
        cluster_name: &str,
    ) -> Result<Vec<kubernetes_lifecycle::EtcdMember>> {
        let body = self
            .etcd_json(cluster_name, "/v3/cluster/member/list", "{}")
            .await?;
        Ok(kubernetes_lifecycle::etcd_members_from_list(&body))
    }

    async fn etcd_member_add(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let row = self
            .member_row(cluster_name, node_id)?
            .context("membership row not found")?;
        let ip = self.member_ip(&row)?;
        let existing = self.etcd_member_list(cluster_name).await?;
        if kubernetes_lifecycle::member_for_ip(&existing, &ip).is_some() {
            return Ok(());
        }
        let port = self.etcd_peer_port(cluster_name)?;
        let body = format!(r#"{{"peerURLs":["https://{ip}:{port}"]}}"#);
        match self
            .etcd_json(cluster_name, "/v3/cluster/member/add", &body)
            .await
        {
            Ok(_) => Ok(()),
            Err(err) if err.to_string().to_ascii_lowercase().contains("exist") => Ok(()),
            Err(err) => Err(err),
        }
    }

    async fn etcd_member_remove(&self, cluster_name: &str, node_id: &str) -> Result<()> {
        let members = self.observe_members(cluster_name).await?;
        if let Some(message) = kubernetes_lifecycle::etcd_removal_refusal(&members, node_id) {
            anyhow::bail!(message);
        }
        let row = self
            .member_row(cluster_name, node_id)?
            .context("membership row not found")?;
        let ip = self.member_ip(&row)?;
        let listed = self.etcd_member_list(cluster_name).await?;
        let Some(member) = kubernetes_lifecycle::member_for_ip(&listed, &ip) else {
            return Ok(());
        };
        let body = format!(r#"{{"ID":"{}"}}"#, member.id);
        self.etcd_json(cluster_name, "/v3/cluster/member/remove", &body)
            .await?;
        Ok(())
    }

    fn member_ip(&self, row: &KubernetesClusterNodeRow) -> Result<String> {
        if !row.advertise_address.is_empty() {
            return Ok(row.advertise_address.clone());
        }
        let node = self.db.get_node(&row.node_id)?.context("node not found")?;
        Ok(node.address.split(':').next().unwrap_or("").to_string())
    }

    fn etcd_peer_port(&self, cluster_name: &str) -> Result<u16> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        Ok(crate::nixgen::kubernetes_listen_ports(&cluster.api_server_endpoint).etcd_peer)
    }

    async fn probe_etcd_member(&self, cluster_name: &str, ip: &str) -> Result<bool> {
        if ip.is_empty() {
            return Ok(false);
        }
        let Some(pki) = self.db.load_kubernetes_pki(cluster_name)? else {
            return Ok(false);
        };
        let port = self.etcd_client_port(cluster_name)?;
        self.nodes
            .etcd_health(
                ip,
                port,
                &kubernetes_pki::etcd_ca_trust_bundle(&pki),
                &pki.apiserver_etcd_client_cert,
                &pki.apiserver_etcd_client_key,
            )
            .await
            .map_err(|e| anyhow!(e))
    }

    fn etcd_client_port(&self, cluster_name: &str) -> Result<u16> {
        let cluster = self
            .db
            .get_kubernetes_cluster(cluster_name)?
            .context("cluster not found")?;
        Ok(crate::nixgen::kubernetes_listen_ports(&cluster.api_server_endpoint).etcd_client)
    }

    async fn etcd_json(&self, cluster_name: &str, path: &str, body: &str) -> Result<String> {
        let pki = self
            .db
            .load_kubernetes_pki(cluster_name)?
            .context("PKI not found")?;
        let port = self.etcd_client_port(cluster_name)?;
        let rows = self.db.list_kubernetes_cluster_nodes(cluster_name)?;
        let mut last_error = anyhow!("no control-plane answered etcd");
        for row in rows.iter().filter(|row| row.role != "worker") {
            let ip = match self.member_ip(row) {
                Ok(ip) if !ip.is_empty() => ip,
                _ => continue,
            };
            match self
                .nodes
                .etcd_post(
                    &ip,
                    port,
                    path,
                    body,
                    &kubernetes_pki::etcd_ca_trust_bundle(&pki),
                    &pki.apiserver_etcd_client_cert,
                    &pki.apiserver_etcd_client_key,
                )
                .await
            {
                Ok((status, response)) if (200..300).contains(&status) => return Ok(response),
                Ok((status, response)) => {
                    last_error = anyhow!("etcd {path} on {ip} returned {status}: {response}");
                }
                Err(err) => last_error = anyhow!(err),
            }
        }
        Err(last_error)
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

/// Private keys and API-server secret configs delivered outside `/nix/store`.
fn pki_secret_files(
    cluster: &str,
    node_id: &str,
    pki: &KubernetesPki,
    encryption: Option<&crate::kubernetes_hardening::EncryptionKeys>,
) -> Vec<SecretFile> {
    let root = crate::nixgen::kubernetes_pki_key_dir(cluster);
    let mut out = vec![
        SecretFile {
            path: format!("{root}/ca.key"),
            mode: 0o600,
            content: pki.ca_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/sa.key"),
            mode: 0o600,
            content: pki.sa_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/etcd/ca.key"),
            mode: 0o600,
            content: pki.etcd_ca_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/apiserver.key"),
            mode: 0o600,
            content: pki.apiserver_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/apiserver-kubelet-client.key"),
            mode: 0o600,
            content: pki.apiserver_kubelet_client_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/apiserver-etcd-client.key"),
            mode: 0o600,
            content: pki.apiserver_etcd_client_key.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/encryption-config.yaml"),
            mode: 0o600,
            content: encryption_provider_config(encryption, pki).into_bytes(),
        },
        SecretFile {
            path: format!("{root}/audit-policy.yaml"),
            mode: 0o600,
            content: DEFAULT_AUDIT_POLICY.as_bytes().to_vec(),
        },
        SecretFile {
            path: format!("{root}/pod-security-admission.yaml"),
            mode: 0o600,
            content: DEFAULT_POD_SECURITY_ADMISSION.as_bytes().to_vec(),
        },
    ];
    if let Some((_, key)) = pki.etcd_node_certs.get(node_id) {
        out.push(SecretFile {
            path: format!("{root}/etcd/server.key"),
            mode: 0o600,
            content: key.as_bytes().to_vec(),
        });
    }
    if let Some((_, key)) = pki.etcd_node_peer_certs.get(node_id) {
        out.push(SecretFile {
            path: format!("{root}/etcd/peer.key"),
            mode: 0o600,
            content: key.as_bytes().to_vec(),
        });
    }
    out
}

/// Prefer stored encryption keys; otherwise derive a bootstrap key from the CA.
fn encryption_provider_config(
    encryption: Option<&crate::kubernetes_hardening::EncryptionKeys>,
    pki: &KubernetesPki,
) -> String {
    if let Some(keys) = encryption {
        return keys.provider_config_yaml();
    }
    use aws_lc_rs::digest::{digest, SHA256};
    let digest = digest(&SHA256, pki.ca_key.as_bytes());
    let key_b64 =
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest.as_ref());
    format!(
        r#"apiVersion: apiserver.config.k8s.io/v1
kind: EncryptionConfiguration
resources:
  - resources:
      - secrets
    providers:
      - secretbox:
          keys:
            - name: key1
              secret: {key_b64}
      - identity: {{}}
"#
    )
}

const DEFAULT_AUDIT_POLICY: &str = r#"apiVersion: audit.k8s.io/v1
kind: Policy
omitStages:
  - RequestReceived
rules:
  - level: None
    resources:
      - group: ""
        resources: ["secrets", "configmaps"]
  - level: Metadata
    omitStages:
      - RequestReceived
"#;

const DEFAULT_POD_SECURITY_ADMISSION: &str = r#"apiVersion: apiserver.config.k8s.io/v1
kind: AdmissionConfiguration
plugins:
  - name: PodSecurity
    configuration:
      apiVersion: pod-security.admission.config.k8s.io/v1
      kind: PodSecurityConfiguration
      defaults:
        enforce: "baseline"
        enforce-version: "latest"
        warn: "restricted"
        warn-version: "latest"
        audit: "restricted"
        audit-version: "latest"
      exemptions:
        usernames: []
        runtimeClasses: []
        namespaces: ["kube-system", "ceph-csi"]
"#;

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
    apiserver_ready_at(&endpoint, &host, ca_pem).await
}

async fn apiserver_ready_at(addr: &str, server_name: &str, ca_pem: &str) -> Result<bool> {
    if server_name.is_empty() {
        return Ok(false);
    }
    let (status, body) = tls_get(addr, server_name, "/readyz", ca_pem, None).await?;
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

async fn tls_post(
    addr: &str,
    server_name: &str,
    path: &str,
    body: &str,
    ca_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> Result<(u16, String)> {
    use rustls::pki_types::ServerName;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(ca_pem)? {
        roots.add(cert).context("etcd CA")?;
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(pem_certs(client_cert_pem)?, pem_key(client_key_pem)?)
        .context("etcd client certificate")?;
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
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {server_name}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes())
        .await
        .context("write etcd request")?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut buf))
        .await
        .context("read timed out")?
        .context("read etcd response")?;
    let text = String::from_utf8_lossy(&buf);
    let (head, response) = text.split_once("\r\n\r\n").unwrap_or((text.as_ref(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Ok((status, response.to_string()))
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

/// Shared mode is applied on the cluster. LVM mode creates a thin pool and
/// does not install a provisioner.
/// A cluster without workers has nowhere else to run pods, so its control
/// planes stay untainted whatever the spec says.
fn control_planes_take_workloads(
    schedule_on_control_plane: bool,
    membership: &[crate::db::KubernetesClusterNodeRow],
) -> bool {
    schedule_on_control_plane
        || !membership
            .iter()
            .any(|row| row.role == "worker" && !row.pending_removal)
}

fn applied_provisioner_manifest(json: &str, api_server_endpoint: &str) -> String {
    let provisioner = crate::kubernetes_local::decode(json)
        .unwrap_or_else(|_| crate::kubernetes_local::LocalProvisioner::disabled());
    if !provisioner.renders_manifests() {
        String::new()
    } else {
        crate::kubernetes_local::manifests_at(&provisioner, api_server_endpoint)
    }
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
    /// "control-plane" or "worker". Empty is treated as control-plane.
    pub role: String,
    /// Kubelet client kubeconfig. Empty keeps the admin kubeconfig for kubelet.
    pub kubelet_kubeconfig: String,
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
                // A ready cluster keeps the nix the operator last applied.
                // Rebuilding it on every controller start would restart kubelet
                // on nodes that are already serving.
                if matches!(
                    cluster.phase.as_str(),
                    "bootstrapping"
                        | "ready"
                        | "scaling"
                        | "upgrading"
                        | PHASE_DELETING
                        | PHASE_RECONFIGURING
                ) {
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
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
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
                            let _ = orchestrator.db.set_kubernetes_cluster_status(
                                &cluster_name,
                                BootstrapPhase::Failed.as_str(),
                                &format!("bootstrap failed: {e:#}"),
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list bootstrapping Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase("ready") {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        orchestrator.reconcile_ready(&cluster_name).await;
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list ready Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase("scaling") {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator.reconcile_scaling(&cluster_name).await {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes scale failed"
                            );
                            let _ = orchestrator.db.set_kubernetes_cluster_status(
                                &cluster_name,
                                "scaling",
                                &format!("node scale error: {e:#}"),
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list scaling Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase(PHASE_RECONFIGURING) {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator.reconcile_reconfigure(&cluster_name).await {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes configuration rollout stopped"
                            );
                            let _ = orchestrator.db.set_kubernetes_cluster_status(
                                &cluster_name,
                                "degraded",
                                &format!("{}: {e:#}", kubernetes_lifecycle::ROLLOUT_STOPPED),
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list reconfiguring Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase("upgrading") {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator.reconcile_upgrade(&cluster_name).await {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes upgrade step failed"
                            );
                            let _ = orchestrator.db.set_kubernetes_cluster_status(
                                &cluster_name,
                                "upgrading",
                                &format!("upgrade step error, retrying: {e:#}"),
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list upgrading Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase(PHASE_RESTORING) {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator.reconcile_restore(&cluster_name).await {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes etcd restore step failed"
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list restoring Kubernetes clusters");
            }
        }

        match db.list_kubernetes_clusters_by_phase(PHASE_ROLLING_BACK) {
            Ok(clusters) => {
                for cluster in clusters {
                    if !orchestrator.claim(&cluster.name) {
                        continue;
                    }
                    let orchestrator = orchestrator.clone();
                    let cluster_name = cluster.name.clone();
                    tokio::spawn(async move {
                        if let Err(e) = orchestrator.reconcile_rollback(&cluster_name).await {
                            error!(
                                cluster = cluster_name,
                                error = %e,
                                "Kubernetes rollback step failed"
                            );
                            let _ = orchestrator.db.set_kubernetes_cluster_status(
                                &cluster_name,
                                PHASE_ROLLING_BACK,
                                &format!("rollback step error, retrying: {e:#}"),
                            );
                        }
                        orchestrator.release(&cluster_name);
                    });
                }
            }
            Err(e) => {
                error!(error = %e, "failed to list rolling-back Kubernetes clusters");
            }
        }

        // Check every 30 seconds
        sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(test)]
mod taint_tests {
    use super::control_planes_take_workloads;
    use crate::db::KubernetesClusterNodeRow;

    fn member(role: &str, pending_removal: bool) -> KubernetesClusterNodeRow {
        KubernetesClusterNodeRow {
            cluster_name: "c".into(),
            node_id: format!("{role}-{pending_removal}"),
            role: role.into(),
            advertise_address: String::new(),
            iface: String::new(),
            initial_cluster: false,
            join_existing: false,
            kubelet_ready: true,
            pending_join: false,
            pending_removal,
            k8s_node_name: String::new(),
        }
    }

    #[test]
    fn control_planes_are_tainted_only_when_workers_exist() {
        let cp = member("control-plane", false);
        let worker = member("worker", false);
        let leaving = member("worker", true);

        assert!(!control_planes_take_workloads(
            false,
            &[cp.clone(), worker.clone()]
        ));
        assert!(control_planes_take_workloads(true, &[cp.clone(), worker]));
        assert!(control_planes_take_workloads(
            false,
            std::slice::from_ref(&cp)
        ));
        assert!(control_planes_take_workloads(false, &[cp, leaving]));
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

#[cfg(test)]
mod release_tests {
    use super::{KubernetesOrchestrator, PHASE_DELETING};
    use crate::db::{Database, KubernetesClusterNodeRow, KubernetesClusterRow, NodeRow};
    use crate::node_client::NodeClients;

    fn node(db: &Database, id: &str) {
        db.upsert_node(&NodeRow {
            id: id.to_string(),
            hostname: id.to_string(),
            // Port 1 on loopback refuses at once.
            address: "127.0.0.1:1".to_string(),
            cpu_cores: 4,
            memory_bytes: 8_000_000_000,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "br0".to_string(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: "approved".to_string(),
            cert_expiry_days: 300,
            luks_method: String::new(),
            dc_id: "DC1".to_string(),
        })
        .unwrap();
    }

    fn cluster(db: &Database, name: &str, members: &[&str]) {
        db.upsert_kubernetes_cluster(&KubernetesClusterRow {
            name: name.to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "10.0.0.1:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 1,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        for id in members {
            db.upsert_kubernetes_cluster_node(&KubernetesClusterNodeRow {
                cluster_name: name.to_string(),
                node_id: id.to_string(),
                role: "control-plane".to_string(),
                advertise_address: "10.0.0.1".to_string(),
                iface: "br0".to_string(),
                initial_cluster: true,
                join_existing: false,
                kubelet_ready: true,
                pending_join: false,
                pending_removal: false,
                k8s_node_name: String::new(),
            })
            .unwrap();
        }
    }

    fn setup() -> (Database, KubernetesOrchestrator) {
        let db = Database::open(":memory:").unwrap();
        node(&db, "shared");
        node(&db, "only-a");
        cluster(&db, "a", &["shared", "only-a"]);
        cluster(&db, "b", &["shared"]);
        let orchestrator = KubernetesOrchestrator::new(db.clone(), NodeClients::new(None));
        (db, orchestrator)
    }

    #[test]
    fn a_node_keeps_kubernetes_only_for_clusters_that_are_not_being_deleted() {
        let (db, orchestrator) = setup();
        assert!(orchestrator.node_in_other_cluster("a", "shared").unwrap());
        assert!(!orchestrator.node_in_other_cluster("a", "only-a").unwrap());

        db.set_kubernetes_cluster_status("b", PHASE_DELETING, "")
            .unwrap();
        assert!(!orchestrator.node_in_other_cluster("a", "shared").unwrap());
    }

    #[tokio::test]
    async fn releasing_marks_the_cluster_deleting_and_reports_unreachable_nodes() {
        let (db, orchestrator) = setup();
        let failed = orchestrator.release_members("a").await.unwrap();

        let mut nodes: Vec<_> = failed.iter().map(|(node, _)| node.as_str()).collect();
        nodes.sort_unstable();
        assert_eq!(nodes, ["only-a", "shared"]);
        assert_eq!(
            db.get_kubernetes_cluster("a").unwrap().unwrap().phase,
            PHASE_DELETING
        );
        assert_eq!(
            db.get_kubernetes_cluster("b").unwrap().unwrap().phase,
            "ready"
        );
    }
}

#[cfg(test)]
mod fake_nodes_tests {
    use super::*;
    use crate::kubernetes_lifecycle::ListedNode;
    use crate::kubernetes_nodes::Fake;

    #[tokio::test]
    async fn fake_nodes_report_api_and_etcd_health() {
        let db = Database::open(":memory:").unwrap();
        let fake = Fake::new(&["10.0.0.1"]);
        fake.with_api_nodes(vec![ListedNode {
            name: "cp-1".into(),
            internal_ips: vec!["10.0.0.1".into()],
            ready: true,
            kubelet_version: "v1.36.6".into(),
        }]);
        let orch = KubernetesOrchestrator::with_nodes(db, NodeClients::new(None), fake.clone());
        assert!(orch
            .nodes
            .apiserver_ready("10.0.0.1:6443", "10.0.0.1", "ca")
            .await
            .unwrap());
        assert!(orch
            .nodes
            .etcd_health("10.0.0.1", 2379, "ca", "cert", "key")
            .await
            .unwrap());
        let listed = kubernetes_nodes::list_nodes(orch.nodes.as_ref(), "10.0.0.1:9091")
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "cp-1");
    }

    #[tokio::test]
    async fn fake_apply_records_pki_secrets_when_pushed() {
        let fake = Fake::new(&["10.0.0.1"]);
        let secrets = vec![SecretFile {
            path: "/var/lib/kcore/kubernetes/demo/pki/ca.key".into(),
            mode: 0o600,
            content: b"key".to_vec(),
        }];
        let id = fake
            .apply_config(
                "10.0.0.1:9091",
                "{ kcore.kubernetes.enable = true; }",
                "",
                &secrets,
            )
            .await
            .unwrap();
        assert!(!id.is_empty());
        assert_eq!(fake.secrets.lock().unwrap().len(), 1);
    }
}
