//! Resume open live-migrate operations after a controller restart.
//!
//! Safe phases only:
//! - `Waiting` → re-call `WaitLiveMigrateReceive` then continue finalize path
//!   via marking Failed if wait fails (operator intervention).
//! - `Reassigned` / `FinalizingDest` / `FinalizingSource` → best-effort
//!   finalize RPCs on dest/source; mark Done when source release succeeds.
//!
//! `Preparing` / `Sending` are left alone (cancel or operator reset-session).

use std::time::Duration;

use tokio::time;
use tracing::{info, warn};

use kcore_sanitize::sanitize_nix_attr_key;

use crate::db::{Database, VmOperationRow};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(30);

pub fn spawn_vm_operation_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, &clients).await {
                warn!(error = %e, "vm operation reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    let open = db
        .list_vm_operations(false)
        .map_err(|e| format!("list open ops: {e}"))?;
    for op in open {
        if op.kind != VmOperationRow::KIND_LIVE_MIGRATE {
            continue;
        }
        match op.phase.as_str() {
            p if p == VmOperationRow::PHASE_WAITING => {
                if let Err(e) = resume_waiting(db, clients, &op).await {
                    warn!(op = %op.id, error = %e, "resume Waiting failed");
                }
            }
            p if p == VmOperationRow::PHASE_REASSIGNED
                || p == VmOperationRow::PHASE_FINALIZING_DEST
                || p == VmOperationRow::PHASE_FINALIZING_SOURCE =>
            {
                if let Err(e) = resume_finalize(db, clients, &op).await {
                    warn!(op = %op.id, error = %e, "resume finalize failed");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn resume_waiting(
    db: &Database,
    clients: &NodeClients,
    op: &VmOperationRow,
) -> Result<(), String> {
    let vm = db
        .get_vm(&op.vm_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "VM missing".to_string())?;
    let target = db
        .get_node(&op.target_node)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "target node missing".to_string())?;
    if clients.get_admin(&target.address).is_none() {
        clients
            .connect(&target.address)
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut admin = clients
        .get_admin(&target.address)
        .ok_or_else(|| "admin client unavailable".to_string())?;
    let runtime_name = sanitize_nix_attr_key(&vm.name);
    let wait = admin
        .wait_live_migrate_receive(node_proto::WaitLiveMigrateReceiveRequest {
            vm_name: runtime_name,
            timeout_seconds: 120,
        })
        .await
        .map_err(|e| e.to_string())?
        .into_inner();
    if !wait.success {
        let _ = db.update_vm_operation_phase(
            &op.id,
            VmOperationRow::PHASE_FAILED,
            Some(true),
            Some(&format!(
                r#"{{"resumeWait":"{}"}}"#,
                wait.message.replace('"', "'")
            )),
            true,
        );
        return Err(wait.message);
    }
    let _ = db.update_vm_operation_phase(
        &op.id,
        VmOperationRow::PHASE_REASSIGNED,
        Some(true),
        None,
        false,
    );
    info!(op = %op.id, "resumed Waiting → Reassigned");
    Ok(())
}

async fn resume_finalize(
    db: &Database,
    clients: &NodeClients,
    op: &VmOperationRow,
) -> Result<(), String> {
    let vm = db
        .get_vm(&op.vm_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "VM missing".to_string())?;
    let target = db
        .get_node(&op.target_node)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "target missing".to_string())?;
    let source = db
        .get_node(&op.source_node)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "source missing".to_string())?;
    let runtime_name = sanitize_nix_attr_key(&vm.name);

    if clients.get_admin(&target.address).is_none() {
        let _ = clients.connect(&target.address).await;
    }
    if let Some(mut admin) = clients.get_admin(&target.address) {
        let _ = db.update_vm_operation_phase(
            &op.id,
            VmOperationRow::PHASE_FINALIZING_DEST,
            Some(true),
            None,
            false,
        );
        let _ = admin
            .finalize_live_migrate_dest(node_proto::FinalizeLiveMigrateDestRequest {
                vm_name: runtime_name.clone(),
            })
            .await;
    }

    let volumes = db
        .list_attached_volumes_for_vm(&vm.id)
        .map_err(|e| e.to_string())?;
    let rbd_volumes: Vec<node_proto::RbdImageRef> = volumes
        .iter()
        .map(|v| node_proto::RbdImageRef {
            pool: v.pool.clone(),
            image: v.image.clone(),
        })
        .collect();
    let (pool, image) = rbd_volumes
        .first()
        .map(|v| (v.pool.clone(), v.image.clone()))
        .unwrap_or_default();

    if clients.get_admin(&source.address).is_none() {
        let _ = clients.connect(&source.address).await;
    }
    if let Some(mut admin) = clients.get_admin(&source.address) {
        let _ = db.update_vm_operation_phase(
            &op.id,
            VmOperationRow::PHASE_FINALIZING_SOURCE,
            Some(true),
            None,
            false,
        );
        match admin
            .finalize_live_migrate_source(node_proto::FinalizeLiveMigrateSourceRequest {
                vm_name: runtime_name,
                rbd_pool: pool,
                rbd_image: image,
                rbd_volumes,
            })
            .await
        {
            Ok(resp) => {
                let resp = resp.into_inner();
                if resp.vmm_stopped && resp.rbd_unmapped {
                    let _ = db.update_vm_operation_phase(
                        &op.id,
                        VmOperationRow::PHASE_DONE,
                        Some(true),
                        None,
                        true,
                    );
                    info!(op = %op.id, "resume finalize → Done");
                    return Ok(());
                }
                Err(format!(
                    "source not released: vmm_stopped={} rbd_unmapped={}",
                    resp.vmm_stopped, resp.rbd_unmapped
                ))
            }
            Err(e) => Err(e.to_string()),
        }
    } else {
        Err("source admin unavailable".into())
    }
}
