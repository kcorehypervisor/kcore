//! `kctl` subcommands for Kubernetes cluster management.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KubernetesClusterManifest {
    kind: String,
    metadata: ManifestMetadata,
    spec: ManifestSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestSpec {
    cluster_cidr: String,
    service_cidr: String,
    #[serde(default = "default_cluster_dns")]
    cluster_dns: String,
    api_server_endpoint: String,
    #[serde(default = "default_cni")]
    cni: String,
    #[serde(default)]
    cilium: ManifestCilium,
    control_plane_nodes: Vec<ManifestNode>,
    #[serde(default)]
    worker_nodes: Vec<ManifestNode>,
    #[serde(default)]
    high_availability: ManifestHA,
    #[serde(default)]
    local_provisioner: ManifestLocalProvisioner,
    /// Masters accept pods. Dedicated workerNodes may be empty.
    #[serde(default)]
    schedule_on_control_plane: bool,
    /// Desired Kubernetes version. Empty uses the host nixpkgs pin. Changing
    /// it and re-applying upgrades the cluster one node at a time.
    #[serde(default)]
    version: String,
    #[serde(default)]
    storage: ManifestStorage,
    #[serde(default)]
    backup: Option<serde_json::Value>,
    #[serde(default)]
    repair: Option<serde_json::Value>,
    #[serde(default = "default_auto_reconcile")]
    auto_reconcile: bool,
    #[serde(default)]
    oidc: Option<ManifestOidc>,
    #[serde(default)]
    network_policy: ManifestNetworkPolicy,
    #[serde(default)]
    cert: Option<ManifestCertPolicy>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestOidc {
    #[serde(default)]
    issuer_url: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    username_claim: String,
    #[serde(default)]
    groups_claim: String,
    #[serde(default)]
    ca_pem: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestNetworkPolicy {
    #[serde(default)]
    default_deny_label: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestCertPolicy {
    #[serde(default)]
    leaf_validity_days: i32,
    #[serde(default)]
    renew_before_days: i32,
}

fn default_auto_reconcile() -> bool {
    true
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestStorage {
    #[serde(default)]
    ceph: Option<ManifestCephStorage>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestCephStorage {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    cluster: String,
    #[serde(default)]
    pool: String,
    #[serde(default)]
    monitors: Vec<String>,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    user_key: String,
    #[serde(default)]
    default: bool,
}

fn default_cluster_dns() -> String {
    "10.96.0.10".to_string()
}

fn default_cni() -> String {
    "cilium".to_string()
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestCilium {
    #[serde(default = "default_cilium_mode")]
    mode: String,
    #[serde(default = "default_mtu")]
    mtu: i32,
    #[serde(default = "default_tunnel")]
    tunnel: String,
    #[serde(default = "default_auto_direct_routes")]
    auto_direct_node_routes: bool,
    #[serde(default)]
    load_balancer_ip_pools: Vec<String>,
}

fn default_cilium_mode() -> String {
    "native-routing".to_string()
}

fn default_mtu() -> i32 {
    1500
}

fn default_tunnel() -> String {
    "disabled".to_string()
}

fn default_auto_direct_routes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestNode {
    node_id: String,
    advertise_address: String,
    #[serde(default = "default_iface")]
    iface: String,
    #[serde(default)]
    initial_cluster: bool,
}

fn default_iface() -> String {
    "enp6s0".to_string()
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestHA {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    vip_address: String,
    #[serde(default)]
    vip_interface: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ManifestLocalProvisioner {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    storage_class_name: String,
    /// "shared" (directories on one formatted volume) or "lvm" (thin pool, no provisioner).
    #[serde(default)]
    mode: String,
    #[serde(default)]
    volumes: Vec<ManifestHostVolume>,
    /// CephCluster name for the RBD CSI StorageClass (optional).
    #[serde(default)]
    ceph_cluster: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestHostVolume {
    name: String,
    node_id: String,
    #[serde(default)]
    storage_class: String,
    #[serde(default)]
    size_bytes: i64,
    #[serde(default)]
    fs_type: String,
    #[serde(default)]
    mount_path: String,
    /// "filesystem" or "pool". Empty follows the provisioner mode.
    #[serde(default)]
    role: String,
    #[serde(default)]
    volume_group: String,
}

fn parse_manifest(file: &str) -> Result<KubernetesClusterManifest> {
    let data = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    parse_manifest_str(&data).with_context(|| format!("parsing YAML in {file}"))
}

fn parse_manifest_str(data: &str) -> Result<KubernetesClusterManifest> {
    let manifest: KubernetesClusterManifest =
        serde_yaml::from_str(data).context("parsing KubernetesCluster YAML")?;
    let kind = manifest.kind.trim().to_ascii_lowercase();
    if kind != "kubernetescluster" && kind != "kubernetes-cluster" && kind != "kubernetes_cluster" {
        bail!("expected kind: KubernetesCluster, got {}", manifest.kind);
    }
    Ok(manifest)
}

fn to_proto_spec(spec: &ManifestSpec) -> controller_proto::KubernetesClusterSpec {
    let cilium_config = Some(controller_proto::CiliumConfig {
        mode: spec.cilium.mode.clone(),
        mtu: spec.cilium.mtu,
        tunnel: spec.cilium.tunnel.clone(),
        auto_direct_node_routes: spec.cilium.auto_direct_node_routes,
        load_balancer_ip_pools: spec.cilium.load_balancer_ip_pools.clone(),
    });

    let ha_config = if spec.high_availability.enabled {
        Some(controller_proto::HighAvailabilityConfig {
            enabled: true,
            vip_address: spec.high_availability.vip_address.clone(),
            vip_interface: spec.high_availability.vip_interface.clone(),
            method: "keepalived".to_string(),
        })
    } else {
        None
    };

    let local = &spec.local_provisioner;
    let local_provisioner =
        if local.enabled || !local.volumes.is_empty() || !local.ceph_cluster.trim().is_empty() {
            Some(controller_proto::KubernetesLocalProvisioner {
                enabled: local.enabled,
                storage_class_name: local.storage_class_name.clone(),
                mode: local.mode.clone(),
                volumes: local
                    .volumes
                    .iter()
                    .map(|volume| controller_proto::KubernetesHostVolume {
                        name: volume.name.clone(),
                        node_id: volume.node_id.clone(),
                        storage_class: volume.storage_class.clone(),
                        size_bytes: volume.size_bytes,
                        fs_type: volume.fs_type.clone(),
                        mount_path: volume.mount_path.clone(),
                        role: volume.role.clone(),
                        volume_group: volume.volume_group.clone(),
                    })
                    .collect(),
                ceph_cluster: local.ceph_cluster.clone(),
            })
        } else {
            None
        };

    controller_proto::KubernetesClusterSpec {
        cluster_cidr: spec.cluster_cidr.clone(),
        service_cidr: spec.service_cidr.clone(),
        cluster_dns: spec.cluster_dns.clone(),
        api_server_endpoint: spec.api_server_endpoint.clone(),
        cni: spec.cni.clone(),
        cilium_config,
        control_plane_nodes: spec
            .control_plane_nodes
            .iter()
            .map(|n| controller_proto::KubernetesNodeSpec {
                node_id: n.node_id.clone(),
                role: "control-plane".to_string(),
                advertise_address: n.advertise_address.clone(),
                iface: n.iface.clone(),
                initial_cluster: n.initial_cluster,
                join_existing: !n.initial_cluster,
            })
            .collect(),
        worker_nodes: spec
            .worker_nodes
            .iter()
            .map(|n| controller_proto::KubernetesNodeSpec {
                node_id: n.node_id.clone(),
                role: "worker".to_string(),
                advertise_address: n.advertise_address.clone(),
                iface: n.iface.clone(),
                initial_cluster: false,
                join_existing: true,
            })
            .collect(),
        high_availability: ha_config,
        local_provisioner,
        schedule_on_control_plane: spec.schedule_on_control_plane,
        version: spec.version.clone(),
        storage_ceph: spec.storage.ceph.as_ref().map(|ceph| {
            controller_proto::KubernetesCephStorage {
                enabled: ceph.enabled,
                cluster: ceph.cluster.clone(),
                pool: ceph.pool.clone(),
                monitors: ceph.monitors.clone(),
                user_id: ceph.user_id.clone(),
                user_key: ceph.user_key.clone(),
                default_storage_class: ceph.default,
            }
        }),
        backup_json: spec
            .backup
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok())
            .unwrap_or_default(),
        repair_json: spec
            .repair
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok())
            .unwrap_or_default(),
        auto_reconcile: spec.auto_reconcile,
        oidc: spec
            .oidc
            .as_ref()
            .map(|o| controller_proto::KubernetesOidc {
                issuer_url: o.issuer_url.clone(),
                client_id: o.client_id.clone(),
                username_claim: o.username_claim.clone(),
                groups_claim: o.groups_claim.clone(),
                ca_pem: o.ca_pem.clone(),
            }),
        network_policy_default_deny_label: spec.network_policy.default_deny_label.clone(),
        cert: spec
            .cert
            .as_ref()
            .map(|c| controller_proto::KubernetesCertPolicy {
                leaf_validity_days: c.leaf_validity_days,
                renew_before_days: c.renew_before_days,
            }),
    }
}

fn worker_slot_count(spec: &controller_proto::KubernetesClusterSpec) -> i32 {
    let dedicated = spec.worker_nodes.len() as i32;
    if spec.schedule_on_control_plane {
        dedicated + spec.control_plane_nodes.len() as i32
    } else {
        dedicated
    }
}

/// An empty stdin is a script that forgot `--yes`, not a "no".
fn read_answer(prompt: &str) -> Result<String> {
    println!("{prompt}");
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input)? == 0 {
        anyhow::bail!("no answer on stdin; pass --yes to run without a prompt");
    }
    Ok(input.trim().to_string())
}

pub async fn apply_from_file(
    info: &ConnectionInfo,
    file: &str,
    force: bool,
    yes: bool,
) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;

    // Plan
    let plan = client
        .plan_kubernetes_cluster(controller_proto::PlanKubernetesClusterRequest {
            name: manifest.metadata.name.clone(),
            spec: Some(to_proto_spec(&manifest.spec)),
            force,
        })
        .await
        .context("plan_kubernetes_cluster rpc")?
        .into_inner();

    println!("=== KubernetesCluster plan: {} ===", manifest.metadata.name);
    println!();
    println!("Action: {}", plan.action);

    if !plan.blocking_issues.is_empty() {
        println!("\n❌ Blocking issues:");
        for issue in &plan.blocking_issues {
            println!("  - {}", issue);
        }
        bail!(
            "cannot proceed: {} blocking issue(s)",
            plan.blocking_issues.len()
        );
    }

    if !plan.warnings.is_empty() {
        println!("\n⚠️  Warnings:");
        for warning in &plan.warnings {
            println!("  - {}", warning);
        }
    }

    println!("\n✅ Node preflight checks:");
    for node_plan in &plan.node_plans {
        let status = if node_plan.preflight_passed {
            "✓"
        } else {
            "✗"
        };
        println!(
            "  {} {} ({})",
            status, node_plan.node_id, node_plan.nix_action
        );
        for issue in &node_plan.issues {
            println!("      - {}", issue);
        }
    }

    if plan.rendered_nix_preview.contains("kind: StorageClass") {
        println!("\nLocal-path manifest:\n{}", plan.rendered_nix_preview);
    }

    if !yes && !read_answer("\n📝 Proceed? (y/n)")?.eq_ignore_ascii_case("y") {
        println!("Aborted");
        return Ok(());
    }

    let spec = Some(to_proto_spec(&manifest.spec));
    let updating = plan.action == "update" || plan.action == "upgrade";
    let result = if updating {
        client
            .update_kubernetes_cluster(controller_proto::UpdateKubernetesClusterRequest {
                name: manifest.metadata.name.clone(),
                spec,
                confirm: true,
                force,
            })
            .await
            .context("update_kubernetes_cluster rpc")?
            .into_inner()
            .cluster
    } else {
        client
            .create_kubernetes_cluster(controller_proto::CreateKubernetesClusterRequest {
                name: manifest.metadata.name.clone(),
                spec,
                confirm: true,
            })
            .await
            .context("create_kubernetes_cluster rpc")?
            .into_inner()
            .cluster
    };

    if let Some(cluster) = result {
        let verb = if updating { "updated" } else { "created" };
        println!("\n✅ Kubernetes cluster {verb}: {}", cluster.name);
        if let Some(status) = cluster.status {
            println!("   Phase: {}", status.phase);
            println!("   Message: {}", status.message);
        }
    }

    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .get_kubernetes_cluster(controller_proto::GetKubernetesClusterRequest {
            name: name.to_string(),
        })
        .await
        .context("get_kubernetes_cluster rpc")?
        .into_inner();

    if let Some(cluster) = response.cluster {
        println!("Name: {}", cluster.name);
        println!("Generation: {}", cluster.generation);

        if let Some(spec) = cluster.spec {
            println!("\nSpec:");
            println!("  Cluster CIDR: {}", spec.cluster_cidr);
            println!("  Service CIDR: {}", spec.service_cidr);
            println!("  API Endpoint: {}", spec.api_server_endpoint);
            println!("  CNI: {}", spec.cni);
            if spec.version.is_empty() {
                println!("  Version: host pin");
            } else {
                println!("  Version: {}", spec.version);
            }

            if let Some(ha) = spec.high_availability {
                if ha.enabled {
                    println!("\nHigh Availability:");
                    println!("  VIP: {}", ha.vip_address);
                    println!("  VIP Interface: {}", ha.vip_interface);
                }
            }

            println!("\nControl Plane Nodes:");
            for node in &spec.control_plane_nodes {
                let init = if node.initial_cluster {
                    " [initial]"
                } else {
                    ""
                };
                println!(
                    "  - {}{}: {} ({})",
                    node.node_id, init, node.advertise_address, node.iface
                );
            }

            if !spec.worker_nodes.is_empty() {
                println!("\nWorker Nodes:");
                for node in &spec.worker_nodes {
                    println!(
                        "  - {}: {} ({})",
                        node.node_id, node.advertise_address, node.iface
                    );
                }
            }

            if spec.schedule_on_control_plane {
                println!("\nScheduling: control-plane nodes accept workloads");
            }

            if let Some(local) = spec.local_provisioner.filter(|local| local.enabled) {
                println!("\nLocal provisioner:");
                let mode = if local.mode.is_empty() {
                    "shared"
                } else {
                    local.mode.as_str()
                };
                println!("  Mode: {mode}");
                println!("  StorageClass: {}", local.storage_class_name);
                for volume in &local.volumes {
                    if volume.role == "pool" {
                        println!(
                            "  - thin pool {} on {} ({}/{}, {} bytes, claims formatted {})",
                            volume.name,
                            volume.node_id,
                            volume.volume_group,
                            volume.name,
                            volume.size_bytes,
                            volume.fs_type
                        );
                    } else {
                        println!(
                            "  - {} on {} ({} {} bytes, {} at {})",
                            volume.name,
                            volume.node_id,
                            volume.storage_class,
                            volume.size_bytes,
                            volume.fs_type,
                            volume.mount_path
                        );
                    }
                }
            }
        }

        if let Some(status) = cluster.status {
            println!("\nStatus:");
            println!("  Phase: {}", status.phase);
            println!("  Message: {}", status.message);
            println!("  Ready Control Planes: {}", status.ready_control_planes);
            println!("  Ready Workers: {}", status.ready_workers);
            if !status.node_versions.is_empty() {
                println!("  Node Versions:");
                let mut versions: Vec<_> = status.node_versions.iter().collect();
                versions.sort();
                for (node_id, version) in versions {
                    println!("    {node_id}: {version}");
                }
            }
        }
    }

    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .list_kubernetes_clusters(controller_proto::ListKubernetesClustersRequest {})
        .await
        .context("list_kubernetes_clusters rpc")?
        .into_inner();

    if response.clusters.is_empty() {
        println!("No Kubernetes clusters found");
        return Ok(());
    }

    println!(
        "{:<20} {:<15} {:<10} {:<5}/{:<5}",
        "NAME", "PHASE", "API ENDPOINT", "CP", "WORKERS"
    );
    println!("{}", "-".repeat(70));

    for cluster in response.clusters {
        let api = if let Some(spec) = &cluster.spec {
            &spec.api_server_endpoint
        } else {
            ""
        };

        let (phase, cp, workers) = if let Some(status) = &cluster.status {
            (
                status.phase.as_str(),
                status.ready_control_planes,
                status.ready_workers,
            )
        } else {
            ("unknown", 0, 0)
        };

        let cp_total = cluster
            .spec
            .as_ref()
            .map(|s| s.control_plane_nodes.len() as i32)
            .unwrap_or(0);
        let worker_total = cluster.spec.as_ref().map(worker_slot_count).unwrap_or(0);

        println!(
            "{:<20} {:<15} {:<10} {}/{:<5} {}/{}",
            cluster.name, phase, api, cp, cp_total, workers, worker_total
        );
    }

    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str, force: bool, yes: bool) -> Result<()> {
    println!("⚠️  Delete Kubernetes cluster '{}'?", name);
    if !force && !yes && read_answer("Type cluster name to confirm:")? != name {
        println!("Aborted");
        return Ok(());
    }

    let mut client = client::controller_client(info).await?;
    let _response = client
        .delete_kubernetes_cluster(controller_proto::DeleteKubernetesClusterRequest {
            name: name.to_string(),
            confirm: true,
            force,
        })
        .await
        .context("delete_kubernetes_cluster rpc")?
        .into_inner();

    println!("✅ Kubernetes cluster '{}' deleted", name);
    Ok(())
}

pub async fn health(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .get_kubernetes_cluster_health(controller_proto::GetKubernetesClusterHealthRequest {
            name: name.to_string(),
        })
        .await
        .context("get_kubernetes_cluster_health rpc")?
        .into_inner();

    println!("=== Kubernetes Cluster Health: {} ===\n", name);
    println!("Overall Health: {}", response.overall_health);
    println!(
        "Control Planes: {}/{}",
        response.ready_control_planes, response.total_control_planes
    );
    println!(
        "Workers: {}/{}",
        response.ready_workers, response.total_workers
    );
    println!(
        "etcd: {} ({} members)",
        if response.etcd_healthy {
            "healthy"
        } else {
            "unhealthy"
        },
        response.etcd_member_count
    );
    println!(
        "API Server: {}",
        if response.api_server_reachable {
            "reachable"
        } else {
            "unreachable"
        }
    );

    if !response.node_health.is_empty() {
        println!("\nNode Health:");
        for node in &response.node_health {
            println!("  {}: {}", node.node_id, node.kubernetes_status);
            println!(
                "    kubelet: {}",
                if node.kubelet_active {
                    "active"
                } else {
                    "inactive"
                }
            );
            if node.etcd_active {
                println!("    etcd: active");
            }
            if node.api_server_active {
                println!("    api-server: active");
            }
        }
    }

    Ok(())
}

pub async fn scale(
    info: &ConnectionInfo,
    name: &str,
    add_control_plane: Vec<String>,
    add_workers: Vec<String>,
    remove_workers: Vec<String>,
) -> Result<()> {
    let mut client = client::controller_client(info).await?;

    let mut add_nodes = Vec::new();

    // Add control plane nodes
    for node_id in add_control_plane {
        let parts: Vec<&str> = node_id.split(':').collect();
        if parts.len() != 2 {
            bail!("control-plane format must be node_id:address (e.g., node-108:192.168.40.108)");
        }
        add_nodes.push(controller_proto::KubernetesNodeSpec {
            node_id: parts[0].to_string(),
            role: "control-plane".to_string(),
            advertise_address: parts[1].to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: false,
            join_existing: true,
        });
    }

    // Add worker nodes
    for node_id in add_workers {
        let parts: Vec<&str> = node_id.split(':').collect();
        if parts.len() != 2 {
            bail!("worker format must be node_id:address (e.g., node-110:192.168.40.110)");
        }
        add_nodes.push(controller_proto::KubernetesNodeSpec {
            node_id: parts[0].to_string(),
            role: "worker".to_string(),
            advertise_address: parts[1].to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: false,
            join_existing: true,
        });
    }

    let _response = client
        .scale_kubernetes_cluster(controller_proto::ScaleKubernetesClusterRequest {
            name: name.to_string(),
            add_nodes,
            remove_node_ids: remove_workers,
            confirm: true,
        })
        .await
        .context("scale_kubernetes_cluster rpc")?
        .into_inner();

    println!("✅ Kubernetes cluster '{}' scaled", name);
    Ok(())
}

fn require_yes(yes: bool, what: &str) -> Result<()> {
    if !yes {
        bail!("{what}; re-run with --yes to go ahead");
    }
    Ok(())
}

pub async fn certs(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .get_kubernetes_certificates(controller_proto::GetKubernetesCertificatesRequest {
            name: name.to_string(),
        })
        .await
        .context("get_kubernetes_certificates rpc")?
        .into_inner();

    if response.leaf_not_after.is_empty() {
        println!("Cluster '{name}' has no recorded certificate expiry yet");
    } else {
        println!("Leaf certificates for '{name}'");
        println!("  expire:     {}", response.leaf_not_after);
        println!("  remaining:  {:.1} days", response.days_remaining);
        println!(
            "  renewal:    {} (starts {} days before expiry)",
            if response.renewal_due {
                "due"
            } else {
                "not due"
            },
            response.renew_before_days
        );
    }
    if !response.ca_not_after.is_empty() {
        println!("Cluster CA");
        println!(
            "  expire:     {}{}",
            response.ca_not_after,
            if response.previous_ca_present {
                " (retiring CA)"
            } else {
                ""
            }
        );
        println!("  remaining:  {:.1} days", response.ca_days_remaining);
        println!("  rotation:   {}", response.ca_rotation_phase);
        if !response.ca_roll_pending.is_empty() {
            println!("  rolling:    {} step", response.ca_roll_pending);
        }
    }
    Ok(())
}

/// One step of `kctl kubernetes rotate-ca`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CaStep {
    Begin,
    Reissue,
    Retire,
}

pub async fn rotate_ca(
    info: &ConnectionInfo,
    name: &str,
    step: CaStep,
    include_etcd: bool,
    skip_backup: bool,
    yes: bool,
) -> Result<()> {
    use controller_proto::KubernetesCaRotationStep as Proto;
    let (proto_step, what) = match step {
        CaStep::Begin => (
            Proto::Begin,
            "BEGIN generates a new cluster CA and rebuilds every node onto old+new trust",
        ),
        CaStep::Reissue => (
            Proto::Reissue,
            "REISSUE signs every leaf certificate with the new CA and rebuilds every node",
        ),
        CaStep::Retire => (
            Proto::Retire,
            "RETIRE removes the old CA from trust and rebuilds every node",
        ),
    };
    require_yes(yes, &format!("{what} of '{name}'"))?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .rotate_kubernetes_ca(controller_proto::RotateKubernetesCaRequest {
            name: name.to_string(),
            step: proto_step as i32,
            confirm: true,
            include_etcd,
            skip_backup,
        })
        .await
        .context("rotate_kubernetes_ca rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn rotate_certs(info: &ConnectionInfo, name: &str, force: bool, yes: bool) -> Result<()> {
    require_yes(
        yes,
        &format!("rotating certificates rebuilds every node of '{name}' one at a time"),
    )?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .rotate_kubernetes_certificates(controller_proto::RotateKubernetesCertificatesRequest {
            name: name.to_string(),
            force,
            confirm: true,
        })
        .await
        .context("rotate_kubernetes_certificates rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn rotate_encryption(info: &ConnectionInfo, name: &str, yes: bool) -> Result<()> {
    require_yes(
        yes,
        &format!("rotating encryption keys restarts API servers of '{name}' and rewrites Secrets"),
    )?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .rotate_kubernetes_encryption(controller_proto::RotateKubernetesEncryptionRequest {
            name: name.to_string(),
            confirm: true,
        })
        .await
        .context("rotate_kubernetes_encryption rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

fn print_snapshot(snapshot: &controller_proto::KubernetesEtcdSnapshot) {
    println!(
        "{}  {}  {} bytes  {}  sha256={}",
        snapshot.id, snapshot.created_at, snapshot.size_bytes, snapshot.trigger, snapshot.sha256
    );
}

pub async fn backup_create(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut client = client::controller_client(info).await?;
    let response = client
        .create_kubernetes_backup(controller_proto::CreateKubernetesBackupRequest {
            name: name.to_string(),
            confirm: true,
        })
        .await
        .context("create_kubernetes_backup rpc")?
        .into_inner();
    match response.snapshot {
        Some(snapshot) => {
            println!("Snapshot taken");
            print_snapshot(&snapshot);
            println!("  path: {}", snapshot.path);
        }
        None => println!("Snapshot taken"),
    }
    Ok(())
}

async fn list_snapshots(
    info: &ConnectionInfo,
    name: &str,
) -> Result<Vec<controller_proto::KubernetesEtcdSnapshot>> {
    let mut client = client::controller_client(info).await?;
    Ok(client
        .list_kubernetes_backups(controller_proto::ListKubernetesBackupsRequest {
            name: name.to_string(),
        })
        .await
        .context("list_kubernetes_backups rpc")?
        .into_inner()
        .snapshots)
}

pub async fn backup_list(info: &ConnectionInfo, name: &str) -> Result<()> {
    let snapshots = list_snapshots(info, name).await?;
    if snapshots.is_empty() {
        println!("No snapshots for '{name}'");
        return Ok(());
    }
    for snapshot in &snapshots {
        print_snapshot(snapshot);
    }
    Ok(())
}

pub async fn backup_delete(
    info: &ConnectionInfo,
    name: &str,
    snapshot: &str,
    yes: bool,
) -> Result<()> {
    require_yes(
        yes,
        &format!("deleting snapshot {snapshot} cannot be undone"),
    )?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .delete_kubernetes_backup(controller_proto::DeleteKubernetesBackupRequest {
            name: name.to_string(),
            snapshot_id: snapshot.to_string(),
            confirm: true,
        })
        .await
        .context("delete_kubernetes_backup rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn restore(info: &ConnectionInfo, name: &str, snapshot: &str, yes: bool) -> Result<()> {
    let snapshots = list_snapshots(info, name).await?;
    let Some(found) = snapshots.iter().find(|s| s.id == snapshot) else {
        bail!("snapshot {snapshot} not found for cluster '{name}'; see `kctl kubernetes backup list {name}`");
    };
    require_yes(
        yes,
        &format!("restoring {} replaces all etcd data of '{name}'", found.id),
    )?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .restore_kubernetes_backup(controller_proto::RestoreKubernetesBackupRequest {
            name: name.to_string(),
            snapshot_id: found.id.clone(),
            confirm: true,
        })
        .await
        .context("restore_kubernetes_backup rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn upgrade_rollback(info: &ConnectionInfo, name: &str, yes: bool) -> Result<()> {
    require_yes(
        yes,
        &format!("rollback moves '{name}' back to its previous Kubernetes version, node by node"),
    )?;
    let mut client = client::controller_client(info).await?;
    let response = client
        .rollback_kubernetes_upgrade(controller_proto::RollbackKubernetesUpgradeRequest {
            name: name.to_string(),
            confirm: true,
        })
        .await
        .context("rollback_kubernetes_upgrade rpc")?
        .into_inner();
    println!("{}", response.message);
    Ok(())
}

pub async fn replace_node(
    info: &ConnectionInfo,
    name: &str,
    old: &str,
    new: &str,
    yes: bool,
) -> Result<()> {
    require_yes(
        yes,
        &format!("replacing {old} with {new} removes {old} from '{name}'"),
    )?;
    let mut client = client::controller_client(info).await?;
    client
        .replace_kubernetes_node(controller_proto::ReplaceKubernetesNodeRequest {
            name: name.to_string(),
            old_node_id: old.to_string(),
            new_node_id: new.to_string(),
            confirm: true,
        })
        .await
        .context("replace_kubernetes_node rpc")?;
    println!("Replacing {old} with {new} in '{name}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_declares_local_lvm_volumes() {
        let manifest = parse_manifest_str(
            r#"
kind: KubernetesCluster
metadata:
  name: lab
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  apiServerEndpoint: 192.168.40.220:6443
  controlPlaneNodes:
    - nodeId: node-107
      advertiseAddress: 192.168.40.107
  scheduleOnControlPlane: true
  localProvisioner:
    enabled: true
    storageClassName: local-path
    volumes:
      - name: kcore-kdata
        nodeId: node-105
        storageClass: lvm
        sizeBytes: 53687091200
"#,
        )
        .unwrap();
        let spec = to_proto_spec(&manifest.spec);
        assert!(spec.schedule_on_control_plane);
        assert_eq!(worker_slot_count(&spec), 1);
        let local = spec.local_provisioner.expect("local provisioner");
        assert!(local.enabled);
        assert_eq!(local.storage_class_name, "local-path");
        assert_eq!(local.volumes.len(), 1);
        assert_eq!(local.volumes[0].name, "kcore-kdata");
        assert_eq!(local.volumes[0].node_id, "node-105");
        assert_eq!(local.volumes[0].storage_class, "lvm");
        assert_eq!(local.volumes[0].size_bytes, 53_687_091_200);
        assert!(local.mode.is_empty());

        let lvm = parse_manifest_str(
            r#"
kind: KubernetesCluster
metadata:
  name: lab
spec:
  clusterCidr: 10.245.0.0/16
  serviceCidr: 10.112.0.0/12
  apiServerEndpoint: 192.168.40.201:7443
  controlPlaneNodes:
    - nodeId: node-105
      advertiseAddress: 192.168.40.105
  localProvisioner:
    enabled: true
    mode: lvm
    storageClassName: kcore-lvm
    volumes:
      - name: kcore-kdata
        nodeId: node-105
        storageClass: lvm
        sizeBytes: 53687091200
        role: pool
        volumeGroup: vg_kcore
"#,
        )
        .unwrap();
        let lvm_spec = to_proto_spec(&lvm.spec);
        let lvm_local = lvm_spec.local_provisioner.expect("lvm provisioner");
        assert_eq!(lvm_local.mode, "lvm");
        assert_eq!(lvm_local.volumes[0].role, "pool");
        assert_eq!(lvm_local.volumes[0].volume_group, "vg_kcore");

        let plain = parse_manifest_str(
            r#"
kind: KubernetesCluster
metadata:
  name: lab
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  apiServerEndpoint: 192.168.40.220:6443
  controlPlaneNodes:
    - nodeId: node-107
      advertiseAddress: 192.168.40.107
"#,
        )
        .unwrap();
        assert!(to_proto_spec(&plain.spec).local_provisioner.is_none());
    }
}

#[cfg(test)]
mod shipped_example_tests {
    use super::*;

    #[test]
    fn shipped_example_parses() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/kubernetes/prod.yaml"
        );
        parse_manifest(path).unwrap();
    }
}
