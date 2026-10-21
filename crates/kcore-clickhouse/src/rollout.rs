//! Pure rollout order for ClickHouse servers and Keeper.

use std::collections::{BTreeMap, BTreeSet};

use crate::spec::{ClusterSpec, PlanAction, PlanDiff};

pub const APPLY_NIX: &str = "apply-nix";
pub const WAIT_NIX: &str = "wait-nix";
pub const WAIT_KEEPER_QUORUM: &str = "wait-keeper-quorum";
pub const WAIT_SERVER_READY: &str = "wait-server-ready";
pub const WAIT_REPLICA: &str = "wait-replica-caught-up";
pub const CREATE_DATABASES: &str = "create-databases";
pub const CREATE_FUNCTIONS: &str = "create-functions";
pub const CREATE_DICTIONARIES: &str = "create-dictionaries";
pub const SET_TTL: &str = "set-system-log-ttl";
pub const VERIFY: &str = "verify-clusters";
pub const STOP_UNIT: &str = "stop-unit";
pub const RESTART_UNIT: &str = "restart-unit";
pub const DROP_REPLICA: &str = "drop-replica";
pub const RELOAD: &str = "reload-config";
pub const RELEASE_VOLUME: &str = "release-volume";
pub const BACKUP: &str = "backup";
pub const BACKUP_ACCESS: &str = "backup-access";
pub const RESTORE: &str = "restore";
pub const RESTORE_ACCESS: &str = "restore-access";
pub const RESTORE_REPLICA: &str = "restore-replica";
pub const LOCAL_PARTS: &str = "local-parts-count";
pub const WARNING: &str = "warning";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    Create,
    Update,
    Upgrade,
    Scale,
    Replace,
    Delete,
    Backup,
    Restore,
    RestoreReplica,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub kind: String,
    pub node_id: String,
    pub unit: String,
    pub detail: String,
    pub group: i32,
    pub state: String,
    pub message: String,
}

impl Step {
    fn new(kind: &str, node: &str, unit: &str, detail: &str, group: i32) -> Self {
        Self {
            kind: kind.into(),
            node_id: node.into(),
            unit: unit.into(),
            detail: detail.into(),
            group,
            state: "pending".into(),
            message: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MemberHealth {
    pub node_id: String,
    pub server_active: bool,
    pub keeper_active: bool,
    pub keeper_role: String,
    pub readonly_tables: i32,
    pub max_delay_seconds: i32,
    pub keeper_session_ok: bool,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct RolloutRequest<'a> {
    pub before: Option<&'a ClusterSpec>,
    pub after: &'a ClusterSpec,
    pub kind: OpKind,
    pub health: &'a [MemberHealth],
    pub diff: Option<&'a PlanDiff>,
    pub intermediate: bool,
    pub retain_data: bool,
    pub skip_access: bool,
    pub backup_name: String,
    pub restore_node: String,
    pub removed_replicas: Vec<(String, String, String)>,
}

pub fn plan_rollout(req: &RolloutRequest<'_>) -> Vec<Step> {
    match req.kind {
        OpKind::Create => create_steps(req.after),
        OpKind::Update => update_steps(req),
        OpKind::Upgrade => upgrade_steps(req),
        OpKind::Scale | OpKind::Replace => scale_steps(req),
        OpKind::Delete => delete_steps(req),
        OpKind::Backup => backup_steps(req.after, &req.backup_name, req.skip_access),
        OpKind::Restore => restore_steps(req.after, &req.backup_name, req.skip_access),
        OpKind::RestoreReplica => vec![
            Step::new(RESTORE_REPLICA, &req.restore_node, "server", "", 1),
            Step::new(WAIT_REPLICA, &req.restore_node, "server", "", 2),
        ],
    }
}

pub fn remaining(steps: &[Step]) -> Vec<Step> {
    steps
        .iter()
        .filter(|step| step.state != "succeeded" && step.state != "skipped")
        .cloned()
        .collect()
}

fn create_steps(spec: &ClusterSpec) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut group = 1;
    for node in member_ids(spec) {
        steps.push(Step::new(APPLY_NIX, &node, "both", "enable", group));
    }
    group += 1;
    for node in member_ids(spec) {
        steps.push(Step::new(WAIT_NIX, &node, "both", "", group));
    }
    group += 1;
    steps.push(Step::new(WAIT_KEEPER_QUORUM, "", "keeper", "", group));
    group += 1;
    for replica in replicas(spec) {
        steps.push(Step::new(
            WAIT_SERVER_READY,
            &replica.node_id,
            "server",
            &replica.shard,
            group,
        ));
    }
    group += 1;
    if let Some(node) = first_writable(spec) {
        steps.push(Step::new(CREATE_DATABASES, &node, "server", "", group));
        group += 1;
        steps.push(Step::new(CREATE_FUNCTIONS, &node, "server", "", group));
        group += 1;
        steps.push(Step::new(CREATE_DICTIONARIES, &node, "server", "", group));
    }
    group += 1;
    let mut ttl = Step::new(SET_TTL, "", "server", "noop", group);
    ttl.state = "skipped".into();
    ttl.message = "engine string already has the TTL".into();
    steps.push(ttl);
    group += 1;
    let verify_node = first_writable(spec).unwrap_or_default();
    steps.push(Step::new(VERIFY, &verify_node, "server", "", group));
    steps
}

fn update_steps(req: &RolloutRequest<'_>) -> Vec<Step> {
    let restart = req
        .diff
        .is_some_and(|d| matches!(d.action, PlanAction::Update { restart: true }));
    let mut steps = Vec::new();
    let nodes = member_ids(req.after);
    if !restart {
        for node in &nodes {
            steps.push(Step::new(APPLY_NIX, node, "both", "reload", 1));
        }
        for node in &nodes {
            steps.push(Step::new(RELOAD, node, "server", "", 2));
        }
        if req
            .diff
            .is_some_and(|d| d.changed_fields.iter().any(|f| f == "systemLogs"))
        {
            for replica in replicas(req.after) {
                steps.push(Step::new(SET_TTL, &replica.node_id, "server", "", 3));
            }
        }
        return steps;
    }
    let mut group = 1;
    steps.extend(rolling_servers(req.after, req.health, &mut group, "target"));
    steps
}

fn upgrade_steps(req: &RolloutRequest<'_>) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut group = 1;
    if req.intermediate {
        steps.extend(rolling_keepers(
            req.after,
            req.health,
            &mut group,
            "intermediate",
        ));
        steps.push(Step::new(
            WAIT_KEEPER_QUORUM,
            "",
            "keeper",
            "intermediate",
            group,
        ));
        group += 1;
    }
    steps.extend(rolling_keepers(req.after, req.health, &mut group, "target"));
    steps.push(Step::new(WAIT_KEEPER_QUORUM, "", "keeper", "target", group));
    group += 1;
    steps.extend(rolling_servers(req.after, req.health, &mut group, "target"));
    steps
}

fn scale_steps(req: &RolloutRequest<'_>) -> Vec<Step> {
    let before = req.before.unwrap_or(req.after);
    let old_nodes: BTreeSet<_> = member_ids(before).into_iter().collect();
    let new_nodes: BTreeSet<_> = member_ids(req.after).into_iter().collect();
    let added: Vec<_> = new_nodes.difference(&old_nodes).cloned().collect();
    let removed_nodes: BTreeSet<_> = old_nodes.difference(&new_nodes).cloned().collect();
    let mut steps = Vec::new();
    let mut group = 1;
    for (shard, replica, node) in &req.removed_replicas {
        if replicas(req.after)
            .iter()
            .filter(|r| r.shard == *shard)
            .count()
            == 0
        {
            steps.push(Step::new(
                WARNING,
                node,
                "server",
                &format!("removing the last replica of shard {shard}"),
                group,
            ));
        }
        steps.push(Step::new(LOCAL_PARTS, node, "server", shard, group));
        group += 1;
        steps.push(Step::new(STOP_UNIT, node, "server", "", group));
        group += 1;
        let survivor = replicas(req.after)
            .into_iter()
            .find(|r| r.shard == *shard && !r.read_only)
            .map(|r| r.node_id)
            .unwrap_or_default();
        steps.push(Step::new(
            DROP_REPLICA,
            &survivor,
            "server",
            &format!("{shard}/{replica}"),
            group,
        ));
        group += 1;
        if !removed_nodes.contains(node) {
            steps.push(Step::new(APPLY_NIX, node, "keeper", "keeper-only", group));
        } else {
            steps.push(Step::new(APPLY_NIX, node, "both", "disable", group));
        }
        group += 1;
        if !req.retain_data {
            steps.push(Step::new(RELEASE_VOLUME, node, "server", "", group));
            group += 1;
        }
    }
    for node in member_ids(req.after) {
        steps.push(Step::new(APPLY_NIX, &node, "both", "reload", group));
    }
    group += 1;
    for node in &added {
        steps.push(Step::new(APPLY_NIX, node, "both", "enable", group));
        group += 1;
        steps.push(Step::new(WAIT_NIX, node, "both", "", group));
        group += 1;
        steps.push(Step::new(CREATE_DATABASES, node, "server", "", group));
        group += 1;
        steps.push(Step::new(WAIT_REPLICA, node, "server", "", group));
        group += 1;
    }
    if added.iter().any(|node| {
        shard_of(req.after, node)
            .is_some_and(|shard| replicas(before).iter().all(|r| r.shard != shard))
    }) {
        steps.push(Step::new(
            WARNING,
            "",
            "server",
            "kcore does not redistribute existing data onto a new shard",
            group,
        ));
    }
    steps
}

fn delete_steps(req: &RolloutRequest<'_>) -> Vec<Step> {
    let spec = req.before.unwrap_or(req.after);
    let mut steps = Vec::new();
    let mut group = 1;
    for replica in replicas(spec) {
        steps.push(Step::new(STOP_UNIT, &replica.node_id, "server", "", group));
        group += 1;
    }
    for keeper in &spec.keepers {
        steps.push(Step::new(STOP_UNIT, &keeper.node_id, "keeper", "", group));
        group += 1;
    }
    for node in member_ids(spec) {
        steps.push(Step::new(APPLY_NIX, &node, "both", "disable", group));
    }
    group += 1;
    if !req.retain_data {
        for node in member_ids(spec) {
            steps.push(Step::new(RELEASE_VOLUME, &node, "both", "", group));
        }
    }
    steps
}

fn backup_steps(spec: &ClusterSpec, name: &str, skip_access: bool) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut seen = BTreeSet::new();
    let mut group = 1;
    for replica in replicas(spec) {
        if replica.read_only || !seen.insert(replica.shard.clone()) {
            continue;
        }
        steps.push(Step::new(
            BACKUP,
            &replica.node_id,
            "server",
            &format!("{name}/{}", replica.shard),
            group,
        ));
        group += 1;
    }
    if !skip_access {
        if let Some(node) = first_writable(spec) {
            steps.push(Step::new(BACKUP_ACCESS, &node, "server", name, group));
        }
    }
    steps
}

fn restore_steps(spec: &ClusterSpec, name: &str, skip_access: bool) -> Vec<Step> {
    let mut steps = backup_steps(spec, name, true);
    for step in &mut steps {
        if step.kind == BACKUP {
            step.kind = RESTORE.into();
        }
    }
    if !skip_access {
        if let Some(node) = first_writable(spec) {
            let group = steps.last().map(|s| s.group + 1).unwrap_or(1);
            steps.push(Step::new(RESTORE_ACCESS, &node, "server", name, group));
        }
    }
    steps
}

fn rolling_keepers(
    spec: &ClusterSpec,
    health: &[MemberHealth],
    group: &mut i32,
    pass: &str,
) -> Vec<Step> {
    let mut steps = Vec::new();
    let leader = health
        .iter()
        .find(|h| h.keeper_role == "leader")
        .map(|h| h.node_id.as_str());
    let mut followers = Vec::new();
    let mut leader_node = None;
    let mut learners = Vec::new();
    for keeper in &spec.keepers {
        if keeper.learner {
            learners.push(keeper.node_id.clone());
        } else if leader == Some(keeper.node_id.as_str()) {
            leader_node = Some(keeper.node_id.clone());
        } else {
            followers.push(keeper.node_id.clone());
        }
    }
    for node in followers.into_iter().chain(leader_node).chain(learners) {
        let role = if spec.keepers.iter().any(|k| k.node_id == node && k.learner) {
            "learner"
        } else if leader == Some(node.as_str()) {
            "leader"
        } else {
            "follower"
        };
        steps.push(Step::new(APPLY_NIX, &node, "keeper", pass, *group));
        *group += 1;
        steps.push(Step::new(
            RESTART_UNIT,
            &node,
            "keeper",
            &format!("{pass}:{role}"),
            *group,
        ));
        *group += 1;
        steps.push(Step::new(WAIT_KEEPER_QUORUM, &node, "keeper", pass, *group));
        *group += 1;
    }
    steps
}

fn rolling_servers(
    spec: &ClusterSpec,
    health: &[MemberHealth],
    group: &mut i32,
    pass: &str,
) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut by_shard: BTreeMap<String, Vec<ReplicaRef>> = BTreeMap::new();
    for replica in replicas(spec) {
        by_shard
            .entry(replica.shard.clone())
            .or_default()
            .push(replica);
    }
    for (shard, members) in by_shard {
        let writable: Vec<_> = members.iter().filter(|r| !r.read_only).cloned().collect();
        let readers: Vec<_> = members.iter().filter(|r| r.read_only).cloned().collect();
        if writable.len() == 1 {
            steps.push(Step::new(
                WARNING,
                &writable[0].node_id,
                "server",
                &format!("shard {shard} has one writable replica; inserts pause while it restarts"),
                *group,
            ));
        }
        for replica in writable.into_iter().chain(readers) {
            if !replica.read_only
                && !writable_gate(spec, &shard, &replica.node_id, health)
                && writable_count(spec, &shard) > 1
            {
                steps.push(Step::new(
                    WARNING,
                    &replica.node_id,
                    "server",
                    "waiting for another writable replica before restart",
                    *group,
                ));
            }
            steps.push(Step::new(
                APPLY_NIX,
                &replica.node_id,
                "server",
                pass,
                *group,
            ));
            *group += 1;
            steps.push(Step::new(
                RESTART_UNIT,
                &replica.node_id,
                "server",
                &format!("{pass}:{}", replica.shard),
                *group,
            ));
            *group += 1;
            steps.push(Step::new(
                WAIT_SERVER_READY,
                &replica.node_id,
                "server",
                &replica.shard,
                *group,
            ));
            *group += 1;
            steps.push(Step::new(
                WAIT_REPLICA,
                &replica.node_id,
                "server",
                &replica.shard,
                *group,
            ));
            *group += 1;
        }
    }
    steps
}

fn writable_count(spec: &ClusterSpec, shard: &str) -> usize {
    replicas(spec)
        .into_iter()
        .filter(|r| r.shard == shard && !r.read_only)
        .count()
}

pub fn writable_gate(spec: &ClusterSpec, shard: &str, node: &str, health: &[MemberHealth]) -> bool {
    let max_delay = spec.max_replica_delay_seconds;
    replicas(spec).into_iter().any(|other| {
        other.shard == shard
            && other.node_id != node
            && !other.read_only
            && health.iter().any(|h| {
                h.node_id == other.node_id
                    && h.server_active
                    && h.readonly_tables == 0
                    && h.max_delay_seconds <= max_delay
                    && h.keeper_session_ok
            })
    })
}

pub fn safe_to_take_down(
    spec: &ClusterSpec,
    node_id: &str,
    health: &[MemberHealth],
) -> Result<(), String> {
    let on_server = replicas(spec).into_iter().any(|r| r.node_id == node_id);
    let keeper = spec.keepers.iter().find(|k| k.node_id == node_id);
    if !on_server && keeper.is_none() {
        return Ok(());
    }
    if health.is_empty() {
        return Err(format!(
            "clickhouse health for {node_id} has not been observed"
        ));
    }
    for replica in replicas(spec)
        .into_iter()
        .filter(|r| r.node_id == node_id && !r.read_only)
    {
        if !writable_gate(spec, &replica.shard, node_id, health) {
            return Err(format!(
                "shard {} would have no other healthy writable replica",
                replica.shard
            ));
        }
    }
    if keeper.is_some() {
        keeper_restart_allowed(spec, node_id, health)?;
    }
    Ok(())
}

/// A Keeper process restart leaves the server up. Voters still need a
/// surviving majority. Learners do not vote.
pub fn keeper_restart_allowed(
    spec: &ClusterSpec,
    node_id: &str,
    health: &[MemberHealth],
) -> Result<(), String> {
    let Some(keeper) = spec.keepers.iter().find(|k| k.node_id == node_id) else {
        return Ok(());
    };
    if health.is_empty() {
        return Err(format!(
            "clickhouse health for {node_id} has not been observed"
        ));
    }
    if keeper.learner {
        return Ok(());
    }
    let voting: Vec<_> = spec.keepers.iter().filter(|k| !k.learner).collect();
    let up = voting
        .iter()
        .filter(|k| {
            k.node_id != node_id
                && health.iter().any(|h| {
                    h.node_id == k.node_id && h.keeper_active && h.keeper_role != "unknown"
                })
        })
        .count();
    if up <= voting.len() / 2 {
        return Err("Keeper quorum would not survive this node".into());
    }
    Ok(())
}

/// Idle cluster phase from the latest member poll.
/// `failed` means a shard has no writable replica or Keeper lost quorum.
pub fn observed_phase(spec: &ClusterSpec, health: &[MemberHealth]) -> &'static str {
    let health_of = |node: &str| health.iter().find(|row| row.node_id == node);
    let voting: Vec<_> = spec.keepers.iter().filter(|k| !k.learner).collect();
    let voting_up = voting
        .iter()
        .filter(|keeper| {
            health_of(&keeper.node_id).is_some_and(|row| {
                row.keeper_active && !row.keeper_role.is_empty() && row.keeper_role != "unknown"
            })
        })
        .count();
    let leaders = voting
        .iter()
        .filter(|keeper| health_of(&keeper.node_id).is_some_and(|row| row.keeper_role == "leader"))
        .count();
    let quorum = voting.is_empty() || (leaders == 1 && voting_up > voting.len() / 2);
    let mut missing_writable = false;
    let mut degraded = false;
    for shard in &spec.shards {
        let mut writable = false;
        for replica in &shard.replicas {
            let Some(live) = health_of(&replica.node_id) else {
                degraded = true;
                continue;
            };
            if !live.server_active
                || live.readonly_tables > 0
                || live.max_delay_seconds > spec.max_replica_delay_seconds
            {
                degraded = true;
            }
            if !replica.read_only && live.server_active && live.readonly_tables == 0 {
                writable = true;
            }
        }
        if !writable {
            missing_writable = true;
        }
    }
    let keeper_down = voting
        .iter()
        .any(|keeper| health_of(&keeper.node_id).is_some_and(|row| !row.keeper_active));
    if missing_writable || !quorum {
        "failed"
    } else if degraded || keeper_down {
        "degraded"
    } else {
        "ready"
    }
}

#[derive(Clone)]
struct ReplicaRef {
    node_id: String,
    shard: String,
    read_only: bool,
}

fn replicas(spec: &ClusterSpec) -> Vec<ReplicaRef> {
    let mut out = Vec::new();
    for shard in &spec.shards {
        for replica in &shard.replicas {
            out.push(ReplicaRef {
                node_id: replica.node_id.clone(),
                shard: shard.shard_name.clone(),
                read_only: replica.read_only,
            });
        }
    }
    out
}

fn member_ids(spec: &ClusterSpec) -> Vec<String> {
    let mut ids = BTreeSet::new();
    for replica in replicas(spec) {
        ids.insert(replica.node_id);
    }
    for keeper in &spec.keepers {
        ids.insert(keeper.node_id.clone());
    }
    ids.into_iter().collect()
}

fn first_writable(spec: &ClusterSpec) -> Option<String> {
    spec.shards
        .iter()
        .min_by(|a, b| a.shard_name.cmp(&b.shard_name))
        .and_then(|shard| {
            shard
                .replicas
                .iter()
                .find(|r| !r.read_only)
                .map(|r| r.node_id.clone())
        })
}

fn shard_of(spec: &ClusterSpec, node: &str) -> Option<String> {
    replicas(spec)
        .into_iter()
        .find(|r| r.node_id == node)
        .map(|r| r.shard)
}

pub fn rollout_invariants(spec: &ClusterSpec, steps: &[Step]) -> Result<(), String> {
    let mut open: BTreeMap<String, String> = BTreeMap::new();
    let mut saw_server_restart = false;
    let mut keeper_roles = Vec::new();
    for step in steps {
        if step.kind == RESTART_UNIT && step.unit == "keeper" {
            if saw_server_restart {
                return Err("keeper restart after a server restart".into());
            }
            if let Some((_, role)) = step.detail.split_once(':') {
                keeper_roles.push(role.to_string());
            }
        }
        if step.kind == RESTART_UNIT && step.unit == "server" {
            saw_server_restart = true;
            let shard = step.detail.split(':').nth(1).unwrap_or("");
            if let Some(other) = open.get(shard) {
                return Err(format!(
                    "shard {shard} restarts {other} and {} together",
                    step.node_id
                ));
            }
            open.insert(shard.to_string(), step.node_id.clone());
        }
        if step.kind == WAIT_REPLICA {
            open.remove(&step.detail);
        }
    }
    let followers_done = keeper_roles.iter().position(|r| r != "follower");
    if let Some(idx) = followers_done {
        if keeper_roles[..idx].iter().any(|r| r != "follower") {
            return Err("voting followers are not first".into());
        }
    }
    if keeper_roles.iter().filter(|r| *r == "leader").count() > 1 {
        return Err("more than one leader restart".into());
    }
    let _ = spec;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests::two_by_two;

    fn health_leader(node: &str) -> MemberHealth {
        MemberHealth {
            node_id: node.into(),
            server_active: true,
            keeper_active: true,
            keeper_role: if node == "node-106" {
                "leader".into()
            } else {
                "follower".into()
            },
            readonly_tables: 0,
            max_delay_seconds: 0,
            keeper_session_ok: true,
            version: String::new(),
        }
    }

    #[test]
    fn create_applies_in_one_group_then_quorum_then_databases() {
        let spec = two_by_two();
        let steps = plan_rollout(&RolloutRequest {
            before: None,
            after: &spec,
            kind: OpKind::Create,
            health: &[],
            diff: None,
            intermediate: false,
            retain_data: false,
            skip_access: false,
            backup_name: String::new(),
            restore_node: String::new(),
            removed_replicas: vec![],
        });
        let applies: Vec<_> = steps.iter().filter(|s| s.kind == APPLY_NIX).collect();
        assert_eq!(applies.len(), 4);
        assert!(applies.iter().all(|s| s.group == applies[0].group));
        let quorum = steps
            .iter()
            .position(|s| s.kind == WAIT_KEEPER_QUORUM)
            .unwrap();
        let databases = steps
            .iter()
            .position(|s| s.kind == CREATE_DATABASES)
            .unwrap();
        assert!(quorum < databases);
        assert!(rollout_invariants(&spec, &steps).is_ok());
    }

    #[test]
    fn upgrade_restarts_followers_then_leader_then_servers() {
        let spec = two_by_two();
        let health = vec![
            health_leader("node-105"),
            health_leader("node-106"),
            health_leader("node-108"),
            health_leader("node-109"),
        ];
        let steps = plan_rollout(&RolloutRequest {
            before: Some(&spec),
            after: &spec,
            kind: OpKind::Upgrade,
            health: &health,
            diff: None,
            intermediate: false,
            retain_data: false,
            skip_access: false,
            backup_name: String::new(),
            restore_node: String::new(),
            removed_replicas: vec![],
        });
        let keepers: Vec<_> = steps
            .iter()
            .filter(|s| s.kind == RESTART_UNIT && s.unit == "keeper")
            .map(|s| s.detail.clone())
            .collect();
        assert!(keepers[0].ends_with("follower") || keepers[1].ends_with("follower"));
        assert!(keepers.iter().any(|d| d.ends_with("leader")));
        let leader_at = keepers.iter().position(|d| d.ends_with("leader")).unwrap();
        assert!(keepers[..leader_at].iter().all(|d| d.ends_with("follower")));
        let first_server = steps
            .iter()
            .position(|s| s.kind == RESTART_UNIT && s.unit == "server")
            .unwrap();
        let last_keeper = steps
            .iter()
            .rposition(|s| s.kind == RESTART_UNIT && s.unit == "keeper")
            .unwrap();
        assert!(last_keeper < first_server);
        assert!(rollout_invariants(&spec, &steps).is_ok());
    }

    #[test]
    fn read_replica_does_not_unlock_a_writable_restart() {
        let mut spec = two_by_two();
        spec.shards[0].replicas[1].read_only = true;
        let health = vec![MemberHealth {
            node_id: "node-106".into(),
            server_active: true,
            keeper_active: true,
            keeper_role: "follower".into(),
            readonly_tables: 0,
            max_delay_seconds: 0,
            keeper_session_ok: true,
            version: String::new(),
        }];
        assert!(!writable_gate(&spec, "1", "node-105", &health));
    }

    #[test]
    fn single_writable_replica_warns_and_resume_skips_succeeded() {
        let mut spec = two_by_two();
        spec.shards[0].replicas.pop();
        let steps = plan_rollout(&RolloutRequest {
            before: Some(&spec),
            after: &spec,
            kind: OpKind::Upgrade,
            health: &[],
            diff: None,
            intermediate: true,
            retain_data: false,
            skip_access: true,
            backup_name: String::new(),
            restore_node: String::new(),
            removed_replicas: vec![],
        });
        assert!(steps.iter().any(|s| s.kind == WARNING
            && s.message.is_empty()
            && s.detail.contains("one writable")));
        assert!(steps
            .iter()
            .any(|s| s.detail == "intermediate" || s.detail.starts_with("intermediate")));
        let mut done = steps.clone();
        done[0].state = "succeeded".into();
        assert_eq!(remaining(&done).len(), steps.len() - 1);
    }

    fn live(node: &str, role: &str, server: bool, keeper: bool) -> MemberHealth {
        MemberHealth {
            node_id: node.into(),
            server_active: server,
            keeper_active: keeper,
            keeper_role: role.into(),
            readonly_tables: 0,
            max_delay_seconds: 0,
            keeper_session_ok: true,
            version: String::new(),
        }
    }

    #[test]
    fn observed_phase_follows_quorum_and_writable_replicas() {
        let spec = two_by_two();
        let healthy = vec![
            live("node-105", "leader", true, true),
            live("node-106", "follower", true, true),
            live("node-108", "follower", true, true),
            live("node-109", "follower", true, false),
        ];
        assert_eq!(observed_phase(&spec, &healthy), "ready");
        let mut keeper_down = healthy.clone();
        keeper_down[1].keeper_active = false;
        keeper_down[1].keeper_role.clear();
        assert_eq!(observed_phase(&spec, &keeper_down), "degraded");
        let mut no_quorum = healthy.clone();
        for row in &mut no_quorum {
            row.keeper_active = false;
            row.keeper_role.clear();
        }
        assert_eq!(observed_phase(&spec, &no_quorum), "failed");
        let mut no_writer = healthy.clone();
        no_writer[0].server_active = false;
        no_writer[1].server_active = false;
        assert_eq!(observed_phase(&spec, &no_writer), "failed");
        assert!(keeper_restart_allowed(&spec, "node-106", &healthy).is_ok());
        assert!(keeper_restart_allowed(&spec, "node-106", &no_quorum).is_err());
    }
}
