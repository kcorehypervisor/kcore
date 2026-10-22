//! Executes a persisted ClickHouse operation one step at a time.
//!
//! Production dials node agents. Tests pass a fake that records calls.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kcore_clickhouse::nixgen::{controller_volume_name, volumes_for_node, VolumeMount};
use kcore_clickhouse::rollout::{
    keeper_restart_allowed, observed_phase, MemberHealth, Step, APPLY_NIX, RESTART_UNIT,
};
use kcore_clickhouse::{safe_to_take_down, ClusterSpec};
use tonic::Request;

use crate::clickhouse_convert::spec_from_json;
use crate::clickhouse_db::SecretRow;
use crate::db::Database;
use crate::node_client::NodeClients;
use crate::node_proto;

static IN_FLIGHT: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct OpContext {
    pub backup_name: String,
    pub skip_access: bool,
    pub retain_data: bool,
    pub target_kind: String,
    pub target_location: String,
    pub target_bucket: String,
    pub databases: Vec<String>,
    pub allow_non_empty: bool,
    pub intermediate: bool,
}

pub struct ApplyOutcome {
    pub status: String,
    pub apply_id: String,
}

pub struct NixStatus {
    pub phase: String,
}

pub struct LiveStatus {
    pub server_active: bool,
    pub keeper_active: bool,
    pub keeper_role: String,
    pub readonly_tables: i32,
    pub max_delay_seconds: i32,
    pub keeper_session_ok: bool,
    pub version: String,
}

pub trait ClickhouseNodes {
    fn apply<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        generation: i64,
        enable: bool,
        detail: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ApplyOutcome, String>> + Send + 'a>>;
    fn nix_status<'a>(
        &'a self,
        node_id: &'a str,
        apply_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<NixStatus, String>> + Send + 'a>>;
    fn status<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveStatus, String>> + Send + 'a>>;
    fn set_unit<'a>(
        &'a self,
        node_id: &'a str,
        unit: &'a str,
        action: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    fn admin<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        command: &'a str,
        detail: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
    fn release<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        role: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
    /// Host preflight. The default accepts so tests that never dial stay quiet.
    fn preflight<'a>(
        &'a self,
        _cluster: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

pub fn spawn(db: Database, clients: NodeClients) {
    tokio::spawn(async move {
        let nodes = GrpcNodes {
            db: db.clone(),
            clients,
        };
        let mut ticks = 0u32;
        loop {
            if let Err(err) = tick(&db, &nodes).await {
                tracing::warn!(error = %err, "clickhouse reconcile");
            }
            ticks = ticks.wrapping_add(1);
            if ticks.is_multiple_of(15) {
                if let Err(err) = refresh_health(&db, &nodes).await {
                    tracing::debug!(error = %err, "clickhouse health");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn tick<N: ClickhouseNodes>(db: &Database, nodes: &N) -> Result<(), String> {
    let ops = db.list_active_clickhouse_operations()?;
    for op in ops {
        drive(db, nodes, &op.id).await?;
    }
    Ok(())
}

pub async fn drive<N: ClickhouseNodes>(db: &Database, nodes: &N, id: &str) -> Result<(), String> {
    {
        let mut flight = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if !flight.insert(id.to_string()) {
            return Ok(());
        }
    }
    let result = drive_inner(db, nodes, id).await;
    if let Ok(mut flight) = IN_FLIGHT.lock() {
        flight.remove(id);
    }
    result
}

async fn drive_inner<N: ClickhouseNodes>(db: &Database, nodes: &N, id: &str) -> Result<(), String> {
    let Some(mut op) = db.get_clickhouse_operation(id)? else {
        return Ok(());
    };
    if op.state == "cancelled" || op.state == "succeeded" || op.state == "failed" {
        return Ok(());
    }
    op.state = "running".into();
    let mut steps: Vec<Step> = serde_json::from_str(&op.steps_json).map_err(|e| e.to_string())?;
    let ctx: OpContext = serde_json::from_str(&op.context_json).unwrap_or_default();
    let Some(cluster) = db.get_clickhouse_cluster(&op.cluster_name)? else {
        op.state = "failed".into();
        op.message = "cluster row disappeared".into();
        db.update_clickhouse_operation(&op)?;
        return Ok(());
    };
    let spec = spec_from_json(&cluster.spec_json)?;
    if preflight_before_steps(&op.kind, &steps) {
        if let Err(err) = nodes.preflight(&cluster.name).await {
            op.state = "failed".into();
            op.message = err.clone();
            db.update_clickhouse_operation(&op)?;
            mark_phase(db, &cluster.name, "failed", &err)?;
            return Ok(());
        }
    }
    let mut index = 0;
    while index < steps.len() {
        if steps[index].state == "succeeded" || steps[index].state == "skipped" {
            index += 1;
            continue;
        }
        if db
            .get_clickhouse_operation(id)?
            .is_some_and(|row| row.state == "cancelled")
        {
            op.state = "cancelled".into();
            break;
        }
        if defer_leader_restart(&spec, &mut steps, index, &health_of(db, &cluster.name)) {
            op.steps_json = serde_json::to_string(&steps).map_err(|e| e.to_string())?;
            db.update_clickhouse_operation(&op)?;
            return Ok(());
        }
        let prior = steps.clone();
        let fate = run_step(
            db,
            nodes,
            &cluster.name,
            cluster.generation,
            &spec,
            &ctx,
            &prior,
            &mut steps[index],
            op.force,
        )
        .await;
        match fate {
            StepFate::Done => {
                steps[index].state = "succeeded".into();
            }
            StepFate::Pause => {
                op.steps_json = serde_json::to_string(&steps).map_err(|e| e.to_string())?;
                db.update_clickhouse_operation(&op)?;
                return Ok(());
            }
            StepFate::Failed(err) => {
                if op.force
                    && matches!(
                        steps[index].kind.as_str(),
                        "stop-unit" | "apply-nix" | "release-volume"
                    )
                    && node_did_not_answer(&err)
                {
                    let note =
                        format!("node did not answer; unit or volume may still be present: {err}");
                    steps[index].state = "skipped".into();
                    steps[index].message = note.clone();
                    op.message = note;
                    index += 1;
                    continue;
                }
                steps[index].state = "failed".into();
                steps[index].message = err.clone();
                op.state = "failed".into();
                op.message = err;
                op.steps_json = serde_json::to_string(&steps).map_err(|e| e.to_string())?;
                db.update_clickhouse_operation(&op)?;
                mark_phase(db, &cluster.name, "failed", &op.message)?;
                return Ok(());
            }
        }
        index += 1;
    }
    if op.state != "cancelled" && op.state != "failed" {
        op.state = "succeeded".into();
        if op.kind == "delete" {
            db.delete_clickhouse_cluster(&cluster.name)?;
        } else {
            let keep = crate::clickhouse_convert::members_from_spec(&cluster.name, &spec)
                .into_iter()
                .map(|member| member.node_id)
                .collect::<Vec<_>>();
            db.prune_clickhouse_members(&cluster.name, &keep)?;
            mark_phase(db, &cluster.name, "ready", "")?;
            clear_active(db, &cluster.name)?;
        }
    }
    op.steps_json = serde_json::to_string(&steps).map_err(|e| e.to_string())?;
    db.update_clickhouse_operation(&op)?;
    Ok(())
}

enum StepFate {
    Done,
    Pause,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
async fn run_step<N: ClickhouseNodes>(
    db: &Database,
    nodes: &N,
    cluster: &str,
    generation: i64,
    spec: &ClusterSpec,
    ctx: &OpContext,
    steps: &[Step],
    step: &mut Step,
    force: bool,
) -> StepFate {
    let _ = force;
    match step.kind.as_str() {
        "warning" => StepFate::Done,
        "apply-nix" => {
            let enable = step.detail != "disable";
            match nodes
                .apply(&step.node_id, cluster, generation, enable, &step.detail)
                .await
            {
                Ok(outcome) if outcome.status == "failed" => StepFate::Failed(outcome.apply_id),
                Ok(outcome) => {
                    step.message = outcome.apply_id;
                    StepFate::Done
                }
                Err(err) => StepFate::Failed(err),
            }
        }
        "wait-nix" => {
            let apply_id = steps
                .iter()
                .rev()
                .find(|prev| prev.kind == APPLY_NIX && prev.node_id == step.node_id)
                .map(|prev| prev.message.as_str())
                .unwrap_or("");
            if apply_id.is_empty() || apply_id == "no-change" {
                return StepFate::Done;
            }
            match nodes.nix_status(&step.node_id, apply_id).await {
                Ok(status) if status.phase == "succeeded" || status.phase == "no-change" => {
                    StepFate::Done
                }
                Ok(status) if status.phase == "failed" => {
                    StepFate::Failed("nixos-rebuild failed".into())
                }
                Ok(_) => timed_out(step, spec).unwrap_or(StepFate::Pause),
                Err(err) => StepFate::Failed(err),
            }
        }
        "restart-unit" | "stop-unit" => {
            let action = if step.kind == RESTART_UNIT {
                "restart"
            } else {
                "stop"
            };
            if step.kind == RESTART_UNIT {
                let health = health_of(db, cluster);
                let gate = if step.unit == "keeper" {
                    keeper_restart_allowed(spec, &step.node_id, &health)
                } else {
                    safe_to_take_down(spec, &step.node_id, &health)
                };
                if let Err(err) = gate {
                    tracing::info!(
                        %err,
                        node = %step.node_id,
                        unit = %step.unit,
                        "clickhouse restart deferred"
                    );
                    return timed_out(step, spec).unwrap_or(StepFate::Pause);
                }
            }
            match nodes.set_unit(&step.node_id, &step.unit, action).await {
                Ok(()) => StepFate::Done,
                Err(err) => StepFate::Failed(err),
            }
        }
        "reload-config" => match nodes.admin(&step.node_id, cluster, "reload", "").await {
            Ok(_) => StepFate::Done,
            Err(err) => StepFate::Failed(err),
        },
        "wait-keeper-quorum" => match quorum(db, nodes, cluster, spec).await {
            Ok(true) => StepFate::Done,
            Ok(false) => timed_out(step, spec).unwrap_or(StepFate::Pause),
            Err(err) => StepFate::Failed(err),
        },
        "wait-server-ready" | "wait-replica-caught-up" => {
            match ready(nodes, cluster, spec, step).await {
                Ok(true) => StepFate::Done,
                Ok(false) => timed_out(step, spec).unwrap_or(StepFate::Pause),
                Err(err) => StepFate::Failed(err),
            }
        }
        "create-databases" => {
            admin_each(
                nodes,
                &step.node_id,
                cluster,
                "ensure-database",
                &spec
                    .databases
                    .iter()
                    .map(|db| db.name.clone())
                    .collect::<Vec<_>>(),
            )
            .await
        }
        "create-functions" => {
            let details: Vec<_> = spec
                .functions
                .iter()
                .map(|f| format!("{}|{}|{}", f.name, f.parameters, f.expression))
                .collect();
            admin_each(nodes, &step.node_id, cluster, "create-function", &details).await
        }
        "create-dictionaries" => {
            let details: Vec<_> = spec
                .dictionaries
                .iter()
                .filter(|d| !d.database.is_empty())
                .map(|d| d.name.clone())
                .collect();
            admin_each(nodes, &step.node_id, cluster, "create-dictionary", &details).await
        }
        "set-system-log-ttl" => {
            let tables = [
                ("query_log", spec.system_logs.query_log_days),
                ("part_log", spec.system_logs.part_log_days),
                ("trace_log", spec.system_logs.trace_log_days),
                ("text_log", spec.system_logs.text_log_days),
            ];
            for (table, days) in tables {
                if let Err(err) = nodes
                    .admin(
                        &step.node_id,
                        cluster,
                        "set-ttl",
                        &format!("{table}:{days}"),
                    )
                    .await
                {
                    return StepFate::Failed(err);
                }
            }
            StepFate::Done
        }
        "verify-clusters" => match nodes.admin(&step.node_id, cluster, "verify", "").await {
            Ok(_) => StepFate::Done,
            Err(err) => StepFate::Failed(err),
        },
        "drop-replica" => match nodes
            .admin(&step.node_id, cluster, "drop-replica", &step.detail)
            .await
        {
            Ok(_) => StepFate::Done,
            Err(err) => StepFate::Failed(err),
        },
        "restore-replica" => match nodes
            .admin(&step.node_id, cluster, "restore-replica", &step.detail)
            .await
        {
            Ok(_) => StepFate::Done,
            Err(err) => StepFate::Failed(err),
        },
        "local-parts-count" => match nodes.admin(&step.node_id, cluster, "local-parts", "").await {
            Ok(count) => {
                let n: i64 = count.trim().parse().unwrap_or(0);
                if n > 0 && !ctx.allow_non_empty {
                    StepFate::Failed(format!("node {} still has {n} local parts", step.node_id))
                } else {
                    StepFate::Done
                }
            }
            Err(err) => StepFate::Failed(err),
        },
        "backup" | "backup-access" | "restore" | "restore-access" => {
            let command = step.kind.as_str();
            let detail = format!(
                "{}|{}|{}|{}|{}|{}",
                ctx.target_kind,
                ctx.target_location,
                ctx.target_bucket,
                ctx.backup_name,
                ctx.databases.join(","),
                if ctx.allow_non_empty { "1" } else { "0" }
            );
            match nodes.admin(&step.node_id, cluster, command, &detail).await {
                Ok(_) => StepFate::Done,
                Err(err) => StepFate::Failed(err),
            }
        }
        "release-volume" => match nodes.release(&step.node_id, cluster, &step.unit).await {
            Ok(()) => StepFate::Done,
            Err(err) => StepFate::Failed(err),
        },
        other => StepFate::Failed(format!("unknown clickhouse step {other}")),
    }
}

async fn admin_each<N: ClickhouseNodes>(
    nodes: &N,
    node: &str,
    cluster: &str,
    command: &str,
    details: &[String],
) -> StepFate {
    if details.is_empty() {
        return StepFate::Done;
    }
    for detail in details {
        if let Err(err) = nodes.admin(node, cluster, command, detail).await {
            return StepFate::Failed(err);
        }
    }
    StepFate::Done
}

fn timed_out(step: &mut Step, spec: &ClusterSpec) -> Option<StepFate> {
    let now = unix_now();
    if step.message.is_empty() || step.message.parse::<i64>().is_err() {
        step.message = now.to_string();
        return None;
    }
    let started: i64 = step.message.parse().unwrap_or(now);
    if now - started > i64::from(spec.step_timeout_seconds) {
        Some(StepFate::Failed(format!(
            "{} timed out on {}",
            step.kind, step.node_id
        )))
    } else {
        None
    }
}

fn node_did_not_answer(err: &str) -> bool {
    let err = err.to_ascii_lowercase();
    err.contains("unavailable")
        || err.contains("transport")
        || err.contains("connection refused")
        || err.contains("timed out")
        || err.contains("timeout")
        || err.contains("deadline")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn health_of(db: &Database, cluster: &str) -> Vec<MemberHealth> {
    let Ok(members) = db.list_clickhouse_members(cluster) else {
        return Vec::new();
    };
    members
        .into_iter()
        .filter_map(|member| serde_json::from_str(&member.health_json).ok())
        .collect()
}

async fn quorum<N: ClickhouseNodes>(
    db: &Database,
    nodes: &N,
    cluster: &str,
    spec: &ClusterSpec,
) -> Result<bool, String> {
    let voting: Vec<_> = spec.keepers.iter().filter(|k| !k.learner).collect();
    let mut leaders = 0;
    let mut up = 0;
    for keeper in &spec.keepers {
        let live = nodes.status(&keeper.node_id, cluster).await?;
        remember(db, cluster, &keeper.node_id, &live)?;
        if !keeper.learner
            && live.keeper_active
            && live.keeper_role != "unknown"
            && !live.keeper_role.is_empty()
        {
            up += 1;
        }
        if live.keeper_role == "leader" {
            leaders += 1;
        }
    }
    Ok(leaders == 1 && up > voting.len() / 2)
}

async fn ready<N: ClickhouseNodes>(
    nodes: &N,
    cluster: &str,
    spec: &ClusterSpec,
    step: &Step,
) -> Result<bool, String> {
    let live = nodes.status(&step.node_id, cluster).await?;
    let server_ok = live.server_active && live.keeper_session_ok;
    if step.kind == "wait-server-ready" {
        return Ok(server_ok);
    }
    Ok(server_ok
        && live.readonly_tables == 0
        && live.max_delay_seconds <= spec.max_replica_delay_seconds)
}

fn remember(db: &Database, cluster: &str, node_id: &str, live: &LiveStatus) -> Result<(), String> {
    let health = MemberHealth {
        node_id: node_id.into(),
        server_active: live.server_active,
        keeper_active: live.keeper_active,
        keeper_role: live.keeper_role.clone(),
        readonly_tables: live.readonly_tables,
        max_delay_seconds: live.max_delay_seconds,
        keeper_session_ok: live.keeper_session_ok,
        version: live.version.clone(),
    };
    let json = serde_json::to_string(&health).map_err(|e| e.to_string())?;
    db.set_clickhouse_member_health(cluster, node_id, &json)
}

fn mark_phase(db: &Database, name: &str, phase: &str, message: &str) -> Result<(), String> {
    let Some(mut row) = db.get_clickhouse_cluster(name)? else {
        return Ok(());
    };
    row.phase = phase.into();
    row.message = message.into();
    if phase == "ready" {
        row.active_operation_id.clear();
    }
    db.update_clickhouse_cluster(&row)
}

fn clear_active(db: &Database, name: &str) -> Result<(), String> {
    let Some(mut row) = db.get_clickhouse_cluster(name)? else {
        return Ok(());
    };
    row.active_operation_id.clear();
    db.update_clickhouse_cluster(&row)
}

pub async fn refresh_health<N: ClickhouseNodes>(db: &Database, nodes: &N) -> Result<(), String> {
    for cluster in db.list_clickhouse_clusters()? {
        let busy = !cluster.active_operation_id.is_empty();
        let spec = spec_from_json(&cluster.spec_json)?;
        let mut health = Vec::new();
        let mut complete = true;
        for node_id in member_node_ids(&spec) {
            match nodes.status(&node_id, &cluster.name).await {
                Ok(live) => {
                    remember(db, &cluster.name, &node_id, &live)?;
                    health.push(health_row(&node_id, &live));
                }
                Err(err) => {
                    tracing::debug!(cluster = %cluster.name, node = %node_id, error = %err, "clickhouse status");
                    complete = false;
                }
            }
        }
        if !complete || busy {
            continue;
        }
        let phase = observed_phase(&spec, &health);
        if cluster.phase != phase {
            mark_phase(db, &cluster.name, phase, "")?;
        }
        if let Err(err) =
            rotate_due_certs(db, nodes, &cluster.name, cluster.generation, &spec, &health).await
        {
            tracing::warn!(cluster = %cluster.name, error = %err, "clickhouse cert rotation");
        }
    }
    Ok(())
}

/// A Keeper leader restart waits until a later voting follower restart has
/// run. The plan is built with empty health, so the leader is only known
/// once a poll has landed.
fn defer_leader_restart(
    spec: &ClusterSpec,
    steps: &mut [Step],
    index: usize,
    health: &[MemberHealth],
) -> bool {
    let step = &steps[index];
    if step.kind != RESTART_UNIT || step.unit != "keeper" {
        return false;
    }
    let is_leader = health
        .iter()
        .any(|row| row.node_id == step.node_id && row.keeper_role == "leader");
    if !is_leader {
        return false;
    }
    let Some(offset) = steps.iter().skip(index + 1).position(|next| {
        next.kind == RESTART_UNIT
            && next.unit == "keeper"
            && next.state != "succeeded"
            && next.state != "skipped"
            && spec
                .keepers
                .iter()
                .any(|keeper| keeper.node_id == next.node_id && !keeper.learner)
            && !health
                .iter()
                .any(|row| row.node_id == next.node_id && row.keeper_role == "leader")
    }) else {
        return false;
    };
    steps.swap(index, index + 1 + offset);
    true
}

fn member_node_ids(spec: &ClusterSpec) -> Vec<String> {
    let mut ids = Vec::new();
    for shard in &spec.shards {
        for replica in &shard.replicas {
            if !ids.iter().any(|id| id == &replica.node_id) {
                ids.push(replica.node_id.clone());
            }
        }
    }
    for keeper in &spec.keepers {
        if !ids.iter().any(|id| id == &keeper.node_id) {
            ids.push(keeper.node_id.clone());
        }
    }
    ids
}

fn health_row(node_id: &str, live: &LiveStatus) -> MemberHealth {
    MemberHealth {
        node_id: node_id.into(),
        server_active: live.server_active,
        keeper_active: live.keeper_active,
        keeper_role: live.keeper_role.clone(),
        readonly_tables: live.readonly_tables,
        max_delay_seconds: live.max_delay_seconds,
        keeper_session_ok: live.keeper_session_ok,
        version: live.version.clone(),
    }
}

async fn rotate_due_certs<N: ClickhouseNodes>(
    db: &Database,
    nodes: &N,
    cluster: &str,
    generation: i64,
    spec: &ClusterSpec,
    health: &[MemberHealth],
) -> Result<(), String> {
    if spec.tls.disabled {
        return Ok(());
    }
    let Some(secrets) = db.get_clickhouse_secrets(cluster)? else {
        return Ok(());
    };
    let now = unix_now();
    let window = i64::from(spec.tls.rotate_before_days) * 86_400;
    let mut restarted_keeper = false;
    for cert in db.list_clickhouse_certs(cluster)? {
        if cert.not_after_unix - now > window {
            continue;
        }
        let Some(node) = db.get_node(&cert.node_id).map_err(|err| err.to_string())? else {
            return Err(format!("node {} is not registered", cert.node_id));
        };
        let issued = crate::clickhouse_pki::issue_member(
            cluster,
            &cert.node_id,
            &node.hostname,
            &advertise_of(spec, &cert.node_id),
            &secrets.ca_cert_pem,
            &secrets.ca_key_pem,
        )?;
        db.upsert_clickhouse_cert(
            cluster,
            &crate::clickhouse_db::CertRow {
                node_id: cert.node_id.clone(),
                cert_pem: issued.cert_pem,
                key_pem: issued.key_pem,
                not_after_unix: issued.not_after_unix,
            },
        )?;
        nodes
            .apply(&cert.node_id, cluster, generation, true, "reload")
            .await?;
        if spec.shards.iter().any(|shard| {
            shard
                .replicas
                .iter()
                .any(|replica| replica.node_id == cert.node_id)
        }) {
            nodes.admin(&cert.node_id, cluster, "reload", "").await?;
        }
        let is_keeper = spec
            .keepers
            .iter()
            .any(|keeper| keeper.node_id == cert.node_id);
        if is_keeper
            && !restarted_keeper
            && keeper_restart_allowed(spec, &cert.node_id, health).is_ok()
        {
            nodes.set_unit(&cert.node_id, "keeper", "restart").await?;
            restarted_keeper = true;
        }
    }
    Ok(())
}

fn advertise_of(spec: &ClusterSpec, node_id: &str) -> String {
    for shard in &spec.shards {
        for replica in &shard.replicas {
            if replica.node_id == node_id {
                return replica.advertise_address.clone();
            }
        }
    }
    spec.keepers
        .iter()
        .find(|keeper| keeper.node_id == node_id)
        .map(|keeper| keeper.advertise_address.clone())
        .unwrap_or_default()
}

async fn ensure_node_volumes(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    node_id: &str,
    detail: &str,
) -> Result<(), String> {
    let role = if detail == "keeper-only" {
        "keeper"
    } else {
        "both"
    };
    let row = db
        .get_clickhouse_cluster(cluster)?
        .ok_or_else(|| format!("clickhouse cluster {cluster} not found"))?;
    let spec = spec_from_json(&row.spec_json)?;
    for mount in volumes_for_node(cluster, &spec, node_id, role) {
        ensure_one_volume(db, clients, cluster, node_id, &mount).await?;
    }
    Ok(())
}

async fn ensure_one_volume(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    node_id: &str,
    mount: &VolumeMount,
) -> Result<(), String> {
    let node = db
        .get_node(node_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("node {node_id} is not registered"))?;
    if node.storage_backend != mount.storage_class {
        return Err(format!(
            "node {} storage backend is {}, not {}",
            node.id, node.storage_backend, mount.storage_class
        ));
    }
    let record = controller_volume_name(cluster, node_id, &mount.role);
    let existing = db
        .get_volume_by_name(&record)
        .map_err(|err| err.to_string())?;
    if let Some(row) = &existing {
        if !row.fs_type.is_empty() && row.fs_type != mount.fs_type {
            return Err(format!(
                "volume {record} is {}, not {}",
                row.fs_type, mount.fs_type
            ));
        }
        if row.fs_type == mount.fs_type
            && row.mount_point == mount.mount_point
            && !row.pool.is_empty()
        {
            let handle =
                crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)?;
            let mut storage = dial_storage(db, clients, node_id).await?;
            mount_clickhouse(&mut storage, &handle, mount).await?;
            return Ok(());
        }
    }
    let (handle, _fresh) = volume_handle(db, clients, &node, mount, &record).await?;
    if existing.as_ref().is_none_or(|row| row.fs_type.is_empty()) {
        let mut storage = dial_storage(db, clients, node_id).await?;
        format_existing(&mut storage, &handle, &mount.fs_type).await?;
    }
    let mut storage = dial_storage(db, clients, node_id).await?;
    mount_clickhouse(&mut storage, &handle, mount).await?;
    store_mount(db, &record, node_id, mount, &handle)?;
    Ok(())
}

async fn volume_handle(
    db: &Database,
    clients: &NodeClients,
    node: &crate::db::NodeRow,
    mount: &VolumeMount,
    record: &str,
) -> Result<(String, bool), String> {
    if let Some(row) = db
        .get_volume_by_name(record)
        .map_err(|err| err.to_string())?
    {
        if !row.pool.is_empty() && !row.image.is_empty() {
            let handle =
                crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)?;
            return Ok((handle, false));
        }
    }
    let mut storage = dial_storage(db, clients, &node.id).await?;
    let created = storage
        .create_volume(tonic::Request::new(node_proto::CreateVolumeRequest {
            volume_id: record.to_string(),
            storage_class: mount.storage_class.clone(),
            size_bytes: mount.size_bytes,
            parameters: std::collections::HashMap::new(),
            lv_name: mount.name.clone(),
        }))
        .await;
    match created {
        Ok(response) => Ok((response.into_inner().backend_handle, true)),
        Err(err) if volume_exists(&err) => {
            Ok((adopt_handle(db, clients, node, mount).await?, false))
        }
        Err(err) => Err(format!("creating {} on {}: {err}", mount.name, node.id)),
    }
}

async fn adopt_handle(
    db: &Database,
    clients: &NodeClients,
    node: &crate::db::NodeRow,
    mount: &VolumeMount,
) -> Result<String, String> {
    if mount.storage_class == "zfs" {
        if mount.volume_group.is_empty() {
            return Err(format!("zfs volume {} is missing its pool", mount.name));
        }
        return Ok(format!("/dev/zvol/{}/{}", mount.volume_group, mount.name));
    }
    let mut admin = admin(db, clients, &node.id).await?;
    let info = admin
        .get_lvm_info(tonic::Request::new(node_proto::GetLvmInfoRequest {}))
        .await
        .map_err(|err| err.to_string())?
        .into_inner();
    let lv = info
        .logical_volumes
        .into_iter()
        .find(|lv| {
            lv.name == mount.name
                && (mount.volume_group.is_empty() || lv.vg_name == mount.volume_group)
        })
        .ok_or_else(|| format!("logical volume {} was not found on {}", mount.name, node.id))?;
    if lv.path.is_empty() {
        Ok(format!("/dev/{}/{}", lv.vg_name, lv.name))
    } else {
        Ok(lv.path)
    }
}

async fn format_existing(
    storage: &mut node_proto::node_storage_client::NodeStorageClient<tonic::transport::Channel>,
    handle: &str,
    fs_type: &str,
) -> Result<(), String> {
    match storage
        .format_volume(tonic::Request::new(node_proto::FormatVolumeRequest {
            backend_handle: handle.to_string(),
            fs_type: fs_type.to_string(),
            force: false,
        }))
        .await
    {
        Ok(_) => Ok(()),
        Err(err)
            if err.message().contains("already has a") || err.message().contains("mounted") =>
        {
            Ok(())
        }
        Err(err) => Err(format!("formatting {handle}: {err}")),
    }
}

async fn mount_clickhouse(
    storage: &mut node_proto::node_storage_client::NodeStorageClient<tonic::transport::Channel>,
    handle: &str,
    mount: &VolumeMount,
) -> Result<(), String> {
    storage
        .mount_volume(tonic::Request::new(node_proto::MountVolumeRequest {
            backend_handle: handle.to_string(),
            mount_point: mount.mount_point.clone(),
            fs_type: mount.fs_type.clone(),
            persistent: true,
        }))
        .await
        .map_err(|err| format!("mounting {} : {err}", mount.mount_point))?;
    Ok(())
}

fn store_mount(
    db: &Database,
    record: &str,
    node_id: &str,
    mount: &VolumeMount,
    handle: &str,
) -> Result<(), String> {
    let (pool, image) = crate::host_volume::split_backend_handle(&mount.storage_class, handle)?;
    let mut row = db
        .get_volume_by_name(record)
        .map_err(|err| err.to_string())?
        .unwrap_or_else(|| crate::db::VolumeRow::new_data(record, mount.size_bytes));
    row.storage_class = mount.storage_class.clone();
    row.host_node_id = node_id.to_string();
    row.pool = pool;
    row.image = image;
    row.size_bytes = mount.size_bytes;
    row.fs_type = mount.fs_type.clone();
    row.mount_point = mount.mount_point.clone();
    row.attach_state = crate::db::VolumeRow::ATTACH_ATTACHED.to_string();
    row.source_json = format!(r#"{{"owner":"clickhouse","role":"{}"}}"#, mount.role);
    db.upsert_volume(&row).map_err(|err| err.to_string())
}

async fn release_node_volumes(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    node_id: &str,
    role: &str,
) -> Result<(), String> {
    let Some(row) = db.get_clickhouse_cluster(cluster)? else {
        return Ok(());
    };
    let spec = spec_from_json(&row.spec_json)?;
    for mount in volumes_for_node(cluster, &spec, node_id, role) {
        release_one_volume(db, clients, cluster, node_id, &mount).await?;
    }
    Ok(())
}

async fn release_one_volume(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    node_id: &str,
    mount: &VolumeMount,
) -> Result<(), String> {
    let record = controller_volume_name(cluster, node_id, &mount.role);
    let existing = db
        .get_volume_by_name(&record)
        .map_err(|err| err.to_string())?;
    let mut storage = dial_storage(db, clients, node_id).await?;
    storage
        .unmount_volume(tonic::Request::new(node_proto::UnmountVolumeRequest {
            mount_point: mount.mount_point.clone(),
            persistent: true,
        }))
        .await
        .map_err(|err| format!("unmounting {}: {err}", mount.mount_point))?;
    let handle = if let Some(row) = &existing {
        crate::host_volume::device_path(&row.storage_class, &row.pool, &row.image)?
    } else if mount.storage_class == "zfs" {
        format!("/dev/zvol/{}/{}", mount.volume_group, mount.name)
    } else {
        format!("/dev/{}/{}", mount.volume_group, mount.name)
    };
    match storage
        .delete_volume(tonic::Request::new(node_proto::DeleteVolumeRequest {
            backend_handle: handle,
        }))
        .await
    {
        Ok(_) => {}
        Err(err) if volume_gone(&err) => {}
        Err(err) => return Err(format!("deleting {}: {err}", mount.name)),
    }
    if let Some(row) = existing {
        db.delete_volume_by_id(&row.id)
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

async fn dial_storage(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<node_proto::node_storage_client::NodeStorageClient<tonic::transport::Channel>, String> {
    let node = db
        .get_node(node_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("node {node_id} is not registered"))?;
    if clients.get_storage(&node.address).is_none() {
        clients
            .connect(&node.address)
            .await
            .map_err(|err| err.to_string())?;
    }
    clients
        .get_storage(&node.address)
        .ok_or_else(|| format!("no storage client for {}", node.address))
}

fn volume_exists(err: &tonic::Status) -> bool {
    let msg = err.message().to_ascii_lowercase();
    err.code() == tonic::Code::AlreadyExists || msg.contains("already exists")
}

fn volume_gone(err: &tonic::Status) -> bool {
    let msg = err.message().to_ascii_lowercase();
    err.code() == tonic::Code::NotFound
        || msg.contains("not found")
        || msg.contains("failed to find")
        || msg.contains("does not exist")
        || msg.contains("no such")
}

pub struct GrpcNodes {
    pub db: Database,
    pub clients: NodeClients,
}

impl ClickhouseNodes for GrpcNodes {
    fn apply<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        generation: i64,
        enable: bool,
        detail: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ApplyOutcome, String>> + Send + 'a>> {
        Box::pin(async move {
            if enable {
                ensure_node_volumes(&self.db, &self.clients, cluster, node_id, detail).await?;
            }
            let mut client = admin(&self.db, &self.clients, node_id).await?;
            let req = build_apply(&self.db, cluster, node_id, generation, enable, detail)?;
            let resp = client
                .apply_clickhouse_config(Request::new(req))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            Ok(ApplyOutcome {
                status: resp.status.clone(),
                apply_id: if resp.apply_id.is_empty() {
                    resp.status
                } else {
                    resp.apply_id
                },
            })
        })
    }

    fn nix_status<'a>(
        &'a self,
        node_id: &'a str,
        apply_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<NixStatus, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut client = admin(&self.db, &self.clients, node_id).await?;
            let resp = client
                .get_nix_apply_status(Request::new(node_proto::GetNixApplyStatusRequest {
                    apply_id: apply_id.into(),
                }))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            let phase = match resp.phase {
                2 => "succeeded",
                3 => "failed",
                1 => "running",
                _ => "running",
            };
            Ok(NixStatus {
                phase: phase.into(),
            })
        })
    }

    fn status<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveStatus, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut client = admin(&self.db, &self.clients, node_id).await?;
            let resp = client
                .get_clickhouse_status(Request::new(node_proto::GetClickhouseStatusRequest {
                    cluster_name: cluster.into(),
                }))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            Ok(LiveStatus {
                server_active: resp.server_active,
                keeper_active: resp.keeper_active,
                keeper_role: resp.keeper_role,
                readonly_tables: resp.readonly_tables,
                max_delay_seconds: resp.max_absolute_delay_seconds,
                keeper_session_ok: resp.keeper_session_ok,
                version: resp.version,
            })
        })
    }

    fn set_unit<'a>(
        &'a self,
        node_id: &'a str,
        unit: &'a str,
        action: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let mut client = admin(&self.db, &self.clients, node_id).await?;
            client
                .set_clickhouse_unit_state(Request::new(
                    node_proto::SetClickhouseUnitStateRequest {
                        cluster_name: String::new(),
                        unit: unit.into(),
                        action: action.into(),
                        timeout_seconds: 300,
                    },
                ))
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    fn admin<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        command: &'a str,
        detail: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let mut client = admin(&self.db, &self.clients, node_id).await?;
            let req = admin_request(cluster, command, detail)?;
            let resp = client
                .run_clickhouse_admin(Request::new(req))
                .await
                .map_err(|e| e.to_string())?
                .into_inner();
            if !resp.ok {
                return Err(resp.message);
            }
            Ok(resp.result)
        })
    }

    fn release<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
        role: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            release_node_volumes(&self.db, &self.clients, cluster, node_id, role).await
        })
    }

    fn preflight<'a>(
        &'a self,
        cluster: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let row = self
                .db
                .get_clickhouse_cluster(cluster)?
                .ok_or_else(|| format!("clickhouse cluster {cluster} not found"))?;
            let spec = spec_from_json(&row.spec_json)?;
            require_preflight(&self.db, &self.clients, cluster, &spec).await
        })
    }
}

async fn admin(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
) -> Result<node_proto::node_admin_client::NodeAdminClient<tonic::transport::Channel>, String> {
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

/// One node's host preflight. `unreachable` is set when the node did not answer.
#[derive(Debug, Clone)]
pub struct NodePreflightReport {
    pub node_id: String,
    pub unreachable: Option<String>,
    pub issues: Vec<node_proto::ClickhousePreflightIssue>,
}

/// Dial every member. A transport failure is recorded on that node.
pub async fn collect_preflight(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    spec: &ClusterSpec,
) -> Result<Vec<NodePreflightReport>, String> {
    let members = crate::clickhouse_convert::members_from_spec(cluster, spec);
    let mut reports = Vec::with_capacity(members.len());
    for member in &members {
        let req = preflight_request(cluster, spec, member);
        match dial_preflight(db, clients, &member.node_id, req).await {
            Ok(resp) => reports.push(NodePreflightReport {
                node_id: member.node_id.clone(),
                unreachable: None,
                issues: resp.issues,
            }),
            Err(err) => reports.push(NodePreflightReport {
                node_id: member.node_id.clone(),
                unreachable: Some(err),
                issues: Vec::new(),
            }),
        }
    }
    Ok(reports)
}

/// Refuse when any member is unreachable or returned an error-severity issue.
pub async fn require_preflight(
    db: &Database,
    clients: &NodeClients,
    cluster: &str,
    spec: &ClusterSpec,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for report in collect_preflight(db, clients, cluster, spec).await? {
        if let Some(err) = report.unreachable {
            errors.push(format!("{}: {err}", report.node_id));
            continue;
        }
        for issue in report.issues {
            if issue.severity == "error" {
                errors.push(format!("{}: {}", report.node_id, issue.message));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Fold host answers into a plan. Silence stays a warning so the RPC still succeeds.
pub fn merge_plan_preflight(
    blocking: &mut Vec<String>,
    warnings: &mut Vec<String>,
    plans: &mut [crate::controller_proto::ClickhouseNodePlan],
    reports: &[NodePreflightReport],
) {
    for report in reports {
        let Some(plan) = plans.iter_mut().find(|plan| plan.node_id == report.node_id) else {
            continue;
        };
        if let Some(err) = &report.unreachable {
            warnings.push(format!(
                "{} did not answer preflight: {err}",
                report.node_id
            ));
            continue;
        }
        for issue in &report.issues {
            if issue.severity == "error" {
                plan.preflight_passed = false;
                plan.issues.push(issue.message.clone());
                blocking.push(format!("{}: {}", report.node_id, issue.message));
            } else if issue.severity == "warning" {
                warnings.push(format!("{}: {}", report.node_id, issue.message));
            }
        }
    }
}

fn preflight_before_steps(kind: &str, steps: &[Step]) -> bool {
    matches!(kind, "create" | "scale" | "replace" | "upgrade")
        && steps.iter().all(|step| step.state == "pending")
}

fn preflight_request(
    cluster: &str,
    spec: &ClusterSpec,
    member: &crate::clickhouse_db::MemberRow,
) -> node_proto::CheckClickhousePreflightRequest {
    let mounts = volumes_for_node(cluster, spec, &member.node_id, "both");
    node_proto::CheckClickhousePreflightRequest {
        cluster_name: cluster.into(),
        server: member
            .server
            .then(|| node_server(spec, cluster, &member.node_id)),
        keeper: member
            .keeper
            .then(|| node_keeper(spec, cluster, &member.node_id)),
        advertise_address: member.advertise_address.clone(),
        iface: member.iface.clone(),
        package: spec.package.clone(),
        ports: Some(node_ports(spec)),
        tls_enabled: !spec.tls.disabled,
        data_volume: host_volume(&mounts, "server"),
        keeper_volume: host_volume(&mounts, "keeper"),
    }
}

fn host_volume(mounts: &[VolumeMount], role: &str) -> Option<node_proto::ClickhouseHostVolume> {
    mounts
        .iter()
        .find(|mount| mount.role == role)
        .map(|mount| node_proto::ClickhouseHostVolume {
            name: mount.name.clone(),
            storage_class: mount.storage_class.clone(),
            volume_group: mount.volume_group.clone(),
            size_bytes: mount.size_bytes,
            fs_type: mount.fs_type.clone(),
        })
}

async fn dial_preflight(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
    req: node_proto::CheckClickhousePreflightRequest,
) -> Result<node_proto::CheckClickhousePreflightResponse, String> {
    let mut client = admin(db, clients, node_id).await?;
    client
        .check_clickhouse_preflight(Request::new(req))
        .await
        .map(|resp| resp.into_inner())
        .map_err(|err| err.to_string())
}

fn build_apply(
    db: &Database,
    cluster: &str,
    node_id: &str,
    generation: i64,
    enable: bool,
    _detail: &str,
) -> Result<node_proto::ApplyClickhouseConfigRequest, String> {
    let row = db
        .get_clickhouse_cluster(cluster)?
        .ok_or_else(|| format!("clickhouse cluster {cluster} not found"))?;
    let spec = spec_from_json(&row.spec_json)?;
    let secrets = db.get_clickhouse_secrets(cluster)?.unwrap_or(SecretRow {
        default_password: String::new(),
        default_sha256: String::new(),
        metrics_password: String::new(),
        metrics_sha256: String::new(),
        interserver_password: String::new(),
        keeper_password: String::new(),
        cluster_secret: String::new(),
        ca_cert_pem: String::new(),
        ca_key_pem: String::new(),
    });
    let certs = db.list_clickhouse_certs(cluster)?;
    let cert = certs.iter().find(|cert| cert.node_id == node_id);
    let members = db.list_clickhouse_members(cluster)?;
    let member = members.iter().find(|member| member.node_id == node_id);
    Ok(node_proto::ApplyClickhouseConfigRequest {
        cluster_name: cluster.into(),
        generation,
        enable: enable && member.is_some(),
        server: member
            .filter(|m| m.server)
            .map(|_| node_server(&spec, cluster, node_id)),
        keeper: member
            .filter(|m| m.keeper)
            .map(|_| node_keeper(&spec, cluster, node_id)),
        advertise_address: member
            .map(|m| m.advertise_address.clone())
            .unwrap_or_default(),
        iface: member.map(|m| m.iface.clone()).unwrap_or_default(),
        package: spec.package.clone(),
        ports: Some(node_ports(&spec)),
        tls_enabled: !spec.tls.disabled,
        secrets: Some(node_proto::ClickhouseNodeSecrets {
            default_password_sha256_hex: secrets.default_sha256,
            interserver_user: "kcore-interserver".into(),
            interserver_password: secrets.interserver_password,
            tls_cert_pem: cert
                .map(|c| c.cert_pem.clone().into_bytes())
                .unwrap_or_default(),
            tls_key_pem: cert
                .map(|c| c.key_pem.clone().into_bytes())
                .unwrap_or_default(),
            tls_ca_pem: secrets.ca_cert_pem.into_bytes(),
            keeper_secret: format!("kcore:{}", secrets.keeper_password),
            cluster_secret: secrets.cluster_secret,
            metrics_password_sha256_hex: secrets.metrics_sha256,
            default_password: secrets.default_password,
            named_collection_secrets: spec
                .named_collections
                .iter()
                .flat_map(|collection| {
                    collection
                        .secret_refs
                        .iter()
                        .map(|(key, value)| (format!("{}.{}", collection.name, key), value.clone()))
                })
                .collect(),
        }),
        data_volume: None,
        keeper_volume: None,
    })
}

fn node_ports(spec: &ClusterSpec) -> node_proto::ClickhousePorts {
    node_proto::ClickhousePorts {
        tcp: spec.ports.tcp,
        tcp_secure: spec.ports.tcp_secure,
        http: spec.ports.http,
        https: spec.ports.https,
        interserver: spec.ports.interserver,
        keeper: spec.ports.keeper,
        keeper_raft: spec.ports.keeper_raft,
        prometheus: spec.ports.prometheus,
    }
}

fn node_server(
    spec: &ClusterSpec,
    cluster: &str,
    node_id: &str,
) -> node_proto::ClickhouseNodeServerConfig {
    let client_port = if spec.tls.disabled {
        spec.ports.tcp
    } else {
        spec.ports.tcp_secure
    };
    let found = spec.shards.iter().find_map(|shard| {
        shard
            .replicas
            .iter()
            .find(|replica| replica.node_id == node_id)
            .map(|replica| (shard, replica))
    });
    let Some((shard, replica)) = found else {
        return node_proto::ClickhouseNodeServerConfig::default();
    };
    node_proto::ClickhouseNodeServerConfig {
        shard_name: shard.shard_name.clone(),
        replica_name: replica.replica_name.clone(),
        data_path: kcore_clickhouse::server_data_path(cluster),
        read_only: replica.read_only,
        remote_shards: spec
            .shards
            .iter()
            .map(|shard| node_proto::ClickhouseRemoteShard {
                shard_name: shard.shard_name.clone(),
                weight: shard.weight,
                replicas: shard
                    .replicas
                    .iter()
                    .map(|replica| node_proto::ClickhouseRemoteReplica {
                        replica_name: replica.replica_name.clone(),
                        host: replica.advertise_address.clone(),
                        port: client_port,
                        secure: !spec.tls.disabled,
                        priority: if replica.read_only { 100 } else { 1 },
                    })
                    .collect(),
            })
            .collect(),
        users: spec
            .users
            .iter()
            .map(|user| node_proto::ClickhouseUserSpec {
                name: user.name.clone(),
                password_sha256_hex: user.password_sha256_hex.clone(),
                networks: user.networks.clone(),
                profile: user.profile.clone(),
                quota: user.quota.clone(),
                access_management: user.access_management,
            })
            .collect(),
        profiles: spec
            .profiles
            .iter()
            .map(|profile| node_proto::ClickhouseSettingsProfile {
                name: profile.name.clone(),
                settings: profile.settings.clone().into_iter().collect(),
            })
            .collect(),
        databases: spec
            .databases
            .iter()
            .map(|db| node_proto::ClickhouseDatabaseSpec {
                name: db.name.clone(),
            })
            .collect(),
        server_settings: spec.server_settings.clone().into_iter().collect(),
        client_networks: spec.client_networks.clone(),
        max_server_memory_ratio: spec.max_server_memory_ratio,
        quotas: spec.quotas.iter().map(node_quota).collect(),
        dictionaries: spec.dictionaries.iter().map(node_dictionary).collect(),
        named_collections: spec.named_collections.iter().map(node_collection).collect(),
        functions: spec
            .functions
            .iter()
            .map(|function| node_proto::ClickhouseFunctionSpec {
                name: function.name.clone(),
                parameters: function.parameters.clone(),
                expression: function.expression.clone(),
            })
            .collect(),
        system_logs: Some(node_proto::ClickhouseSystemLogs {
            query_log_days: spec.system_logs.query_log_days,
            part_log_days: spec.system_logs.part_log_days,
            trace_log_days: spec.system_logs.trace_log_days,
            text_log_days: spec.system_logs.text_log_days,
        }),
        metrics_public: spec.metrics_public,
    }
}

fn node_quota(quota: &kcore_clickhouse::spec::QuotaSpec) -> node_proto::ClickhouseQuotaSpec {
    node_proto::ClickhouseQuotaSpec {
        name: quota.name.clone(),
        intervals: quota
            .intervals
            .iter()
            .map(|interval| node_proto::ClickhouseQuotaInterval {
                duration_seconds: interval.duration_seconds,
                queries: interval.queries,
                errors: interval.errors,
                result_rows: interval.result_rows,
                read_rows: interval.read_rows,
                execution_time_seconds: interval.execution_time_seconds,
            })
            .collect(),
    }
}

fn node_dictionary(
    dict: &kcore_clickhouse::spec::DictionarySpec,
) -> node_proto::ClickhouseDictionarySpec {
    node_proto::ClickhouseDictionarySpec {
        name: dict.name.clone(),
        source: dict.source.clone(),
        query: dict.query.clone(),
        url: dict.url.clone(),
        primary_key: dict.primary_key.clone(),
        layout: dict.layout.clone(),
        lifetime_seconds: dict.lifetime_seconds,
        attributes: dict
            .attributes
            .iter()
            .map(|attr| node_proto::ClickhouseDictionaryAttribute {
                name: attr.name.clone(),
                r#type: attr.r#type.clone(),
                nullable: attr.nullable,
            })
            .collect(),
        database: dict.database.clone(),
    }
}

fn node_collection(
    collection: &kcore_clickhouse::spec::NamedCollectionSpec,
) -> node_proto::ClickhouseNamedCollectionSpec {
    node_proto::ClickhouseNamedCollectionSpec {
        name: collection.name.clone(),
        keys: collection.keys.clone().into_iter().collect(),
        secret_refs: collection.secret_refs.clone().into_iter().collect(),
    }
}

fn node_keeper(
    spec: &ClusterSpec,
    cluster: &str,
    node_id: &str,
) -> node_proto::ClickhouseNodeKeeperConfig {
    let Some(keeper) = spec.keepers.iter().find(|k| k.node_id == node_id) else {
        return node_proto::ClickhouseNodeKeeperConfig::default();
    };
    node_proto::ClickhouseNodeKeeperConfig {
        server_id: keeper.server_id,
        data_path: kcore_clickhouse::keeper_data_path(cluster),
        raft_peers: spec
            .keepers
            .iter()
            .map(|peer| node_proto::ClickhouseRaftPeer {
                server_id: peer.server_id,
                host: peer.advertise_address.clone(),
                port: spec.ports.keeper_raft,
                learner: peer.learner,
            })
            .collect(),
    }
}

fn backup_parts(
    detail: &str,
) -> (
    node_proto::ClickhouseBackupTarget,
    Vec<String>,
    String,
    bool,
) {
    let mut parts = detail.split('|');
    let kind = parts.next().unwrap_or("").to_string();
    let location = parts.next().unwrap_or("").to_string();
    let bucket = parts.next().unwrap_or("").to_string();
    let name = parts.next().unwrap_or("").to_string();
    let databases = parts
        .next()
        .unwrap_or("")
        .split(',')
        .filter(|item| !item.is_empty())
        .map(|item| item.to_string())
        .collect();
    let allow = parts.next() == Some("1");
    (
        node_proto::ClickhouseBackupTarget {
            kind,
            location,
            s3_bucket: bucket,
            s3_credential_ref: String::new(),
        },
        databases,
        name,
        allow,
    )
}

fn admin_request(
    cluster: &str,
    command: &str,
    detail: &str,
) -> Result<node_proto::RunClickhouseAdminRequest, String> {
    use node_proto::run_clickhouse_admin_request::Command;
    let command = match command {
        "reload" => {
            Command::ReloadConfig(node_proto::run_clickhouse_admin_request::ReloadConfig {})
        }
        "ensure-database" => Command::EnsureDatabase(
            node_proto::run_clickhouse_admin_request::EnsureReplicatedDatabase {
                database: detail.into(),
                shard_name: String::new(),
                replica_name: String::new(),
            },
        ),
        "drop-replica" => {
            let (shard, replica) = detail.split_once('/').unwrap_or(("", detail));
            Command::DropReplica(node_proto::run_clickhouse_admin_request::DropReplica {
                replica_name: replica.into(),
                shard_name: shard.into(),
                databases: vec![],
            })
        }
        "restore-replica" => {
            Command::RestoreReplica(node_proto::run_clickhouse_admin_request::RestoreReplica {
                database: detail.into(),
            })
        }
        "set-ttl" => {
            let (table, days) = detail.split_once(':').unwrap_or((detail, "0"));
            Command::SetSystemLogTtl(node_proto::run_clickhouse_admin_request::SetSystemLogTtl {
                table: table.into(),
                days: days.parse().unwrap_or(0),
            })
        }
        "local-parts" => {
            Command::LocalPartsCount(node_proto::run_clickhouse_admin_request::LocalPartsCount {})
        }
        "verify" => {
            Command::VerifyClusters(node_proto::run_clickhouse_admin_request::VerifyClusters {})
        }
        "create-function" => {
            let mut parts = detail.splitn(3, '|');
            Command::CreateFunction(node_proto::run_clickhouse_admin_request::CreateFunction {
                name: parts.next().unwrap_or("").into(),
                parameters: parts.next().unwrap_or("").into(),
                expression: parts.next().unwrap_or("").into(),
            })
        }
        "create-dictionary" => {
            Command::CreateDictionary(node_proto::run_clickhouse_admin_request::CreateDictionary {
                name: detail.into(),
            })
        }
        "backup" | "backup-access" | "restore" | "restore-access" => {
            let (target, databases, backup_name, allow_non_empty) = backup_parts(detail);
            match command {
                "backup" => Command::Backup(node_proto::run_clickhouse_admin_request::Backup {
                    target: Some(target),
                    databases,
                    backup_name,
                    base_backup_name: String::new(),
                    shard_name: String::new(),
                }),
                "backup-access" => {
                    Command::BackupAccess(node_proto::run_clickhouse_admin_request::BackupAccess {
                        target: Some(target),
                        backup_name,
                    })
                }
                "restore" => Command::Restore(node_proto::run_clickhouse_admin_request::Restore {
                    target: Some(target),
                    databases,
                    backup_name,
                    allow_non_empty_tables: allow_non_empty,
                    shard_name: String::new(),
                }),
                _ => Command::RestoreAccess(
                    node_proto::run_clickhouse_admin_request::RestoreAccess {
                        target: Some(target),
                        backup_name,
                    },
                ),
            }
        }
        other => return Err(format!("unknown admin command {other}")),
    };
    Ok(node_proto::RunClickhouseAdminRequest {
        cluster_name: cluster.into(),
        command: Some(command),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::clickhouse_db::OperationRow;
    use kcore_clickhouse::rollout::STOP_UNIT;

    struct Fake {
        units: Mutex<Vec<String>>,
        reloads: Mutex<u32>,
        applies: Mutex<u32>,
        fail_unit: Mutex<Option<String>>,
        fail_preflight: Mutex<Option<String>>,
    }

    impl ClickhouseNodes for Fake {
        fn apply<'a>(
            &'a self,
            _node_id: &'a str,
            _cluster: &'a str,
            _generation: i64,
            _enable: bool,
            _detail: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<ApplyOutcome, String>> + Send + 'a>> {
            Box::pin(async move {
                *self.applies.lock().unwrap() += 1;
                Ok(ApplyOutcome {
                    status: "no-change".into(),
                    apply_id: "no-change".into(),
                })
            })
        }
        fn nix_status<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<NixStatus, String>> + Send + 'a>> {
            Box::pin(async {
                Ok(NixStatus {
                    phase: "succeeded".into(),
                })
            })
        }
        fn status<'a>(
            &'a self,
            node_id: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<LiveStatus, String>> + Send + 'a>> {
            let node_id = node_id.to_string();
            Box::pin(async move {
                Ok(LiveStatus {
                    server_active: true,
                    keeper_active: true,
                    keeper_role: if node_id.ends_with("105") {
                        "leader"
                    } else {
                        "follower"
                    }
                    .into(),
                    readonly_tables: 0,
                    max_delay_seconds: 0,
                    keeper_session_ok: true,
                    version: "26.2.4.23".into(),
                })
            })
        }
        fn set_unit<'a>(
            &'a self,
            node_id: &'a str,
            unit: &'a str,
            action: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            let record = format!("{action} {unit} {node_id}");
            let fail = self.fail_unit.lock().unwrap().clone();
            Box::pin(async move {
                self.units.lock().unwrap().push(record);
                match fail {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            })
        }
        fn admin<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            command: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            let command = command.to_string();
            Box::pin(async move {
                if command == "reload" {
                    *self.reloads.lock().unwrap() += 1;
                }
                Ok("0".into())
            })
        }
        fn release<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }
        fn preflight<'a>(
            &'a self,
            _: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
            let err = self.fail_preflight.lock().unwrap().clone();
            Box::pin(async move { err.map_or(Ok(()), Err) })
        }
    }

    fn fake_nodes() -> Fake {
        Fake {
            units: Mutex::new(vec![]),
            reloads: Mutex::new(0),
            applies: Mutex::new(0),
            fail_unit: Mutex::new(None),
            fail_preflight: Mutex::new(None),
        }
    }

    const ANALYTICS: &str = r#"
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  shards:
    - replicas:
        - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
        - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - replicas:
        - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
        - {nodeId: node-109, advertiseAddress: 192.168.40.109, iface: enp6s0}
  keepers:
    - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
    - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
"#;

    fn seed_analytics(db: &Database, spec: &kcore_clickhouse::ClusterSpec, op_id: &str) {
        for id in ["node-105", "node-106", "node-108", "node-109"] {
            db.upsert_node(&crate::db::NodeRow {
                id: id.into(),
                hostname: id.into(),
                address: "127.0.0.1:9".into(),
                cpu_cores: 2,
                memory_bytes: 1024,
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
                dc_id: "dc1".into(),
            })
            .unwrap();
        }
        db.insert_clickhouse_cluster(&crate::clickhouse_db::ClusterRow {
            name: "analytics".into(),
            generation: 2,
            phase: "upgrading".into(),
            message: String::new(),
            spec_json: serde_json::to_string(spec).unwrap(),
            observed_version: String::new(),
            active_operation_id: op_id.into(),
        })
        .unwrap();
        db.replace_clickhouse_members(
            "analytics",
            &crate::clickhouse_convert::members_from_spec("analytics", spec),
        )
        .unwrap();
    }

    fn member_health(node: &str, server: bool, keeper: bool, role: &str) -> MemberHealth {
        MemberHealth {
            node_id: node.into(),
            server_active: server,
            keeper_active: keeper,
            keeper_role: role.into(),
            readonly_tables: 0,
            max_delay_seconds: 0,
            keeper_session_ok: server,
            version: String::new(),
        }
    }

    fn write_health(db: &Database, health: &MemberHealth) {
        db.set_clickhouse_member_health(
            "analytics",
            &health.node_id,
            &serde_json::to_string(health).unwrap(),
        )
        .unwrap();
    }

    fn queue_restart(db: &Database, op_id: &str, step: Step, force: bool) {
        db.insert_clickhouse_operation(&OperationRow {
            id: op_id.into(),
            cluster_name: "analytics".into(),
            kind: "upgrade".into(),
            state: "pending".into(),
            steps_json: serde_json::to_string(&vec![step]).unwrap(),
            context_json: "{}".into(),
            message: String::new(),
            force,
        })
        .unwrap();
    }

    fn restart_step(node: &str, unit: &str) -> Step {
        Step {
            kind: RESTART_UNIT.into(),
            node_id: node.into(),
            unit: unit.into(),
            detail: String::new(),
            group: 1,
            state: "pending".into(),
            message: String::new(),
        }
    }

    #[tokio::test]
    async fn reload_update_never_restarts_a_unit() {
        let _ = Arc::new(0);
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let yaml = r#"
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  shards:
    - replicas:
        - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
        - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - replicas:
        - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
        - {nodeId: node-109, advertiseAddress: 192.168.40.109, iface: enp6s0}
  keepers:
    - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
    - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
"#;
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(yaml).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        for id in ["node-105", "node-106", "node-108", "node-109"] {
            db.upsert_node(&crate::db::NodeRow {
                id: id.into(),
                hostname: id.into(),
                address: "127.0.0.1:9".into(),
                cpu_cores: 2,
                memory_bytes: 1024,
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
                dc_id: "dc1".into(),
            })
            .unwrap();
        }
        let mut after = spec.clone();
        after
            .server_settings
            .insert("max_concurrent_queries".into(), "10".into());
        let diff = kcore_clickhouse::diff(&spec, &after);
        let steps = kcore_clickhouse::plan_rollout(&kcore_clickhouse::RolloutRequest {
            before: Some(&spec),
            after: &after,
            kind: kcore_clickhouse::OpKind::Update,
            health: &[],
            diff: Some(&diff),
            intermediate: false,
            retain_data: false,
            skip_access: false,
            backup_name: String::new(),
            restore_node: String::new(),
            removed_replicas: vec![],
        });
        assert!(steps
            .iter()
            .all(|step| step.kind != RESTART_UNIT && step.kind != STOP_UNIT));
        db.insert_clickhouse_cluster(&crate::clickhouse_db::ClusterRow {
            name: "analytics".into(),
            generation: 2,
            phase: "updating".into(),
            message: String::new(),
            spec_json: serde_json::to_string(&after).unwrap(),
            observed_version: String::new(),
            active_operation_id: "op1".into(),
        })
        .unwrap();
        db.replace_clickhouse_members(
            "analytics",
            &crate::clickhouse_convert::members_from_spec("analytics", &after),
        )
        .unwrap();
        db.insert_clickhouse_operation(&OperationRow {
            id: "op1".into(),
            cluster_name: "analytics".into(),
            kind: "update".into(),
            state: "pending".into(),
            steps_json: serde_json::to_string(&steps).unwrap(),
            context_json: "{}".into(),
            message: String::new(),
            force: false,
        })
        .unwrap();
        drive(&db, &fake, "op1").await.unwrap();
        assert!(fake.units.lock().unwrap().is_empty());
        assert!(*fake.reloads.lock().unwrap() > 0);
        assert_eq!(
            db.get_clickhouse_operation("op1").unwrap().unwrap().state,
            "succeeded"
        );
    }

    #[tokio::test]
    async fn refresh_rotates_an_expired_server_cert_and_marks_ready() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let yaml = r#"
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  shards:
    - replicas:
        - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
        - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - replicas:
        - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
        - {nodeId: node-109, advertiseAddress: 192.168.40.109, iface: enp6s0}
  keepers:
    - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
    - {nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0}
    - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
"#;
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(yaml).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        db.upsert_node(&crate::db::NodeRow {
            id: "node-109".into(),
            hostname: "node-109".into(),
            address: "127.0.0.1:9".into(),
            cpu_cores: 2,
            memory_bytes: 1024,
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
            dc_id: "dc1".into(),
        })
        .unwrap();
        db.insert_clickhouse_cluster(&crate::clickhouse_db::ClusterRow {
            name: "analytics".into(),
            generation: 1,
            phase: "pending".into(),
            message: String::new(),
            spec_json: serde_json::to_string(&spec).unwrap(),
            observed_version: String::new(),
            active_operation_id: String::new(),
        })
        .unwrap();
        let ca = crate::clickhouse_pki::issue_ca("analytics").unwrap();
        db.upsert_clickhouse_secrets(
            "analytics",
            &SecretRow {
                default_password: "pw".into(),
                default_sha256: "digest".into(),
                metrics_password: "metrics".into(),
                metrics_sha256: "digest".into(),
                interserver_password: "inter".into(),
                keeper_password: "keeper".into(),
                cluster_secret: "secret".into(),
                ca_cert_pem: ca.cert_pem,
                ca_key_pem: ca.key_pem,
            },
        )
        .unwrap();
        db.upsert_clickhouse_cert(
            "analytics",
            &crate::clickhouse_db::CertRow {
                node_id: "node-109".into(),
                cert_pem: "old".into(),
                key_pem: "old".into(),
                not_after_unix: 1,
            },
        )
        .unwrap();
        refresh_health(&db, &fake).await.unwrap();
        assert_eq!(
            db.get_clickhouse_cluster("analytics")
                .unwrap()
                .unwrap()
                .phase,
            "ready"
        );
        let certs = db.list_clickhouse_certs("analytics").unwrap();
        assert_eq!(certs.len(), 1);
        assert!(certs[0].not_after_unix > 1);
        assert!(certs[0].cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(*fake.reloads.lock().unwrap() >= 1);
        assert!(fake.units.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restart_pauses_when_the_other_replica_is_down() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-replica");
        write_health(&db, &member_health("node-105", true, true, "follower"));
        write_health(&db, &member_health("node-106", false, true, "follower"));
        write_health(&db, &member_health("node-108", true, true, "leader"));
        write_health(&db, &member_health("node-109", true, false, ""));
        queue_restart(&db, "op-replica", restart_step("node-105", "server"), true);
        drive(&db, &fake, "op-replica").await.unwrap();
        assert!(fake.units.lock().unwrap().is_empty());
        let op = db.get_clickhouse_operation("op-replica").unwrap().unwrap();
        assert_eq!(op.state, "running");
        let steps: Vec<Step> = serde_json::from_str(&op.steps_json).unwrap();
        assert_eq!(steps[0].state, "pending");
        assert!(steps[0].message.parse::<i64>().is_ok());
    }

    #[tokio::test]
    async fn keeper_restart_pauses_when_quorum_would_break() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-keeper");
        write_health(&db, &member_health("node-105", true, true, "follower"));
        write_health(&db, &member_health("node-106", true, false, "unknown"));
        write_health(&db, &member_health("node-108", true, true, "leader"));
        queue_restart(&db, "op-keeper", restart_step("node-105", "keeper"), false);
        drive(&db, &fake, "op-keeper").await.unwrap();
        assert!(fake.units.lock().unwrap().is_empty());
        let op = db.get_clickhouse_operation("op-keeper").unwrap().unwrap();
        assert_eq!(op.state, "running");
    }

    #[tokio::test]
    async fn missing_health_pauses_until_the_step_times_out() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        spec.step_timeout_seconds = 1;
        seed_analytics(&db, &spec, "op-timeout");
        let mut step = restart_step("node-105", "server");
        step.message = (unix_now() - 30).to_string();
        queue_restart(&db, "op-timeout", step, true);
        drive(&db, &fake, "op-timeout").await.unwrap();
        assert!(fake.units.lock().unwrap().is_empty());
        let op = db.get_clickhouse_operation("op-timeout").unwrap().unwrap();
        assert_eq!(op.state, "failed");
        assert!(op.message.contains("timed out"));
    }

    #[tokio::test]
    async fn force_skips_a_stop_only_when_the_node_did_not_answer() {
        let fake = fake_nodes();
        *fake.fail_unit.lock().unwrap() = Some("connection refused".into());
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-silent");
        let mut step = restart_step("node-109", "server");
        step.kind = STOP_UNIT.into();
        queue_restart(&db, "op-silent", step, true);
        drive(&db, &fake, "op-silent").await.unwrap();
        let op = db.get_clickhouse_operation("op-silent").unwrap().unwrap();
        assert_eq!(op.state, "succeeded");
        assert!(op.message.contains("may still be present"));
        let steps: Vec<Step> = serde_json::from_str(&op.steps_json).unwrap();
        assert_eq!(steps[0].state, "skipped");

        let answered = fake_nodes();
        *answered.fail_unit.lock().unwrap() = Some("unit is degraded".into());
        let db = Database::open(":memory:").unwrap();
        seed_analytics(&db, &spec, "op-answered");
        let mut step = restart_step("node-109", "server");
        step.kind = STOP_UNIT.into();
        queue_restart(&db, "op-answered", step, true);
        drive(&db, &answered, "op-answered").await.unwrap();
        let op = db.get_clickhouse_operation("op-answered").unwrap().unwrap();
        assert_eq!(op.state, "failed");
        assert!(!op.message.contains("may still be present"));
    }

    #[tokio::test]
    async fn refresh_records_health_during_an_operation_without_rotating() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-busy");
        let ca = crate::clickhouse_pki::issue_ca("analytics").unwrap();
        db.upsert_clickhouse_secrets(
            "analytics",
            &SecretRow {
                default_password: "pw".into(),
                default_sha256: "digest".into(),
                metrics_password: "metrics".into(),
                metrics_sha256: "digest".into(),
                interserver_password: "inter".into(),
                keeper_password: "keeper".into(),
                cluster_secret: "secret".into(),
                ca_cert_pem: ca.cert_pem,
                ca_key_pem: ca.key_pem,
            },
        )
        .unwrap();
        db.upsert_clickhouse_cert(
            "analytics",
            &crate::clickhouse_db::CertRow {
                node_id: "node-109".into(),
                cert_pem: "old".into(),
                key_pem: "old".into(),
                not_after_unix: 1,
            },
        )
        .unwrap();
        refresh_health(&db, &fake).await.unwrap();
        let cluster = db.get_clickhouse_cluster("analytics").unwrap().unwrap();
        assert_eq!(cluster.phase, "upgrading");
        assert_eq!(cluster.active_operation_id, "op-busy");
        let members = db.list_clickhouse_members("analytics").unwrap();
        assert!(members.iter().any(|member| {
            member.node_id == "node-105"
                && member.health_json.contains("\"keeper_role\":\"leader\"")
        }));
        let certs = db.list_clickhouse_certs("analytics").unwrap();
        assert_eq!(certs[0].cert_pem, "old");
        assert_eq!(*fake.reloads.lock().unwrap(), 0);
        assert_eq!(*fake.applies.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn keeper_leader_restart_waits_behind_a_pending_follower() {
        let fake = fake_nodes();
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-leader");
        write_health(&db, &member_health("node-105", true, true, "leader"));
        write_health(&db, &member_health("node-106", true, true, "follower"));
        write_health(&db, &member_health("node-108", true, true, "follower"));
        db.insert_clickhouse_operation(&OperationRow {
            id: "op-leader".into(),
            cluster_name: "analytics".into(),
            kind: "upgrade".into(),
            state: "pending".into(),
            steps_json: serde_json::to_string(&vec![
                restart_step("node-105", "keeper"),
                restart_step("node-106", "keeper"),
            ])
            .unwrap(),
            context_json: "{}".into(),
            message: String::new(),
            force: false,
        })
        .unwrap();
        drive(&db, &fake, "op-leader").await.unwrap();
        assert!(fake.units.lock().unwrap().is_empty());
        let op = db.get_clickhouse_operation("op-leader").unwrap().unwrap();
        let steps: Vec<Step> = serde_json::from_str(&op.steps_json).unwrap();
        assert_eq!(steps[0].node_id, "node-106");
        assert_eq!(steps[1].node_id, "node-105");
        assert_eq!(op.state, "running");
        drive(&db, &fake, "op-leader").await.unwrap();
        let units = fake.units.lock().unwrap().clone();
        assert_eq!(
            units.first().map(String::as_str),
            Some("restart keeper node-106")
        );
        assert!(units.iter().any(|unit| unit == "restart keeper node-105"));
    }

    fn pending_apply() -> Step {
        Step {
            kind: APPLY_NIX.into(),
            node_id: "node-105".into(),
            unit: String::new(),
            detail: String::new(),
            group: 1,
            state: "pending".into(),
            message: String::new(),
        }
    }

    #[test]
    fn preflight_runs_only_before_the_first_step_of_a_membership_change() {
        let pending = pending_apply();
        assert!(preflight_before_steps(
            "create",
            std::slice::from_ref(&pending)
        ));
        assert!(preflight_before_steps(
            "scale",
            std::slice::from_ref(&pending)
        ));
        assert!(preflight_before_steps(
            "replace",
            std::slice::from_ref(&pending)
        ));
        assert!(preflight_before_steps(
            "upgrade",
            std::slice::from_ref(&pending)
        ));
        assert!(!preflight_before_steps(
            "update",
            std::slice::from_ref(&pending)
        ));
        assert!(!preflight_before_steps(
            "delete",
            std::slice::from_ref(&pending)
        ));
        let mut started = pending.clone();
        started.state = "succeeded".into();
        assert!(!preflight_before_steps("upgrade", &[started, pending]));
    }

    #[test]
    fn plan_preflight_merges_errors_and_keeps_silence_as_a_warning() {
        let mut blocking = Vec::new();
        let mut warnings = Vec::new();
        let mut plans = vec![
            crate::controller_proto::ClickhouseNodePlan {
                node_id: "node-105".into(),
                preflight_passed: true,
                ..Default::default()
            },
            crate::controller_proto::ClickhouseNodePlan {
                node_id: "node-106".into(),
                preflight_passed: true,
                ..Default::default()
            },
        ];
        merge_plan_preflight(
            &mut blocking,
            &mut warnings,
            &mut plans,
            &[
                NodePreflightReport {
                    node_id: "node-105".into(),
                    unreachable: None,
                    issues: vec![node_proto::ClickhousePreflightIssue {
                        severity: "error".into(),
                        check: "port".into(),
                        message: "port 8443 is already used by nginx".into(),
                    }],
                },
                NodePreflightReport {
                    node_id: "node-106".into(),
                    unreachable: Some("connection refused".into()),
                    issues: vec![],
                },
            ],
        );
        assert!(blocking.iter().any(|line| line.contains("nginx")));
        assert!(!plans[0].preflight_passed);
        assert!(plans[0].issues.iter().any(|line| line.contains("nginx")));
        assert!(warnings.iter().any(|line| line.contains("did not answer")));
        assert!(plans[1].preflight_passed);
        assert!(blocking.iter().all(|line| !line.contains("node-106")));
    }

    #[tokio::test]
    async fn create_refuses_before_nix_when_preflight_fails() {
        let fake = fake_nodes();
        *fake.fail_preflight.lock().unwrap() = Some("iface is configured by DHCP".into());
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-preflight");
        db.insert_clickhouse_operation(&OperationRow {
            id: "op-preflight".into(),
            cluster_name: "analytics".into(),
            kind: "create".into(),
            state: "pending".into(),
            steps_json: serde_json::to_string(&vec![pending_apply()]).unwrap(),
            context_json: "{}".into(),
            message: String::new(),
            force: true,
        })
        .unwrap();
        drive(&db, &fake, "op-preflight").await.unwrap();
        let op = db
            .get_clickhouse_operation("op-preflight")
            .unwrap()
            .unwrap();
        assert_eq!(op.state, "failed");
        assert!(op.message.contains("DHCP"));
        assert_eq!(*fake.applies.lock().unwrap(), 0);
        assert_eq!(
            db.get_clickhouse_cluster("analytics")
                .unwrap()
                .unwrap()
                .phase,
            "failed"
        );
    }

    #[tokio::test]
    async fn a_started_roll_does_not_preflight_again() {
        let fake = fake_nodes();
        *fake.fail_preflight.lock().unwrap() = Some("iface is configured by DHCP".into());
        let db = Database::open(":memory:").unwrap();
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(ANALYTICS).unwrap();
        kcore_clickhouse::normalize(&mut spec);
        seed_analytics(&db, &spec, "op-started");
        let mut done = pending_apply();
        done.state = "succeeded".into();
        db.insert_clickhouse_operation(&OperationRow {
            id: "op-started".into(),
            cluster_name: "analytics".into(),
            kind: "scale".into(),
            state: "running".into(),
            steps_json: serde_json::to_string(&vec![done]).unwrap(),
            context_json: "{}".into(),
            message: String::new(),
            force: false,
        })
        .unwrap();
        drive(&db, &fake, "op-started").await.unwrap();
        assert_eq!(
            db.get_clickhouse_operation("op-started")
                .unwrap()
                .unwrap()
                .state,
            "succeeded"
        );
    }
}
