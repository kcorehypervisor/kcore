//! Combined controller read for the operator console.

use axum::routing::get;
use axum::Json;
use axum::Router;

use crate::controller_client;
use crate::dto::AgentSnapshotDto;
use crate::mappers::{
    audit_events_from_proto, ceph_clusters_from_proto, compliance_from_proto, conflicts_from_proto,
    network_overview_from_proto, networks_from_proto, object_stores_from_proto,
    replication_status_from_proto, shared_filesystems_from_proto, storage_overview_from_proto,
    vm_operations_from_proto, vm_rows_from_proto, volumes_from_proto,
};
use crate::state::dashboard_config;

const READS: usize = 13;

fn note<T>(label: &str, result: Result<T, anyhow::Error>, warnings: &mut Vec<String>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(err) => {
            warnings.push(format!("{label}: {err:#}"));
            None
        }
    }
}

pub async fn load_agent_snapshot() -> AgentSnapshotDto {
    let cfg = dashboard_config();
    let (
        compliance,
        vms,
        networks,
        network_overview,
        storage,
        replication,
        conflicts,
        volumes,
        ceph,
        filesystems,
        objects,
        operations,
        audit,
    ) = tokio::join!(
        controller_client::get_compliance(cfg),
        controller_client::list_vms(cfg),
        controller_client::list_networks(cfg),
        controller_client::get_network_overview(cfg),
        controller_client::get_storage_overview(cfg),
        controller_client::get_replication_status(cfg),
        controller_client::list_replication_conflicts(cfg),
        controller_client::list_volumes(cfg),
        controller_client::list_ceph_clusters(cfg),
        controller_client::list_shared_filesystems(cfg),
        controller_client::list_object_stores(cfg),
        controller_client::list_vm_operations(cfg, true),
        controller_client::list_audit_events(cfg, 100, "", ""),
    );

    let mut warnings = Vec::new();
    let compliance = note("compliance", compliance, &mut warnings).map(compliance_from_proto);
    let vms = note("vms", vms, &mut warnings)
        .map(vm_rows_from_proto)
        .unwrap_or_default();
    let networks = note("networks", networks, &mut warnings)
        .map(networks_from_proto)
        .unwrap_or_default();
    let network_overview =
        note("network overview", network_overview, &mut warnings).map(network_overview_from_proto);
    let storage = note("storage", storage, &mut warnings).map(storage_overview_from_proto);
    let replication =
        note("replication", replication, &mut warnings).map(replication_status_from_proto);
    let conflicts = note("conflicts", conflicts, &mut warnings)
        .map(conflicts_from_proto)
        .unwrap_or_default();
    let volumes = note("volumes", volumes, &mut warnings)
        .map(volumes_from_proto)
        .unwrap_or_default();
    let ceph_clusters = note("ceph", ceph, &mut warnings)
        .map(ceph_clusters_from_proto)
        .unwrap_or_default();
    let shared_filesystems = note("shared filesystems", filesystems, &mut warnings)
        .map(shared_filesystems_from_proto)
        .unwrap_or_default();
    let object_stores = note("object stores", objects, &mut warnings)
        .map(object_stores_from_proto)
        .unwrap_or_default();
    let operations = note("operations", operations, &mut warnings)
        .map(vm_operations_from_proto)
        .unwrap_or_default();
    let audit = note("audit", audit, &mut warnings)
        .map(audit_events_from_proto)
        .unwrap_or_default();

    let source = if warnings.len() == READS {
        "unavailable"
    } else {
        "controller"
    };

    AgentSnapshotDto {
        source: source.to_string(),
        warnings,
        compliance,
        vms,
        volumes,
        networks,
        network_overview,
        storage,
        ceph_clusters,
        shared_filesystems,
        object_stores,
        operations,
        audit,
        replication,
        conflicts,
    }
}

async fn agent_snapshot_handler() -> Json<AgentSnapshotDto> {
    Json(load_agent_snapshot().await)
}

pub fn agent_routes() -> Router {
    Router::new().route("/api/agent-snapshot", get(agent_snapshot_handler))
}
