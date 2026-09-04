use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::warn;

use crate::ceph_cluster_spec;
use crate::db::{CephClusterRow, CephClusterStatusRow, Database};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(15);

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
            db.upsert_ceph_cluster_status(&CephClusterStatusRow {
                name: row.name.clone(),
                observed_generation: row.generation,
                phase: "failed".into(),
                health_message: error.clone(),
                ceph_status_json: String::new(),
                last_transition_at: String::new(),
            })
            .map_err(|e| e.to_string())?;
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

    for (index, node) in spec.nodes.iter().enumerate() {
        let daemon_id = sanitize_daemon_id(&node.node_id, index);
        let mut admin = admin_for(db, clients, &node.node_id).await?;
        let applied = admin
            .apply_ceph_config(Request::new(node_proto::ApplyCephConfigRequest {
                ceph_nix: generate_ceph_nix(&row.name, &spec, node, &daemon_id),
                rebuild: true,
                fsid: spec.fsid.clone(),
                mon: true,
                mgr: true,
                osd: true,
                daemon_id,
                keyring: Vec::new(),
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !applied.success {
            return Err(applied.message);
        }
        let osd = admin
            .bootstrap_ceph_osd(Request::new(node_proto::BootstrapCephOsdRequest {
                osd_device: node.osd_device.clone(),
                force_wipe: spec.force_wipe,
                cluster_name: row.name.clone(),
            }))
            .await
            .map_err(|e| e.to_string())?
            .into_inner();
        if !osd.success {
            return Err(osd.message);
        }
    }

    let first = spec
        .nodes
        .first()
        .ok_or_else(|| "cluster has no nodes".to_string())?;
    let mut admin = admin_for(db, clients, &first.node_id).await?;
    let health = admin
        .get_ceph_health(Request::new(node_proto::GetCephHealthRequest {
            cluster_name: row.name.clone(),
        }))
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    let phase = match health.health_status.as_str() {
        "HEALTH_OK" => "healthy",
        "HEALTH_WARN" => "degraded",
        _ => "failed",
    };
    db.upsert_ceph_cluster_status(&CephClusterStatusRow {
        name: row.name.clone(),
        observed_generation: row.generation,
        phase: phase.into(),
        health_message: format!(
            "{} (osd up {}, in {})",
            health.health_status, health.osd_up, health.osd_in
        ),
        ceph_status_json: health.raw_status,
        last_transition_at: String::new(),
    })
    .map_err(|e| e.to_string())
}

fn sanitize_daemon_id(node_id: &str, index: usize) -> String {
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

fn generate_ceph_nix(
    cluster_name: &str,
    spec: &crate::controller_proto::CephClusterSpec,
    node: &crate::controller_proto::CephClusterNodeSpec,
    daemon_id: &str,
) -> String {
    format!(
        r#"{{ ... }}: {{
  kcore.ceph = {{
    enable = true;
    clusterName = "{cluster_name}";
    fsid = "{fsid}";
    publicNetwork = "{public_network}";
    clusterNetwork = "{cluster_network}";
    monAddress = "{mon_addr}";
    daemonId = "{daemon_id}";
    enableMon = true;
    enableMgr = true;
    enableOsd = true;
    poolSize = {size};
    poolMinSize = {min_size};
  }};
}}
"#,
        fsid = spec.fsid,
        public_network = spec.public_network,
        cluster_network = spec.cluster_network,
        mon_addr = node.mon_addr,
        size = spec.size,
        min_size = spec.min_size
    )
}
