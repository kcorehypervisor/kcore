//! DiskLayout reconciler.
//!
//! Every tick, list every `disk_layouts` row whose most recent
//! `disk_layout_status.observed_generation` is behind the row's `generation`
//! (or has no status yet), connect to the owning node, and call
//! [`ApplyDiskLayout`] with `apply = true` and `rebuild = true`.
//!
//! By default the reconciler refuses to apply while the node still hosts VMs
//! (the node-agent would refuse in-use disks anyway). With `evacuate = true`
//! the controller cordons the node, drains VMs using the same paths as
//! `DrainNode`, applies the layout, then uncordons.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::{debug, error, info, warn};

use crate::config::NetworkConfig;
use crate::db::{Database, DiskLayoutRow, DiskLayoutStatusRow};
use crate::grpc::{ControllerService, NodeDrainOptions, SubCaState};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(15);
const NODE_RPC_TIMEOUT_SECS: i32 = 600;

/// Dependencies beyond DB + node clients (Nix push during evacuate drain).
pub struct DiskLayoutReconcilerConfig {
    pub default_network: NetworkConfig,
    pub sub_ca: Arc<Mutex<SubCaState>>,
}

/// Spawn the DiskLayout reconciler loop in the background.
pub fn spawn_disk_layout_reconciler(
    db: Database,
    clients: NodeClients,
    cfg: DiskLayoutReconcilerConfig,
) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        // First tick fires immediately, so skip it to avoid a thundering herd
        // on process start.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, &clients, &cfg).await {
                warn!(error = %e, "disk layout reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(
    db: &Database,
    clients: &NodeClients,
    cfg: &DiskLayoutReconcilerConfig,
) -> Result<(), String> {
    let pending: Vec<DiskLayoutRow> = db
        .list_disk_layouts_needing_reconcile()
        .map_err(|e| format!("list pending disk layouts: {e}"))?;
    if pending.is_empty() {
        return Ok(());
    }
    debug!(count = pending.len(), "reconciling disk layouts");
    for row in pending {
        if let Err(e) = reconcile_row(db, clients, cfg, &row).await {
            warn!(
                error = %e,
                name = %row.name,
                node_id = %row.node_id,
                generation = row.generation,
                "disk layout reconcile row failed"
            );
            let _ = db.upsert_disk_layout_status(&DiskLayoutStatusRow {
                name: row.name.clone(),
                observed_generation: 0,
                phase: "failed".to_string(),
                refusal_reason: String::new(),
                message: format!("controller reconcile error: {e}"),
                last_transition_at: String::new(),
            });
        }
    }
    Ok(())
}

async fn reconcile_row(
    db: &Database,
    clients: &NodeClients,
    cfg: &DiskLayoutReconcilerConfig,
    row: &DiskLayoutRow,
) -> Result<(), String> {
    if row.evacuate {
        reconcile_row_with_evacuate(db, clients, cfg, row).await
    } else {
        reconcile_row_refuse_if_vms(db, clients, row).await
    }
}

async fn reconcile_row_refuse_if_vms(
    db: &Database,
    clients: &NodeClients,
    row: &DiskLayoutRow,
) -> Result<(), String> {
    let vms = db
        .list_vms_for_node(&row.node_id)
        .map_err(|e| format!("listing vms on {}: {e}", row.node_id))?;
    if !vms.is_empty() {
        let names = vms
            .iter()
            .map(|vm| vm.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "node {} still hosts {} VM(s) ({names}); drain or migrate them before applying a disk layout",
            row.node_id,
            vms.len()
        ));
    }
    apply_disk_layout_on_node(db, clients, row).await
}

async fn reconcile_row_with_evacuate(
    db: &Database,
    clients: &NodeClients,
    cfg: &DiskLayoutReconcilerConfig,
    row: &DiskLayoutRow,
) -> Result<(), String> {
    let node = db
        .get_node(&row.node_id)
        .map_err(|e| format!("lookup node {}: {e}", row.node_id))?
        .ok_or_else(|| format!("node {} not registered", row.node_id))?;
    let prior_status = node.status.clone();
    let restore_status = schedulable_status_after_evacuate(&prior_status);

    db.update_node_status(&row.node_id, "draining")
        .map_err(|e| format!("cordon node {}: {e}", row.node_id))?;
    db.upsert_disk_layout_status(&DiskLayoutStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "pending".to_string(),
        refusal_reason: String::new(),
        message: "cordoned node (not schedulable); evacuating VMs".to_string(),
        last_transition_at: String::new(),
    })
    .map_err(|e| format!("update status for {}: {e}", row.name))?;

    let workload = ControllerService::new(
        db.clone(),
        clients.clone(),
        cfg.default_network.clone(),
        cfg.sub_ca.clone(),
        None,
        false,
        false,
    );

    let drain_outcome = workload
        .drain_node_vms(
            &row.node_id,
            "",
            NodeDrainOptions {
                mark_draining_at_start: false,
                update_final_node_status: false,
            },
        )
        .await
        .map_err(|e| format!("evacuate VMs on {}: {e}", row.node_id))?;

    if !drain_outcome.errors.is_empty() {
        let detail = drain_outcome.errors.join("; ");
        let _ = db.update_node_status(&row.node_id, &restore_status);
        db.upsert_disk_layout_status(&DiskLayoutStatusRow {
            name: row.name.clone(),
            observed_generation: row.generation,
            phase: "refused".to_string(),
            refusal_reason: "evacuation_failed".to_string(),
            message: detail,
            last_transition_at: String::new(),
        })
        .map_err(|e| format!("upsert status for {}: {e}", row.name))?;
        info!(
            name = %row.name,
            node_id = %row.node_id,
            "disk layout evacuate refused: drain errors"
        );
        return Ok(());
    }

    let remaining = db
        .list_vms_for_node(&row.node_id)
        .map_err(|e| format!("listing vms on {} after drain: {e}", row.node_id))?;
    if !remaining.is_empty() {
        let names = remaining
            .iter()
            .map(|vm| vm.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = db.update_node_status(&row.node_id, &restore_status);
        db.upsert_disk_layout_status(&DiskLayoutStatusRow {
            name: row.name.clone(),
            observed_generation: row.generation,
            phase: "refused".to_string(),
            refusal_reason: "evacuation_failed".to_string(),
            message: format!("node still hosts VM(s): {names}"),
            last_transition_at: String::new(),
        })
        .map_err(|e| format!("upsert status for {}: {e}", row.name))?;
        return Ok(());
    }

    db.upsert_disk_layout_status(&DiskLayoutStatusRow {
        name: row.name.clone(),
        observed_generation: 0,
        phase: "pending".to_string(),
        refusal_reason: String::new(),
        message: "evacuation complete; applying disk layout".to_string(),
        last_transition_at: String::new(),
    })
    .map_err(|e| format!("update status for {}: {e}", row.name))?;

    let apply_result = apply_disk_layout_on_node(db, clients, row).await;
    let _ = db.update_node_status(&row.node_id, &restore_status);
    apply_result
}

async fn apply_disk_layout_on_node(
    db: &Database,
    clients: &NodeClients,
    row: &DiskLayoutRow,
) -> Result<(), String> {
    let node = db
        .get_node(&row.node_id)
        .map_err(|e| format!("lookup node {}: {e}", row.node_id))?
        .ok_or_else(|| format!("node {} not registered", row.node_id))?;
    let address = node.address.clone();
    if address.is_empty() {
        return Err(format!("node {} has no address", row.node_id));
    }

    if clients.get_admin(&address).is_none() {
        clients
            .connect(&address)
            .await
            .map_err(|e| format!("connect to node {address}: {e}"))?;
    }
    let mut admin = clients
        .get_admin(&address)
        .ok_or_else(|| format!("no admin client for {address}"))?;

    let req = node_proto::ApplyDiskLayoutRequest {
        disk_layout_nix: row.layout_nix.clone(),
        apply: true,
        timeout_seconds: NODE_RPC_TIMEOUT_SECS,
        rebuild: true,
    };
    let resp = admin
        .apply_disk_layout(Request::new(req))
        .await
        .map_err(|e| format!("apply_disk_layout rpc: {e}"))?
        .into_inner();

    let phase = if resp.success {
        "applied"
    } else if !resp.refusal_reason.is_empty() {
        "refused"
    } else {
        "failed"
    };
    db.upsert_disk_layout_status(&DiskLayoutStatusRow {
        name: row.name.clone(),
        observed_generation: row.generation,
        phase: phase.to_string(),
        refusal_reason: resp.refusal_reason.clone(),
        message: resp.message.clone(),
        last_transition_at: String::new(),
    })
    .map_err(|e| format!("upsert status for {}: {e}", row.name))?;

    match phase {
        "applied" => info!(
            name = %row.name,
            node_id = %row.node_id,
            generation = row.generation,
            "disk layout applied on node"
        ),
        "refused" => info!(
            name = %row.name,
            node_id = %row.node_id,
            refusal_reason = %resp.refusal_reason,
            "node-agent refused disk layout; operator must drain VMs and resubmit"
        ),
        _ => error!(
            name = %row.name,
            node_id = %row.node_id,
            message = %resp.message,
            "disk layout apply failed on node"
        ),
    }
    Ok(())
}

/// Node status to restore after evacuate so the scheduler accepts new VMs again.
fn schedulable_status_after_evacuate(prior_status: &str) -> String {
    match prior_status {
        "draining" | "drained" => "ready".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedulable_status_restores_ready_from_drain_states() {
        assert_eq!(schedulable_status_after_evacuate("draining"), "ready");
        assert_eq!(schedulable_status_after_evacuate("drained"), "ready");
        assert_eq!(schedulable_status_after_evacuate("not-ready"), "not-ready");
    }
}
