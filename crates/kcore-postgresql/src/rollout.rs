//! Pure step planner and safety gates for PostgreSQL cluster operations.
//!
//! The controller engine executes steps in order and re-reads live health
//! before each one: Patroni may fail over in the middle of an operation, so a
//! step that names [`LEADER`] resolves to whoever leads at that moment.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::spec::{self, ClusterSpec};

pub const APPLY_NIX: &str = "apply-nix";
pub const WAIT_NIX: &str = "wait-nix";
pub const WAIT_ETCD_QUORUM: &str = "wait-etcd-quorum";
pub const WAIT_LEADER: &str = "wait-leader";
pub const WAIT_STREAMING: &str = "wait-streaming";
pub const RELOAD_MEMBER: &str = "reload-member";
pub const PATCH_DCS: &str = "patch-dynamic-config";
pub const RESTART_MEMBER: &str = "restart-member";
pub const RESTART_ETCD: &str = "restart-etcd";
pub const SWITCHOVER: &str = "switchover";
pub const FAILOVER: &str = "failover";
pub const PAUSE: &str = "pause";
pub const RESUME: &str = "resume";
pub const STOP_UNIT: &str = "stop-unit";
pub const START_UNIT: &str = "start-unit";
pub const STOP_POSTGRES: &str = "stop-postgres";
pub const REINIT_MEMBER: &str = "reinit-member";
pub const ENSURE_ROLES: &str = "ensure-roles";
pub const ENSURE_DATABASES: &str = "ensure-databases";
pub const ENSURE_EXTENSIONS: &str = "ensure-extensions";
pub const ANALYZE: &str = "analyze";
pub const PG_UPGRADE_CHECK: &str = "pg-upgrade-check";
pub const PG_UPGRADE_RUN: &str = "pg-upgrade-run";
pub const SET_INITIALIZE_KEY: &str = "set-initialize-key";
pub const REMOVE_OLD_DATA: &str = "remove-old-data";
pub const ETCD_MEMBER_ADD: &str = "etcd-member-add";
pub const ETCD_MEMBER_REMOVE: &str = "etcd-member-remove";
pub const RELEASE_VOLUME: &str = "release-volume";
pub const VERIFY: &str = "verify";
pub const WARNING: &str = "warning";

/// Node placeholder: the Patroni leader when the step runs.
pub const LEADER: &str = "@leader";
/// Node placeholder: any healthy etcd voter when the step runs.
pub const VOTER: &str = "@voter";

/// Apply-nix details: which values file the engine renders.
pub const NIX_FULL: &str = "full";
pub const NIX_ETCD_ONLY: &str = "etcd-only";
pub const NIX_DISABLE: &str = "disable";
pub const NIX_UPGRADE: &str = "upgrade";
pub const NIX_ROLLBACK: &str = "rollback";
pub const NIX_JOIN: &str = "join";

pub const UNIT_PATRONI: &str = "patroni";
pub const UNIT_ETCD: &str = "etcd";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpKind {
    Create,
    Update,
    UpgradeMinor,
    UpgradeMajor,
    FinalizeUpgrade,
    RollbackUpgrade,
    Switchover,
    Failover,
    Reinit,
    Maintenance,
    ScaleOut,
    ScaleIn,
    Replace,
    Delete,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Create => "create",
            OpKind::Update => "update",
            OpKind::UpgradeMinor => "upgrade-minor",
            OpKind::UpgradeMajor => "upgrade-major",
            OpKind::FinalizeUpgrade => "finalize-upgrade",
            OpKind::RollbackUpgrade => "rollback-upgrade",
            OpKind::Switchover => "switchover",
            OpKind::Failover => "failover",
            OpKind::Reinit => "reinit",
            OpKind::Maintenance => "maintenance",
            OpKind::ScaleOut => "scale-out",
            OpKind::ScaleIn => "scale-in",
            OpKind::Replace => "replace",
            OpKind::Delete => "delete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [
            OpKind::Create,
            OpKind::Update,
            OpKind::UpgradeMinor,
            OpKind::UpgradeMajor,
            OpKind::FinalizeUpgrade,
            OpKind::RollbackUpgrade,
            OpKind::Switchover,
            OpKind::Failover,
            OpKind::Reinit,
            OpKind::Maintenance,
            OpKind::ScaleOut,
            OpKind::ScaleIn,
            OpKind::Replace,
            OpKind::Delete,
        ]
        .into_iter()
        .find(|k| k.as_str() == s)
    }

    /// Phase shown while an operation of this kind runs.
    pub fn phase(self) -> &'static str {
        match self {
            OpKind::Create => "bootstrapping",
            OpKind::Update | OpKind::Maintenance | OpKind::Reinit => "updating",
            OpKind::UpgradeMinor
            | OpKind::UpgradeMajor
            | OpKind::FinalizeUpgrade
            | OpKind::RollbackUpgrade => "upgrading",
            OpKind::Switchover | OpKind::Failover => "switching-over",
            OpKind::ScaleOut | OpKind::ScaleIn | OpKind::Replace => "scaling",
            OpKind::Delete => "deleting",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub kind: String,
    pub node_id: String,
    pub unit: String,
    pub detail: String,
    pub state: String,
    pub message: String,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: i64,
}

impl Step {
    pub fn new(kind: &str, node: &str, unit: &str, detail: &str) -> Self {
        Self {
            kind: kind.into(),
            node_id: node.into(),
            unit: unit.into(),
            detail: detail.into(),
            state: "pending".into(),
            message: String::new(),
            started_at: 0,
            finished_at: 0,
        }
    }

    pub fn line(&self) -> String {
        let mut out = self.kind.clone();
        if !self.node_id.is_empty() {
            out.push(' ');
            out.push_str(&self.node_id);
        }
        if !self.unit.is_empty() {
            out.push_str(&format!(" [{}]", self.unit));
        }
        if !self.detail.is_empty() {
            out.push_str(&format!(" ({})", self.detail));
        }
        out
    }

    pub fn done(&self) -> bool {
        matches!(self.state.as_str(), "succeeded" | "skipped")
    }
}

/// Steps not yet succeeded or skipped: what a resumed operation runs.
pub fn remaining(steps: &[Step]) -> Vec<Step> {
    steps.iter().filter(|s| !s.done()).cloned().collect()
}

/// One member as the health loop last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemberHealth {
    pub node_id: String,
    pub patroni_active: bool,
    pub etcd_active: bool,
    /// leader, sync_standby, quorum_standby, replica, or empty
    pub role: String,
    pub state: String,
    pub timeline: i64,
    /// -1 when unknown
    pub lag_bytes: i64,
    pub pending_restart: bool,
    pub version: String,
    pub etcd_leader: bool,
    pub paused: bool,
    pub holds_vip: bool,
    pub data_free_bytes: i64,
    pub message: String,
}

impl MemberHealth {
    pub fn is_leader(&self) -> bool {
        self.role == "leader"
    }

    pub fn streaming(&self, max_lag: u64) -> bool {
        self.patroni_active
            && self.state == "streaming"
            && self.lag_bytes >= 0
            && (self.lag_bytes as u64) <= max_lag
    }
}

pub fn leader_of(health: &[MemberHealth]) -> Option<&str> {
    health
        .iter()
        .find(|h| h.is_leader() && h.patroni_active && h.state == "running")
        .map(|h| h.node_id.as_str())
}

pub struct RolloutRequest<'a> {
    pub kind: OpKind,
    pub spec: &'a ClusterSpec,
    /// etcd voters (node ids).
    pub voters: &'a [String],
    pub health: &'a [MemberHealth],
    /// Patroni leader now, if known.
    pub leader: Option<&'a str>,
    pub dcs_changed: bool,
    pub sql_changed: bool,
    /// Rolling restart needed (Update).
    pub restart: bool,
    /// Candidate (switchover/failover), node (reinit), or the new member
    /// (replace).
    pub target: &'a str,
    /// Old member (replace), members added (scale-out) or removed (scale-in).
    pub nodes: &'a [String],
    /// Maintenance: true pauses, false resumes.
    pub pause: bool,
}

impl<'a> RolloutRequest<'a> {
    pub fn new(kind: OpKind, spec: &'a ClusterSpec, voters: &'a [String]) -> Self {
        Self {
            kind,
            spec,
            voters,
            health: &[],
            leader: None,
            dcs_changed: false,
            sql_changed: false,
            restart: false,
            target: "",
            nodes: &[],
            pause: false,
        }
    }
}

fn pg_nodes(spec: &ClusterSpec) -> Vec<String> {
    spec.postgres_members().map(|m| m.node_id.clone()).collect()
}

fn all_nodes(spec: &ClusterSpec) -> Vec<String> {
    spec.members.iter().map(|m| m.node_id.clone()).collect()
}

/// Leader at plan time, else the bootstrap member.
fn planned_leader(req: &RolloutRequest<'_>) -> String {
    req.leader
        .map(str::to_string)
        .or_else(|| leader_of(req.health).map(str::to_string))
        .or_else(|| req.spec.bootstrap_node().map(str::to_string))
        .unwrap_or_default()
}

/// Standbys in restart order: noFailover members first (they never become
/// leader, so rolling them first costs nothing), then by node id.
fn standbys(spec: &ClusterSpec, leader: &str) -> Vec<String> {
    let mut out: Vec<&spec::Member> = spec
        .postgres_members()
        .filter(|m| m.node_id != leader)
        .collect();
    out.sort_by(|a, b| {
        b.no_failover
            .cmp(&a.no_failover)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    out.into_iter().map(|m| m.node_id.clone()).collect()
}

fn apply_all(steps: &mut Vec<Step>, nodes: &[String], detail: &str) {
    for n in nodes {
        steps.push(Step::new(APPLY_NIX, n, "", detail));
    }
    for n in nodes {
        steps.push(Step::new(WAIT_NIX, n, "", ""));
    }
}

fn reload_all(steps: &mut Vec<Step>, nodes: &[String]) {
    for n in nodes {
        steps.push(Step::new(RELOAD_MEMBER, n, UNIT_PATRONI, ""));
    }
}

fn sql_steps(steps: &mut Vec<Step>) {
    steps.push(Step::new(ENSURE_ROLES, LEADER, "", ""));
    steps.push(Step::new(ENSURE_DATABASES, LEADER, "", ""));
    steps.push(Step::new(ENSURE_EXTENSIONS, LEADER, "", ""));
}

/// Standbys one at a time, then a switchover to a rolled standby, then the
/// old leader. One member: a warning and a restart in place.
fn rolling_restart(steps: &mut Vec<Step>, spec: &ClusterSpec, leader: &str) {
    let rest = standbys(spec, leader);
    for n in &rest {
        steps.push(Step::new(RESTART_MEMBER, n, UNIT_PATRONI, ""));
        steps.push(Step::new(WAIT_STREAMING, n, "", ""));
    }
    if rest.is_empty() {
        steps.push(Step::new(
            WARNING,
            leader,
            "",
            "single member: the restart is downtime",
        ));
        steps.push(Step::new(RESTART_MEMBER, leader, UNIT_PATRONI, "in-place"));
        steps.push(Step::new(WAIT_LEADER, leader, "", ""));
        return;
    }
    steps.push(Step::new(SWITCHOVER, leader, "", "rolled"));
    steps.push(Step::new(WAIT_LEADER, LEADER, "", ""));
    steps.push(Step::new(RESTART_MEMBER, leader, UNIT_PATRONI, ""));
    steps.push(Step::new(WAIT_STREAMING, leader, "", ""));
}

fn etcd_roll(steps: &mut Vec<Step>, voters: &[String], health: &[MemberHealth]) {
    let mut order: Vec<&String> = voters.iter().collect();
    order.sort_by_key(|v| health.iter().any(|h| &h.node_id == *v && h.etcd_leader));
    for v in order {
        steps.push(Step::new(RESTART_ETCD, v, UNIT_ETCD, ""));
        steps.push(Step::new(WAIT_ETCD_QUORUM, "", "", ""));
    }
}

pub fn plan_rollout(req: &RolloutRequest<'_>) -> Vec<Step> {
    let spec = req.spec;
    let mut steps = Vec::new();
    let leader = planned_leader(req);
    let pg = pg_nodes(spec);
    let all = all_nodes(spec);
    match req.kind {
        OpKind::Create => {
            let voters: Vec<String> = req.voters.to_vec();
            apply_all(&mut steps, &voters, NIX_ETCD_ONLY);
            steps.push(Step::new(WAIT_ETCD_QUORUM, "", "", ""));
            let boot = spec.bootstrap_node().unwrap_or_default().to_string();
            steps.push(Step::new(APPLY_NIX, &boot, "", NIX_FULL));
            steps.push(Step::new(WAIT_NIX, &boot, "", ""));
            steps.push(Step::new(WAIT_LEADER, &boot, "", ""));
            for n in pg.iter().filter(|n| **n != boot) {
                steps.push(Step::new(APPLY_NIX, n, "", NIX_FULL));
                steps.push(Step::new(WAIT_NIX, n, "", ""));
                steps.push(Step::new(WAIT_STREAMING, n, "", ""));
            }
            sql_steps(&mut steps);
            steps.push(Step::new(VERIFY, LEADER, "", ""));
        }
        OpKind::Update => {
            apply_all(&mut steps, &all, NIX_FULL);
            reload_all(&mut steps, &pg);
            if req.dcs_changed {
                steps.push(Step::new(PATCH_DCS, LEADER, "", ""));
            }
            if req.restart {
                rolling_restart(&mut steps, spec, &leader);
            }
            if req.sql_changed {
                sql_steps(&mut steps);
            }
        }
        OpKind::UpgradeMinor => {
            apply_all(&mut steps, &all, NIX_FULL);
            etcd_roll(&mut steps, req.voters, req.health);
            rolling_restart(&mut steps, spec, &leader);
            steps.push(Step::new(ENSURE_EXTENSIONS, LEADER, "", "update"));
        }
        OpKind::UpgradeMajor => {
            let rest = standbys(spec, &leader);
            steps.push(Step::new(PG_UPGRADE_CHECK, &leader, "", ""));
            steps.push(Step::new(PAUSE, LEADER, "", ""));
            for n in rest.iter().chain(std::iter::once(&leader)) {
                steps.push(Step::new(STOP_UNIT, n, UNIT_PATRONI, ""));
            }
            for n in rest.iter().chain(std::iter::once(&leader)) {
                steps.push(Step::new(STOP_POSTGRES, n, "", ""));
            }
            apply_all(&mut steps, &all, NIX_UPGRADE);
            steps.push(Step::new(PG_UPGRADE_RUN, &leader, "", ""));
            steps.push(Step::new(SET_INITIALIZE_KEY, &leader, "", ""));
            steps.push(Step::new(START_UNIT, &leader, UNIT_PATRONI, ""));
            steps.push(Step::new(RESUME, &leader, "", ""));
            steps.push(Step::new(WAIT_LEADER, &leader, "", ""));
            for n in &rest {
                steps.push(Step::new(START_UNIT, n, UNIT_PATRONI, ""));
                steps.push(Step::new(WAIT_STREAMING, n, "", ""));
            }
            steps.push(Step::new(ENSURE_EXTENSIONS, LEADER, "", "update"));
            steps.push(Step::new(ANALYZE, LEADER, "", ""));
        }
        OpKind::FinalizeUpgrade => {
            for n in &pg {
                steps.push(Step::new(REMOVE_OLD_DATA, n, "", "previous"));
            }
            apply_all(&mut steps, &all, NIX_FULL);
        }
        OpKind::RollbackUpgrade => {
            let rest = standbys(spec, &leader);
            steps.push(Step::new(
                WARNING,
                "",
                "",
                "writes made after the upgrade are lost",
            ));
            steps.push(Step::new(PAUSE, LEADER, "", ""));
            for n in rest.iter().chain(std::iter::once(&leader)) {
                steps.push(Step::new(STOP_UNIT, n, UNIT_PATRONI, ""));
            }
            for n in rest.iter().chain(std::iter::once(&leader)) {
                steps.push(Step::new(STOP_POSTGRES, n, "", ""));
            }
            apply_all(&mut steps, &all, NIX_ROLLBACK);
            steps.push(Step::new(SET_INITIALIZE_KEY, &leader, "", ""));
            steps.push(Step::new(START_UNIT, &leader, UNIT_PATRONI, ""));
            steps.push(Step::new(RESUME, &leader, "", ""));
            steps.push(Step::new(WAIT_LEADER, &leader, "", ""));
            for n in &rest {
                steps.push(Step::new(START_UNIT, n, UNIT_PATRONI, ""));
                steps.push(Step::new(WAIT_STREAMING, n, "", ""));
            }
            for n in &pg {
                steps.push(Step::new(REMOVE_OLD_DATA, n, "", "abandoned"));
            }
        }
        OpKind::Switchover => {
            steps.push(Step::new(SWITCHOVER, &leader, "", req.target));
            let wait = if req.target.is_empty() {
                LEADER
            } else {
                req.target
            };
            steps.push(Step::new(WAIT_LEADER, wait, "", ""));
        }
        OpKind::Failover => {
            steps.push(Step::new(FAILOVER, req.target, "", ""));
            steps.push(Step::new(WAIT_LEADER, req.target, "", ""));
        }
        OpKind::Reinit => {
            steps.push(Step::new(REINIT_MEMBER, req.target, UNIT_PATRONI, ""));
            steps.push(Step::new(WAIT_STREAMING, req.target, "", ""));
        }
        OpKind::Maintenance => {
            steps.push(Step::new(
                if req.pause { PAUSE } else { RESUME },
                LEADER,
                "",
                "",
            ));
        }
        OpKind::ScaleOut => {
            let existing: Vec<String> = all
                .iter()
                .filter(|n| !req.nodes.contains(n))
                .cloned()
                .collect();
            let existing_pg: Vec<String> = pg
                .iter()
                .filter(|n| !req.nodes.contains(n))
                .cloned()
                .collect();
            apply_all(&mut steps, &existing, NIX_FULL);
            reload_all(&mut steps, &existing_pg);
            for n in req.nodes {
                steps.push(Step::new(APPLY_NIX, n, "", NIX_FULL));
                steps.push(Step::new(WAIT_NIX, n, "", ""));
                steps.push(Step::new(WAIT_STREAMING, n, "", ""));
            }
        }
        OpKind::ScaleIn => {
            for n in req.nodes {
                if *n == leader {
                    steps.push(Step::new(SWITCHOVER, n, "", ""));
                    steps.push(Step::new(WAIT_LEADER, LEADER, "", ""));
                }
                steps.push(Step::new(STOP_UNIT, n, UNIT_PATRONI, ""));
                steps.push(Step::new(APPLY_NIX, n, "", NIX_DISABLE));
                steps.push(Step::new(WAIT_NIX, n, "", ""));
            }
            apply_all(&mut steps, &all, NIX_FULL);
            reload_all(&mut steps, &pg);
            for n in req.nodes {
                steps.push(Step::new(RELEASE_VOLUME, n, "", ""));
            }
        }
        OpKind::Replace => {
            let old = req.nodes.first().cloned().unwrap_or_default();
            let new = req.target.to_string();
            let voter = req.voters.contains(&old) || req.voters.contains(&new);
            if old == leader {
                steps.push(Step::new(SWITCHOVER, &old, "", ""));
                steps.push(Step::new(WAIT_LEADER, LEADER, "", ""));
            }
            steps.push(Step::new(STOP_UNIT, &old, UNIT_PATRONI, ""));
            if voter {
                steps.push(Step::new(STOP_UNIT, &old, UNIT_ETCD, ""));
                steps.push(Step::new(ETCD_MEMBER_REMOVE, VOTER, "", &old));
            }
            steps.push(Step::new(APPLY_NIX, &old, "", NIX_DISABLE));
            steps.push(Step::new(WAIT_NIX, &old, "", ""));
            if voter {
                steps.push(Step::new(ETCD_MEMBER_ADD, VOTER, "", &new));
            }
            steps.push(Step::new(APPLY_NIX, &new, "", NIX_JOIN));
            steps.push(Step::new(WAIT_NIX, &new, "", ""));
            if voter {
                steps.push(Step::new(WAIT_ETCD_QUORUM, "", "", ""));
            }
            let others: Vec<String> = all.iter().filter(|n| **n != new).cloned().collect();
            let others_pg: Vec<String> = pg.iter().filter(|n| **n != new).cloned().collect();
            apply_all(&mut steps, &others, NIX_FULL);
            reload_all(&mut steps, &others_pg);
            if pg.contains(&new) {
                steps.push(Step::new(WAIT_STREAMING, &new, "", ""));
            }
            steps.push(Step::new(RELEASE_VOLUME, &old, "", ""));
        }
        OpKind::Delete => {
            let rest = standbys(spec, &leader);
            for n in rest.iter().chain(pg.iter().filter(|n| **n == leader)) {
                steps.push(Step::new(STOP_UNIT, n, UNIT_PATRONI, ""));
            }
            for v in req.voters {
                steps.push(Step::new(STOP_UNIT, v, UNIT_ETCD, ""));
            }
            apply_all(&mut steps, &all, NIX_DISABLE);
            for n in &all {
                steps.push(Step::new(RELEASE_VOLUME, n, "", ""));
            }
        }
    }
    steps
}

/// May `node` go down now? Refuses when another member is already down or
/// lagging, when `node` is the leader of a multi-member cluster, or when the
/// synchronous quorum would become unsatisfiable.
pub fn safe_to_restart(
    spec: &ClusterSpec,
    health: &[MemberHealth],
    node: &str,
) -> Result<(), String> {
    let pg: Vec<&spec::Member> = spec.postgres_members().collect();
    let leader = leader_of(health).ok_or("no running Patroni leader")?;
    if leader == node {
        return if pg.len() <= 1 {
            Ok(())
        } else {
            Err(format!("{node} is the leader; switch over first"))
        };
    }
    let max_lag = spec.max_replication_lag_bytes;
    let mut healthy_standbys = 0u32;
    for m in pg
        .iter()
        .filter(|m| m.node_id != node && m.node_id != leader)
    {
        let h = health
            .iter()
            .find(|h| h.node_id == m.node_id)
            .ok_or_else(|| format!("no health for {}", m.node_id))?;
        if !h.streaming(max_lag) {
            return Err(format!(
                "{} is not streaming within {max_lag} bytes; one member down at a time",
                m.node_id
            ));
        }
        healthy_standbys += 1;
    }
    if spec.replication.mode != spec::MODE_ASYNC && healthy_standbys < spec.replication.sync_count {
        return Err(format!(
            "restarting {node} leaves {healthy_standbys} streaming standbys for syncCount {}",
            spec.replication.sync_count
        ));
    }
    Ok(())
}

/// May the etcd member on `node` restart? Every other voter must be up and
/// the ensemble must have a leader.
pub fn etcd_restart_allowed(
    voters: &[String],
    health: &[MemberHealth],
    node: &str,
) -> Result<(), String> {
    let mut leader = false;
    for v in voters {
        let h = health
            .iter()
            .find(|h| &h.node_id == v)
            .ok_or_else(|| format!("no health for etcd voter {v}"))?;
        leader |= h.etcd_leader && h.etcd_active;
        if v != node && !h.etcd_active {
            return Err(format!("etcd on {v} is down"));
        }
    }
    if !leader {
        return Err("etcd has no leader".into());
    }
    Ok(())
}

pub fn etcd_quorum(voters: &[String], health: &[MemberHealth]) -> bool {
    let up = voters
        .iter()
        .filter(|v| health.iter().any(|h| &h.node_id == *v && h.etcd_active))
        .count();
    up * 2 > voters.len()
}

/// Best member to take over from the leader: streaming within the failover
/// lag, not noFailover, not excluded; highest failoverPriority, then least
/// lag, then node id. `only` restricts the choice when non-empty.
pub fn switchover_candidate(
    spec: &ClusterSpec,
    health: &[MemberHealth],
    exclude: &[String],
    only: &[String],
) -> Option<String> {
    let max_lag = spec.replication.maximum_lag_on_failover_bytes;
    let mut best: Option<(&spec::Member, &MemberHealth)> = None;
    for m in spec.postgres_members() {
        if m.no_failover || exclude.contains(&m.node_id) {
            continue;
        }
        if !only.is_empty() && !only.contains(&m.node_id) {
            continue;
        }
        let Some(h) = health.iter().find(|h| h.node_id == m.node_id) else {
            continue;
        };
        if h.is_leader() || !h.streaming(max_lag) {
            continue;
        }
        let better = match best {
            None => true,
            Some((bm, bh)) => {
                (
                    m.failover_priority,
                    -h.lag_bytes,
                    std::cmp::Reverse(&m.node_id),
                ) > (
                    bm.failover_priority,
                    -bh.lag_bytes,
                    std::cmp::Reverse(&bm.node_id),
                )
            }
        };
        if better {
            best = Some((m, h));
        }
    }
    best.map(|(m, _)| m.node_id.clone())
}

/// `ready`, `degraded` or `failed` from live health.
pub fn observed_phase(
    spec: &ClusterSpec,
    voters: &[String],
    health: &[MemberHealth],
) -> &'static str {
    if leader_of(health).is_none() || !etcd_quorum(voters, health) {
        return "failed";
    }
    let max_lag = spec.max_replication_lag_bytes;
    let leader = leader_of(health).unwrap_or_default();
    let mut streaming = 0u32;
    let mut degraded = false;
    for m in spec.postgres_members().filter(|m| m.node_id != leader) {
        match health.iter().find(|h| h.node_id == m.node_id) {
            Some(h) if h.streaming(max_lag) => streaming += 1,
            _ => degraded = true,
        }
    }
    if voters
        .iter()
        .any(|v| !health.iter().any(|h| &h.node_id == v && h.etcd_active))
    {
        degraded = true;
    }
    if health.iter().any(|h| h.paused || h.pending_restart) {
        degraded = true;
    }
    if spec.replication.mode != spec::MODE_ASYNC && streaming < spec.replication.sync_count {
        degraded = true;
    }
    if degraded {
        "degraded"
    } else {
        "ready"
    }
}

/// Properties every plan must have. Checked by tests and before the engine
/// stores an operation.
pub fn rollout_invariants(steps: &[Step], leader: &str) -> Result<(), String> {
    let mut restarted = BTreeSet::new();
    let mut switched = false;
    let mut down: Option<String> = None;
    for (i, s) in steps.iter().enumerate() {
        match s.kind.as_str() {
            RESTART_MEMBER => {
                if !restarted.insert(s.node_id.clone()) {
                    return Err(format!("step {i}: {} restarted twice", s.node_id));
                }
                if s.node_id == leader && !switched && s.detail != "in-place" {
                    return Err(format!(
                        "step {i}: leader {leader} restarted before a switchover"
                    ));
                }
                if let Some(d) = &down {
                    return Err(format!(
                        "step {i}: {} restarted while {d} is not back",
                        s.node_id
                    ));
                }
                down = Some(s.node_id.clone());
            }
            REINIT_MEMBER => {
                if s.node_id == leader {
                    return Err(format!("step {i}: reinit of the leader"));
                }
                down = Some(s.node_id.clone());
            }
            WAIT_STREAMING | WAIT_LEADER => {
                if down.as_deref() == Some(s.node_id.as_str()) || s.node_id == LEADER {
                    down = None;
                }
            }
            SWITCHOVER => switched = true,
            _ => {}
        }
    }
    if down.is_some() {
        return Err("plan ends with a member down".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests::sample;
    use crate::spec::Member;

    fn voters(spec: &ClusterSpec) -> Vec<String> {
        spec.members.iter().map(|m| m.node_id.clone()).collect()
    }

    fn healthy(spec: &ClusterSpec, leader: &str) -> Vec<MemberHealth> {
        spec.members
            .iter()
            .map(|m| MemberHealth {
                node_id: m.node_id.clone(),
                patroni_active: !m.witness,
                etcd_active: true,
                role: if m.node_id == leader {
                    "leader".into()
                } else if m.witness {
                    String::new()
                } else {
                    "replica".into()
                },
                state: if m.node_id == leader {
                    "running".into()
                } else {
                    "streaming".into()
                },
                timeline: 1,
                etcd_leader: m.node_id == leader,
                ..Default::default()
            })
            .collect()
    }

    fn kinds(steps: &[Step]) -> Vec<String> {
        steps.iter().map(Step::line).collect()
    }

    #[test]
    fn create_starts_etcd_everywhere_then_the_bootstrap_member() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let steps = plan_rollout(&RolloutRequest::new(OpKind::Create, &spec, &v));
        let lines = kinds(&steps);
        assert_eq!(lines[0], "apply-nix node-105 (etcd-only)");
        assert_eq!(lines[6], "wait-etcd-quorum");
        assert_eq!(lines[7], "apply-nix node-105 (full)");
        assert_eq!(lines[9], "wait-leader node-105");
        assert!(lines.contains(&"wait-streaming node-108".to_string()));
        assert_eq!(lines.last().unwrap(), "verify @leader");
        rollout_invariants(&steps, "node-105").unwrap();
    }

    #[test]
    fn restart_update_rolls_standbys_then_switches_over_then_old_leader() {
        let (_, mut spec) = sample();
        spec.members[2].no_failover = true;
        let v = voters(&spec);
        let health = healthy(&spec, "node-105");
        let mut req = RolloutRequest::new(OpKind::Update, &spec, &v);
        req.health = &health;
        req.restart = true;
        req.dcs_changed = true;
        let steps = plan_rollout(&req);
        let restarts: Vec<&str> = steps
            .iter()
            .filter(|s| s.kind == RESTART_MEMBER)
            .map(|s| s.node_id.as_str())
            .collect();
        assert_eq!(restarts, vec!["node-108", "node-106", "node-105"]);
        let sw = steps.iter().position(|s| s.kind == SWITCHOVER).unwrap();
        let last = steps
            .iter()
            .rposition(|s| s.kind == RESTART_MEMBER)
            .unwrap();
        assert!(sw < last);
        assert!(steps.iter().any(|s| s.kind == PATCH_DCS));
        rollout_invariants(&steps, "node-105").unwrap();
    }

    #[test]
    fn reload_only_update_never_restarts() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let mut req = RolloutRequest::new(OpKind::Update, &spec, &v);
        req.dcs_changed = true;
        req.sql_changed = true;
        let steps = plan_rollout(&req);
        assert!(!steps
            .iter()
            .any(|s| s.kind == RESTART_MEMBER || s.kind == SWITCHOVER));
        assert_eq!(steps.iter().filter(|s| s.kind == PATCH_DCS).count(), 1);
        assert!(steps.iter().any(|s| s.kind == ENSURE_ROLES));
    }

    #[test]
    fn single_member_restart_warns() {
        let (_, mut spec) = sample();
        spec.members.truncate(1);
        let v = voters(&spec);
        let mut req = RolloutRequest::new(OpKind::Update, &spec, &v);
        req.restart = true;
        let steps = plan_rollout(&req);
        assert_eq!(steps.iter().filter(|s| s.kind == WARNING).count(), 1);
        rollout_invariants(&steps, "node-105").unwrap();
    }

    #[test]
    fn minor_upgrade_rolls_etcd_leader_last() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let mut health = healthy(&spec, "node-106");
        for h in &mut health {
            h.etcd_leader = h.node_id == "node-105";
        }
        let mut req = RolloutRequest::new(OpKind::UpgradeMinor, &spec, &v);
        req.health = &health;
        let steps = plan_rollout(&req);
        let etcd: Vec<&str> = steps
            .iter()
            .filter(|s| s.kind == RESTART_ETCD)
            .map(|s| s.node_id.as_str())
            .collect();
        assert_eq!(etcd.last(), Some(&"node-105"));
        let pg_last = steps.iter().rfind(|s| s.kind == RESTART_MEMBER).unwrap();
        assert_eq!(pg_last.node_id, "node-106");
        rollout_invariants(&steps, "node-106").unwrap();
    }

    #[test]
    fn major_upgrade_stops_standbys_before_the_leader_and_upgrades_the_leader() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let mut req = RolloutRequest::new(OpKind::UpgradeMajor, &spec, &v);
        req.leader = Some("node-106");
        let steps = plan_rollout(&req);
        let lines = kinds(&steps);
        assert_eq!(lines[0], "pg-upgrade-check node-106");
        let stops: Vec<&str> = steps
            .iter()
            .filter(|s| s.kind == STOP_POSTGRES)
            .map(|s| s.node_id.as_str())
            .collect();
        assert_eq!(stops.last(), Some(&"node-106"));
        let run = steps.iter().position(|s| s.kind == PG_UPGRADE_RUN).unwrap();
        let apply = steps.iter().position(|s| s.detail == NIX_UPGRADE).unwrap();
        let key = steps
            .iter()
            .position(|s| s.kind == SET_INITIALIZE_KEY)
            .unwrap();
        let start = steps.iter().position(|s| s.kind == START_UNIT).unwrap();
        assert!(apply < run && run < key && key < start);
        assert_eq!(steps[start].node_id, "node-106");
    }

    #[test]
    fn scale_in_of_the_leader_switches_over_first() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let nodes = vec!["node-105".to_string()];
        let mut req = RolloutRequest::new(OpKind::ScaleIn, &spec, &v);
        req.leader = Some("node-105");
        req.nodes = &nodes;
        let steps = plan_rollout(&req);
        assert_eq!(steps[0].kind, SWITCHOVER);
        assert_eq!(steps[2].line(), "stop-unit node-105 [patroni]");
    }

    #[test]
    fn replace_of_a_voter_removes_then_adds_the_etcd_member() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let nodes = vec!["node-108".to_string()];
        let mut req = RolloutRequest::new(OpKind::Replace, &spec, &v);
        req.leader = Some("node-105");
        req.nodes = &nodes;
        req.target = "node-110";
        let steps = plan_rollout(&req);
        let rm = steps
            .iter()
            .position(|s| s.kind == ETCD_MEMBER_REMOVE)
            .unwrap();
        let add = steps
            .iter()
            .position(|s| s.kind == ETCD_MEMBER_ADD)
            .unwrap();
        let join = steps.iter().position(|s| s.detail == NIX_JOIN).unwrap();
        assert!(rm < add && add < join);
        assert_eq!(steps.last().unwrap().line(), "release-volume node-108");
    }

    #[test]
    fn restart_gate_keeps_one_member_down_and_the_sync_quorum() {
        let (_, spec) = sample();
        let mut health = healthy(&spec, "node-105");
        assert!(safe_to_restart(&spec, &health, "node-106").is_ok());
        assert!(safe_to_restart(&spec, &health, "node-105").is_err());
        health[2].state = "starting".into();
        assert!(safe_to_restart(&spec, &health, "node-106").is_err());
        // node-108 is the one down; restarting it again is allowed only if
        // node-106 still covers syncCount 1.
        assert!(safe_to_restart(&spec, &health, "node-108").is_ok());
        health[1].lag_bytes = -1;
        assert!(safe_to_restart(&spec, &health, "node-108").is_err());
    }

    #[test]
    fn etcd_gate_and_quorum() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let mut health = healthy(&spec, "node-105");
        assert!(etcd_restart_allowed(&v, &health, "node-106").is_ok());
        health[2].etcd_active = false;
        assert!(etcd_restart_allowed(&v, &health, "node-106").is_err());
        assert!(etcd_quorum(&v, &health));
        health[1].etcd_active = false;
        assert!(!etcd_quorum(&v, &health));
        assert_eq!(observed_phase(&spec, &v, &health), "failed");
    }

    #[test]
    fn candidate_skips_nofailover_lagging_and_excluded_members() {
        let (_, mut spec) = sample();
        let mut health = healthy(&spec, "node-105");
        assert_eq!(
            switchover_candidate(&spec, &health, &[], &[]).as_deref(),
            Some("node-106")
        );
        spec.members[1].no_failover = true;
        assert_eq!(
            switchover_candidate(&spec, &health, &[], &[]).as_deref(),
            Some("node-108")
        );
        health[2].lag_bytes = 10 << 20;
        assert_eq!(switchover_candidate(&spec, &health, &[], &[]), None);
        spec.members[1].no_failover = false;
        assert_eq!(
            switchover_candidate(&spec, &health, &["node-106".into()], &[]),
            None
        );
    }

    #[test]
    fn phase_reports_degraded_for_a_down_standby() {
        let (_, spec) = sample();
        let v = voters(&spec);
        let mut health = healthy(&spec, "node-105");
        assert_eq!(observed_phase(&spec, &v, &health), "ready");
        health[1].patroni_active = false;
        assert_eq!(observed_phase(&spec, &v, &health), "degraded");
    }

    #[test]
    fn witness_is_never_restarted_or_switched_to() {
        let (_, mut spec) = sample();
        spec.members[2].witness = true;
        let v = voters(&spec);
        let health = healthy(&spec, "node-105");
        let mut req = RolloutRequest::new(OpKind::Update, &spec, &v);
        req.health = &health;
        req.restart = true;
        let steps = plan_rollout(&req);
        assert!(!steps
            .iter()
            .any(|s| s.node_id == "node-108" && s.kind == RESTART_MEMBER));
        assert_eq!(
            switchover_candidate(&spec, &health, &[], &[]).as_deref(),
            Some("node-106")
        );
    }

    proptest::proptest! {
        #[test]
        fn every_rolling_plan_satisfies_the_invariants(
            n in 1usize..=5,
            leader_idx in 0usize..5,
            nofail in proptest::collection::vec(proptest::bool::ANY, 5),
            kind in 0u8..3,
        ) {
            let (_, base) = sample();
            let mut spec = base.clone();
            spec.members = (0..n).map(|i| Member {
                node_id: format!("node-{i}"),
                advertise_address: format!("10.0.0.{}", i + 1),
                iface: "eth0".into(),
                no_failover: nofail[i] && i != 0,
                ..Default::default()
            }).collect();
            let leader = format!("node-{}", leader_idx % n);
            let v = voters(&spec);
            let health = healthy(&spec, &leader);
            let kind = [OpKind::Update, OpKind::UpgradeMinor, OpKind::Create][kind as usize];
            let mut req = RolloutRequest::new(kind, &spec, &v);
            req.health = &health;
            req.restart = true;
            req.leader = Some(&leader);
            let steps = plan_rollout(&req);
            let check_leader = if kind == OpKind::Create { "node-0".to_string() } else { leader.clone() };
            proptest::prop_assert!(rollout_invariants(&steps, &check_leader).is_ok(), "{:?}", kinds(&steps));
            if kind != OpKind::Create {
                let restarts: BTreeSet<String> = steps.iter().filter(|s| s.kind == RESTART_MEMBER).map(|s| s.node_id.clone()).collect();
                proptest::prop_assert_eq!(restarts.len(), n);
            }
        }
    }
}
