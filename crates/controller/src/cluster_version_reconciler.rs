//! Keep every approved, heartbeating node on the desired kcore release.
//!
//! `SetClusterVersion` writes the desired row and starts a rollout through
//! [`start_cluster_version`]. This reconciler calls the same function when a
//! node drifts or joins. After a failed or cancelled rollout it holds until
//! the operator applies again (the desired row's `last_rollout_*` matches a
//! terminal update that is not succeeded).

use std::time::Duration;

use tracing::warn;

use crate::cluster_update_spec;
use crate::cluster_version::{self, EnterpriseTarget};
use crate::config::ReplicationConfig;
use crate::controller_proto::{ClusterUpdate, ClusterUpdatePhase, SetClusterVersionResponse};
use crate::db::{
    ClusterDesiredVersionRow, ClusterUpdateNodeRow, ClusterUpdateRow, Database, NodeRow,
};
use crate::ha;

const RECONCILE_TICK: Duration = Duration::from_secs(30);
const HEARTBEAT_STALE_SECS: i64 = 4 * ha::NODE_HEARTBEAT_INTERVAL_SECS;

/// What the operator (or this reconciler) asked to run.
#[derive(Debug, Clone)]
pub struct DesiredVersionInput {
    pub version: String,
    pub flake_ref: String,
    pub channel: String,
    pub drain_vms: bool,
    pub reboot: bool,
    pub no_canary: bool,
    pub canary_soak_seconds: i32,
    pub canary_node_id: String,
    pub dry_run: bool,
    /// Persist the desired row before starting. SetClusterVersion does; the
    /// reconciler does not (the row is already there).
    pub persist_desired: bool,
    /// Only approved, heartbeating nodes with a known version. The reconciler
    /// uses this; an operator apply still targets every node that reports a
    /// version (including empty, which counts as upgrade).
    pub only_ready_nodes: bool,
}

#[derive(Debug)]
pub enum StartOutcome {
    /// Every candidate node already runs the tag.
    Unchanged {
        version: String,
        flake_rev: String,
        message: String,
        response: SetClusterVersionResponse,
    },
    DryRun {
        response: SetClusterVersionResponse,
    },
    /// Same tag is already rolling out.
    AlreadyRolling {
        response: SetClusterVersionResponse,
    },
    /// A different update (or a rollback) is in flight.
    Busy {
        name: String,
        phase: String,
    },
    /// Last rollout for this desired tag failed or was cancelled; wait for
    /// the operator to apply again.
    Holding {
        reason: String,
    },
    Started {
        response: SetClusterVersionResponse,
    },
}

/// Whether the reconciler should call [`start_cluster_version`] this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConvergeDecision {
    SkipNoDesired,
    SkipNotLeaseHolder,
    SkipActiveUpdate { name: String },
    SkipHolding { reason: String },
    SkipInSync,
    Start,
}

pub fn may_act(db: &Database, replication: Option<&ReplicationConfig>) -> bool {
    let Some(replication) = replication else {
        return true;
    };
    match db.get_cluster_lease("failover") {
        Ok(Some(lease)) => {
            lease.holder == replication.controller_id && lease.expires_at_ms > ha::now_unix_ms()
        }
        _ => false,
    }
}

/// Decide whether this tick should start a rollout. Pure over the rows the
/// caller already loaded, so unit tests do not need a live database.
pub fn decide_converge(
    desired: Option<&ClusterDesiredVersionRow>,
    active_update: Option<&(String, String)>,
    holding_reason: Option<&str>,
    off_version_count: usize,
    lease_holder: bool,
) -> ConvergeDecision {
    if !lease_holder {
        return ConvergeDecision::SkipNotLeaseHolder;
    }
    let Some(desired) = desired else {
        return ConvergeDecision::SkipNoDesired;
    };
    if desired.version.trim().is_empty() {
        return ConvergeDecision::SkipNoDesired;
    }
    if let Some((name, _)) = active_update {
        return ConvergeDecision::SkipActiveUpdate { name: name.clone() };
    }
    if let Some(reason) = holding_reason {
        return ConvergeDecision::SkipHolding {
            reason: reason.to_string(),
        };
    }
    if off_version_count == 0 {
        return ConvergeDecision::SkipInSync;
    }
    ConvergeDecision::Start
}

/// A failed or cancelled version-* update that matches the desired record's
/// last rollout blocks automatic retry until the operator applies again.
pub fn holding_reason(
    desired: &ClusterDesiredVersionRow,
    updates: &[ClusterUpdateRow],
) -> Option<String> {
    if desired.last_rollout_name.is_empty() {
        return None;
    }
    let row = updates
        .iter()
        .find(|u| u.name == desired.last_rollout_name)?;
    if row.generation != desired.last_rollout_generation {
        return None;
    }
    match row.phase.as_str() {
        "failed" => Some(format!(
            "last rollout {} failed; apply the desired version again to retry",
            row.name
        )),
        "cancelled" => Some(format!(
            "last rollout {} was cancelled; apply the desired version again to retry",
            row.name
        )),
        _ => None,
    }
}

pub fn active_version_update(updates: &[ClusterUpdateRow]) -> Option<(String, String)> {
    updates.iter().find_map(|u| {
        matches!(
            u.phase.as_str(),
            "pending" | "ready" | "rolling_out" | "rolling_back"
        )
        .then(|| (u.name.clone(), u.phase.clone()))
    })
}

/// Approved, heartbeating nodes with a known kcore version that is not `target`.
pub fn off_version_ready_nodes(
    db: &Database,
    target: &str,
    nodes: &[NodeRow],
) -> Result<Vec<(String, String)>, String> {
    let want = cluster_version::parse_version(target);
    let mut out = Vec::new();
    for node in nodes {
        if node.approval_status != "approved" || node.status == "not-ready" {
            continue;
        }
        if !db
            .node_recently_seen(&node.id, HEARTBEAT_STALE_SECS)
            .map_err(|e| format!("heartbeat for {}: {e}", node.id))?
        {
            continue;
        }
        let version = db
            .get_node_host_status(&node.id)
            .map_err(|e| format!("host status for {}: {e}", node.id))?
            .map(|h| h.kcore_version)
            .unwrap_or_default();
        if version.trim().is_empty() {
            continue;
        }
        if cluster_version::parse_version(&version) == want {
            continue;
        }
        out.push((node.id.clone(), version));
    }
    Ok(out)
}

pub fn spawn_cluster_version_reconciler(db: Database, replication: Option<ReplicationConfig>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(RECONCILE_TICK);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = reconcile_once(&db, replication.as_ref()).await {
                warn!(error = %e, "cluster version reconcile tick failed");
            }
        }
    });
}

async fn reconcile_once(
    db: &Database,
    replication: Option<&ReplicationConfig>,
) -> Result<(), String> {
    let desired = db
        .get_cluster_desired_version()
        .map_err(|e| format!("desired version: {e}"))?;
    let updates = db
        .list_cluster_updates()
        .map_err(|e| format!("list updates: {e}"))?;
    let nodes = db.list_nodes().map_err(|e| format!("list nodes: {e}"))?;
    let active = active_version_update(&updates);
    let hold = desired.as_ref().and_then(|d| holding_reason(d, &updates));
    let off = match desired.as_ref() {
        Some(d) if !d.version.is_empty() => off_version_ready_nodes(db, &d.version, &nodes)?,
        _ => Vec::new(),
    };
    let decision = decide_converge(
        desired.as_ref(),
        active.as_ref(),
        hold.as_deref(),
        off.len(),
        may_act(db, replication),
    );
    match decision {
        ConvergeDecision::Start => {}
        _ => return Ok(()),
    }
    let desired = desired.expect("Start implies a desired row");
    let input = DesiredVersionInput {
        version: desired.version.clone(),
        flake_ref: desired.flake_ref.clone(),
        channel: desired.channel.clone(),
        drain_vms: desired.drain_vms,
        reboot: desired.reboot,
        no_canary: desired.no_canary,
        canary_soak_seconds: desired.canary_soak_seconds,
        canary_node_id: desired.canary_node_id.clone(),
        dry_run: false,
        persist_desired: false,
        only_ready_nodes: true,
    };
    match start_cluster_version(db, input).await {
        Ok(StartOutcome::Started { response }) => {
            tracing::info!(
                update = %response.update_name,
                version = %response.version,
                "cluster version reconciler started a rollout"
            );
        }
        Ok(StartOutcome::Unchanged { .. } | StartOutcome::AlreadyRolling { .. }) => {}
        Ok(StartOutcome::Holding { reason }) => {
            tracing::debug!(%reason, "cluster version reconciler holding");
        }
        Ok(StartOutcome::Busy { name, phase }) => {
            tracing::debug!(%name, %phase, "cluster version reconciler skipped; update busy");
        }
        Ok(StartOutcome::DryRun { .. }) => {}
        Err(e) => return Err(e.to_string()),
    }
    Ok(())
}

/// Resolve the tag, optionally persist the desired row, and start a
/// `version-vX-Y-Z` ClusterUpdate when any candidate node is off-version.
pub async fn start_cluster_version(
    db: &Database,
    input: DesiredVersionInput,
) -> Result<StartOutcome, tonic::Status> {
    use tonic::Status;

    let tag = cluster_version::normalize_tag(&input.version).map_err(Status::invalid_argument)?;
    let subscription = db.get_subscription().map_err(internal_db)?;
    let channel = match input.channel.trim() {
        "" => {
            if subscription
                .as_ref()
                .is_some_and(|s| !s.token.trim().is_empty())
            {
                "enterprise"
            } else {
                "community"
            }
        }
        "community" | "enterprise" => input.channel.trim(),
        other => {
            return Err(Status::invalid_argument(format!(
                "channel must be community or enterprise, got {other}"
            )));
        }
    };
    if channel == "enterprise" && !input.flake_ref.trim().is_empty() {
        return Err(Status::invalid_argument(
            "flake_ref cannot be set with channel=enterprise",
        ));
    }
    let flake_ref = match input.flake_ref.trim() {
        "" => cluster_version::DEFAULT_FLAKE_REF.to_string(),
        other => other.to_string(),
    };
    let name = cluster_version::update_name(&tag);
    let updates = db.list_cluster_updates().map_err(internal_db)?;
    if let Some(active) = updates.iter().find(|u| {
        matches!(
            u.phase.as_str(),
            "pending" | "ready" | "rolling_out" | "rolling_back"
        )
    }) {
        if active.name != name || active.phase == "rolling_back" {
            return Ok(StartOutcome::Busy {
                name: active.name.clone(),
                phase: active.phase.clone(),
            });
        }
        let spec = cluster_update_spec::spec_from_json(&active.spec_json)
            .map_err(|e| Status::internal(format!("decode spec: {e}")))?;
        let mut rows = db
            .list_cluster_update_nodes(&active.name)
            .map_err(internal_db)?;
        crate::cluster_update_reconciler::rollout_order(db, &mut rows);
        let ordered: Vec<String> = rows.into_iter().map(|row| row.node_id).collect();
        let canary = crate::canary::pick(db, &spec, &ordered).unwrap_or_default();
        let canary_soak_seconds = if canary.is_empty() {
            0
        } else {
            spec.strategy
                .as_ref()
                .map(|s| s.canary_soak_seconds)
                .unwrap_or_default()
        };
        return Ok(StartOutcome::AlreadyRolling {
            response: SetClusterVersionResponse {
                canary_node_id: canary,
                canary_soak_seconds,
                direction: String::new(),
                version: tag,
                flake_rev: active.flake_rev.clone(),
                target_node_ids: spec
                    .selector
                    .as_ref()
                    .map(|s| s.node_ids.clone())
                    .unwrap_or_default(),
                update_name: active.name.clone(),
                cluster_update: Some(cluster_update_row_to_proto(active, spec)),
                message: "this version is already rolling out".into(),
            },
        });
    }

    if input.only_ready_nodes {
        if let Some(desired) = db.get_cluster_desired_version().map_err(internal_db)? {
            if let Some(reason) = holding_reason(&desired, &updates) {
                return Ok(StartOutcome::Holding { reason });
            }
        }
    }

    let (release, enterprise_target) = if channel == "enterprise" {
        let token = subscription
            .as_ref()
            .map(|s| s.token.as_str())
            .unwrap_or("");
        let ent = crate::enterprise::resolve_release(token, &tag)
            .await
            .map_err(|e| {
                Status::failed_precondition(format!(
                    "enterprise channel for {tag}: {e}; check the subscription with `kctl get subscription`"
                ))
            })?;
        let release = cluster_version::resolve_release_at_rev(
            crate::enterprise::DEFAULT_FLAKE_REF,
            &tag,
            &ent.kcore_rev,
        )
        .await
        .map_err(Status::failed_precondition)?;
        (
            release,
            Some(EnterpriseTarget {
                prebuilt_url: ent.prebuilt_url,
                prebuilt_sha256: ent.prebuilt_sha256,
            }),
        )
    } else {
        let release = cluster_version::resolve_release(&flake_ref, &tag)
            .await
            .map_err(Status::failed_precondition)?;
        (release, None)
    };
    cluster_version::check_schema(&release, crate::db::MIN_COMPATIBLE_SCHEMA_READER)
        .map_err(Status::failed_precondition)?;

    if !(0..=crate::canary::MAX_SOAK_SECS).contains(&input.canary_soak_seconds) {
        return Err(Status::invalid_argument(format!(
            "canary_soak_seconds must be between 0 and {}",
            crate::canary::MAX_SOAK_SECS
        )));
    }

    let versions = candidate_versions(db, &input)?;
    let (direction, target_ids) = cluster_version::plan(&release.version, &versions);
    let soak_secs = match input.canary_soak_seconds {
        0 => crate::canary::default_soak_secs(
            &release.nixos_release,
            &crate::canary::running_nixos_release(),
        ),
        secs => secs,
    };
    let canary_node = input.canary_node_id.trim();
    let strategy =
        cluster_version::strategy((!input.no_canary).then_some((soak_secs, canary_node)));

    if input.persist_desired && !input.dry_run {
        // An operator apply clears any hold from a failed or cancelled rollout.
        db.upsert_cluster_desired_version(&ClusterDesiredVersionRow {
            version: tag.clone(),
            flake_ref: flake_ref.clone(),
            channel: channel.to_string(),
            drain_vms: input.drain_vms,
            reboot: input.reboot,
            no_canary: input.no_canary,
            canary_soak_seconds: input.canary_soak_seconds,
            canary_node_id: canary_node.to_string(),
            last_rollout_generation: 0,
            last_rollout_name: String::new(),
            updated_at: String::new(),
        })
        .map_err(internal_db)?;
    }

    let mut resp = SetClusterVersionResponse {
        direction: direction.to_string(),
        version: tag.clone(),
        flake_rev: release.flake_rev.clone(),
        target_node_ids: target_ids.clone(),
        ..Default::default()
    };
    if target_ids.is_empty() {
        let message = format!("every node runs {tag}");
        resp.message = message.clone();
        return Ok(StartOutcome::Unchanged {
            version: tag,
            flake_rev: release.flake_rev,
            message,
            response: resp,
        });
    }

    let spec = cluster_version::update_spec_with_channel(
        &name,
        &flake_ref,
        &release,
        target_ids.clone(),
        input.drain_vms,
        input.reboot,
        strategy,
        channel,
        enterprise_target,
    );
    cluster_update_spec::validate_spec(&spec).map_err(Status::invalid_argument)?;
    let mut ordered: Vec<ClusterUpdateNodeRow> = target_ids
        .iter()
        .map(|node_id| ClusterUpdateNodeRow {
            update_name: name.clone(),
            node_id: node_id.clone(),
            observed_generation: 0,
            phase: "pending".into(),
            current_version: String::new(),
            target_version: release.version.clone(),
            prepared_closure: String::new(),
            current_generation: String::new(),
            target_generation: String::new(),
            requires_reboot: false,
            last_error: String::new(),
            last_transition_at: String::new(),
        })
        .collect();
    crate::cluster_update_reconciler::rollout_order(db, &mut ordered);
    let ordered: Vec<String> = ordered.into_iter().map(|row| row.node_id).collect();
    if !input.no_canary {
        crate::canary::check_named(db, &ordered, canary_node).map_err(Status::invalid_argument)?;
    }
    if let Some(canary) = crate::canary::pick(db, &spec, &ordered) {
        resp.canary_node_id = canary;
        resp.canary_soak_seconds = soak_secs;
    }
    if input.dry_run {
        resp.message = format!("dry run: would {direction} {} node(s)", target_ids.len());
        return Ok(StartOutcome::DryRun { response: resp });
    }

    let spec_json = cluster_update_spec::spec_to_json(&spec)
        .map_err(|e| Status::internal(format!("encode spec: {e}")))?;
    let existing = db.get_cluster_update(&name).map_err(internal_db)?;
    let generation = existing
        .as_ref()
        .map(|row| row.generation.saturating_add(1))
        .unwrap_or(1);
    db.upsert_cluster_update(&ClusterUpdateRow {
        name: name.clone(),
        generation,
        target_version: release.version.clone(),
        flake_ref: flake_ref.clone(),
        flake_rev: release.flake_rev.clone(),
        spec_json,
        phase: "ready".into(),
        approval_status: "approved".into(),
        created_at: existing.map(|row| row.created_at).unwrap_or_default(),
        updated_at: String::new(),
    })
    .map_err(internal_db)?;
    let node_rows: Vec<ClusterUpdateNodeRow> = resp
        .target_node_ids
        .iter()
        .map(|node_id| ClusterUpdateNodeRow {
            update_name: name.clone(),
            node_id: node_id.clone(),
            observed_generation: generation,
            phase: "pending".into(),
            current_version: String::new(),
            target_version: release.version.clone(),
            prepared_closure: String::new(),
            current_generation: String::new(),
            target_generation: String::new(),
            requires_reboot: false,
            last_error: String::new(),
            last_transition_at: String::new(),
        })
        .collect();
    db.replace_cluster_update_nodes(&name, &node_rows)
        .map_err(internal_db)?;

    if let Some(mut desired) = db.get_cluster_desired_version().map_err(internal_db)? {
        desired.last_rollout_generation = generation;
        desired.last_rollout_name = name.clone();
        db.upsert_cluster_desired_version(&desired)
            .map_err(internal_db)?;
    }

    let stored = db
        .get_cluster_update(&name)
        .map_err(internal_db)?
        .ok_or_else(|| Status::internal("cluster update vanished"))?;
    resp.update_name = name;
    resp.cluster_update = Some(cluster_update_row_to_proto(&stored, spec));
    resp.message = if resp.canary_node_id.is_empty() {
        format!("{direction} to {tag} started")
    } else {
        format!(
            "{direction} to {tag} started; canary {} soaks for {}s before the other nodes",
            resp.canary_node_id, resp.canary_soak_seconds
        )
    };
    Ok(StartOutcome::Started { response: resp })
}

fn candidate_versions(
    db: &Database,
    input: &DesiredVersionInput,
) -> Result<Vec<(String, String)>, tonic::Status> {
    let nodes = db.list_nodes().map_err(internal_db)?;
    if !input.only_ready_nodes {
        return Ok(nodes
            .into_iter()
            .map(|node| {
                let version = db
                    .get_node_host_status(&node.id)
                    .ok()
                    .flatten()
                    .map(|host| host.kcore_version)
                    .unwrap_or_default();
                (node.id, version)
            })
            .collect());
    }
    let mut all = Vec::new();
    for node in &nodes {
        if node.approval_status != "approved" || node.status == "not-ready" {
            continue;
        }
        if !db
            .node_recently_seen(&node.id, HEARTBEAT_STALE_SECS)
            .unwrap_or(false)
        {
            continue;
        }
        let version = db
            .get_node_host_status(&node.id)
            .ok()
            .flatten()
            .map(|h| h.kcore_version)
            .unwrap_or_default();
        if version.trim().is_empty() {
            continue;
        }
        all.push((node.id.clone(), version));
    }
    Ok(all)
}

fn cluster_update_row_to_proto(
    row: &ClusterUpdateRow,
    spec: crate::controller_proto::ClusterUpdateSpec,
) -> ClusterUpdate {
    ClusterUpdate {
        spec: Some(spec),
        generation: row.generation,
        phase: match row.phase.as_str() {
            "pending" => ClusterUpdatePhase::Pending as i32,
            "ready" => ClusterUpdatePhase::Ready as i32,
            "rolling_out" => ClusterUpdatePhase::RollingOut as i32,
            "succeeded" => ClusterUpdatePhase::Succeeded as i32,
            "failed" => ClusterUpdatePhase::Failed as i32,
            "cancelled" => ClusterUpdatePhase::Cancelled as i32,
            "rolling_back" => ClusterUpdatePhase::RollingBack as i32,
            _ => ClusterUpdatePhase::Unspecified as i32,
        },
        ..Default::default()
    }
}

fn internal_db(err: rusqlite::Error) -> tonic::Status {
    tonic::Status::internal(format!("database: {err}"))
}

/// Build the status fields for GetClusterVersion.
pub fn status_fields(
    db: &Database,
    desired: Option<&ClusterDesiredVersionRow>,
    latest: Option<&ClusterUpdateRow>,
    nodes: &[crate::controller_proto::NodeVersion],
) -> (bool, String) {
    let Some(desired) = desired else {
        return (false, String::new());
    };
    if desired.version.is_empty() {
        return (false, String::new());
    }
    let updates = db.list_cluster_updates().unwrap_or_default();
    if let Some((name, phase)) = active_version_update(&updates) {
        return (false, format!("update {name} is {phase}"));
    }
    let want = cluster_version::parse_version(&desired.version);
    let mut off = 0usize;
    for n in nodes {
        if n.version.trim().is_empty() {
            continue;
        }
        if cluster_version::parse_version(&n.version) != want {
            off += 1;
        }
    }
    if off == 0 {
        let _ = latest;
        return (true, String::new());
    }
    if let Some(reason) = holding_reason(desired, &updates) {
        return (false, reason);
    }
    (
        false,
        format!("{off} node(s) are not on {}", desired.version),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster_version::Release;

    fn desired(version: &str) -> ClusterDesiredVersionRow {
        ClusterDesiredVersionRow {
            version: version.into(),
            flake_ref: String::new(),
            channel: "community".into(),
            drain_vms: false,
            reboot: false,
            no_canary: false,
            canary_soak_seconds: 0,
            canary_node_id: String::new(),
            last_rollout_generation: 1,
            last_rollout_name: "version-v0-9-0".into(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn decide_converge_covers_the_gates() {
        assert_eq!(
            decide_converge(None, None, None, 1, true),
            ConvergeDecision::SkipNoDesired
        );
        assert_eq!(
            decide_converge(Some(&desired("v0.9.0")), None, None, 1, false),
            ConvergeDecision::SkipNotLeaseHolder
        );
        assert_eq!(
            decide_converge(
                Some(&desired("v0.9.0")),
                Some(&("version-v0-9-0".into(), "rolling_out".into())),
                None,
                1,
                true
            ),
            ConvergeDecision::SkipActiveUpdate {
                name: "version-v0-9-0".into()
            }
        );
        assert_eq!(
            decide_converge(
                Some(&desired("v0.9.0")),
                None,
                Some("last rollout failed"),
                1,
                true
            ),
            ConvergeDecision::SkipHolding {
                reason: "last rollout failed".into()
            }
        );
        assert_eq!(
            decide_converge(Some(&desired("v0.9.0")), None, None, 0, true),
            ConvergeDecision::SkipInSync
        );
        assert_eq!(
            decide_converge(Some(&desired("v0.9.0")), None, None, 2, true),
            ConvergeDecision::Start
        );
    }

    #[test]
    fn holding_reason_only_for_failed_or_cancelled_matching_generation() {
        let d = desired("v0.9.0");
        let mut row = ClusterUpdateRow {
            name: "version-v0-9-0".into(),
            generation: 1,
            target_version: "0.9.0".into(),
            flake_ref: String::new(),
            flake_rev: String::new(),
            spec_json: "{}".into(),
            phase: "failed".into(),
            approval_status: "approved".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(holding_reason(&d, &[row.clone()])
            .unwrap()
            .contains("failed"));
        row.phase = "cancelled".into();
        assert!(holding_reason(&d, &[row.clone()])
            .unwrap()
            .contains("cancelled"));
        row.phase = "succeeded".into();
        assert!(holding_reason(&d, &[row.clone()]).is_none());
        row.phase = "failed".into();
        row.generation = 2;
        assert!(holding_reason(&d, &[row]).is_none());
    }

    #[test]
    fn force_reboot_maps_to_boot_activation() {
        let release = Release {
            tag: "v0.9.0".into(),
            version: "0.9.0".into(),
            flake_rev: "a".repeat(40),
            schema_version: 80,
            min_compatible_reader: 56,
            nixos_release: "26.11".into(),
        };
        let auto = cluster_version::update_spec_with_channel(
            "version-v0-9-0",
            cluster_version::DEFAULT_FLAKE_REF,
            &release,
            vec!["n1".into()],
            false,
            false,
            cluster_version::strategy(None),
            "community",
            None,
        );
        assert_eq!(
            auto.activation_mode,
            crate::controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationAuto
                as i32
        );
        let forced = cluster_version::update_spec_with_channel(
            "version-v0-9-0",
            cluster_version::DEFAULT_FLAKE_REF,
            &release,
            vec!["n1".into()],
            true,
            true,
            cluster_version::strategy(None),
            "community",
            None,
        );
        assert_eq!(
            forced.activation_mode,
            crate::controller_proto::ClusterUpdateActivationMode::ClusterUpdateActivationBoot
                as i32
        );
        assert!(forced.drain_vms);
    }
}
