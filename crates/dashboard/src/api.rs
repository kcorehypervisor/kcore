//! Leptos server functions — call controller gRPC using [`crate::state::dashboard_config`].

use leptos::prelude::*;

use crate::controller_client;
use crate::dto::{
    AuditEventDto, CephClusterDetailDto, CephClusterRowDto, CertificateRowDto, ClusterHealthDto,
    ComplianceDto, ContainerDetailDto, ContainerRowDto, CrlSummaryDto, CryptoConfigDto,
    DbmsDetailDto, DbmsRowDto, DiskLayoutDetailDto, DiskLayoutRowDto, KubernetesClusterDetailDto,
    KubernetesClusterRowDto, NetworkOverviewDto, NetworkRowDto, NodeDetailDto, NodeRowDto,
    ObjectStoreDetailDto, ObjectStoreRowDto, OperationDetailDto, PkiStatusDto,
    ReplicationConflictDto, ReplicationStatusDto, SbomSummaryDto, SharedFilesystemDetailDto,
    SharedFilesystemRowDto, SnapshotPolicyDto, SnapshotRowDto, SshKeyDetailDto, SshKeyRowDto,
    StorageClassDetailDto, StorageClassDto, StorageOverviewDto, VmDetailDto, VmOperationRowDto,
    VmsPageDto, VolumeDetailDto, VolumeRowDto,
};
use crate::mappers::{
    audit_events_from_proto, ceph_cluster_detail_from_proto, ceph_clusters_from_proto,
    certificates_from_proto, cluster_health_from_proto, compliance_from_proto,
    conflicts_from_proto, container_detail_from_proto, containers_from_proto,
    crl_summary_from_proto, crypto_config_from_proto, dbms_detail_from_postgresql,
    dbms_from_postgresqls, disk_layout_detail_from_proto, disk_layouts_from_proto,
    filter_audit_events, filter_vm_operations, inventory_volume_detail,
    kubernetes_clusters_from_proto, kubernetes_detail_from_proto, merge_ceph_capacity,
    network_overview_from_proto, networks_from_proto, node_detail_from_proto, nodes_from_proto,
    object_store_detail_from_proto, object_stores_from_proto, operation_detail_from_proto,
    pki_status_from_proto, replication_status_from_proto, sbom_summary_from_proto,
    shared_filesystem_detail_from_proto, shared_filesystems_from_proto,
    snapshot_policies_from_proto, snapshot_policy_from_proto, snapshots_from_proto,
    ssh_key_detail_from_proto, ssh_keys_from_proto, storage_class_detail,
    storage_classes_from_nodes, storage_overview_from_proto, vm_detail_from_proto,
    vm_operations_from_proto, vms_page_from_proto, volume_detail_from_info,
    volumes_with_host_inventory,
};
use crate::state::dashboard_config;

fn map_err(e: anyhow::Error) -> ServerFnError {
    ServerFnError::new(format!("{e:#}"))
}

#[server(GetComplianceReport, "/api")]
pub async fn get_compliance_dto() -> Result<ComplianceDto, ServerFnError> {
    let cfg = dashboard_config();
    let r = controller_client::get_compliance(cfg)
        .await
        .map_err(map_err)?;
    Ok(compliance_from_proto(r))
}

#[server(ListVmsPage, "/api")]
pub async fn list_vms_page(page: u32, page_size: u32) -> Result<VmsPageDto, ServerFnError> {
    let cfg = dashboard_config();
    let vms = controller_client::list_vms(cfg).await.map_err(map_err)?;
    Ok(vms_page_from_proto(
        vms,
        page,
        crate::format::normalize_page_size(page_size),
    ))
}

#[server(ListNetworks, "/api")]
pub async fn list_networks_dto() -> Result<Vec<NetworkRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let nets = controller_client::list_networks(cfg)
        .await
        .map_err(map_err)?;
    Ok(networks_from_proto(nets))
}

#[server(GetNetworkOverview, "/api")]
pub async fn get_network_overview_dto() -> Result<NetworkOverviewDto, ServerFnError> {
    let cfg = dashboard_config();
    let overview = controller_client::get_network_overview(cfg)
        .await
        .map_err(map_err)?;
    Ok(network_overview_from_proto(overview))
}

#[server(GetStorageOverview, "/api")]
pub async fn get_storage_overview_dto() -> Result<StorageOverviewDto, ServerFnError> {
    let cfg = dashboard_config();
    let overview = controller_client::get_storage_overview(cfg)
        .await
        .map_err(map_err)?;
    Ok(storage_overview_from_proto(overview))
}

#[server(GetReplicationStatus, "/api")]
pub async fn get_replication_status_dto() -> Result<ReplicationStatusDto, ServerFnError> {
    let cfg = dashboard_config();
    let status = controller_client::get_replication_status(cfg)
        .await
        .map_err(map_err)?;
    Ok(replication_status_from_proto(status))
}

#[server(ListReplicationConflicts, "/api")]
pub async fn list_replication_conflicts_dto() -> Result<Vec<ReplicationConflictDto>, ServerFnError>
{
    let cfg = dashboard_config();
    let resp = controller_client::list_replication_conflicts(cfg)
        .await
        .map_err(map_err)?;
    Ok(conflicts_from_proto(resp))
}

#[server(ListVolumes, "/api")]
pub async fn list_volumes_dto() -> Result<Vec<VolumeRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let vols = controller_client::list_volumes(cfg)
        .await
        .map_err(map_err)?;
    // Node inventory is extra. A storage RPC failure must not hide Ceph,
    // LVM, or ZFS volumes the controller already registered.
    let storage = controller_client::get_storage_overview(cfg).await.ok();
    Ok(volumes_with_host_inventory(vols, storage.as_ref()))
}

#[server(ListCephClusters, "/api")]
pub async fn list_ceph_clusters_dto() -> Result<Vec<CephClusterRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let clusters = controller_client::list_ceph_clusters(cfg)
        .await
        .map_err(map_err)?;
    Ok(ceph_clusters_from_proto(clusters))
}

#[server(GetCephClusterDetail, "/api")]
pub async fn get_ceph_cluster_detail_dto(
    name: String,
) -> Result<CephClusterDetailDto, ServerFnError> {
    let cfg = dashboard_config();
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(ServerFnError::new("cluster name is required"));
    }
    let cluster = controller_client::get_ceph_cluster(cfg, &name)
        .await
        .map_err(map_err)?;
    let mut detail = ceph_cluster_detail_from_proto(cluster);
    match controller_client::get_ceph_cluster_capacity(cfg, &name).await {
        Ok(cap) => {
            detail.capacity = merge_ceph_capacity(detail.capacity, cap);
            let cluster_slices = vec![
                ("used".to_string(), detail.capacity.used_bytes),
                ("available".to_string(), detail.capacity.avail_bytes),
            ];
            detail.cluster_pie_svg =
                crate::ceph_status::pie_svg(&cluster_slices, "Cluster capacity");
            let class_slices: Vec<(String, i64)> = detail
                .capacity
                .by_class
                .iter()
                .map(|c| (c.device_class.clone(), c.total_bytes))
                .collect();
            detail.class_pie_svg =
                crate::ceph_status::pie_svg(&class_slices, "Device class capacity");
        }
        Err(e) => {
            if detail.capacity.error.is_empty() {
                detail.capacity.error = format!("live capacity unavailable: {e:#}");
            }
        }
    }
    Ok(detail)
}

#[server(ListSharedFilesystems, "/api")]
pub async fn list_shared_filesystems_dto() -> Result<Vec<SharedFilesystemRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let filesystems = controller_client::list_shared_filesystems(cfg)
        .await
        .map_err(map_err)?;
    Ok(shared_filesystems_from_proto(filesystems))
}

#[server(ListObjectStores, "/api")]
pub async fn list_object_stores_dto() -> Result<Vec<ObjectStoreRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let stores = controller_client::list_object_stores(cfg)
        .await
        .map_err(map_err)?;
    Ok(object_stores_from_proto(stores))
}

#[server(ListVmOperations, "/api")]
pub async fn list_operations_dto(q: String) -> Result<Vec<VmOperationRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let ops = controller_client::list_vm_operations(cfg, true)
        .await
        .map_err(map_err)?;
    Ok(filter_vm_operations(vm_operations_from_proto(ops), &q))
}

#[server(ListDbms, "/api")]
pub async fn list_dbms_dto() -> Result<Vec<DbmsRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let rows = controller_client::list_postgresqls(cfg)
        .await
        .map_err(map_err)?;
    Ok(dbms_from_postgresqls(rows))
}

#[server(GetDbms, "/api")]
pub async fn get_dbms_dto(name: String) -> Result<DbmsDetailDto, ServerFnError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ServerFnError::new("missing database service name"));
    }
    let cfg = dashboard_config();
    let info = controller_client::get_postgresql(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(dbms_detail_from_postgresql(info))
}

#[server(ListKubernetesClusters, "/api")]
pub async fn list_kubernetes_clusters_dto() -> Result<Vec<KubernetesClusterRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let clusters = controller_client::list_kubernetes_clusters(cfg)
        .await
        .map_err(map_err)?;
    Ok(kubernetes_clusters_from_proto(clusters))
}

#[server(GetKubernetesCluster, "/api")]
pub async fn get_kubernetes_cluster_dto(
    name: String,
) -> Result<KubernetesClusterDetailDto, ServerFnError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ServerFnError::new("missing Kubernetes cluster name"));
    }
    let cfg = dashboard_config();
    let cluster = controller_client::get_kubernetes_cluster(cfg, name)
        .await
        .map_err(map_err)?;
    let health = controller_client::get_kubernetes_cluster_health(cfg, name)
        .await
        .map_err(|err| format!("{err:#}"));
    Ok(kubernetes_detail_from_proto(cluster, health))
}

#[server(ListAuditEvents, "/api")]
pub async fn list_audit_events_dto(
    q: String,
    action: String,
    since: String,
    limit: u32,
) -> Result<Vec<AuditEventDto>, ServerFnError> {
    let cfg = dashboard_config();
    let limit = limit.clamp(1, 1000);
    let events = controller_client::list_audit_events(cfg, limit, since.trim(), action.trim())
        .await
        .map_err(map_err)?;
    Ok(filter_audit_events(
        audit_events_from_proto(events),
        &q,
        action.trim(),
        since.trim(),
    ))
}

fn require_name<'a>(name: &'a str, label: &str) -> Result<&'a str, ServerFnError> {
    let name = name.trim();
    if name.is_empty() {
        Err(ServerFnError::new(format!("missing {label}")))
    } else {
        Ok(name)
    }
}

#[server(GetClusterHealth, "/api")]
pub async fn get_cluster_health_dto() -> Result<ClusterHealthDto, ServerFnError> {
    let cfg = dashboard_config();
    let health = controller_client::get_cluster_health(cfg)
        .await
        .map_err(map_err)?;
    Ok(cluster_health_from_proto(health))
}

#[server(ListNodes, "/api")]
pub async fn list_nodes_dto() -> Result<Vec<NodeRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let nodes = controller_client::list_nodes(cfg).await.map_err(map_err)?;
    Ok(nodes_from_proto(nodes))
}

#[server(GetNode, "/api")]
pub async fn get_node_dto(id: String) -> Result<NodeDetailDto, ServerFnError> {
    let id = require_name(&id, "node")?;
    let cfg = dashboard_config();
    let node = controller_client::get_node(cfg, id)
        .await
        .map_err(map_err)?;
    let devices = controller_client::list_pci_devices(cfg, id)
        .await
        .map_err(|err| format!("{err:#}"));
    Ok(node_detail_from_proto(node, devices))
}

#[server(GetVmDetail, "/api")]
pub async fn get_vm_detail_dto(name: String) -> Result<VmDetailDto, ServerFnError> {
    let name = require_name(&name, "virtual machine")?.to_string();
    let cfg = dashboard_config();
    let vm = controller_client::get_vm(cfg, &name)
        .await
        .map_err(map_err)?;
    let migrate = controller_client::get_live_migrate_receive_status(cfg, &name)
        .await
        .map_err(|err| format!("{err:#}"));
    Ok(vm_detail_from_proto(vm, migrate))
}

#[server(GetVolumeDetail, "/api")]
pub async fn get_volume_detail_dto(
    name: String,
    node: String,
) -> Result<VolumeDetailDto, ServerFnError> {
    let name = require_name(&name, "volume")?.to_string();
    let node = node.trim().to_string();
    let cfg = dashboard_config();
    match controller_client::get_volume(cfg, &name).await {
        Ok(info) => Ok(volume_detail_from_info(info, true)),
        Err(err) => {
            let storage = controller_client::get_storage_overview(cfg).await.ok();
            if let Some(storage) = storage.as_ref() {
                if let Some(detail) = inventory_volume_detail(&name, &node, storage) {
                    return Ok(detail);
                }
            }
            Err(map_err(err))
        }
    }
}

#[server(ListSnapshots, "/api")]
pub async fn list_snapshots_dto(volume: String) -> Result<Vec<SnapshotRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let snaps = controller_client::list_volume_snapshots(cfg, volume.trim())
        .await
        .map_err(map_err)?;
    Ok(snapshots_from_proto(snaps))
}

#[server(GetSnapshot, "/api")]
pub async fn get_snapshot_dto(name: String) -> Result<SnapshotRowDto, ServerFnError> {
    let name = require_name(&name, "snapshot")?.to_string();
    let cfg = dashboard_config();
    let snaps = controller_client::list_volume_snapshots(cfg, "")
        .await
        .map_err(map_err)?;
    snapshots_from_proto(snaps)
        .into_iter()
        .find(|snap| snap.name == name || snap.id == name)
        .ok_or_else(|| ServerFnError::new(format!("snapshot '{name}' not found")))
}

#[server(ListSnapshotPolicies, "/api")]
pub async fn list_snapshot_policies_dto() -> Result<Vec<SnapshotPolicyDto>, ServerFnError> {
    let cfg = dashboard_config();
    let policies = controller_client::list_snapshot_policies(cfg)
        .await
        .map_err(map_err)?;
    Ok(snapshot_policies_from_proto(policies))
}

#[server(GetSnapshotPolicy, "/api")]
pub async fn get_snapshot_policy_dto(name: String) -> Result<SnapshotPolicyDto, ServerFnError> {
    let name = require_name(&name, "snapshot schedule")?;
    let cfg = dashboard_config();
    let policy = controller_client::get_snapshot_policy(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(snapshot_policy_from_proto(policy))
}

#[server(GetSharedFilesystem, "/api")]
pub async fn get_shared_filesystem_dto(
    name: String,
) -> Result<SharedFilesystemDetailDto, ServerFnError> {
    let name = require_name(&name, "shared filesystem")?;
    let cfg = dashboard_config();
    let fs = controller_client::get_shared_filesystem(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(shared_filesystem_detail_from_proto(fs))
}

#[server(GetObjectStore, "/api")]
pub async fn get_object_store_dto(name: String) -> Result<ObjectStoreDetailDto, ServerFnError> {
    let name = require_name(&name, "object store")?;
    let cfg = dashboard_config();
    let store = controller_client::get_object_store(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(object_store_detail_from_proto(store))
}

#[server(GetOperation, "/api")]
pub async fn get_operation_dto(id: String) -> Result<OperationDetailDto, ServerFnError> {
    let id = require_name(&id, "operation")?;
    let cfg = dashboard_config();
    let op = controller_client::get_vm_operation(cfg, id)
        .await
        .map_err(map_err)?;
    Ok(operation_detail_from_proto(op))
}

#[server(GetNetwork, "/api")]
pub async fn get_network_dto(node: String, name: String) -> Result<NetworkRowDto, ServerFnError> {
    let name = require_name(&name, "network")?.to_string();
    let node = node.trim().to_string();
    let cfg = dashboard_config();
    let nets = controller_client::list_networks(cfg)
        .await
        .map_err(map_err)?;
    networks_from_proto(nets)
        .into_iter()
        .find(|net| net.name == name && (node.is_empty() || net.node_id == node))
        .ok_or_else(|| ServerFnError::new(format!("network '{name}' not found")))
}

#[server(ListContainers, "/api")]
pub async fn list_containers_dto() -> Result<Vec<ContainerRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let rows = controller_client::list_containers(cfg)
        .await
        .map_err(map_err)?;
    Ok(containers_from_proto(rows))
}

#[server(GetContainer, "/api")]
pub async fn get_container_dto(
    name: String,
    node: String,
) -> Result<ContainerDetailDto, ServerFnError> {
    let name = require_name(&name, "container")?;
    let cfg = dashboard_config();
    let row = controller_client::get_container(cfg, name, node.trim())
        .await
        .map_err(map_err)?;
    Ok(container_detail_from_proto(row))
}

#[server(ListDiskLayouts, "/api")]
pub async fn list_disk_layouts_dto() -> Result<Vec<DiskLayoutRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let rows = controller_client::list_disk_layouts(cfg)
        .await
        .map_err(map_err)?;
    Ok(disk_layouts_from_proto(rows))
}

#[server(GetDiskLayout, "/api")]
pub async fn get_disk_layout_dto(name: String) -> Result<DiskLayoutDetailDto, ServerFnError> {
    let name = require_name(&name, "disk layout")?;
    let cfg = dashboard_config();
    let row = controller_client::get_disk_layout(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(disk_layout_detail_from_proto(row))
}

#[server(ListStorageClasses, "/api")]
pub async fn list_storage_classes_dto() -> Result<Vec<StorageClassDto>, ServerFnError> {
    let cfg = dashboard_config();
    let nodes = controller_client::list_nodes(cfg).await.map_err(map_err)?;
    Ok(storage_classes_from_nodes(&nodes_from_proto(nodes)))
}

#[server(GetStorageClass, "/api")]
pub async fn get_storage_class_dto(name: String) -> Result<StorageClassDetailDto, ServerFnError> {
    let name = require_name(&name, "storage class")?.to_string();
    let cfg = dashboard_config();
    let nodes = controller_client::list_nodes(cfg).await.map_err(map_err)?;
    Ok(storage_class_detail(&name, nodes_from_proto(nodes)))
}

#[server(ListCertificates, "/api")]
pub async fn list_certificates_dto() -> Result<Vec<CertificateRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let rows = controller_client::list_certificates(cfg)
        .await
        .map_err(map_err)?;
    Ok(certificates_from_proto(rows))
}

#[server(GetPkiStatus, "/api")]
pub async fn get_pki_status_dto() -> Result<PkiStatusDto, ServerFnError> {
    let cfg = dashboard_config();
    let status = controller_client::get_pki_status(cfg)
        .await
        .map_err(map_err)?;
    Ok(pki_status_from_proto(status))
}

#[server(GetCrlSummary, "/api")]
pub async fn get_crl_summary_dto() -> Result<CrlSummaryDto, ServerFnError> {
    let cfg = dashboard_config();
    let crl = controller_client::get_crl(cfg).await.map_err(map_err)?;
    Ok(crl_summary_from_proto(crl))
}

#[server(ListSshKeys, "/api")]
pub async fn list_ssh_keys_dto() -> Result<Vec<SshKeyRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let keys = controller_client::list_ssh_keys(cfg)
        .await
        .map_err(map_err)?;
    Ok(ssh_keys_from_proto(keys))
}

#[server(GetSshKey, "/api")]
pub async fn get_ssh_key_dto(name: String) -> Result<SshKeyDetailDto, ServerFnError> {
    let name = require_name(&name, "SSH key")?;
    let cfg = dashboard_config();
    let key = controller_client::get_ssh_key(cfg, name)
        .await
        .map_err(map_err)?;
    Ok(ssh_key_detail_from_proto(key))
}

#[server(GetSbomSummaries, "/api")]
pub async fn get_sbom_summaries_dto() -> Result<Vec<SbomSummaryDto>, ServerFnError> {
    let cfg = dashboard_config();
    let mut rows = Vec::new();
    for kind in ["crates", "iso-closure"] {
        let result = controller_client::export_sbom(cfg, kind)
            .await
            .map_err(|err| format!("{err:#}"));
        rows.push(sbom_summary_from_proto(kind, result));
    }
    Ok(rows)
}

#[server(GetCryptoConfig, "/api")]
pub async fn get_crypto_config_dto() -> Result<CryptoConfigDto, ServerFnError> {
    let cfg = dashboard_config();
    let cfg_proto = controller_client::get_crypto_config(cfg)
        .await
        .map_err(map_err)?;
    Ok(crypto_config_from_proto(cfg_proto))
}
