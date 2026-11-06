//! Reconciles [`ClusterUpdate`](crate::controller_proto::ClusterUpdate) rows by
//! calling [`PrepareSystemUpdate`](crate::node_proto::node_admin_client::NodeAdminClient::prepare_system_update)
//! then [`ActivateSystemUpdate`](crate::node_proto::node_admin_client::NodeAdminClient::activate_system_update)
//! on each target node, one at a time. The first node that fails stops the
//! rollout: nodes not yet started are cancelled and the failed node is left
//! on (or put back on) the generation it ran before.
//!
//! A canary update soaks its first node on the target and compares it with
//! the other nodes before any other node starts.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::{debug, warn};

use crate::cluster_update_spec::{activation_mode_string, spec_from_json};
use crate::config::NetworkConfig;
use crate::db::{ClusterUpdateNodeRow, ClusterUpdateRow, Database};
use crate::grpc::{ControllerService, NodeDrainOptions, SubCaState};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(12);
const NODE_PREPARE_TIMEOUT_SECS: i32 = 3600;
const NODE_ACTIVATE_TIMEOUT_SECS: i32 = 900;
/// Longer than the node's own switch guard (900s), so a node that reverts a
/// switch reports why before the controller gives up on it.
const ACTIVATE_CONFIRM_SECS: i64 = 1200;
/// From the reboot until the node confirms the staged generation. Covers the
/// boot, kcore-boot-confirm's own timeout, and a reboot back.
const REBOOT_CONFIRM_SECS: i64 = 1800;

/// What the reconciler needs to migrate guests before a reboot.
pub struct ClusterUpdateReconcilerConfig {
    pub default_network: NetworkConfig,
    pub sub_ca: Arc<Mutex<SubCaState>>,
}

pub fn spawn_cluster_update_reconciler(
    db: Database,
    clients: NodeClients,
    cfg: ClusterUpdateReconcilerConfig,
) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, &clients, &cfg).await {
                warn!(error = %e, "cluster update reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(
    db: &Database,
    clients: &NodeClients,
    cfg: &ClusterUpdateReconcilerConfig,
) -> Result<(), String> {
    let updates: Vec<ClusterUpdateRow> = db
        .list_cluster_updates_for_reconcile()
        .map_err(|e| format!("list cluster updates: {e}"))?;
    if updates.is_empty() {
        return Ok(());
    }
    debug!(count = updates.len(), "cluster update reconcile");
    for u in updates {
        if let Err(e) = reconcile_update(db, clients, cfg, &u).await {
            warn!(
                error = %e,
                name = %u.name,
                "cluster update reconcile failed"
            );
        }
    }
    Ok(())
}

async fn reconcile_update(
    db: &Database,
    clients: &NodeClients,
    cfg: &ClusterUpdateReconcilerConfig,
    update: &ClusterUpdateRow,
) -> Result<(), String> {
    let mut nodes = db
        .list_cluster_update_nodes(&update.name)
        .map_err(|e| format!("list cluster_update_nodes: {e}"))?;
    if nodes.is_empty() {
        return Ok(());
    }

    if update.phase == "ready" {
        db.patch_cluster_update_phase(&update.name, "rolling_out")
            .map_err(|e| format!("patch phase rolling_out: {e}"))?;
    }

    if update.phase != "rolling_back" {
        if let Some(failed) = first_failed(&nodes) {
            for node_row in nodes.iter_mut().filter(|n| not_started(&n.phase)) {
                node_row.phase = "cancelled".into();
                node_row.last_error = format!("not started: {failed} failed");
                db.upsert_cluster_update_node(node_row)
                    .map_err(|e| format!("cancel node after failure: {e}"))?;
            }
        }
    }

    // Terminal cluster phases handled below when all nodes settle.

    if nodes
        .iter()
        .all(|n| n.phase == "succeeded" || n.phase == "cancelled" || n.phase == "failed")
    {
        let any_failed = nodes.iter().any(|n| n.phase == "failed");
        let final_phase = if any_failed {
            "failed"
        } else if nodes.iter().all(|n| n.phase == "cancelled") {
            "cancelled"
        } else {
            "succeeded"
        };
        db.patch_cluster_update_phase(&update.name, final_phase)
            .map_err(|e| format!("finalize cluster update: {e}"))?;
        crate::canary::forget(&update.name);
        return Ok(());
    }

    let spec = spec_from_json(&update.spec_json).map_err(|e| format!("parse spec_json: {e}"))?;
    let activation = activation_mode_string(&spec).to_string();
    rollout_order(db, &mut nodes);
    let ordered: Vec<String> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let canary = crate::canary::pick(db, &spec, &ordered);
    if let Some(pos) = canary
        .as_deref()
        .and_then(|id| nodes.iter().position(|n| n.node_id == id))
    {
        let row = nodes.remove(pos);
        nodes.insert(0, row);
    }

    // Pick the first node still needing work. The canary goes first and
    // controllers last.
    for node_row in &mut nodes {
        let node = db
            .get_node(&node_row.node_id)
            .map_err(|e| format!("get node: {e}"))?
            .ok_or_else(|| format!("node {} disappeared", node_row.node_id))?;
        let address = node.address.clone();
        if address.is_empty() {
            warn!(node_id = %node.id, "cluster update: node has no address");
            continue;
        }

        match node_row.phase.as_str() {
            "pending" => {
                let host = db.get_node_host_status(&node_row.node_id).ok().flatten();
                let has_generation = host.as_ref().is_some_and(|row| {
                    row.update_capabilities
                        .split(',')
                        .any(|cap| cap == "nixos-generation")
                });
                let req = node_proto::PrepareSystemUpdateRequest {
                    update_name: update.name.clone(),
                    flake_ref: update.flake_ref.clone(),
                    flake_rev: update.flake_rev.clone(),
                    system_profile: spec
                        .target
                        .as_ref()
                        .map(|t| t.system_profile.clone())
                        .unwrap_or_default(),
                    host_system: String::new(),
                    timeout_seconds: NODE_PREPARE_TIMEOUT_SECS,
                    update_kind: if has_generation {
                        if spec.update_kind.is_empty() {
                            "kcore".into()
                        } else {
                            spec.update_kind.clone()
                        }
                    } else {
                        "bootstrap".into()
                    },
                    nixpkgs_rev: spec
                        .target
                        .as_ref()
                        .map(|t| t.nixpkgs_rev.clone())
                        .unwrap_or_default(),
                    flake_tag: if has_generation {
                        spec.target
                            .as_ref()
                            .map(|t| t.tag.clone())
                            .unwrap_or_default()
                    } else {
                        String::new()
                    },
                    ..Default::default()
                };
                let result = prepare_on_node(clients, &address, req).await;
                match result {
                    Ok(resp) if resp.success => {
                        let closure = resp.prepared_closure.clone();
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: "prepared".into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: closure,
                            current_generation: resp.current_generation,
                            target_generation: resp.target_generation,
                            requires_reboot: resp.requires_reboot,
                            last_error: String::new(),
                            last_transition_at: String::new(),
                        })
                        .map_err(|e| format!("upsert node prepared: {e}"))?;
                    }
                    Ok(resp) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: "failed".into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: String::new(),
                            current_generation: String::new(),
                            target_generation: String::new(),
                            requires_reboot: false,
                            last_error: resp.message,
                            last_transition_at: String::new(),
                        })
                        .map_err(|e| format!("mark node failed: {e}"))?;
                    }
                    Err(e) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: "failed".into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: String::new(),
                            current_generation: String::new(),
                            target_generation: String::new(),
                            requires_reboot: false,
                            last_error: e.clone(),
                            last_transition_at: String::new(),
                        })
                        .map_err(|e2| format!("mark node rpc failed: {e2}"))?;
                    }
                }
                return Ok(());
            }
            "prepared" => {
                if let Ok(Some(member)) = db.get_slurm_cluster_node(&node_row.node_id) {
                    if member.roles.split(',').any(|role| role == "compute") {
                        match drain_slurm_member(db, clients, &node_row.node_id).await {
                            Ok(()) => {
                                db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                                    phase: "drained".into(),
                                    ..node_row.clone()
                                })
                                .map_err(|e| format!("mark slurm node drained: {e}"))?;
                            }
                            Err(err) => {
                                db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                                    phase: "failed".into(),
                                    last_error: err,
                                    ..node_row.clone()
                                })
                                .map_err(|e| format!("mark slurm drain failed: {e}"))?;
                            }
                        }
                        return Ok(());
                    }
                }
                match kubernetes_host_action(db, &node_row.node_id, "prepared") {
                    Ok(crate::kubernetes_lifecycle::HostAction::WaitOtherControlPlane) => {
                        return Ok(());
                    }
                    Ok(crate::kubernetes_lifecycle::HostAction::Drain { status }) => {
                        let recorded = status.unwrap_or("").to_string();
                        match drain_kubernetes_member(db, clients, &node_row.node_id).await {
                            Ok(()) => {
                                db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                                    phase: "drained".into(),
                                    last_error: recorded,
                                    ..node_row.clone()
                                })
                                .map_err(|e| format!("mark node drained: {e}"))?;
                            }
                            Err(err) => {
                                db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                                    phase: "failed".into(),
                                    last_error: err,
                                    ..node_row.clone()
                                })
                                .map_err(|e| format!("mark drain failed: {e}"))?;
                            }
                        }
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(err) => return Err(err),
                }
                let closure = node_row.prepared_closure.clone();
                let req = node_proto::ActivateSystemUpdateRequest {
                    update_name: update.name.clone(),
                    activation_mode: activation.clone(),
                    prepared_closure: closure,
                };
                let failed_phase = if may_confirm_activation(node_row, &activation) {
                    // The switch can restart this controller before the call
                    // returns. Its successor must confirm, not switch again.
                    db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "confirming".into(),
                        ..node_row.clone()
                    })
                    .map_err(|e| format!("mark node confirming: {e}"))?;
                    "confirming"
                } else {
                    "failed"
                };
                let result = activate_on_node(clients, &address, req).await;
                match result {
                    Ok(resp) if resp.success => {
                        let has_generation = db
                            .get_node_host_status(&node_row.node_id)
                            .ok()
                            .flatten()
                            .is_some_and(|row| {
                                row.update_capabilities
                                    .split(',')
                                    .any(|cap| cap == "nixos-generation")
                            });
                        let phase = if resp.requires_reboot || resp.phase == "reboot_required" {
                            "reboot_required"
                        } else if !has_generation {
                            "bootstrapping"
                        } else {
                            finished_phase(canary.as_deref(), &node_row.node_id)
                        };
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: phase.into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: node_row.prepared_closure.clone(),
                            current_generation: node_row.current_generation.clone(),
                            target_generation: activated_generation(node_row, &resp),
                            requires_reboot: resp.requires_reboot,
                            last_error: String::new(),
                            last_transition_at: String::new(),
                        })
                        .map_err(|e| format!("mark node {phase}: {e}"))?;
                    }
                    Ok(resp) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: failed_phase.into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: node_row.prepared_closure.clone(),
                            current_generation: node_row.current_generation.clone(),
                            target_generation: node_row.target_generation.clone(),
                            requires_reboot: node_row.requires_reboot,
                            last_error: resp.message,
                            last_transition_at: String::new(),
                        })
                        .map_err(|e| format!("mark activate failed: {e}"))?;
                    }
                    Err(e) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            update_name: update.name.clone(),
                            node_id: node_row.node_id.clone(),
                            observed_generation: update.generation,
                            phase: failed_phase.into(),
                            current_version: String::new(),
                            target_version: update.target_version.clone(),
                            prepared_closure: node_row.prepared_closure.clone(),
                            current_generation: node_row.current_generation.clone(),
                            target_generation: node_row.target_generation.clone(),
                            requires_reboot: node_row.requires_reboot,
                            last_error: e.clone(),
                            last_transition_at: String::new(),
                        })
                        .map_err(|e2| format!("mark activate rpc failed: {e2}"))?;
                    }
                }
                return Ok(());
            }
            "drained" => {
                let closure = node_row.prepared_closure.clone();
                let req = node_proto::ActivateSystemUpdateRequest {
                    update_name: update.name.clone(),
                    activation_mode: activation.clone(),
                    prepared_closure: closure,
                };
                match activate_on_node(clients, &address, req).await {
                    Ok(resp) if resp.success => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: "waiting_ready".into(),
                            target_generation: activated_generation(node_row, &resp),
                            requires_reboot: resp.requires_reboot,
                            last_error: node_row.last_error.clone(),
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark node waiting_ready: {e}"))?;
                    }
                    Ok(resp) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: "failed".into(),
                            last_error: resp.message,
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark activate failed: {e}"))?;
                    }
                    Err(err) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: "failed".into(),
                            last_error: err,
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark activate rpc failed: {e}"))?;
                    }
                }
                return Ok(());
            }
            "waiting_ready" => {
                match kubernetes_ready_for_uncordon(db, clients, node_row).await {
                    Ok(true) => {
                        if let Err(err) =
                            uncordon_kubernetes_member(db, clients, &node_row.node_id).await
                        {
                            db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                                last_error: err,
                                ..node_row.clone()
                            })
                            .map_err(|e| format!("record uncordon error: {e}"))?;
                            return Ok(());
                        }
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: finished_phase(canary.as_deref(), &node_row.node_id).into(),
                            last_error: String::new(),
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark node succeeded: {e}"))?;
                    }
                    Ok(false) => {}
                    Err(err) => {
                        warn!(
                            node_id = %node_row.node_id,
                            error = %err,
                            "kubernetes node is not ready for uncordon"
                        );
                    }
                }
                return Ok(());
            }
            "bootstrapping" => {
                let has_generation = db
                    .get_node_host_status(&node_row.node_id)
                    .ok()
                    .flatten()
                    .is_some_and(|row| {
                        row.update_capabilities
                            .split(',')
                            .any(|cap| cap == "nixos-generation")
                    });
                if has_generation {
                    db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "pending".into(),
                        last_error: String::new(),
                        ..node_row.clone()
                    })
                    .map_err(|e| format!("generation follow-up: {e}"))?;
                }
            }
            "confirming" => {
                let status = status_on_node(
                    clients,
                    &address,
                    node_proto::GetSystemUpdateStatusRequest {
                        update_name: node_row.update_name.clone(),
                    },
                )
                .await
                .ok();
                if let Some(status) = status.as_ref().filter(|s| s.phase == "failed") {
                    db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "failed".into(),
                        last_error: status.message.clone(),
                        ..node_row.clone()
                    })
                    .map_err(|e| format!("mark reverted node failed: {e}"))?;
                    return Ok(());
                }
                let node_confirmed = status.as_ref().is_some_and(|s| s.phase != "switch_pending");
                let host = db.get_node_host_status(&node_row.node_id).ok().flatten();
                let running = host
                    .as_ref()
                    .map(|row| row.nixos_generation.clone())
                    .unwrap_or_default();
                refresh_target_generation(db, clients, &address, node_row, &running).await?;
                let age_secs = db
                    .cluster_update_node_age_secs(&update.name, &node_row.node_id)
                    .map_err(|e| format!("read confirming age: {e}"))?;
                let outcome = confirm_activation(
                    host.as_ref()
                        .map(|row| row.nixos_generation.as_str())
                        .unwrap_or(""),
                    host.as_ref()
                        .map(|row| row.booted_generation.as_str())
                        .unwrap_or(""),
                    &node_row.target_generation,
                    node_confirmed,
                    age_secs,
                );
                match outcome {
                    Some(true) => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: finished_phase(canary.as_deref(), &node_row.node_id).into(),
                            last_error: String::new(),
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark confirmed node succeeded: {e}"))?;
                    }
                    Some(false) => {
                        let reason = format!(
                            "the node did not confirm the target generation within {ACTIVATE_CONFIRM_SECS}s"
                        );
                        fail_and_revert(
                            db,
                            clients,
                            cfg,
                            &address,
                            node_row,
                            spec.drain_vms,
                            reason,
                        )
                        .await?;
                    }
                    None => {}
                }
                return Ok(());
            }
            "reboot_required" | "rebooting" => {
                let status = status_on_node(
                    clients,
                    &address,
                    node_proto::GetSystemUpdateStatusRequest {
                        update_name: node_row.update_name.clone(),
                    },
                )
                .await;
                let age_secs = db
                    .cluster_update_node_age_secs(&update.name, &node_row.node_id)
                    .map_err(|e| format!("read reboot age: {e}"))?;
                let timed_out = node_row.phase == "rebooting" && age_secs >= REBOOT_CONFIRM_SECS;
                let Ok(status) = status else {
                    if timed_out {
                        let reason = format!(
                            "the node did not come back on the staged generation within {REBOOT_CONFIRM_SECS}s"
                        );
                        fail_and_revert(
                            db,
                            clients,
                            cfg,
                            &address,
                            node_row,
                            spec.drain_vms,
                            reason,
                        )
                        .await?;
                    }
                    return Ok(());
                };
                if status.phase == "failed" {
                    db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                        phase: "failed".into(),
                        last_error: format!(
                            "{}; the node booted its previous generation",
                            status.message
                        ),
                        ..node_row.clone()
                    })
                    .map_err(|e| format!("mark reverted node failed: {e}"))?;
                    return Ok(());
                }
                if !status.target_generation.is_empty()
                    && status.target_generation != node_row.target_generation
                {
                    node_row.target_generation = status.target_generation.clone();
                }
                match reboot_step(node_row, &status, &spec.reboot_policy, timed_out) {
                    RebootStep::Succeeded => {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: finished_phase(canary.as_deref(), &node_row.node_id).into(),
                            requires_reboot: false,
                            last_error: String::new(),
                            ..node_row.clone()
                        })
                        .map_err(|e| format!("mark rebooted node succeeded: {e}"))?;
                    }
                    RebootStep::Reboot => {
                        let svc = ControllerService::new(
                            db.clone(),
                            clients.clone(),
                            cfg.default_network.clone(),
                            cfg.sub_ca.clone(),
                            None,
                            false,
                            false,
                        );
                        let drain = NodeDrainOptions {
                            mark_draining_at_start: false,
                            update_final_node_status: false,
                        };
                        match svc
                            .reboot_update_node(node_row, spec.drain_vms, drain)
                            .await
                        {
                            Ok(message) => {
                                debug!(node_id = %node_row.node_id, %message, "cluster update reboot");
                            }
                            Err(err) => {
                                let reason = format!("reboot: {}", err.message());
                                fail_and_revert(
                                    db,
                                    clients,
                                    cfg,
                                    &address,
                                    node_row,
                                    spec.drain_vms,
                                    reason,
                                )
                                .await?;
                            }
                        }
                    }
                    RebootStep::TimedOut => {
                        let reason = format!(
                            "the staged generation was not confirmed within {REBOOT_CONFIRM_SECS}s"
                        );
                        fail_and_revert(
                            db,
                            clients,
                            cfg,
                            &address,
                            node_row,
                            spec.drain_vms,
                            reason,
                        )
                        .await?;
                    }
                    RebootStep::Wait => {}
                }
                return Ok(());
            }
            "soaking" | "awaiting_promotion" => {
                soak_step(db, clients, cfg, &address, update, &spec, node_row).await?;
                return Ok(());
            }
            "rolling_back" => {
                let schema_owner = db
                    .get_node_host_status(&node_row.node_id)
                    .ok()
                    .flatten()
                    .is_some_and(|row| row.schema_version == crate::db::SCHEMA_VERSION);
                if schema_owner
                    && spec.previous_schema_version > 0
                    && spec.previous_schema_version < crate::db::SCHEMA_VERSION
                {
                    if let Err(e) = db.migrate_down(spec.previous_schema_version) {
                        db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                            phase: "failed".into(),
                            last_error: e,
                            ..node_row.clone()
                        })
                        .map_err(|err| format!("mark schema rollback failed: {err}"))?;
                        return Ok(());
                    }
                }
                let req = node_proto::RollbackSystemUpdateRequest {
                    update_name: update.name.clone(),
                };
                let last_error = match rollback_on_node(clients, &address, req).await {
                    Ok(resp) if resp.success && resp.reboot_required => {
                        let rebooted = reboot_into_previous(
                            db,
                            clients,
                            cfg,
                            &address,
                            node_row,
                            spec.drain_vms,
                        )
                        .await;
                        after_revert(&resp.message, rebooted)
                    }
                    _ => "rolled back (MVP)".into(),
                };
                db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                    update_name: update.name.clone(),
                    node_id: node_row.node_id.clone(),
                    observed_generation: update.generation,
                    phase: "cancelled".into(),
                    current_version: String::new(),
                    target_version: update.target_version.clone(),
                    prepared_closure: String::new(),
                    current_generation: String::new(),
                    target_generation: String::new(),
                    requires_reboot: false,
                    last_error,
                    last_transition_at: String::new(),
                })
                .map_err(|e| format!("mark rollback: {e}"))?;
                return Ok(());
            }
            _ => {}
        }
    }
    Ok(())
}

/// Slurm members by role, then other nodes, then controllers.
/// True when a controller in `peers` listens on the node's host.
pub(crate) fn runs_controller(
    db: &Database,
    peers: &[crate::db::ControllerPeerRow],
    node_id: &str,
) -> bool {
    let address = db
        .get_node(node_id)
        .ok()
        .flatten()
        .map(|node| node.address)
        .unwrap_or_default();
    let host = address.split(':').next().unwrap_or("");
    peers
        .iter()
        .any(|peer| peer.address == address || peer.address.split(':').next() == Some(host))
}

pub(crate) fn rollout_order(db: &Database, nodes: &mut [ClusterUpdateNodeRow]) {
    let peers = db.list_controller_peers().unwrap_or_default();
    nodes.sort_by(|left, right| {
        let slurm_rank = |node_id: &str| {
            let member = db.get_slurm_cluster_node(node_id).ok().flatten()?;
            let roles: Vec<&str> = member.roles.split(',').collect();
            if roles.contains(&"dbd") {
                Some(0u8)
            } else if roles.contains(&"controller") {
                Some(1u8)
            } else if roles.contains(&"compute") {
                Some(2u8)
            } else {
                Some(3u8)
            }
        };
        match (slurm_rank(&left.node_id), slurm_rank(&right.node_id)) {
            (Some(left_rank), Some(right_rank)) => {
                (left_rank, left.node_id.clone()).cmp(&(right_rank, right.node_id.clone()))
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => {
                let rank = |node_id: &str| {
                    (
                        u8::from(runs_controller(db, &peers, node_id)),
                        node_id.to_string(),
                    )
                };
                rank(&left.node_id).cmp(&rank(&right.node_id))
            }
        }
    });
}

/// A node that finished its update. The canary soaks before the rollout
/// moves on.
fn finished_phase(canary: Option<&str>, node_id: &str) -> &'static str {
    if canary == Some(node_id) {
        "soaking"
    } else {
        "succeeded"
    }
}

/// Judge the canary against every other node. A failure after the grace
/// period puts it back on its previous generation and stops the rollout. At
/// the end of the soak a pass lets the other nodes start, and no verdict
/// waits for `kctl update cluster promote`.
async fn soak_step(
    db: &Database,
    clients: &NodeClients,
    cfg: &ClusterUpdateReconcilerConfig,
    address: &str,
    update: &ClusterUpdateRow,
    spec: &crate::controller_proto::ClusterUpdateSpec,
    node_row: &ClusterUpdateNodeRow,
) -> Result<(), String> {
    use crate::canary::{self, Verdict};
    let running = db
        .get_node_host_status(&node_row.node_id)
        .ok()
        .flatten()
        .map(|row| row.kcore_version)
        .unwrap_or_default();
    let on_target = crate::cluster_version::parse_version(&running).is_some()
        && crate::cluster_version::parse_version(&running)
            == crate::cluster_version::parse_version(&update.target_version);
    let age_secs = db
        .cluster_update_node_age_secs(&update.name, &node_row.node_id)
        .map_err(|e| format!("read soak age: {e}"))?;
    let soak_secs = canary::soak_secs(spec);
    // Agent calls and Ceph latency right after the update are the update's
    // own noise; both count from the end of the grace period.
    let judged_secs = age_secs - canary::GRACE_SECS.min(soak_secs);
    let calls_since = (judged_secs >= 0)
        .then(|| {
            std::time::Instant::now()
                .checked_sub(Duration::from_secs(u64::try_from(judged_secs).unwrap_or(0)))
        })
        .flatten();
    let peers: Vec<canary::NodeSample> = db
        .list_nodes()
        .map_err(|e| format!("list nodes: {e}"))?
        .iter()
        .filter(|n| n.id != node_row.node_id)
        .map(|n| canary::sample(db, clients, &n.id, calls_since))
        .collect();
    let ceph = if calls_since.is_some() {
        canary::ceph_latency(db, clients, &update.name, &node_row.node_id).await
    } else {
        None
    };
    let verdict = canary::judge(
        &canary::sample(db, clients, &node_row.node_id, calls_since),
        on_target,
        &peers,
        ceph.as_ref(),
    );
    match soak_progress(&verdict, &node_row.phase, age_secs, soak_secs) {
        SoakStep::Revert => {
            let Verdict::Fail(reason) = verdict else {
                unreachable!("only a failure reverts");
            };
            canary::forget(&update.name);
            fail_and_revert(
                db,
                clients,
                cfg,
                address,
                node_row,
                spec.drain_vms,
                format!("canary: {reason}"),
            )
            .await
        }
        SoakStep::Promote => {
            canary::forget(&update.name);
            db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                phase: "succeeded".into(),
                last_error: String::new(),
                ..node_row.clone()
            })
            .map_err(|e| format!("promote canary: {e}"))
        }
        SoakStep::AwaitOperator => {
            let Verdict::Unknown(reason) = verdict else {
                unreachable!("only no verdict waits for the operator");
            };
            db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
                phase: "awaiting_promotion".into(),
                last_error: format!(
                    "canary soaked {soak_secs}s with no verdict: {reason}; run `kctl update cluster promote {}` or cancel the update",
                    update.name
                ),
                ..node_row.clone()
            })
            .map_err(|e| format!("hold canary: {e}"))
        }
        SoakStep::Wait => Ok(()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SoakStep {
    Revert,
    Promote,
    AwaitOperator,
    Wait,
}

fn soak_progress(
    verdict: &crate::canary::Verdict,
    phase: &str,
    age_secs: i64,
    soak_secs: i64,
) -> SoakStep {
    use crate::canary::{Verdict, GRACE_SECS};
    let past_grace = age_secs >= GRACE_SECS.min(soak_secs);
    match verdict {
        Verdict::Fail(_) if past_grace => SoakStep::Revert,
        _ if phase != "soaking" || age_secs < soak_secs => SoakStep::Wait,
        Verdict::Pass => SoakStep::Promote,
        Verdict::Unknown(_) => SoakStep::AwaitOperator,
        Verdict::Fail(_) => SoakStep::Wait,
    }
}

fn first_failed(nodes: &[ClusterUpdateNodeRow]) -> Option<String> {
    nodes
        .iter()
        .find(|n| n.phase == "failed")
        .map(|n| n.node_id.clone())
}

/// Nothing was activated on these nodes yet.
fn not_started(phase: &str) -> bool {
    matches!(phase, "pending" | "prepared")
}

#[derive(Debug, PartialEq, Eq)]
enum RebootStep {
    Succeeded,
    Reboot,
    TimedOut,
    Wait,
}

/// A staged boot succeeds once the node booted the target and
/// kcore-boot-confirm has blessed it. `auto` reboots a node that has not
/// booted it yet; any other policy waits for `kctl update cluster reboot`.
fn reboot_step(
    node_row: &ClusterUpdateNodeRow,
    status: &node_proto::GetSystemUpdateStatusResponse,
    reboot_policy: &str,
    timed_out: bool,
) -> RebootStep {
    let booted_target = !node_row.target_generation.is_empty()
        && crate::kubernetes_lifecycle::generation_is_current(
            "",
            &status.booted_generation,
            &node_row.target_generation,
            true,
        );
    if booted_target && status.phase == "activated" {
        RebootStep::Succeeded
    } else if timed_out {
        RebootStep::TimedOut
    } else if node_row.phase == "reboot_required" && reboot_policy == "auto" && !booted_target {
        RebootStep::Reboot
    } else {
        RebootStep::Wait
    }
}

/// Put the node back on the generation it ran before the update, then mark
/// it failed, which stops the rollout.
async fn fail_and_revert(
    db: &Database,
    clients: &NodeClients,
    cfg: &ClusterUpdateReconcilerConfig,
    address: &str,
    node_row: &ClusterUpdateNodeRow,
    drain_vms: bool,
    reason: String,
) -> Result<(), String> {
    let undo = rollback_on_node(
        clients,
        address,
        node_proto::RollbackSystemUpdateRequest {
            update_name: node_row.update_name.clone(),
        },
    )
    .await;
    let last_error = match undo {
        Ok(resp) if resp.success && resp.reboot_required => {
            let rebooted =
                reboot_into_previous(db, clients, cfg, address, node_row, drain_vms).await;
            format!("{reason}; {}", after_revert(&resp.message, rebooted))
        }
        Ok(resp) if resp.success => format!("{reason}; {}", resp.message),
        Ok(resp) => format!("{reason}; rollback failed: {}", resp.message),
        Err(e) => format!("{reason}; rollback failed: {e}"),
    };
    db.upsert_cluster_update_node(&ClusterUpdateNodeRow {
        phase: "failed".into(),
        last_error,
        ..node_row.clone()
    })
    .map_err(|e| format!("mark node failed: {e}"))
}

/// What `last_error` says after a revert that left the node on the update
/// generation. The agent's advice to reboot only stands when the controller
/// could not reboot the node itself.
fn after_revert(agent_message: &str, rebooted: Result<(), String>) -> String {
    match rebooted {
        Ok(()) => "rolled back; rebooting into the previous generation".into(),
        Err(why) => format!("{agent_message}; not rebooted: {why}"),
    }
}

/// A node rolled back while it ran the update generation stays on it until
/// it reboots. Move its guests off when the update drains, then reboot it
/// into the previous generation. `Err` says why the node was not rebooted.
async fn reboot_into_previous(
    db: &Database,
    clients: &NodeClients,
    cfg: &ClusterUpdateReconcilerConfig,
    address: &str,
    node_row: &ClusterUpdateNodeRow,
    drain_vms: bool,
) -> Result<(), String> {
    let guests = db.list_vms_for_node(&node_row.node_id).unwrap_or_default();
    if !guests.is_empty() {
        if !drain_vms {
            return Err("guests are still on the node and the update does not drain them".into());
        }
        let svc = ControllerService::new(
            db.clone(),
            clients.clone(),
            cfg.default_network.clone(),
            cfg.sub_ca.clone(),
            None,
            false,
            false,
        );
        let drain = NodeDrainOptions {
            mark_draining_at_start: false,
            update_final_node_status: false,
        };
        match svc.drain_node_vms(&node_row.node_id, "", drain).await {
            Ok(outcome) if outcome.errors.is_empty() => {}
            Ok(outcome) => {
                return Err(format!("drain failed: {}", outcome.errors.join("; ")));
            }
            Err(e) => return Err(format!("drain failed: {}", e.message())),
        }
    }
    if clients.get_admin(address).is_none() {
        if let Err(e) = clients.connect(address).await {
            return Err(format!("connect: {e}"));
        }
    }
    let Some(mut admin) = clients.get_admin(address) else {
        return Err("no admin client".into());
    };
    match admin
        .reboot_node(Request::new(node_proto::RebootNodeRequest {}))
        .await
    {
        Ok(resp) if resp.get_ref().success => Ok(()),
        Ok(resp) => Err(resp.into_inner().message),
        Err(e) => Err(e.to_string()),
    }
}

/// The agent rebuilds with the node config current at activation, so the
/// activated closure replaces the one recorded at prepare.
fn activated_generation(
    node_row: &ClusterUpdateNodeRow,
    resp: &node_proto::ActivateSystemUpdateResponse,
) -> String {
    if resp.target_generation.is_empty() {
        node_row.target_generation.clone()
    } else {
        resp.target_generation.clone()
    }
}

/// When the node does not run the recorded target, ask it which closure it
/// activated. A switch that restarted the agent never returned that closure.
async fn refresh_target_generation(
    db: &Database,
    clients: &NodeClients,
    address: &str,
    node_row: &mut ClusterUpdateNodeRow,
    running: &str,
) -> Result<(), String> {
    if running.is_empty() || running == node_row.target_generation {
        return Ok(());
    }
    let Ok(status) = status_on_node(
        clients,
        address,
        node_proto::GetSystemUpdateStatusRequest {
            update_name: node_row.update_name.clone(),
        },
    )
    .await
    else {
        return Ok(());
    };
    if status.target_generation.is_empty() || status.target_generation == node_row.target_generation
    {
        return Ok(());
    }
    node_row.target_generation = status.target_generation;
    db.upsert_cluster_update_node(node_row)
        .map_err(|e| format!("record activated generation: {e}"))
}

/// A switch that restarts the node agent kills the `nixos-rebuild` the agent
/// waits on, so the agent reports a failure for a switch that went through.
/// Such a failure is confirmed against the generation the node reports next.
fn may_confirm_activation(node_row: &ClusterUpdateNodeRow, activation: &str) -> bool {
    !node_row.target_generation.is_empty() && !node_row.requires_reboot && activation != "boot"
}

/// `Some(true)` once the node runs the target generation and no longer
/// reports the switch pending, `Some(false)` after [`ACTIVATE_CONFIRM_SECS`]
/// without that, `None` while still waiting.
fn confirm_activation(
    nixos_generation: &str,
    booted_generation: &str,
    target_generation: &str,
    node_confirmed: bool,
    age_secs: i64,
) -> Option<bool> {
    if node_confirmed
        && !target_generation.is_empty()
        && crate::kubernetes_lifecycle::generation_is_current(
            nixos_generation,
            booted_generation,
            target_generation,
            false,
        )
    {
        Some(true)
    } else if age_secs >= ACTIVATE_CONFIRM_SECS {
        Some(false)
    } else {
        None
    }
}

fn kubernetes_host_action(
    db: &Database,
    node_id: &str,
    phase: &str,
) -> Result<crate::kubernetes_lifecycle::HostAction, String> {
    let memberships = db
        .list_kubernetes_nodes_by_node_id(node_id)
        .map_err(|e| format!("list kubernetes membership: {e}"))?;
    let is_member = !memberships.is_empty();
    let is_control_plane = memberships.iter().any(|row| row.role == "control-plane");
    let mut control_plane_count = 0usize;
    for membership in &memberships {
        if membership.role == "worker" {
            continue;
        }
        let rows = db
            .list_kubernetes_cluster_nodes(&membership.cluster_name)
            .map_err(|e| format!("list cluster members: {e}"))?;
        control_plane_count = control_plane_count.max(
            rows.iter()
                .filter(|row| row.role == "control-plane")
                .count(),
        );
    }
    let another_control_plane_draining = other_control_plane_is_mid_update(db, node_id)?;
    Ok(crate::kubernetes_lifecycle::host_update_action(
        &crate::kubernetes_lifecycle::HostUpdateView {
            is_member,
            is_control_plane,
            control_plane_count,
            another_control_plane_draining,
            phase: phase.to_string(),
            drain: crate::kubernetes_lifecycle::DrainOutcome::NotRun,
            activate: crate::kubernetes_lifecycle::ActivateOutcome::NotRun,
            node_ready: false,
            on_generation: false,
        },
    ))
}

fn other_control_plane_is_mid_update(db: &Database, node_id: &str) -> Result<bool, String> {
    let updates = db
        .list_cluster_updates()
        .map_err(|e| format!("list cluster updates: {e}"))?;
    for update in updates {
        let nodes = db
            .list_cluster_update_nodes(&update.name)
            .map_err(|e| format!("list update nodes: {e}"))?;
        for node in nodes {
            if node.node_id == node_id {
                continue;
            }
            if node.phase != "drained" && node.phase != "waiting_ready" {
                continue;
            }
            let memberships = db
                .list_kubernetes_nodes_by_node_id(&node.node_id)
                .map_err(|e| format!("list kubernetes membership: {e}"))?;
            if memberships.iter().any(|row| row.role == "control-plane") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn drain_slurm_member(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<(), String> {
    let member = db
        .get_slurm_cluster_node(node_id)
        .map_err(|e| format!("get slurm membership: {e}"))?
        .ok_or_else(|| format!("node {node_id} is not a Slurm member"))?;
    let cluster = db
        .get_slurm_cluster(&member.cluster_name)
        .map_err(|e| format!("get slurm cluster: {e}"))?
        .ok_or_else(|| format!("SlurmCluster {} disappeared", member.cluster_name))?;
    let spec = crate::slurm_cluster_spec::spec_from_json(&cluster.spec_json)
        .map_err(|e| format!("decode slurm spec: {e}"))?;
    let controller = db
        .get_node(&spec.controller.node_id)
        .map_err(|e| format!("get slurm controller node: {e}"))?
        .ok_or_else(|| "slurm controller node is not registered".to_string())?;
    if clients.get_admin(&controller.address).is_none() {
        clients
            .connect(&controller.address)
            .await
            .map_err(|e| format!("connect slurm controller {}: {e}", controller.address))?;
    }
    let mut admin = clients
        .get_admin(&controller.address)
        .ok_or_else(|| format!("no admin client for {}", controller.address))?;
    let response = admin
        .slurm_control(Request::new(node_proto::SlurmControlRequest {
            verb: "drain".into(),
            node_name: node_id.to_string(),
            reason: "kcore cluster update".into(),
        }))
        .await
        .map_err(|e| format!("drain slurm node {node_id}: {e}"))?
        .into_inner();
    if !response.success {
        return Err(format!("drain slurm node {node_id}: {}", response.message));
    }
    Ok(())
}

async fn drain_kubernetes_member(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<(), String> {
    let (peer, name) = kubernetes_peer_and_name(db, clients, node_id).await?;
    crate::kubernetes_admin::drain_node(clients, &peer, &name).await
}

async fn uncordon_kubernetes_member(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<(), String> {
    let (peer, name) = kubernetes_peer_and_name(db, clients, node_id).await?;
    crate::kubernetes_admin::cordon(clients, &peer, &name, false).await
}

async fn kubernetes_ready_for_uncordon(
    db: &Database,
    clients: &NodeClients,
    node_row: &ClusterUpdateNodeRow,
) -> Result<bool, String> {
    let (peer, name) = kubernetes_peer_and_name(db, clients, &node_row.node_id).await?;
    let nodes = crate::kubernetes_admin::list_nodes(clients, &peer).await?;
    let ready = crate::kubernetes_admin::node_is_ready(&nodes, &name);
    let host = db.get_node_host_status(&node_row.node_id).ok().flatten();
    let on_generation = crate::kubernetes_lifecycle::generation_is_current(
        host.as_ref()
            .map(|row| row.nixos_generation.as_str())
            .unwrap_or(""),
        host.as_ref()
            .map(|row| row.booted_generation.as_str())
            .unwrap_or(""),
        &node_row.target_generation,
        node_row.requires_reboot,
    );
    Ok(ready && on_generation)
}

async fn kubernetes_peer_and_name(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<(String, String), String> {
    let memberships = db
        .list_kubernetes_nodes_by_node_id(node_id)
        .map_err(|e| format!("list kubernetes membership: {e}"))?;
    let membership = memberships
        .first()
        .ok_or_else(|| format!("node {node_id} is not a kubernetes member"))?;
    let rows = db
        .list_kubernetes_cluster_nodes(&membership.cluster_name)
        .map_err(|e| format!("list cluster members: {e}"))?;
    let candidates = peer_candidates(&rows, node_id);
    if candidates.is_empty() {
        return Err(format!(
            "cluster {} has no control-plane",
            membership.cluster_name
        ));
    }
    let mut failures = Vec::new();
    let mut reached = None;
    for candidate in candidates {
        let peer = db
            .get_node(candidate)
            .map_err(|e| format!("get node: {e}"))?;
        let Some(peer) = peer.filter(|peer| !peer.address.is_empty()) else {
            failures.push(format!("{candidate}: no agent address"));
            continue;
        };
        match crate::kubernetes_admin::list_nodes(clients, &peer.address).await {
            Ok(listed) => {
                reached = Some((peer.address, listed));
                break;
            }
            Err(e) => failures.push(format!("{candidate}: {e}")),
        }
    }
    let Some((peer_address, listed)) = reached else {
        return Err(format!(
            "no control-plane of cluster {} answered: {}",
            membership.cluster_name,
            failures.join("; ")
        ));
    };
    if !failures.is_empty() {
        warn!(
            node_id,
            skipped = %failures.join("; "),
            "kubernetes control-plane peers unreachable, using {peer_address}"
        );
    }
    let node = db
        .get_node(node_id)
        .map_err(|e| format!("get node: {e}"))?
        .ok_or_else(|| format!("node {node_id} disappeared"))?;
    let ip = if membership.advertise_address.is_empty() {
        node.address.split(':').next().unwrap_or("").to_string()
    } else {
        membership.advertise_address.clone()
    };
    let name = crate::kubernetes_admin::registered_node_name(
        &listed,
        &membership.k8s_node_name,
        &node.hostname,
        &ip,
    )?;
    if membership.k8s_node_name != name {
        let mut stored = membership.clone();
        stored.k8s_node_name = name.clone();
        db.upsert_kubernetes_cluster_node(&stored)
            .map_err(|e| format!("store kubernetes node name: {e}"))?;
    }
    Ok((peer_address, name))
}

/// Control planes to ask about `node_id`, other members first. The node itself
/// is the last resort because its API server goes down during the switch.
fn peer_candidates<'a>(
    rows: &'a [crate::db::KubernetesClusterNodeRow],
    node_id: &str,
) -> Vec<&'a str> {
    let control_planes = rows.iter().filter(|row| row.role == "control-plane");
    control_planes
        .clone()
        .filter(|row| row.node_id != node_id)
        .chain(control_planes.filter(|row| row.node_id == node_id))
        .map(|row| row.node_id.as_str())
        .collect()
}

async fn prepare_on_node(
    clients: &NodeClients,
    address: &str,
    req: node_proto::PrepareSystemUpdateRequest,
) -> Result<node_proto::PrepareSystemUpdateResponse, String> {
    if clients.get_admin(address).is_none() {
        clients
            .connect(address)
            .await
            .map_err(|e| format!("connect node {address}: {e}"))?;
    }
    let mut admin = clients
        .get_admin(address)
        .ok_or_else(|| format!("no admin client for {address}"))?;
    Ok(admin
        .prepare_system_update(Request::new(req))
        .await
        .map_err(|e| format!("prepare_system_update: {e}"))?
        .into_inner())
}

async fn activate_on_node(
    clients: &NodeClients,
    address: &str,
    req: node_proto::ActivateSystemUpdateRequest,
) -> Result<node_proto::ActivateSystemUpdateResponse, String> {
    if clients.get_admin(address).is_none() {
        clients
            .connect(address)
            .await
            .map_err(|e| format!("connect node {address}: {e}"))?;
    }
    let mut admin = clients
        .get_admin(address)
        .ok_or_else(|| format!("no admin client for {address}"))?;
    Ok(admin
        .activate_system_update(Request::new(req))
        .await
        .map_err(|e| format!("activate_system_update: {e}"))?
        .into_inner())
}

async fn status_on_node(
    clients: &NodeClients,
    address: &str,
    req: node_proto::GetSystemUpdateStatusRequest,
) -> Result<node_proto::GetSystemUpdateStatusResponse, String> {
    if clients.get_admin(address).is_none() {
        clients
            .connect(address)
            .await
            .map_err(|e| format!("connect node {address}: {e}"))?;
    }
    let mut admin = clients
        .get_admin(address)
        .ok_or_else(|| format!("no admin client for {address}"))?;
    Ok(admin
        .get_system_update_status(Request::new(req))
        .await
        .map_err(|e| format!("get_system_update_status: {e}"))?
        .into_inner())
}

async fn rollback_on_node(
    clients: &NodeClients,
    address: &str,
    req: node_proto::RollbackSystemUpdateRequest,
) -> Result<node_proto::RollbackSystemUpdateResponse, String> {
    if clients.get_admin(address).is_none() {
        clients
            .connect(address)
            .await
            .map_err(|e| format!("connect node {address}: {e}"))?;
    }
    let mut admin = clients
        .get_admin(address)
        .ok_or_else(|| format!("no admin client for {address}"))?;
    Ok(admin
        .rollback_system_update(Request::new(req))
        .await
        .map_err(|e| format!("rollback_system_update: {e}"))?
        .into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::KubernetesClusterNodeRow;

    fn member(node_id: &str, role: &str) -> KubernetesClusterNodeRow {
        KubernetesClusterNodeRow {
            cluster_name: "k".into(),
            node_id: node_id.into(),
            role: role.into(),
            advertise_address: String::new(),
            iface: String::new(),
            initial_cluster: false,
            join_existing: false,
            kubelet_ready: false,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        }
    }

    fn update_node(target_generation: &str, requires_reboot: bool) -> ClusterUpdateNodeRow {
        ClusterUpdateNodeRow {
            update_name: "u".into(),
            node_id: "node-107".into(),
            observed_generation: 1,
            phase: "prepared".into(),
            current_version: String::new(),
            target_version: String::new(),
            prepared_closure: String::new(),
            current_generation: "/nix/store/old".into(),
            target_generation: target_generation.into(),
            requires_reboot,
            last_error: String::new(),
            last_transition_at: String::new(),
        }
    }

    #[test]
    fn only_switch_activations_with_a_target_are_confirmed() {
        assert!(may_confirm_activation(
            &update_node("/nix/store/new", false),
            "auto"
        ));
        assert!(may_confirm_activation(
            &update_node("/nix/store/new", false),
            "switch"
        ));
        assert!(!may_confirm_activation(
            &update_node("/nix/store/new", false),
            "boot"
        ));
        assert!(!may_confirm_activation(
            &update_node("/nix/store/new", true),
            "auto"
        ));
        assert!(!may_confirm_activation(&update_node("", false), "auto"));
    }

    #[test]
    fn a_failed_switch_succeeds_once_the_node_runs_the_target() {
        let target = "/nix/store/new";
        assert_eq!(
            confirm_activation(target, "/nix/store/old", target, true, 20),
            Some(true)
        );
        assert_eq!(
            confirm_activation("/nix/store/old", "/nix/store/old", target, true, 20),
            None
        );
        assert_eq!(confirm_activation("", "", target, true, 20), None);
        assert_eq!(
            confirm_activation(
                "/nix/store/old",
                "/nix/store/old",
                target,
                true,
                ACTIVATE_CONFIRM_SECS
            ),
            Some(false)
        );
        assert_eq!(confirm_activation("", "", "", true, 20), None);
    }

    #[test]
    fn a_switch_the_node_has_not_confirmed_is_not_a_success() {
        let target = "/nix/store/new";
        assert_eq!(
            confirm_activation(target, "/nix/store/old", target, false, 20),
            None
        );
        assert_eq!(
            confirm_activation(
                target,
                "/nix/store/old",
                target,
                false,
                ACTIVATE_CONFIRM_SECS
            ),
            Some(false)
        );
    }

    #[test]
    fn peer_candidates_try_every_other_control_plane_before_self() {
        let rows = vec![
            member("node-105", "control-plane"),
            member("node-107", "control-plane"),
            member("node-200", "worker"),
            member("node-119", "control-plane"),
        ];
        assert_eq!(
            peer_candidates(&rows, "node-107"),
            vec!["node-105", "node-119", "node-107"]
        );
        assert_eq!(
            peer_candidates(&rows, "node-200"),
            vec!["node-105", "node-107", "node-119"]
        );
        assert!(peer_candidates(&[member("node-200", "worker")], "node-200").is_empty());
    }

    fn node_status(booted: &str, phase: &str) -> node_proto::GetSystemUpdateStatusResponse {
        node_proto::GetSystemUpdateStatusResponse {
            success: true,
            phase: phase.into(),
            booted_generation: booted.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_staged_boot_succeeds_only_once_the_node_kept_it() {
        let mut row = update_node("/nix/store/new", true);
        row.phase = "rebooting".into();
        let step = |booted, phase, timed_out| {
            reboot_step(&row, &node_status(booted, phase), "auto", timed_out)
        };
        assert_eq!(
            step("/nix/store/new", "activated", false),
            RebootStep::Succeeded
        );
        assert_eq!(
            step("/nix/store/new", "boot_pending", false),
            RebootStep::Wait
        );
        assert_eq!(
            step("/nix/store/old", "boot_pending", false),
            RebootStep::Wait
        );
        assert_eq!(
            step("/nix/store/new", "boot_pending", true),
            RebootStep::TimedOut
        );
    }

    #[test]
    fn only_the_auto_policy_reboots_a_staged_node() {
        let mut row = update_node("/nix/store/new", true);
        row.phase = "reboot_required".into();
        let pending = node_status("/nix/store/old", "boot_pending");
        assert_eq!(
            reboot_step(&row, &pending, "auto", false),
            RebootStep::Reboot
        );
        assert_eq!(
            reboot_step(&row, &pending, "if-required", false),
            RebootStep::Wait
        );
        let rebooted = node_status("/nix/store/new", "activated");
        assert_eq!(
            reboot_step(&row, &rebooted, "if-required", false),
            RebootStep::Succeeded
        );
    }

    #[test]
    fn a_failure_cancels_only_nodes_that_have_not_started() {
        let mut failed = update_node("", false);
        failed.phase = "failed".into();
        failed.node_id = "node-1".into();
        let mut done = update_node("", false);
        done.phase = "succeeded".into();
        assert_eq!(first_failed(&[done.clone(), failed]), Some("node-1".into()));
        assert_eq!(first_failed(&[done]), None);
        assert!(not_started("pending"));
        assert!(not_started("prepared"));
        for phase in [
            "succeeded",
            "confirming",
            "reboot_required",
            "rebooting",
            "drained",
        ] {
            assert!(!not_started(phase), "{phase}");
        }
    }

    #[test]
    fn the_canary_soaks_then_passes_waits_or_reverts() {
        use crate::canary::Verdict;
        let pass = Verdict::Pass;
        let fail = Verdict::Fail("guests down".into());
        let unknown = Verdict::Unknown("no peers".into());
        assert_eq!(soak_progress(&pass, "soaking", 10, 3600), SoakStep::Wait);
        assert_eq!(
            soak_progress(&pass, "soaking", 3600, 3600),
            SoakStep::Promote
        );
        assert_eq!(
            soak_progress(&unknown, "soaking", 3600, 3600),
            SoakStep::AwaitOperator
        );
        assert_eq!(
            soak_progress(&pass, "awaiting_promotion", 9999, 3600),
            SoakStep::Wait,
            "only the operator ends a held canary"
        );
        assert_eq!(
            soak_progress(&fail, "soaking", 60, 3600),
            SoakStep::Wait,
            "guests may still be starting"
        );
        assert_eq!(soak_progress(&fail, "soaking", 300, 3600), SoakStep::Revert);
        assert_eq!(
            soak_progress(&fail, "awaiting_promotion", 300, 3600),
            SoakStep::Revert
        );
        assert_eq!(
            soak_progress(&fail, "soaking", 60, 60),
            SoakStep::Revert,
            "a short soak is still judged at its end"
        );
        assert_eq!(finished_phase(Some("a"), "a"), "soaking");
        assert_eq!(finished_phase(Some("a"), "b"), "succeeded");
        assert_eq!(finished_phase(None, "a"), "succeeded");
    }

    fn test_cfg() -> ClusterUpdateReconcilerConfig {
        ClusterUpdateReconcilerConfig {
            default_network: NetworkConfig {
                gateway_interface: "eno1".into(),
                external_ip: "203.0.113.10".into(),
                gateway_ip: "10.0.0.1".into(),
                internal_netmask: "255.255.255.0".into(),
            },
            sub_ca: Arc::new(Mutex::new(SubCaState::default())),
        }
    }

    #[tokio::test]
    async fn a_reverted_node_with_guests_and_no_drain_is_not_rebooted() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_node(&ready_node("a")).unwrap();
        db.upsert_vm(&crate::db::VmRow {
            id: "vm-1".into(),
            name: "web-1".into(),
            cpu: 1,
            memory_bytes: 1 << 30,
            image_path: String::new(),
            image_url: String::new(),
            image_sha256: String::new(),
            image_format: "raw".into(),
            image_size: 0,
            network: "default".into(),
            auto_start: true,
            node_id: "a".into(),
            created_at: String::new(),
            runtime_state: "running".into(),
            cloud_init_user_data: String::new(),
            storage_backend: "filesystem".into(),
            storage_size_bytes: 0,
            vm_ip: String::new(),
            pci_devices: String::new(),
            labels: String::new(),
        })
        .unwrap();
        let mut row = update_node("", false);
        row.node_id = "a".into();
        let clients = NodeClients::new(None);
        let said = reboot_into_previous(&db, &clients, &test_cfg(), "a:9091", &row, false)
            .await
            .unwrap_err();
        assert!(said.starts_with("guests are still on the node"), "{said}");
        let unreachable = reboot_into_previous(
            &Database::open(":memory:").unwrap(),
            &clients,
            &test_cfg(),
            "127.0.0.1:1",
            &row,
            false,
        )
        .await;
        assert!(unreachable.is_err(), "{unreachable:?}");
    }

    #[test]
    fn a_revert_keeps_the_reboot_advice_only_when_the_node_was_not_rebooted() {
        let advice = "rolled back; reboot the node to leave the update generation";
        assert_eq!(
            after_revert(advice, Ok(())),
            "rolled back; rebooting into the previous generation"
        );
        let kept = after_revert(advice, Err("connect: refused".into()));
        assert!(kept.starts_with(advice), "{kept}");
        assert!(kept.ends_with("not rebooted: connect: refused"), "{kept}");
    }

    fn ready_node(id: &str) -> crate::db::NodeRow {
        crate::db::NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: format!("{id}:9091"),
            cpu_cores: 0,
            memory_bytes: 0,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: 0,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    #[tokio::test]
    async fn a_soaked_canary_is_promoted_or_held_for_the_operator() {
        use crate::controller_proto::{
            ClusterUpdateSpec, ClusterUpdateStrategy, ClusterUpdateStrategyType,
        };
        let db = Database::open(":memory:").unwrap();
        for id in ["a", "b"] {
            db.upsert_node(&ready_node(id)).unwrap();
            db.update_heartbeat(id, 0, 0, 0, "").unwrap();
        }
        db.upsert_node_host_status("a", "0.7.0", "", "", "nixos-generation", 0, "")
            .unwrap();
        let spec = ClusterUpdateSpec {
            name: "u".into(),
            strategy: Some(ClusterUpdateStrategy {
                r#type: ClusterUpdateStrategyType::ClusterUpdateStrategyCanary as i32,
                max_unavailable: 1,
                canary_soak_seconds: 60,
                ..Default::default()
            }),
            ..Default::default()
        };
        let clients = NodeClients::new(None);
        let soak = |name: &str, since: &str| {
            let update = ClusterUpdateRow {
                name: name.into(),
                generation: 1,
                target_version: "0.7.0".into(),
                flake_ref: "github:kcorehypervisor/kcore".into(),
                flake_rev: "a".repeat(40),
                spec_json: crate::cluster_update_spec::spec_to_json(&spec).unwrap(),
                phase: "rolling_out".into(),
                approval_status: "approved".into(),
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_cluster_update(&update).unwrap();
            let mut row = update_node("", false);
            row.update_name = name.into();
            row.node_id = "a".into();
            row.phase = "soaking".into();
            row.last_transition_at = since.into();
            db.upsert_cluster_update_node(&row).unwrap();
            (update, row)
        };
        let phase = |name: &str| db.list_cluster_update_nodes(name).unwrap()[0].clone();
        let long_ago = "2000-01-01 00:00:00";

        let (update, row) = soak("fresh", "");
        db.upsert_cluster_update_node(&row).unwrap();
        soak_step(&db, &clients, &test_cfg(), "a:9091", &update, &spec, &row)
            .await
            .unwrap();
        assert_eq!(phase("fresh").phase, "soaking", "the soak has not ended");

        let (update, row) = soak("soaked", long_ago);
        soak_step(&db, &clients, &test_cfg(), "a:9091", &update, &spec, &row)
            .await
            .unwrap();
        assert_eq!(phase("soaked").phase, "succeeded");

        db.update_node_status("b", "not-ready").unwrap();
        let (update, row) = soak("alone", long_ago);
        soak_step(&db, &clients, &test_cfg(), "a:9091", &update, &spec, &row)
            .await
            .unwrap();
        let held = phase("alone");
        assert_eq!(held.phase, "awaiting_promotion");
        assert!(
            held.last_error
                .contains("kctl update cluster promote alone"),
            "{}",
            held.last_error
        );
    }
}
