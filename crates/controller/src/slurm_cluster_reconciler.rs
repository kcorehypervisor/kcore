//! Applies SlurmCluster generations to host nodes.
//!
//! One `slurm.conf`, `cgroup.conf`, and `gres.conf` per generation are rendered
//! once and sent to every member. Only the per-node fragment differs by role.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tracing::{info, warn};

use crate::config::ReplicationConfig;
use crate::db::{Database, SlurmClusterNodeRow, SlurmClusterRow};
use crate::node_client::NodeClients;
use crate::node_proto;
use crate::slurm_cluster_spec::{self, Member, SlurmClusterSpec};
use crate::slurm_render::{self, GpuEntry};

const TICK: Duration = Duration::from_secs(30);
const NIX_APPLY_WAIT: Duration = Duration::from_secs(600);
const NIX_APPLY_POLL: Duration = Duration::from_secs(2);
const STATUS_WAIT: Duration = Duration::from_secs(300);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait SlurmNodeOps: Send + Sync {
    fn preflight<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::CheckSlurmPreflightRequest,
    ) -> BoxFuture<'a, Result<node_proto::CheckSlurmPreflightResponse, String>>;
    fn apply<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::ApplySlurmConfigRequest,
    ) -> BoxFuture<'a, Result<node_proto::ApplySlurmConfigResponse, String>>;
    fn status<'a>(
        &'a self,
        address: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::GetSlurmStatusResponse, String>>;
    fn control<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::SlurmControlRequest,
    ) -> BoxFuture<'a, Result<node_proto::SlurmControlResponse, String>>;
    fn nix_apply_status<'a>(
        &'a self,
        address: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>>;
}

pub struct RealSlurmNodeOps {
    clients: NodeClients,
}

impl RealSlurmNodeOps {
    pub fn new(clients: NodeClients) -> Self {
        Self { clients }
    }

    async fn admin(
        &self,
        address: &str,
    ) -> Result<node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>, String>
    {
        if self.clients.get_admin(address).is_none() {
            self.clients
                .connect(address)
                .await
                .map_err(|e| format!("connect to {address}: {e}"))?;
        }
        self.clients
            .get_admin(address)
            .ok_or_else(|| format!("no admin client for {address}"))
    }
}

impl SlurmNodeOps for RealSlurmNodeOps {
    fn preflight<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::CheckSlurmPreflightRequest,
    ) -> BoxFuture<'a, Result<node_proto::CheckSlurmPreflightResponse, String>> {
        Box::pin(async move {
            let mut admin = self.admin(address).await?;
            admin
                .check_slurm_preflight(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| format!("CheckSlurmPreflight {address}: {e}"))
        })
    }

    fn apply<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::ApplySlurmConfigRequest,
    ) -> BoxFuture<'a, Result<node_proto::ApplySlurmConfigResponse, String>> {
        Box::pin(async move {
            let mut admin = self.admin(address).await?;
            admin
                .apply_slurm_config(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| format!("ApplySlurmConfig {address}: {e}"))
        })
    }

    fn status<'a>(
        &'a self,
        address: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::GetSlurmStatusResponse, String>> {
        Box::pin(async move {
            let mut admin = self.admin(address).await?;
            admin
                .get_slurm_status(tonic::Request::new(node_proto::GetSlurmStatusRequest {}))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| format!("GetSlurmStatus {address}: {e}"))
        })
    }

    fn control<'a>(
        &'a self,
        address: &'a str,
        request: node_proto::SlurmControlRequest,
    ) -> BoxFuture<'a, Result<node_proto::SlurmControlResponse, String>> {
        Box::pin(async move {
            let mut admin = self.admin(address).await?;
            admin
                .slurm_control(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| format!("SlurmControl {address}: {e}"))
        })
    }

    fn nix_apply_status<'a>(
        &'a self,
        address: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>> {
        Box::pin(async move {
            let mut admin = self.admin(address).await?;
            let response = admin
                .get_nix_apply_status(tonic::Request::new(node_proto::GetNixApplyStatusRequest {
                    apply_id: apply_id.to_string(),
                }))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| format!("GetNixApplyStatus {address}: {e}"))?;
            node_proto::NixApplyPhase::try_from(response.phase)
                .map_err(|_| format!("unknown nix apply phase {}", response.phase))
        })
    }
}

pub fn spawn_slurm_cluster_reconciler(
    db: Database,
    clients: NodeClients,
    replication: Option<ReplicationConfig>,
) {
    tokio::spawn(async move {
        let ops = RealSlurmNodeOps::new(clients);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;
            if !may_act(&db, replication.as_ref()) {
                continue;
            }
            if let Err(err) = reconcile_once(&db, &ops).await {
                warn!(error = %err, "slurm cluster reconcile pass failed");
            }
        }
    });
}

fn may_act(db: &Database, replication: Option<&ReplicationConfig>) -> bool {
    let Some(replication) = replication else {
        return true;
    };
    match db.get_cluster_lease("failover") {
        Ok(Some(lease)) => {
            lease.holder == replication.controller_id
                && lease.expires_at_ms > crate::ha::now_unix_ms()
        }
        _ => false,
    }
}

async fn reconcile_once(db: &Database, ops: &dyn SlurmNodeOps) -> Result<(), String> {
    let rows = db
        .list_slurm_clusters_needing_reconcile()
        .map_err(|e| format!("list slurm clusters: {e}"))?;
    for row in rows {
        let result = reconcile_row(db, ops, &row).await;
        if let Err(err) = result {
            warn!(cluster = %row.name, error = %err, "slurm reconcile failed");
            let status = serde_json::json!({
                "observedGeneration": 0,
                "phase": "failed",
                "message": err,
                "nodes": [],
            });
            let _ = db.set_slurm_cluster_status(&row.name, &status.to_string());
        }
    }
    Ok(())
}

async fn reconcile_row(
    db: &Database,
    ops: &dyn SlurmNodeOps,
    row: &SlurmClusterRow,
) -> Result<(), String> {
    let spec = slurm_cluster_spec::spec_from_json(&row.spec_json)
        .map_err(|e| format!("decode slurm spec: {e}"))?;
    let deleting = serde_json::from_str::<serde_json::Value>(&row.status_json)
        .ok()
        .and_then(|status| {
            status
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .as_deref()
        == Some("deleting");
    if deleting {
        return reconcile_delete(db, ops, row, &spec).await;
    }
    reconcile_apply(db, ops, row, &spec).await
}

fn members_for(db: &Database, spec: &SlurmClusterSpec) -> Result<BTreeMap<String, Member>, String> {
    let mut lookup = HashMap::new();
    for node_id in &spec.login.node_ids {
        if let Some(node) = db
            .get_node(node_id)
            .map_err(|e| format!("get node {node_id}: {e}"))?
        {
            lookup.insert(
                node_id.clone(),
                (host_address(&node.address), spec.controller.iface.clone()),
            );
        }
    }
    Ok(slurm_cluster_spec::members(spec, |id| {
        lookup.get(id).cloned()
    }))
}

fn gpu_inventory(db: &Database) -> Result<BTreeMap<String, Vec<GpuEntry>>, String> {
    let mut out: BTreeMap<String, Vec<GpuEntry>> = BTreeMap::new();
    for row in db
        .list_node_gpus(None)
        .map_err(|e| format!("list GPUs: {e}"))?
    {
        if (!row.kind.is_empty() && row.kind != "gpu") || !row.parent.is_empty() {
            continue;
        }
        out.entry(row.node_id).or_default().push(GpuEntry {
            model: if row.model.is_empty() {
                row.name
            } else {
                row.model
            },
            address: row.address,
        });
    }
    Ok(out)
}

fn ordered_members(spec: &SlurmClusterSpec, members: &BTreeMap<String, Member>) -> Vec<Member> {
    let mut out: Vec<Member> = members.values().cloned().collect();
    out.sort_by(|left, right| {
        let rank = |member: &Member| {
            if member.has_role("dbd") {
                0
            } else if member.has_role("controller") {
                if spec
                    .backup_controller
                    .as_ref()
                    .is_some_and(|b| b.node_id == member.node_id)
                {
                    2
                } else {
                    1
                }
            } else if member.has_role("compute") {
                3
            } else {
                4
            }
        };
        (rank(left), left.node_id.clone()).cmp(&(rank(right), right.node_id.clone()))
    });
    out
}

fn ceph_cluster_iface(db: &Database, node_id: &str) -> String {
    db.list_ceph_clusters()
        .ok()
        .into_iter()
        .flatten()
        .find_map(|row| {
            crate::ceph_cluster_spec::spec_from_json(&row.spec_json)
                .ok()
                .and_then(|spec| {
                    spec.nodes
                        .iter()
                        .find(|n| n.node_id == node_id)
                        .map(|n| n.cluster_iface.clone())
                })
        })
        .unwrap_or_default()
}

async fn reconcile_apply(
    db: &Database,
    ops: &dyn SlurmNodeOps,
    row: &SlurmClusterRow,
    spec: &SlurmClusterSpec,
) -> Result<(), String> {
    let members = members_for(db, spec)?;
    let gpus = gpu_inventory(db)?;
    let ordered = ordered_members(spec, &members);
    let mut fragments = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for member in &ordered {
        let fragment = slurm_render::render_node_fragment(
            spec,
            member,
            &members,
            &gpus,
            spec.gpu_driver && member.has_role("compute"),
        );
        hashes.insert(member.node_id.clone(), slurm_render::config_hash(&fragment));
        fragments.insert(member.node_id.clone(), fragment);
    }

    for member in &ordered {
        let node = db
            .get_node(&member.node_id)
            .map_err(|e| format!("get node {}: {e}", member.node_id))?
            .ok_or_else(|| format!("node {} disappeared", member.node_id))?;
        let fragment = fragments.get(&member.node_id).cloned().unwrap_or_default();
        let gpu_ids = gpus
            .get(&member.node_id)
            .map(|entries| entries.iter().map(|e| e.address.clone()).collect())
            .unwrap_or_default();
        let preflight = ops
            .preflight(
                &node.address,
                node_proto::CheckSlurmPreflightRequest {
                    slurm_nix: fragment,
                    address: member.address.clone(),
                    iface: member.iface.clone(),
                    compute: member.has_role("compute"),
                    member_addresses: members.values().map(|m| m.address.clone()).collect(),
                    gpu_pci_ids: gpu_ids,
                    ceph_cluster_iface: ceph_cluster_iface(db, &member.node_id),
                },
            )
            .await?;
        let blocking: Vec<String> = preflight
            .issues
            .iter()
            .filter(|issue| issue.blocking)
            .map(|issue| format!("{}: {}", issue.check, issue.message))
            .collect();
        if !blocking.is_empty() {
            let status = serde_json::json!({
                "observedGeneration": 0,
                "phase": "failed",
                "message": format!("{}: {}", member.node_id, blocking.join("; ")),
                "nodes": [],
            });
            db.set_slurm_cluster_status(&row.name, &status.to_string())
                .map_err(|e| format!("store failed status: {e}"))?;
            return Ok(());
        }
    }

    let existing_nodes = db
        .list_slurm_cluster_nodes(&row.name)
        .map_err(|e| format!("list slurm nodes: {e}"))?;
    let existing_by_id: HashMap<&str, &SlurmClusterNodeRow> = existing_nodes
        .iter()
        .map(|n| (n.node_id.as_str(), n))
        .collect();
    let node_rows: Vec<SlurmClusterNodeRow> = ordered
        .iter()
        .map(|member| {
            let hash = hashes.get(&member.node_id).cloned().unwrap_or_default();
            let previous = existing_by_id.get(member.node_id.as_str());
            let applied = previous
                .filter(|p| p.config_hash == hash)
                .map(|p| p.applied_generation)
                .unwrap_or(0);
            SlurmClusterNodeRow {
                node_id: member.node_id.clone(),
                cluster_name: row.name.clone(),
                roles: member.roles.join(","),
                applied_generation: applied,
                slurm_state: previous
                    .map(|p| p.slurm_state.clone())
                    .unwrap_or_else(|| "unknown".into()),
                config_hash: previous.map(|p| p.config_hash.clone()).unwrap_or_default(),
                updated_at: String::new(),
            }
        })
        .collect();
    db.replace_slurm_cluster_nodes(&row.name, &node_rows)
        .map_err(|e| format!("store slurm nodes: {e}"))?;

    for member in &ordered {
        if member.has_role("compute") {
            if let Some(node) = db
                .get_node(&member.node_id)
                .map_err(|e| format!("get node: {e}"))?
            {
                if node.status == "ready" {
                    db.update_node_status(&member.node_id, "cordoned")
                        .map_err(|e| format!("cordon {}: {e}", member.node_id))?;
                }
            }
        }
    }

    let mut failures = Vec::new();
    for member in &ordered {
        let node = match db
            .get_node(&member.node_id)
            .map_err(|e| format!("get node: {e}"))?
        {
            Some(node) => node,
            None => {
                failures.push(format!("node {} is not registered", member.node_id));
                continue;
            }
        };
        let hash = hashes.get(&member.node_id).cloned().unwrap_or_default();
        let previous = db
            .get_slurm_cluster_node(&member.node_id)
            .map_err(|e| format!("get slurm node: {e}"))?;
        let rotation = previous
            .as_ref()
            .is_some_and(|p| p.config_hash == hash && p.applied_generation < row.generation);
        let gpu_ids = gpus
            .get(&member.node_id)
            .map(|entries| entries.iter().map(|e| e.address.clone()).collect())
            .unwrap_or_default();
        let response = ops
            .apply(
                &node.address,
                node_proto::ApplySlurmConfigRequest {
                    slurm_nix: fragments.get(&member.node_id).cloned().unwrap_or_default(),
                    munge_key: row.munge_key.clone(),
                    dbd_password: if member.has_role("dbd") && spec.accounting.enabled {
                        row.dbd_password.as_bytes().to_vec()
                    } else {
                        Vec::new()
                    },
                    rebuild: !rotation,
                    accounting: spec.accounting.enabled,
                    gpu_pci_ids: gpu_ids,
                },
            )
            .await?;
        if !response.success {
            failures.push(format!("{}: {}", member.node_id, response.message));
            continue;
        }
        if !response.apply_id.is_empty() {
            if let Err(err) = wait_for_nix_apply(ops, &node.address, &response.apply_id).await {
                failures.push(format!("{}: {err}", member.node_id));
                continue;
            }
        }
        db.update_slurm_cluster_node_state(
            &member.node_id,
            row.generation,
            previous
                .as_ref()
                .map(|p| p.slurm_state.clone())
                .unwrap_or_else(|| "unknown".into())
                .as_str(),
            &hash,
        )
        .map_err(|e| format!("update slurm node state: {e}"))?;
    }

    if !failures.is_empty() {
        let status = serde_json::json!({
            "observedGeneration": 0,
            "phase": "failed",
            "message": failures.join("; "),
            "nodes": [],
        });
        db.set_slurm_cluster_status(&row.name, &status.to_string())
            .map_err(|e| format!("store failed status: {e}"))?;
        return Ok(());
    }

    let controller = db
        .get_node(&spec.controller.node_id)
        .map_err(|e| format!("get controller node: {e}"))?
        .ok_or_else(|| "controller node is not registered".to_string())?;
    let status = ops.status(&controller.address).await?;
    let states = parse_sinfo(&status.sinfo);
    let mut all_applied = true;
    let mut node_statuses = Vec::new();
    for member in &ordered {
        let state = states
            .get(&member.node_id)
            .cloned()
            .unwrap_or_else(|| "unknown".into());
        db.update_slurm_cluster_node_state(
            &member.node_id,
            row.generation,
            &state,
            hashes
                .get(&member.node_id)
                .map(String::as_str)
                .unwrap_or(""),
        )
        .map_err(|e| format!("update slurm node state: {e}"))?;
        if let Some(node) = db
            .get_slurm_cluster_node(&member.node_id)
            .map_err(|e| format!("get slurm node: {e}"))?
        {
            if node.applied_generation < row.generation {
                all_applied = false;
            }
            node_statuses.push(serde_json::json!({
                "nodeId": node.node_id,
                "roles": node.roles.split(',').collect::<Vec<_>>(),
                "slurmState": node.slurm_state,
                "appliedGeneration": node.applied_generation,
                "configHash": node.config_hash,
            }));
        }
    }
    let controller_ok = status.slurmctld == "active";
    let phase = if all_applied && controller_ok {
        "ready"
    } else if all_applied {
        "degraded"
    } else {
        "applying"
    };
    let message = if phase == "ready" {
        "all Slurm members applied".to_string()
    } else if controller_ok {
        "waiting for members to report the current generation".to_string()
    } else {
        format!("slurmctld is {}", status.slurmctld)
    };
    let status_json = serde_json::json!({
        "observedGeneration": if phase == "ready" { row.generation } else { 0 },
        "phase": phase,
        "message": message,
        "nodes": node_statuses,
    });
    db.set_slurm_cluster_status(&row.name, &status_json.to_string())
        .map_err(|e| format!("store slurm status: {e}"))?;
    info!(cluster = %row.name, phase, "slurm reconcile pass complete");
    Ok(())
}

async fn reconcile_delete(
    db: &Database,
    ops: &dyn SlurmNodeOps,
    row: &SlurmClusterRow,
    spec: &SlurmClusterSpec,
) -> Result<(), String> {
    let members = members_for(db, spec)?;
    let ordered = ordered_members(spec, &members);
    let fragment = slurm_render::render_disabled_fragment();
    let mut failures = Vec::new();
    for member in &ordered {
        let Some(node) = db
            .get_node(&member.node_id)
            .map_err(|e| format!("get node: {e}"))?
        else {
            continue;
        };
        match ops
            .apply(
                &node.address,
                node_proto::ApplySlurmConfigRequest {
                    slurm_nix: fragment.clone(),
                    munge_key: Vec::new(),
                    dbd_password: Vec::new(),
                    rebuild: true,
                    accounting: false,
                    gpu_pci_ids: Vec::new(),
                },
            )
            .await
        {
            Ok(response) if response.success => {
                if !response.apply_id.is_empty() {
                    if let Err(err) =
                        wait_for_nix_apply(ops, &node.address, &response.apply_id).await
                    {
                        failures.push(format!("{}: {err}", member.node_id));
                    }
                }
            }
            Ok(response) => failures.push(format!("{}: {}", member.node_id, response.message)),
            Err(err) => failures.push(format!("{}: {err}", member.node_id)),
        }
    }

    if failures.is_empty() {
        let controller = db
            .get_node(&spec.controller.node_id)
            .map_err(|e| format!("get controller node: {e}"))?;
        if let Some(controller) = controller {
            let deadline = tokio::time::Instant::now() + STATUS_WAIT;
            loop {
                match ops.status(&controller.address).await {
                    Ok(status)
                        if inactive(&status.munge)
                            && inactive(&status.slurmctld)
                            && inactive(&status.slurmd)
                            && inactive(&status.slurmdbd) =>
                    {
                        break;
                    }
                    Ok(_) if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(TICK).await;
                    }
                    Ok(_) => break,
                    Err(err) => {
                        warn!(cluster = %row.name, error = %err, "slurm status during delete");
                        break;
                    }
                }
            }
        }
    }

    if failures.is_empty() {
        for member in &ordered {
            if !member.has_role("compute") {
                continue;
            }
            if let Some(node) = db
                .get_node(&member.node_id)
                .map_err(|e| format!("get node: {e}"))?
            {
                if matches!(node.status.as_str(), "cordoned" | "draining" | "drained") {
                    db.update_node_status(&member.node_id, "ready")
                        .map_err(|e| format!("uncordon {}: {e}", member.node_id))?;
                }
            }
        }
        db.delete_slurm_cluster(&row.name)
            .map_err(|e| format!("delete slurm cluster: {e}"))?;
        return Ok(());
    }

    let status = serde_json::json!({
        "observedGeneration": 0,
        "phase": "degraded",
        "message": format!("deletion is waiting on: {}", failures.join("; ")),
        "nodes": [],
    });
    db.set_slurm_cluster_status(&row.name, &status.to_string())
        .map_err(|e| format!("store delete status: {e}"))?;
    Ok(())
}

async fn wait_for_nix_apply(
    ops: &dyn SlurmNodeOps,
    address: &str,
    apply_id: &str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + NIX_APPLY_WAIT;
    loop {
        match ops.nix_apply_status(address, apply_id).await? {
            node_proto::NixApplyPhase::Succeeded => return Ok(()),
            node_proto::NixApplyPhase::Failed => {
                return Err(format!("nixos-rebuild {apply_id} failed"))
            }
            node_proto::NixApplyPhase::Unknown => {
                return Err(format!("node has no record of nix apply {apply_id}"))
            }
            _ if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(NIX_APPLY_POLL).await;
            }
            _ => return Err(format!("timed out waiting for nix apply {apply_id}")),
        }
    }
}

fn parse_sinfo(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(state) = parts.next() else {
            continue;
        };
        let state = if state.starts_with("drain") {
            "drain"
        } else if state.starts_with("alloc") {
            "alloc"
        } else if state.starts_with("idle") {
            "idle"
        } else if state.starts_with("down") {
            "down"
        } else {
            "unknown"
        };
        out.insert(name.to_string(), state.to_string());
    }
    out
}

fn inactive(state: &str) -> bool {
    state.is_empty() || state == "inactive" || state == "failed" || state == "unknown"
}

fn host_address(address: &str) -> String {
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => {
            host.to_string()
        }
        _ => address.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slurm_cluster_spec::{
        SlurmComputeSpec, SlurmControllerSpec, SlurmLoginSpec, SlurmPartitionSpec,
        SlurmReserveSpec, SlurmStorageSpec,
    };

    fn spec() -> SlurmClusterSpec {
        SlurmClusterSpec {
            cluster_name: "hpc-a".into(),
            controller: SlurmControllerSpec {
                node_id: "node-1".into(),
                address: "10.0.0.1".into(),
                iface: "enp6s0".into(),
            },
            backup_controller: None,
            login: SlurmLoginSpec {
                node_ids: vec!["node-1".into()],
            },
            accounting: Default::default(),
            compute: vec![SlurmComputeSpec {
                node_id: "node-2".into(),
                address: "10.0.0.2".into(),
                iface: "enp6s0".into(),
                partitions: vec!["batch".into()],
                reserve: SlurmReserveSpec::default(),
                gres: "none".into(),
            }],
            partitions: vec![SlurmPartitionSpec {
                name: "batch".into(),
                default: true,
                max_time: String::new(),
                state: String::new(),
            }],
            users: Vec::new(),
            storage: SlurmStorageSpec::default(),
            extra_config: String::new(),
            allow_controller_host: false,
            gpu_driver: false,
        }
    }

    #[test]
    fn members_are_ordered_dbd_controller_compute_login() {
        let mut spec = spec();
        spec.login.node_ids.push("node-3".into());
        spec.accounting.enabled = true;
        let members = slurm_cluster_spec::members(&spec, |id| {
            Some((
                format!("10.0.0.{}", id.trim_start_matches("node-")),
                "enp6s0".into(),
            ))
        });
        let ordered = ordered_members(&spec, &members);
        assert_eq!(ordered[0].node_id, "node-1");
        assert!(ordered[0].has_role("dbd"));
        assert_eq!(ordered.last().unwrap().node_id, "node-3");
    }

    #[test]
    fn sinfo_states_are_normalized() {
        let states = parse_sinfo("node-2 idle\nnode-3 drain*\nnode-4 mixed\n");
        assert_eq!(states["node-2"], "idle");
        assert_eq!(states["node-3"], "drain");
        assert_eq!(states["node-4"], "unknown");
    }

    #[derive(Default)]
    struct FakeOps {
        blocking: bool,
        inactive: bool,
        applies: std::sync::Mutex<Vec<node_proto::ApplySlurmConfigRequest>>,
    }

    impl SlurmNodeOps for FakeOps {
        fn preflight<'a>(
            &'a self,
            _address: &'a str,
            _request: node_proto::CheckSlurmPreflightRequest,
        ) -> BoxFuture<'a, Result<node_proto::CheckSlurmPreflightResponse, String>> {
            Box::pin(async move {
                Ok(node_proto::CheckSlurmPreflightResponse {
                    ok: !self.blocking,
                    issues: if self.blocking {
                        vec![node_proto::SlurmPreflightIssue {
                            check: "address".into(),
                            message: "address is not on iface".into(),
                            blocking: true,
                        }]
                    } else {
                        Vec::new()
                    },
                })
            })
        }

        fn apply<'a>(
            &'a self,
            _address: &'a str,
            request: node_proto::ApplySlurmConfigRequest,
        ) -> BoxFuture<'a, Result<node_proto::ApplySlurmConfigResponse, String>> {
            self.applies.lock().unwrap().push(request);
            Box::pin(async move {
                Ok(node_proto::ApplySlurmConfigResponse {
                    success: true,
                    message: "queued".into(),
                    apply_id: String::new(),
                })
            })
        }

        fn status<'a>(
            &'a self,
            _address: &'a str,
        ) -> BoxFuture<'a, Result<node_proto::GetSlurmStatusResponse, String>> {
            let inactive = self.inactive;
            Box::pin(async move {
                Ok(node_proto::GetSlurmStatusResponse {
                    munge: if inactive { "inactive" } else { "active" }.into(),
                    slurmctld: if inactive { "inactive" } else { "active" }.into(),
                    slurmd: if inactive { "inactive" } else { "active" }.into(),
                    slurmdbd: "inactive".into(),
                    sinfo: if inactive {
                        String::new()
                    } else {
                        "node-2 idle\nnode-3 idle\n".into()
                    },
                    config_hash: String::new(),
                })
            })
        }

        fn control<'a>(
            &'a self,
            _address: &'a str,
            _request: node_proto::SlurmControlRequest,
        ) -> BoxFuture<'a, Result<node_proto::SlurmControlResponse, String>> {
            Box::pin(async move {
                Ok(node_proto::SlurmControlResponse {
                    success: true,
                    message: "ok".into(),
                })
            })
        }

        fn nix_apply_status<'a>(
            &'a self,
            _address: &'a str,
            _apply_id: &'a str,
        ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>> {
            Box::pin(async move { Ok(node_proto::NixApplyPhase::Succeeded) })
        }
    }

    fn node(id: &str, address: &str) -> crate::db::NodeRow {
        crate::db::NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: address.into(),
            cpu_cores: 4,
            memory_bytes: 8 << 30,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: "br0".into(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    fn cluster_row(spec: &SlurmClusterSpec, status: &str) -> SlurmClusterRow {
        SlurmClusterRow {
            name: "hpc-a".into(),
            spec_json: slurm_cluster_spec::spec_to_json(spec).unwrap(),
            generation: 1,
            munge_key: vec![7u8; 1024],
            dbd_password: String::new(),
            status_json: status.into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn create_applies_members_and_cordons_compute() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_node(&node("node-1", "127.0.0.1:9101")).unwrap();
        db.upsert_node(&node("node-2", "127.0.0.1:9102")).unwrap();
        let spec = spec();
        let row = cluster_row(
            &spec,
            r#"{"observedGeneration":0,"phase":"pending","message":"","nodes":[]}"#,
        );
        db.upsert_slurm_cluster(&row).unwrap();
        let ops = FakeOps::default();
        reconcile_row(&db, &ops, &row).await.unwrap();
        let stored = db.get_slurm_cluster("hpc-a").unwrap().unwrap();
        let status: serde_json::Value = serde_json::from_str(&stored.status_json).unwrap();
        assert_eq!(status["phase"], "ready");
        assert_eq!(db.get_node("node-2").unwrap().unwrap().status, "cordoned");
        let applies = ops.applies.lock().unwrap();
        assert_eq!(applies.len(), 2);
        assert_eq!(applies[0].munge_key.len(), 1024);
        let nodes = db.list_slurm_cluster_nodes("hpc-a").unwrap();
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().all(|n| n.applied_generation == 1));
    }

    #[tokio::test]
    async fn blocking_preflight_does_not_apply() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_node(&node("node-1", "127.0.0.1:9101")).unwrap();
        db.upsert_node(&node("node-2", "127.0.0.1:9102")).unwrap();
        let spec = spec();
        let row = cluster_row(
            &spec,
            r#"{"observedGeneration":0,"phase":"pending","message":"","nodes":[]}"#,
        );
        db.upsert_slurm_cluster(&row).unwrap();
        let ops = FakeOps {
            blocking: true,
            ..Default::default()
        };
        reconcile_row(&db, &ops, &row).await.unwrap();
        assert!(ops.applies.lock().unwrap().is_empty());
        let stored = db.get_slurm_cluster("hpc-a").unwrap().unwrap();
        let status: serde_json::Value = serde_json::from_str(&stored.status_json).unwrap();
        assert_eq!(status["phase"], "failed");
    }

    #[tokio::test]
    async fn update_adds_a_compute_node() {
        let db = Database::open(":memory:").unwrap();
        for (id, port) in [("node-1", 9101), ("node-2", 9102), ("node-3", 9103)] {
            db.upsert_node(&node(id, &format!("127.0.0.1:{port}")))
                .unwrap();
        }
        let mut spec = spec();
        let mut row = cluster_row(
            &spec,
            r#"{"observedGeneration":0,"phase":"pending","message":"","nodes":[]}"#,
        );
        db.upsert_slurm_cluster(&row).unwrap();
        let ops = FakeOps::default();
        reconcile_row(&db, &ops, &row).await.unwrap();
        spec.compute.push(SlurmComputeSpec {
            node_id: "node-3".into(),
            address: "10.0.0.3".into(),
            iface: "enp6s0".into(),
            partitions: vec!["batch".into()],
            reserve: SlurmReserveSpec::default(),
            gres: "none".into(),
        });
        row.spec_json = slurm_cluster_spec::spec_to_json(&spec).unwrap();
        row.generation = 2;
        row.status_json =
            r#"{"observedGeneration":0,"phase":"applying","message":"","nodes":[]}"#.into();
        db.upsert_slurm_cluster(&row).unwrap();
        reconcile_row(&db, &ops, &row).await.unwrap();
        assert_eq!(db.list_slurm_cluster_nodes("hpc-a").unwrap().len(), 3);
        assert!(db
            .get_slurm_cluster("hpc-a")
            .unwrap()
            .unwrap()
            .status_json
            .contains(r#""phase":"ready""#));
    }

    #[tokio::test]
    async fn delete_disables_units_and_uncordons() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_node(&node("node-1", "127.0.0.1:9101")).unwrap();
        db.upsert_node(&node("node-2", "127.0.0.1:9102")).unwrap();
        db.update_node_status("node-2", "cordoned").unwrap();
        let spec = spec();
        let row = cluster_row(
            &spec,
            r#"{"observedGeneration":0,"phase":"deleting","message":"","nodes":[]}"#,
        );
        db.upsert_slurm_cluster(&row).unwrap();
        let ops = FakeOps {
            inactive: true,
            ..Default::default()
        };
        reconcile_row(&db, &ops, &row).await.unwrap();
        assert!(db.get_slurm_cluster("hpc-a").unwrap().is_none());
        assert_eq!(db.get_node("node-2").unwrap().unwrap().status, "ready");
    }

    #[tokio::test]
    async fn rotation_pushes_the_new_key_without_a_rebuild() {
        let db = Database::open(":memory:").unwrap();
        db.upsert_node(&node("node-1", "127.0.0.1:9101")).unwrap();
        db.upsert_node(&node("node-2", "127.0.0.1:9102")).unwrap();
        let spec = spec();
        let row = cluster_row(
            &spec,
            r#"{"observedGeneration":0,"phase":"pending","message":"","nodes":[]}"#,
        );
        db.upsert_slurm_cluster(&row).unwrap();
        let ops = FakeOps::default();
        reconcile_row(&db, &ops, &row).await.unwrap();
        ops.applies.lock().unwrap().clear();
        let key = vec![9u8; 1024];
        db.rotate_slurm_munge_key("hpc-a", &key).unwrap();
        let rotated = db.get_slurm_cluster("hpc-a").unwrap().unwrap();
        reconcile_row(&db, &ops, &rotated).await.unwrap();
        let applies = ops.applies.lock().unwrap();
        assert!(!applies.is_empty());
        assert!(applies.iter().all(|request| request.munge_key == key));
        assert!(applies.iter().all(|request| !request.rebuild));
    }
}
