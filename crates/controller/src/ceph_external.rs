//! A CephCluster with `spec.external` is somebody else's Ceph. kcore does not
//! bootstrap mons or OSDs for it. The reconciler writes the client config to
//! every approved, ready node that is not an internal Ceph member, then reads
//! health through one of them.
//!
//! The node agent maps `kcore-vms` as `client.admin`, so those are the only
//! pool and entity accepted today.

use tonic::Request;

use crate::ceph_cluster_spec;
use crate::controller_proto::{CephClusterSpec, CephExternalSpec};
use crate::db::{CephClusterRow, CephClusterStatusRow, Database, NodeRow};
use crate::node_client::NodeClients;
use crate::node_proto;

/// Pool the node agent and the controller's volume paths use.
pub(crate) const MAPPED_POOL: &str = "kcore-vms";
const MAPPED_ENTITY: &str = "client.admin";

pub(crate) fn check_supported(ext: &CephExternalSpec) -> Result<(), String> {
    if ext.pool.trim() != MAPPED_POOL {
        return Err(format!(
            "external: kcore maps pool {MAPPED_POOL} only; spec.external.pool is '{}'",
            ext.pool.trim()
        ));
    }
    let entity = ext.client_name.trim();
    if !entity.is_empty() && entity != MAPPED_ENTITY {
        return Err(format!(
            "external: the node agent runs as {MAPPED_ENTITY}; spec.external.clientName is '{entity}'"
        ));
    }
    if ext.client_keyring.trim().is_empty() {
        return Err(format!(
            "external: spec.external.clientKeyring must hold the [{MAPPED_ENTITY}] keyring"
        ));
    }
    Ok(())
}

/// Nodes that need the external client config: approved, ready, and not a
/// member of a Ceph cluster kcore runs itself.
pub(crate) fn client_nodes(db: &Database) -> Result<Vec<NodeRow>, String> {
    let mut members = std::collections::HashSet::new();
    for cluster in db.list_ceph_clusters().map_err(|e| e.to_string())? {
        if let Ok(spec) = ceph_cluster_spec::spec_from_json(&cluster.spec_json) {
            members.extend(spec.nodes.into_iter().map(|n| n.node_id));
        }
    }
    Ok(db
        .list_nodes()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|n| n.approval_status == "approved" && n.status == "ready")
        .filter(|n| !members.contains(&n.id))
        .collect())
}

pub(crate) fn status_message(
    ext: &CephExternalSpec,
    health: &str,
    clients: usize,
    missing: &[String],
) -> String {
    let mut msg = format!(
        "external: VMs map pool {} via mons {}; {health}; client config on {clients} node(s)",
        ext.pool.trim(),
        ext.mon_hosts.trim()
    );
    if !missing.is_empty() {
        msg.push_str(&format!("; missing on {}", missing.join(", ")));
    }
    msg
}

pub(crate) fn phase_for(health: &str, missing: &[String]) -> &'static str {
    match crate::ceph_cluster_reconciler::phase_from_health(health) {
        "healthy" if missing.is_empty() => "healthy",
        "failed" => "failed",
        _ => "degraded",
    }
}

fn write_status(
    db: &Database,
    row: &CephClusterRow,
    phase: &str,
    message: String,
    raw: String,
) -> Result<(), String> {
    // Generation 0 keeps the cluster queued, so new nodes get the client
    // config and health keeps being read.
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: phase.into(),
        health_message: message,
        ceph_status_json: raw,
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())
}

pub(crate) async fn reconcile(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
    spec: &CephClusterSpec,
) -> Result<(), String> {
    let ext = ceph_cluster_spec::active_external(spec)
        .ok_or_else(|| "spec.external is not set".to_string())?;
    check_supported(ext)?;
    let targets = client_nodes(db)?;
    if targets.is_empty() {
        return write_status(
            db,
            row,
            "pending",
            "external: no approved ready node to carry the client config yet".into(),
            String::new(),
        );
    }

    let mut applied: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for node in &targets {
        let result = match crate::ceph_cluster_reconciler::admin_for(db, clients, &node.id).await {
            Ok(mut admin) => admin
                .apply_ceph_client_config(Request::new(node_proto::ApplyCephClientConfigRequest {
                    mon_hosts: ext.mon_hosts.trim().to_string(),
                    client_name: MAPPED_ENTITY.into(),
                    keyring: ext.client_keyring.clone(),
                    fsid: spec.fsid.trim().to_string(),
                }))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| e.message().to_string()),
            Err(e) => Err(e),
        };
        match result {
            Ok(resp) if resp.success => applied.push(node.id.clone()),
            Ok(resp) => missing.push(format!("{} ({})", node.id, resp.message)),
            Err(e) => missing.push(format!("{} ({e})", node.id)),
        }
    }
    let Some(probe) = applied.first() else {
        return Err(format!("external: {}", missing.join("; ")));
    };

    let health = match crate::ceph_cluster_reconciler::admin_for(db, clients, probe).await {
        Ok(mut admin) => admin
            .get_ceph_health(Request::new(node_proto::GetCephHealthRequest {
                cluster_name: "ceph".into(),
            }))
            .await
            .map(|r| r.into_inner())
            .map_err(|e| e.message().to_string()),
        Err(e) => Err(e),
    };
    match health {
        Ok(h) => {
            let phase = phase_for(&h.health_status, &missing);
            write_status(
                db,
                row,
                phase,
                status_message(ext, &h.health_status, applied.len(), &missing),
                h.raw_status,
            )
        }
        Err(e) => write_status(
            db,
            row,
            "failed",
            format!("external: health from {probe}: {e}"),
            String::new(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(pool: &str, name: &str, keyring: &str) -> CephExternalSpec {
        CephExternalSpec {
            mon_hosts: "10.0.0.1:6789".into(),
            pool: pool.into(),
            client_keyring: keyring.into(),
            client_name: name.into(),
        }
    }

    #[test]
    fn only_the_pool_and_entity_the_node_agent_uses_are_accepted() {
        let keyring = "[client.admin]\nkey = AQB==\n";
        assert!(check_supported(&ext("kcore-vms", "", keyring)).is_ok());
        assert!(check_supported(&ext("kcore-vms", "client.admin", keyring)).is_ok());
        let err = check_supported(&ext("rbd", "", keyring)).unwrap_err();
        assert!(err.contains("kcore-vms") && err.contains("'rbd'"), "{err}");
        let err = check_supported(&ext("kcore-vms", "client.kcore", keyring)).unwrap_err();
        assert!(err.contains("client.kcore"), "{err}");
        assert!(check_supported(&ext("kcore-vms", "", " ")).is_err());
    }

    #[test]
    fn healthy_needs_ok_health_and_every_node_configured() {
        assert_eq!(phase_for("HEALTH_OK", &[]), "healthy");
        assert_eq!(phase_for("HEALTH_OK", &["n2 (x)".into()]), "degraded");
        assert_eq!(phase_for("HEALTH_WARN", &[]), "degraded");
        assert_eq!(phase_for("HEALTH_ERR", &[]), "failed");
    }

    #[test]
    fn the_message_names_the_pool_and_the_nodes_still_missing_config() {
        let e = ext("kcore-vms", "", "k");
        let msg = status_message(&e, "HEALTH_OK", 2, &["n3 (down)".into()]);
        assert!(msg.starts_with("external: VMs map pool kcore-vms"), "{msg}");
        assert!(msg.contains("2 node(s)") && msg.contains("missing on n3 (down)"));
    }

    #[test]
    fn nodes_in_an_internal_cluster_get_no_client_config() {
        let db = Database::open(":memory:").unwrap();
        assert!(client_nodes(&db).unwrap().is_empty());
    }
}
