//! CephCluster reconciler — push Nix, bootstrap keys/MON/MGR/OSD, ensure RBD pool.

use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::warn;

use crate::ceph_cluster_spec;
use crate::db::{CephClusterRow, CephClusterStatusRow, Database};
use crate::node_client::NodeClients;
use crate::node_proto;
use kcore_sanitize::nix_escape;

const RECONCILE_TICK: Duration = Duration::from_secs(15);
const CEPH_CLUSTER_NAME: &str = "ceph";
const RBD_POOL: &str = "kcore-vms";
const NIX_APPLY_WAIT: Duration = Duration::from_secs(600);
/// Returned when health was recorded and the caller must not overwrite it
/// with phase=failed.
const NOT_HEALTHY_YET: &str = "ceph not healthy yet:";

pub fn spawn_ceph_cluster_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(error) = reconcile_once(&db, &clients).await {
                warn!(%error, "ceph cluster reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    for row in db
        .list_ceph_clusters_needing_reconcile()
        .map_err(|e| e.to_string())?
    {
        if let Err(error) = reconcile_row(db, clients, &row).await {
            if error.starts_with(NOT_HEALTHY_YET) {
                // reconcile_row already stored degraded/failed from `ceph -s`.
                // Stamping phase=failed here turned every HEALTH_WARN into a
                // hard failure and hid the real health string.
                warn!(%error, cluster = %row.name, "ceph cluster not healthy yet");
                continue;
            }
            let _ = db.upsert_ceph_cluster_status(&CephClusterStatusRow {
                name: row.name.clone(),
                observed_generation: 0,
                phase: "failed".into(),
                health_message: error.clone(),
                ceph_status_json: String::new(),
                last_transition_at: String::new(),
            });
        }
    }
    Ok(())
}

async fn admin_for(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<crate::node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>, String>
{
    let node = db
        .get_node(node_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("node {node_id} is not registered"))?;
    if clients.get_admin(&node.address).is_none() {
        clients
            .connect(&node.address)
            .await
            .map_err(|e| e.to_string())?;
    }
    clients
        .get_admin(&node.address)
        .ok_or_else(|| format!("no admin client for {}", node.address))
}

fn mon_map_string(spec: &crate::controller_proto::CephClusterSpec) -> String {
    spec.nodes
        .iter()
        .map(|n| {
            let daemon = sanitize_daemon_id(&n.node_id, 0);
            let ip = n
                .mon_addr
                .split(':')
                .next()
                .unwrap_or(n.mon_addr.as_str())
                .trim();
            format!("{daemon}={ip}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn reconcile_row(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
) -> Result<(), String> {
    let spec = ceph_cluster_spec::spec_from_json(&row.spec_json).map_err(|e| e.to_string())?;
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "bootstrapping".into(),
        health_message: String::new(),
        ceph_status_json: String::new(),
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;

    let mon_map = mon_map_string(&spec);
    let mut bootstrap = row.bootstrap_json.clone().into_bytes();

    for (index, node) in spec.nodes.iter().enumerate() {
        let daemon_id = sanitize_daemon_id(&node.node_id, index);
        let mut admin = admin_for(db, clients, &node.node_id).await?;
        let mut applied = admin
            .apply_ceph_config(Request::new(node_proto::ApplyCephConfigRequest {
                ceph_nix: generate_ceph_nix(&spec, node, &daemon_id, &mon_map),
                rebuild: true,
                fsid: spec.fsid.clone(),
                mon: true,
                mgr: true,
                osd: true,
                daemon_id: daemon_id.clone(),
                keyring: bootstrap.clone(),
                mon_map: mon_map.clone(),
                public_addr: node
                    .mon_addr
                    .split(':')
                    .next()
                    .unwrap_or(&node.mon_addr)
                    .to_string(),
                cluster_addr: node.cluster_addr.clone(),
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !applied.success {
            return Err(applied.message);
        }
        if !applied.keyring.is_empty() && bootstrap.is_empty() {
            bootstrap = applied.keyring.clone();
            let bootstrap_str = String::from_utf8_lossy(&bootstrap).to_string();
            db.set_ceph_cluster_bootstrap(&row.name, &bootstrap_str)
                .map_err(|e| e.to_string())?;
        }
        if !applied.daemons_started {
            if !applied.apply_id.is_empty() {
                wait_for_nix_apply(&mut admin, &applied.apply_id).await?;
                admin = admin_for(db, clients, &node.node_id).await?;
            }
            applied = admin
                .apply_ceph_config(Request::new(node_proto::ApplyCephConfigRequest {
                    ceph_nix: generate_ceph_nix(&spec, node, &daemon_id, &mon_map),
                    rebuild: false,
                    fsid: spec.fsid.clone(),
                    mon: true,
                    mgr: true,
                    osd: true,
                    daemon_id: daemon_id.clone(),
                    keyring: bootstrap.clone(),
                    mon_map: mon_map.clone(),
                    public_addr: node
                        .mon_addr
                        .split(':')
                        .next()
                        .unwrap_or(&node.mon_addr)
                        .to_string(),
                    cluster_addr: node.cluster_addr.clone(),
                }))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            if !applied.success || !applied.daemons_started {
                return Err(if applied.message.is_empty() {
                    format!("mon/mgr did not start on {}", node.node_id)
                } else {
                    applied.message
                });
            }
            if !applied.keyring.is_empty() && bootstrap.is_empty() {
                bootstrap = applied.keyring.clone();
                let bootstrap_str = String::from_utf8_lossy(&bootstrap).to_string();
                db.set_ceph_cluster_bootstrap(&row.name, &bootstrap_str)
                    .map_err(|e| e.to_string())?;
            }
        }

        let devices = ceph_cluster_spec::osd_devices(node);
        let osd = admin
            .reconcile_ceph_osds(Request::new(node_proto::ReconcileCephOsdsRequest {
                osd_devices: devices,
                force_wipe: spec.force_wipe,
                encrypt_osds: spec.encrypt_osds,
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !osd.success {
            return Err(osd.message);
        }
    }

    // Members still in the spec are up, so `ceph mon rm` has a quorum to talk
    // to. Doing this first made removal depend on a node that had not joined.
    retire_removed_members(db, clients, row, &spec).await?;

    let first = spec
        .nodes
        .first()
        .ok_or_else(|| "cluster has no nodes".to_string())?;
    let mut admin = admin_for(db, clients, &first.node_id).await?;
    let pool = admin
        .ensure_ceph_pool(Request::new(node_proto::EnsureCephPoolRequest {
            pool: RBD_POOL.into(),
            size: spec.size,
            min_size: spec.min_size,
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    if !pool.success {
        return Err(pool.message);
    }

    let health = admin
        .get_ceph_health(Request::new(node_proto::GetCephHealthRequest {
            cluster_name: CEPH_CLUSTER_NAME.into(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    let phase = phase_from_health(&health.health_status);
    let observed = if phase == "healthy" {
        row.generation
    } else {
        // Stay queued for degraded/failed until HEALTH_OK.
        0
    };
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: observed,
        phase: phase.into(),
        health_message: format!(
            "{} (osd up {}, in {})",
            health.health_status, health.osd_up, health.osd_in
        ),
        ceph_status_json: health.raw_status,
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())?;
    if phase != "healthy" {
        return Err(format!(
            "{NOT_HEALTHY_YET} {} (osd up {}, in {})",
            health.health_status, health.osd_up, health.osd_in
        ));
    }
    Ok(())
}

async fn retire_removed_members(
    db: &Database,
    clients: &NodeClients,
    row: &CephClusterRow,
    spec: &crate::controller_proto::CephClusterSpec,
) -> Result<(), String> {
    let retired = db
        .list_retired_ceph_nodes(&row.name)
        .map_err(|e| e.to_string())?;
    for node_id in retired {
        if spec.nodes.iter().any(|n| n.node_id == node_id) {
            db.clear_retired_ceph_node(&row.name, &node_id)
                .map_err(|e| e.to_string())?;
            continue;
        }
        let mut admin = admin_for(db, clients, &node_id).await?;
        let torn = admin
            .teardown_ceph_node(Request::new(node_proto::TeardownCephNodeRequest {}))
            .await
            .map_err(|e| format!("teardown {node_id}: {e}"))?
            .into_inner();
        if !torn.success {
            return Err(format!("teardown {node_id}: {}", torn.message));
        }
        if !torn.apply_id.is_empty() {
            wait_for_nix_apply(&mut admin, &torn.apply_id).await?;
        }
        if !torn.daemon_id.is_empty() {
            if let Some(survivor) = spec.nodes.first() {
                let mut survivor_admin = admin_for(db, clients, &survivor.node_id).await?;
                let removed = survivor_admin
                    .remove_ceph_mon(Request::new(node_proto::RemoveCephMonRequest {
                        daemon_id: torn.daemon_id.clone(),
                    }))
                    .await
                    .map_err(|e| {
                        format!(
                            "ceph mon rm {} via {}: {e}",
                            torn.daemon_id, survivor.node_id
                        )
                    })?
                    .into_inner();
                if !removed.success {
                    return Err(format!(
                        "ceph mon rm {} via {}: {}",
                        torn.daemon_id, survivor.node_id, removed.message
                    ));
                }
            }
        }
        db.clear_retired_ceph_node(&row.name, &node_id)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn wait_for_nix_apply(
    admin: &mut crate::node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>,
    apply_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + NIX_APPLY_WAIT;
    loop {
        match admin
            .get_nix_apply_status(Request::new(node_proto::GetNixApplyStatusRequest {
                apply_id: apply_id.to_string(),
            }))
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                match resp.phase {
                    p if p == node_proto::NixApplyPhase::Succeeded as i32 => return Ok(()),
                    p if p == node_proto::NixApplyPhase::Failed as i32 => {
                        return Err(format!("nixos-rebuild {apply_id} failed: {}", resp.message));
                    }
                    p if p == node_proto::NixApplyPhase::Unknown as i32 => {
                        warn!(%apply_id, message = %resp.message, "no nix apply verdict; continuing");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            Err(e) if e.code() == tonic::Code::Unavailable => {}
            Err(e) if e.code() == tonic::Code::Unimplemented => return Ok(()),
            Err(e) => return Err(format!("polling nix apply {apply_id}: {e}")),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for nix apply {apply_id}"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

pub(crate) fn phase_from_health(health_status: &str) -> &'static str {
    match health_status {
        "HEALTH_OK" => "healthy",
        "HEALTH_WARN" => "degraded",
        _ => "failed",
    }
}

pub(crate) fn sanitize_daemon_id(node_id: &str, index: usize) -> String {
    let id: String = node_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if id.is_empty() {
        format!("node-{index}")
    } else {
        id
    }
}

pub(crate) fn generate_ceph_nix(
    spec: &crate::controller_proto::CephClusterSpec,
    node: &crate::controller_proto::CephClusterNodeSpec,
    daemon_id: &str,
    mon_map: &str,
) -> String {
    let mon_host: String = mon_map
        .split(',')
        .filter_map(|p| p.split_once('=').map(|(_, ip)| ip.trim().to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let mon_ids: String = mon_map
        .split(',')
        .filter_map(|p| p.split_once('=').map(|(id, _)| id.trim().to_string()))
        .collect::<Vec<_>>()
        .join(",");
    let public_addr = node
        .mon_addr
        .split(':')
        .next()
        .unwrap_or(&node.mon_addr)
        .trim();
    format!(
        r#"{{ ... }}: {{
  kcore.ceph = {{
    enable = true;
    clusterName = "{cluster}";
    fsid = "{fsid}";
    publicNetwork = "{public_network}";
    clusterNetwork = "{cluster_network}";
    monAddress = "{mon_host}";
    monInitialMembers = "{mon_ids}";
    publicAddr = "{public_addr}";
    clusterAddr = "{cluster_addr}";
    daemonId = "{daemon_id}";
    enableMon = true;
    enableMgr = true;
    enableOsd = true;
    poolSize = {size};
    poolMinSize = {min_size};
  }};
}}
"#,
        cluster = nix_escape(CEPH_CLUSTER_NAME),
        fsid = nix_escape(&spec.fsid),
        public_network = nix_escape(&spec.public_network),
        cluster_network = nix_escape(&spec.cluster_network),
        cluster_addr = nix_escape(&node.cluster_addr),
        size = spec.size,
        min_size = spec.min_size,
        mon_host = nix_escape(&mon_host),
        mon_ids = nix_escape(&mon_ids),
        public_addr = nix_escape(public_addr),
        daemon_id = nix_escape(daemon_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_proto::{CephClusterNodeSpec, CephClusterSpec};

    fn sample_spec() -> CephClusterSpec {
        CephClusterSpec {
            fsid: "fsid-1".into(),
            public_network: "10.10.0.0/24".into(),
            cluster_network: "10.20.0.0/24".into(),
            size: 3,
            min_size: 2,
            force_wipe: false,
            encrypt_osds: true,
            nodes: vec![
                CephClusterNodeSpec {
                    node_id: "dell.tower/1".into(),
                    mon_addr: "10.10.0.11:6789".into(),
                    cluster_addr: "10.20.0.11".into(),
                    public_iface: "eth1".into(),
                    cluster_iface: "eth2".into(),
                    osd_device: "/dev/nvme0n1".into(),
                    osd_devices: vec![],
                },
                CephClusterNodeSpec {
                    node_id: "dell-2".into(),
                    mon_addr: "10.10.0.12:6789".into(),
                    cluster_addr: "10.20.0.12".into(),
                    public_iface: "eth1".into(),
                    cluster_iface: "eth2".into(),
                    osd_device: "/dev/nvme0n1".into(),
                    osd_devices: vec![],
                },
            ],
        }
    }

    #[test]
    fn phase_from_health_maps_ceph_status_strings() {
        assert_eq!(phase_from_health("HEALTH_OK"), "healthy");
        assert_eq!(phase_from_health("HEALTH_WARN"), "degraded");
        assert_eq!(phase_from_health("HEALTH_ERR"), "failed");
        assert_eq!(phase_from_health(""), "failed");
        assert_eq!(phase_from_health("unknown"), "failed");
    }

    #[test]
    fn sanitize_daemon_id_keeps_safe_chars_and_replaces_rest() {
        assert_eq!(sanitize_daemon_id("dell-1", 0), "dell-1");
        assert_eq!(sanitize_daemon_id("dell.tower/1", 0), "dell-tower-1");
        assert_eq!(sanitize_daemon_id("@@@", 2), "---");
        assert_eq!(sanitize_daemon_id("", 3), "node-3");
    }

    #[test]
    fn generate_ceph_nix_uses_fixed_cluster_name_and_all_mons() {
        let spec = sample_spec();
        let mon_map = mon_map_string(&spec);
        assert!(mon_map.contains("dell-tower-1=10.10.0.11"));
        assert!(mon_map.contains("dell-2=10.10.0.12"));
        let nix = generate_ceph_nix(&spec, &spec.nodes[0], "dell-tower-1", &mon_map);
        assert!(nix.contains("clusterName = \"ceph\";"));
        assert!(!nix.contains("clusterName = \"lab\""));
        assert!(nix.contains("monAddress = \"10.10.0.11,10.10.0.12\";"));
        assert!(nix.contains("monInitialMembers = \"dell-tower-1,dell-2\";"));
        assert!(nix.contains("publicAddr = \"10.10.0.11\";"));
        assert!(nix.contains("clusterAddr = \"10.20.0.11\";"));
        assert!(nix.contains("daemonId = \"dell-tower-1\";"));
        assert!(nix.contains("poolSize = 3;"));
    }
}
