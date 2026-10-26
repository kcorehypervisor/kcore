//! Reconciles [`ClusterUpdate`](crate::controller_proto::ClusterUpdate) rows by
//! calling [`PrepareSystemUpdate`](crate::node_proto::node_admin_client::NodeAdminClient::prepare_system_update)
//! then [`ActivateSystemUpdate`](crate::node_proto::node_admin_client::NodeAdminClient::activate_system_update)
//! on each target node (serial one-at-a-time MVP).

use std::time::Duration;

use tokio::time;
use tonic::Request;
use tracing::{debug, warn};

use crate::cluster_update_spec::{activation_mode_string, spec_from_json};
use crate::db::{ClusterUpdateNodeRow, ClusterUpdateRow, Database};
use crate::node_client::NodeClients;
use crate::node_proto;

const RECONCILE_TICK: Duration = Duration::from_secs(12);
const NODE_PREPARE_TIMEOUT_SECS: i32 = 3600;
const NODE_ACTIVATE_TIMEOUT_SECS: i32 = 900;

pub fn spawn_cluster_update_reconciler(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let mut ticker = time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, &clients).await {
                warn!(error = %e, "cluster update reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(db: &Database, clients: &NodeClients) -> Result<(), String> {
    let updates: Vec<ClusterUpdateRow> = db
        .list_cluster_updates_for_reconcile()
        .map_err(|e| format!("list cluster updates: {e}"))?;
    if updates.is_empty() {
        return Ok(());
    }
    debug!(count = updates.len(), "cluster update reconcile");
    for u in updates {
        if let Err(e) = reconcile_update(db, clients, &u).await {
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
        return Ok(());
    }

    let spec = spec_from_json(&update.spec_json).map_err(|e| format!("parse spec_json: {e}"))?;
    let activation = activation_mode_string(&spec).to_string();
    let peers = db.list_controller_peers().unwrap_or_default();
    nodes.sort_by(|left, right| {
        let rank = |node_id: &str| {
            let address = db
                .get_node(node_id)
                .ok()
                .flatten()
                .map(|node| node.address)
                .unwrap_or_default();
            let host = address.split(':').next().unwrap_or("").to_string();
            let controller = peers.iter().any(|peer| {
                peer.address == address || peer.address.split(':').next() == Some(host.as_str())
            });
            (u8::from(controller), node_id.to_string())
        };
        rank(&left.node_id).cmp(&rank(&right.node_id))
    });

    // Pick the first node still needing work. Controllers sort last.
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
                            "succeeded"
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
                            target_generation: node_row.target_generation.clone(),
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
                            phase: "failed".into(),
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
                            phase: "failed".into(),
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
                            phase: "succeeded".into(),
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
            "reboot_required" => {}
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
                let _ = rollback_on_node(clients, &address, req).await;
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
                    last_error: "rolled back (MVP)".into(),
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
}
