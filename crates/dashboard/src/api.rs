//! Leptos server functions — call controller gRPC using [`crate::state::dashboard_config`].

use leptos::prelude::*;

use crate::controller_client;
use crate::dto::{
    AuditEventDto, CephClusterRowDto, ComplianceDto, NetworkOverviewDto, NetworkRowDto,
    ObjectStoreRowDto, ReplicationConflictDto, ReplicationStatusDto, SharedFilesystemRowDto,
    StorageOverviewDto, VmOperationRowDto, VmsPageDto, VolumeRowDto,
};
use crate::mappers::{
    audit_events_from_proto, ceph_clusters_from_proto, compliance_from_proto, conflicts_from_proto,
    filter_audit_events, filter_vm_operations, network_overview_from_proto, networks_from_proto,
    object_stores_from_proto, replication_status_from_proto, shared_filesystems_from_proto,
    storage_overview_from_proto, vm_operations_from_proto, vms_page_from_proto, volumes_from_proto,
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
pub async fn list_vms_page(page: u32) -> Result<VmsPageDto, ServerFnError> {
    let cfg = dashboard_config();
    let vms = controller_client::list_vms(cfg).await.map_err(map_err)?;
    Ok(vms_page_from_proto(vms, page))
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
    Ok(volumes_from_proto(vols))
}

#[server(ListCephClusters, "/api")]
pub async fn list_ceph_clusters_dto() -> Result<Vec<CephClusterRowDto>, ServerFnError> {
    let cfg = dashboard_config();
    let clusters = controller_client::list_ceph_clusters(cfg)
        .await
        .map_err(map_err)?;
    Ok(ceph_clusters_from_proto(clusters))
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
