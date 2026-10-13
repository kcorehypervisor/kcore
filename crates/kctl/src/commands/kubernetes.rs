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
    #[serde(default)]
    volumes: Vec<ManifestHostVolume>,
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
    let local_provisioner = if local.enabled || !local.volumes.is_empty() {
        Some(controller_proto::KubernetesLocalProvisioner {
            enabled: local.enabled,
            storage_class_name: local.storage_class_name.clone(),
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
                })
                .collect(),
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
    }
}

pub async fn apply_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let manifest = parse_manifest(file)?;
    let mut client = client::controller_client(info).await?;

    // Plan
    let plan = client
        .plan_kubernetes_cluster(controller_proto::PlanKubernetesClusterRequest {
            name: manifest.metadata.name.clone(),
            spec: Some(to_proto_spec(&manifest.spec)),
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

    println!("\n📝 Proceed? (y/n)");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("Aborted");
        return Ok(());
    }

    let spec = Some(to_proto_spec(&manifest.spec));
    let updating = plan.action == "update";
    let result = if updating {
        client
            .update_kubernetes_cluster(controller_proto::UpdateKubernetesClusterRequest {
                name: manifest.metadata.name.clone(),
                spec,
                confirm: true,
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

            if let Some(local) = spec.local_provisioner.filter(|local| local.enabled) {
                println!("\nLocal provisioner:");
                println!("  StorageClass: {}", local.storage_class_name);
                for volume in &local.volumes {
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

        if let Some(status) = cluster.status {
            println!("\nStatus:");
            println!("  Phase: {}", status.phase);
            println!("  Message: {}", status.message);
            println!("  Ready Control Planes: {}", status.ready_control_planes);
            println!("  Ready Workers: {}", status.ready_workers);
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
        let worker_total = cluster
            .spec
            .as_ref()
            .map(|s| s.worker_nodes.len() as i32)
            .unwrap_or(0);

        println!(
            "{:<20} {:<15} {:<10} {}/{:<5} {}/{}",
            cluster.name, phase, api, cp, cp_total, workers, worker_total
        );
    }

    Ok(())
}

pub async fn delete(info: &ConnectionInfo, name: &str, force: bool) -> Result<()> {
    println!("⚠️  Delete Kubernetes cluster '{}'?", name);
    if !force {
        println!("Type cluster name to confirm:");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if input.trim() != name {
            println!("Aborted");
            return Ok(());
        }
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
        let local = spec.local_provisioner.expect("local provisioner");
        assert!(local.enabled);
        assert_eq!(local.storage_class_name, "local-path");
        assert_eq!(local.volumes.len(), 1);
        assert_eq!(local.volumes[0].name, "kcore-kdata");
        assert_eq!(local.volumes[0].node_id, "node-105");
        assert_eq!(local.volumes[0].storage_class, "lvm");
        assert_eq!(local.volumes[0].size_bytes, 53_687_091_200);

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
