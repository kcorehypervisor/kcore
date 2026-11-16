//! Executes PostgreSQL cluster operations one step at a time and keeps member
//! health current.
//!
//! Patroni, not this engine, decides automatic failover. The engine drives
//! planned changes and re-reads the live leader before every step, because
//! Patroni may promote another member in the middle of an operation. Steps
//! that wait for a gate return [`Fate::Wait`] and are picked up on the next
//! tick, so a controller restart resumes at the first step that has not
//! succeeded. Only the holder of the failover lease acts (`may_act`), the same
//! single-actor rule as the Slurm reconciler.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use kcore_postgresql::rollout::{self, MemberHealth, OpKind, Step};
use kcore_postgresql::spec::{self, ClusterSpec};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::ReplicationConfig;
use crate::db::{Database, PgClusterRow, PgOperationRow};
use crate::node_client::NodeClients;
use crate::node_proto;
use crate::postgresql_cluster_pki as pki;
use crate::webhooks;

const TICK: Duration = Duration::from_secs(10);
pub const EVENT_FAILOVER: &str = "failover";
pub const EVENT_SWITCHOVER: &str = "switchover";

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Calls into one node agent, addressed by node id. Tests pass a fake.
pub trait PostgresqlNodes: Send + Sync {
    fn preflight<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::CheckPostgresqlClusterPreflightRequest,
    ) -> BoxFuture<'a, Result<node_proto::CheckPostgresqlClusterPreflightResponse, String>>;
    fn apply<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::ApplyPostgresqlClusterConfigRequest,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn nix_status<'a>(
        &'a self,
        node_id: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>>;
    fn status<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::GetPostgresqlClusterStatusResponse, String>>;
    fn admin<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::RunPostgresqlAdminRequest,
    ) -> BoxFuture<'a, Result<node_proto::RunPostgresqlAdminResponse, String>>;
}

pub struct GrpcNodes {
    db: Database,
    clients: NodeClients,
}

impl GrpcNodes {
    pub fn new(db: Database, clients: NodeClients) -> Self {
        Self { db, clients }
    }

    async fn admin_client(
        &self,
        node_id: &str,
    ) -> Result<
        node_proto::node_admin_client::NodeAdminClient<crate::node_client::NodeChannel>,
        String,
    > {
        let node = self
            .db
            .get_node(node_id)
            .map_err(|e| format!("reading node {node_id}: {e}"))?
            .ok_or_else(|| format!("node {node_id} is not registered"))?;
        if self.clients.get_admin(&node.address).is_none() {
            self.clients
                .connect(&node.address)
                .await
                .map_err(|e| format!("unreachable: connect to {node_id}: {e}"))?;
        }
        self.clients
            .get_admin(&node.address)
            .ok_or_else(|| format!("unreachable: no admin client for {node_id}"))
    }
}

fn rpc_err(node_id: &str, what: &str, e: tonic::Status) -> String {
    if matches!(
        e.code(),
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded
    ) {
        format!("unreachable: {what} {node_id}: {}", e.message())
    } else {
        format!("{what} {node_id}: {}", e.message())
    }
}

impl PostgresqlNodes for GrpcNodes {
    fn preflight<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::CheckPostgresqlClusterPreflightRequest,
    ) -> BoxFuture<'a, Result<node_proto::CheckPostgresqlClusterPreflightResponse, String>> {
        Box::pin(async move {
            let mut c = self.admin_client(node_id).await?;
            c.check_postgresql_cluster_preflight(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| rpc_err(node_id, "CheckPostgresqlClusterPreflight", e))
        })
    }

    fn apply<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::ApplyPostgresqlClusterConfigRequest,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut c = self.admin_client(node_id).await?;
            c.apply_postgresql_cluster_config(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner().apply_id)
                .map_err(|e| rpc_err(node_id, "ApplyPostgresqlClusterConfig", e))
        })
    }

    fn nix_status<'a>(
        &'a self,
        node_id: &'a str,
        apply_id: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>> {
        Box::pin(async move {
            let mut c = self.admin_client(node_id).await?;
            let resp = c
                .get_nix_apply_status(tonic::Request::new(node_proto::GetNixApplyStatusRequest {
                    apply_id: apply_id.to_string(),
                }))
                .await
                .map_err(|e| rpc_err(node_id, "GetNixApplyStatus", e))?
                .into_inner();
            node_proto::NixApplyPhase::try_from(resp.phase)
                .map_err(|_| format!("unknown nix apply phase {}", resp.phase))
        })
    }

    fn status<'a>(
        &'a self,
        node_id: &'a str,
        cluster: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::GetPostgresqlClusterStatusResponse, String>> {
        Box::pin(async move {
            let mut c = self.admin_client(node_id).await?;
            c.get_postgresql_cluster_status(tonic::Request::new(
                node_proto::GetPostgresqlClusterStatusRequest {
                    cluster_name: cluster.to_string(),
                },
            ))
            .await
            .map(|r| r.into_inner())
            .map_err(|e| rpc_err(node_id, "GetPostgresqlClusterStatus", e))
        })
    }

    fn admin<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::RunPostgresqlAdminRequest,
    ) -> BoxFuture<'a, Result<node_proto::RunPostgresqlAdminResponse, String>> {
        Box::pin(async move {
            let mut c = self.admin_client(node_id).await?;
            c.run_postgresql_admin(tonic::Request::new(request))
                .await
                .map(|r| r.into_inner())
                .map_err(|e| rpc_err(node_id, "RunPostgresqlAdmin", e))
        })
    }
}

pub fn unreachable(err: &str) -> bool {
    err.starts_with("unreachable:")
}

/// What an operation carries besides its steps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OpContext {
    /// Spec before the operation, for steps on members that leave it.
    pub old_spec_json: String,
    pub from_package: String,
    pub to_package: String,
    pub drop_roles: Vec<String>,
    pub drop_databases: Vec<String>,
    pub retain_data: bool,
    /// Members whose RESTART_MEMBER step succeeded; switchover "rolled"
    /// picks among them.
    pub rolled: Vec<String>,
}

pub enum Fate {
    Done(String),
    Skip(String),
    Wait(String),
    Fail(String),
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn spec_from_json(raw: &str) -> Result<ClusterSpec, String> {
    let mut spec: ClusterSpec =
        serde_json::from_str(raw).map_err(|e| format!("decode spec: {e}"))?;
    spec::normalize(&mut spec);
    Ok(spec)
}

pub fn voters_of(db: &Database, cluster: &str) -> Vec<String> {
    db.list_pg_members(cluster)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.etcd_voter)
        .map(|m| m.node_id)
        .collect()
}

pub fn health_of(db: &Database, cluster: &str) -> Vec<MemberHealth> {
    db.list_pg_members(cluster)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let mut h: MemberHealth = serde_json::from_str(&m.health_json).ok()?;
            h.node_id = m.node_id;
            Some(h)
        })
        .collect()
}

pub fn spawn(
    db: Database,
    clients: NodeClients,
    replication: Option<ReplicationConfig>,
    hooks: webhooks::Dispatcher,
) {
    tokio::spawn(async move {
        let nodes = GrpcNodes::new(db.clone(), clients);
        let mut ticker = tokio::time::interval(TICK);
        loop {
            ticker.tick().await;
            if !may_act(&db, replication.as_ref()) {
                continue;
            }
            if let Err(e) = tick(&db, &nodes, Some(&hooks)).await {
                warn!(error = %e, "postgresql cluster pass failed");
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

pub async fn tick<N: PostgresqlNodes>(
    db: &Database,
    nodes: &N,
    hooks: Option<&webhooks::Dispatcher>,
) -> Result<(), String> {
    for row in db.list_pg_clusters().map_err(|e| e.to_string())? {
        if let Err(e) = refresh_health(db, nodes, &row, hooks).await {
            warn!(cluster = %row.name, error = %e, "postgresql health refresh failed");
        }
    }
    for op in db.list_running_pg_operations().map_err(|e| e.to_string())? {
        if let Err(e) = drive(db, nodes, &op.id).await {
            warn!(operation = %op.id, error = %e, "postgresql operation pass failed");
        }
    }
    Ok(())
}

/// Poll every member, store per-member health, the leader and timeline, and
/// record a failover event when the leader changed without an operation.
pub async fn refresh_health<N: PostgresqlNodes>(
    db: &Database,
    nodes: &N,
    row: &PgClusterRow,
    hooks: Option<&webhooks::Dispatcher>,
) -> Result<Vec<MemberHealth>, String> {
    let spec = spec_from_json(&row.spec_json)?;
    let mut statuses = BTreeMap::new();
    for m in &spec.members {
        let status = nodes.status(&m.node_id, &row.name).await;
        statuses.insert(m.node_id.clone(), status);
    }
    let health = merge_health(&spec, &statuses);
    for h in &health {
        let json = serde_json::to_string(h).unwrap_or_default();
        let _ = db.set_pg_member_health(&h.node_id, &json);
    }
    let leader = rollout::leader_of(&health).unwrap_or_default().to_string();
    let timeline = health
        .iter()
        .find(|h| h.node_id == leader)
        .map(|h| h.timeline)
        .unwrap_or(row.timeline);
    let mut versions: Vec<String> = health
        .iter()
        .filter(|h| !h.version.is_empty())
        .map(|h| h.version.clone())
        .collect();
    versions.sort();
    versions.dedup();
    let paused = health.iter().any(|h| h.paused);
    db.set_pg_cluster_live(&row.name, &leader, timeline, &versions.join(","), paused)
        .map_err(|e| e.to_string())?;

    if !row.leader_node_id.is_empty() && !leader.is_empty() && leader != row.leader_node_id {
        let planned = !row.active_operation_id.is_empty();
        let kind = if planned {
            EVENT_SWITCHOVER
        } else {
            EVENT_FAILOVER
        };
        let message = if planned {
            format!("leader moved during operation {}", row.active_operation_id)
        } else {
            "Patroni promoted a new leader".to_string()
        };
        let _ = db.append_pg_event(
            &row.name,
            kind,
            &row.leader_node_id,
            &leader,
            timeline,
            &message,
        );
        if !planned {
            warn!(cluster = %row.name, old = %row.leader_node_id, new = %leader, timeline, "postgresql failover");
            if let Some(hooks) = hooks {
                hooks.emit(
                    webhooks::EVENT_POSTGRESQL_FAILOVER,
                    format!("postgresql_cluster/{}", row.name),
                    serde_json::json!({
                        "cluster": row.name,
                        "oldLeader": row.leader_node_id,
                        "newLeader": leader,
                        "timeline": timeline,
                    }),
                );
            }
        }
    }
    if row.active_operation_id.is_empty() && !matches!(row.phase.as_str(), "pending" | "deleting") {
        let voters = voters_of(db, &row.name);
        let phase = rollout::observed_phase(&spec, &voters, &health);
        if phase != row.phase {
            let _ = db.set_pg_cluster_phase(&row.name, phase, "");
        }
    }
    Ok(health)
}

/// Combine each node's own status with the cluster view from the leader (or
/// any member that answered).
pub fn merge_health(
    spec: &ClusterSpec,
    statuses: &BTreeMap<String, Result<node_proto::GetPostgresqlClusterStatusResponse, String>>,
) -> Vec<MemberHealth> {
    let view: Vec<node_proto::PostgresqlClusterMemberStatus> = statuses
        .values()
        .filter_map(|s| s.as_ref().ok())
        .max_by_key(|s| (s.role == "primary", s.members.len()))
        .map(|s| s.members.clone())
        .unwrap_or_default();
    spec.members
        .iter()
        .map(|m| {
            let mut h = MemberHealth {
                node_id: m.node_id.clone(),
                ..Default::default()
            };
            match statuses.get(&m.node_id) {
                Some(Ok(s)) => {
                    h.patroni_active = s.patroni_active;
                    h.etcd_active = s.etcd_active;
                    h.etcd_leader = s.etcd_leader;
                    h.paused = s.paused;
                    h.pending_restart = s.pending_restart;
                    h.version = s.version.clone();
                    h.holds_vip = s.holds_vip;
                    h.data_free_bytes = s.data_free_bytes;
                    h.timeline = s.timeline;
                    h.state = s.state.clone();
                    h.message = s.message.clone();
                }
                Some(Err(e)) => h.message = e.clone(),
                None => {}
            }
            if let Some(v) = view.iter().find(|v| v.name == m.node_id) {
                h.role = v.role.clone();
                h.state = v.state.clone();
                h.timeline = v.timeline;
                h.lag_bytes = v.lag_bytes;
                h.pending_restart |= v.pending_restart;
            } else if !m.witness {
                h.lag_bytes = -1;
            }
            h
        })
        .collect()
}

fn load_steps(op: &PgOperationRow) -> Result<Vec<Step>, String> {
    serde_json::from_str(&op.steps_json).map_err(|e| format!("decode steps: {e}"))
}

fn save(
    db: &Database,
    op: &PgOperationRow,
    state: &str,
    steps: &[Step],
    message: &str,
) -> Result<(), String> {
    db.update_pg_operation(
        &op.id,
        state,
        &serde_json::to_string(steps).unwrap_or_default(),
        message,
    )
    .map_err(|e| e.to_string())
}

/// Run the operation until a step waits, fails, or every step is done.
pub async fn drive<N: PostgresqlNodes>(db: &Database, nodes: &N, id: &str) -> Result<(), String> {
    let Some(op) = db.get_pg_operation(id).map_err(|e| e.to_string())? else {
        return Ok(());
    };
    if op.state != "running" {
        return Ok(());
    }
    let kind = OpKind::parse(&op.kind).ok_or_else(|| format!("unknown kind {}", op.kind))?;
    let mut ctx: OpContext = serde_json::from_str(&op.context_json).unwrap_or_default();
    let mut steps = load_steps(&op)?;
    for i in 0..steps.len() {
        if steps[i].done() {
            continue;
        }
        // Cancel stops before the next step.
        if let Ok(Some(fresh)) = db.get_pg_operation(id) {
            if fresh.state == "cancelled" {
                let _ = db.finish_pg_operation_on_cluster(&op.cluster_name, None, None);
                return Ok(());
            }
        }
        let Some(row) = db
            .get_pg_cluster(&op.cluster_name)
            .map_err(|e| e.to_string())?
        else {
            save(db, &op, "failed", &steps, "cluster row is gone")?;
            return Ok(());
        };
        let spec = spec_from_json(&row.spec_json)?;
        let health = refresh_health(db, nodes, &row, None)
            .await
            .unwrap_or_default();
        if steps[i].started_at == 0 {
            steps[i].started_at = now_unix();
        }
        steps[i].state = "running".into();
        let fate = run_step(
            db, nodes, &op, kind, &row, &spec, &health, &mut ctx, &steps, i,
        )
        .await;
        let timeout = i64::from(spec.step_timeout_seconds);
        match fate {
            Fate::Done(msg) => {
                steps[i].state = "succeeded".into();
                steps[i].message = msg;
                steps[i].finished_at = now_unix();
            }
            Fate::Skip(msg) => {
                steps[i].state = "skipped".into();
                steps[i].message = msg;
                steps[i].finished_at = now_unix();
            }
            Fate::Wait(msg) => {
                if now_unix() - steps[i].started_at > timeout {
                    steps[i].state = "failed".into();
                    steps[i].message = format!("timed out after {timeout}s: {msg}");
                    return fail(db, &op, kind, &steps, &steps[i].message.clone());
                }
                steps[i].message = msg;
                save(db, &op, "running", &steps, "")?;
                save_context(db, &op.id, &ctx)?;
                return Ok(());
            }
            Fate::Fail(msg) => {
                steps[i].state = "failed".into();
                steps[i].message = msg.clone();
                steps[i].finished_at = now_unix();
                return fail(db, &op, kind, &steps, &msg);
            }
        }
        db.update_pg_operation(
            &op.id,
            "running",
            &serde_json::to_string(&steps).unwrap_or_default(),
            "",
        )
        .map_err(|e| e.to_string())?;
        save_context(db, &op.id, &ctx)?;
    }
    finish(db, nodes, &op, kind, &ctx, &steps).await
}

fn save_context(db: &Database, id: &str, ctx: &OpContext) -> Result<(), String> {
    db.set_pg_operation_context(id, &serde_json::to_string(ctx).unwrap_or_default())
        .map_err(|e| e.to_string())
}

fn fail(
    db: &Database,
    op: &PgOperationRow,
    kind: OpKind,
    steps: &[Step],
    msg: &str,
) -> Result<(), String> {
    warn!(operation = %op.id, cluster = %op.cluster_name, error = %msg, "postgresql operation failed");
    save(db, op, "failed", steps, msg)?;
    let phase = if kind == OpKind::Create {
        "failed"
    } else {
        "degraded"
    };
    let _ = db.set_pg_cluster_phase(&op.cluster_name, phase, msg);
    let _ = db.finish_pg_operation_on_cluster(&op.cluster_name, None, None);
    Ok(())
}

async fn finish<N: PostgresqlNodes>(
    db: &Database,
    nodes: &N,
    op: &PgOperationRow,
    kind: OpKind,
    ctx: &OpContext,
    steps: &[Step],
) -> Result<(), String> {
    save(db, op, "succeeded", steps, "")?;
    info!(operation = %op.id, cluster = %op.cluster_name, kind = kind.as_str(), "postgresql operation succeeded");
    match kind {
        OpKind::Delete => {
            db.delete_pg_cluster(&op.cluster_name)
                .map_err(|e| e.to_string())?;
            return Ok(());
        }
        OpKind::ScaleIn | OpKind::Replace => {
            let spec = db
                .get_pg_cluster(&op.cluster_name)
                .map_err(|e| e.to_string())?
                .map(|r| spec_from_json(&r.spec_json))
                .transpose()?;
            for m in db
                .list_pg_members(&op.cluster_name)
                .map_err(|e| e.to_string())?
            {
                if spec
                    .as_ref()
                    .is_some_and(|s| s.member(&m.node_id).is_none())
                {
                    db.delete_pg_member(&op.cluster_name, &m.node_id)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        _ => {}
    }
    let previous = match kind {
        OpKind::UpgradeMajor => Some(ctx.from_package.as_str()),
        OpKind::FinalizeUpgrade | OpKind::RollbackUpgrade => Some(""),
        _ => None,
    };
    db.finish_pg_operation_on_cluster(&op.cluster_name, Some(op.target_generation), previous)
        .map_err(|e| e.to_string())?;
    if let Some(row) = db
        .get_pg_cluster(&op.cluster_name)
        .map_err(|e| e.to_string())?
    {
        let health = refresh_health(db, nodes, &row, None)
            .await
            .unwrap_or_default();
        let spec = spec_from_json(&row.spec_json)?;
        let phase = rollout::observed_phase(&spec, &voters_of(db, &row.name), &health);
        let _ = db.set_pg_cluster_phase(&row.name, phase, "");
    }
    Ok(())
}

fn resolve_node(step: &Step, voters: &[String], health: &[MemberHealth]) -> Option<String> {
    match step.node_id.as_str() {
        rollout::LEADER => rollout::leader_of(health).map(str::to_string),
        rollout::VOTER => voters
            .iter()
            .find(|v| health.iter().any(|h| &h.node_id == *v && h.etcd_active))
            .cloned(),
        "" => Some(String::new()),
        other => Some(other.to_string()),
    }
}

/// Spec as the nodes should see it for this step. A major upgrade runs on
/// the old package until its first Nix apply.
fn step_spec(spec: &ClusterSpec, package: &str) -> ClusterSpec {
    let mut out = spec.clone();
    if !package.is_empty() {
        out.package = package.to_string();
    }
    out
}

fn admin_request(
    cluster: &str,
    spec: &ClusterSpec,
    node_id: &str,
    command: node_proto::run_postgresql_admin_request::Command,
) -> node_proto::RunPostgresqlAdminRequest {
    node_proto::RunPostgresqlAdminRequest {
        cluster_name: cluster.to_string(),
        spec_json: serde_json::to_string(spec).unwrap_or_default(),
        node_id: node_id.to_string(),
        command: Some(command),
    }
}

async fn admin<N: PostgresqlNodes>(
    nodes: &N,
    node: &str,
    request: node_proto::RunPostgresqlAdminRequest,
    force: bool,
) -> Fate {
    match nodes.admin(node, request).await {
        Ok(resp) if resp.ok => Fate::Done(resp.message),
        Ok(resp) => Fate::Fail(resp.message),
        Err(e) if force && unreachable(&e) => Fate::Skip(format!("{e} (force)")),
        Err(e) if unreachable(&e) => Fate::Wait(e),
        Err(e) => Fate::Fail(e),
    }
}

/// Secrets and the member certificate for one node, issuing the certificate
/// when missing or due.
pub fn node_secrets(
    db: &Database,
    cluster: &str,
    spec: &ClusterSpec,
    node_id: &str,
) -> Result<node_proto::PostgresqlNodeSecrets, String> {
    let secrets = db
        .get_pg_secrets(cluster)
        .map_err(|e| e.to_string())?
        .ok_or("cluster secrets are missing")?;
    let rotate_at = now_unix() + i64::from(spec.tls.rotate_before_days) * 86_400;
    let cert = match db
        .get_pg_cert(cluster, node_id)
        .map_err(|e| e.to_string())?
    {
        Some(c) if c.not_after_unix > rotate_at => c,
        _ => {
            let member = spec.member(node_id).ok_or("not a member")?;
            let hostname = db
                .get_node(node_id)
                .ok()
                .flatten()
                .map(|n| n.hostname)
                .unwrap_or_default();
            let mut addresses = vec![member.advertise_address.clone()];
            if let Some((vip, _)) = spec.vip.as_ref().and_then(|v| spec::parse_cidr(&v.address)) {
                addresses.push(vip.to_string());
            }
            let (cert_pem, key_pem, not_after) = pki::issue_member(
                &secrets.ca_cert_pem,
                &secrets.ca_key_pem,
                node_id,
                &hostname,
                &addresses,
            )
            .map_err(|e| format!("issue certificate for {node_id}: {e}"))?;
            let row = crate::db::PgCertRow {
                cluster_name: cluster.to_string(),
                node_id: node_id.to_string(),
                cert_pem,
                key_pem,
                not_after_unix: not_after,
            };
            db.upsert_pg_cert(&row).map_err(|e| e.to_string())?;
            row
        }
    };
    Ok(node_proto::PostgresqlNodeSecrets {
        superuser_password: secrets.superuser_password,
        replication_password: secrets.replication_password,
        rewind_password: secrets.rewind_password,
        restapi_password: secrets.restapi_password,
        tls_cert_pem: cert.cert_pem.into_bytes(),
        tls_key_pem: cert.key_pem.into_bytes(),
        tls_ca_pem: secrets.ca_cert_pem.into_bytes(),
    })
}

/// Passwords for login roles that have no verifier, generated once and kept.
pub fn role_passwords(
    db: &Database,
    cluster: &str,
    spec: &ClusterSpec,
) -> Result<BTreeMap<String, String>, String> {
    let mut secrets = db
        .get_pg_secrets(cluster)
        .map_err(|e| e.to_string())?
        .ok_or("cluster secrets are missing")?;
    let mut map: BTreeMap<String, String> =
        serde_json::from_str(&secrets.role_passwords_json).unwrap_or_default();
    let mut changed = false;
    for role in &spec.roles {
        if role.login && role.password_scram.is_empty() && !map.contains_key(&role.name) {
            map.insert(
                role.name.clone(),
                pki::random_password().map_err(|e| e.to_string())?,
            );
            changed = true;
        }
    }
    if changed {
        secrets.role_passwords_json = serde_json::to_string(&map).unwrap_or_default();
        db.upsert_pg_secrets(&secrets).map_err(|e| e.to_string())?;
    }
    Ok(map)
}

fn apply_id_of(steps: &[Step], i: usize, node: &str) -> Option<String> {
    steps[..i]
        .iter()
        .rev()
        .find(|s| s.kind == rollout::APPLY_NIX && s.node_id == node)
        .and_then(|s| s.message.strip_prefix("apply ").map(str::to_string))
}

#[allow(clippy::too_many_arguments)]
async fn run_step<N: PostgresqlNodes>(
    db: &Database,
    nodes: &N,
    op: &PgOperationRow,
    kind: OpKind,
    row: &PgClusterRow,
    spec: &ClusterSpec,
    health: &[MemberHealth],
    ctx: &mut OpContext,
    steps: &[Step],
    i: usize,
) -> Fate {
    use node_proto::run_postgresql_admin_request::{
        Command as C, EnsureSql, PgUpgrade, UnitAction,
    };
    let step = &steps[i];
    let cluster = row.name.as_str();
    let voters = voters_of(db, cluster);
    let force = op.force;
    let Some(node) = resolve_node(step, &voters, health) else {
        return Fate::Wait(format!("no member for {} yet", step.node_id));
    };
    let old_spec = if ctx.old_spec_json.is_empty() {
        spec.clone()
    } else {
        spec_from_json(&ctx.old_spec_json).unwrap_or_else(|_| spec.clone())
    };
    let req = |command: C| admin_request(cluster, spec, &node, command);
    match step.kind.as_str() {
        rollout::WARNING => Fate::Done(step.detail.clone()),
        rollout::APPLY_NIX => {
            let (stage, render_spec, previous) = match step.detail.as_str() {
                rollout::NIX_UPGRADE => (
                    "upgrade",
                    step_spec(spec, &ctx.to_package),
                    ctx.from_package.clone(),
                ),
                rollout::NIX_ROLLBACK => {
                    ("rollback", step_spec(spec, &ctx.to_package), String::new())
                }
                rollout::NIX_DISABLE => ("disable", old_spec.clone(), String::new()),
                rollout::NIX_ETCD_ONLY => ("etcd-only", spec.clone(), String::new()),
                rollout::NIX_JOIN => ("join", spec.clone(), String::new()),
                _ => {
                    let prev = if kind == OpKind::FinalizeUpgrade {
                        String::new()
                    } else {
                        row.previous_package.clone()
                    };
                    let stage = if prev.is_empty() { "full" } else { "upgrade" };
                    (stage, spec.clone(), prev)
                }
            };
            let secrets = if stage == "disable" {
                None
            } else {
                match node_secrets(db, cluster, &render_spec, &node) {
                    Ok(s) => Some(s),
                    Err(e) => return Fate::Fail(e),
                }
            };
            let request = node_proto::ApplyPostgresqlClusterConfigRequest {
                cluster_name: cluster.to_string(),
                generation: op.target_generation,
                spec_json: serde_json::to_string(&render_spec).unwrap_or_default(),
                node_id: node.clone(),
                etcd_voters: voters.clone(),
                stage: stage.to_string(),
                previous_package: previous,
                secrets,
            };
            match nodes.apply(&node, request).await {
                Ok(id) => Fate::Done(format!("apply {id}")),
                Err(e) if force && unreachable(&e) => Fate::Skip(format!("{e} (force)")),
                Err(e) if unreachable(&e) => Fate::Wait(e),
                Err(e) => Fate::Fail(e),
            }
        }
        rollout::WAIT_NIX => {
            let Some(apply_id) = apply_id_of(steps, i, &node) else {
                return Fate::Skip("apply was skipped".into());
            };
            match nodes.nix_status(&node, &apply_id).await {
                Ok(node_proto::NixApplyPhase::Succeeded) => {
                    let _ = db.set_pg_member_generation(&node, op.target_generation);
                    Fate::Done("nixos-rebuild switch succeeded".into())
                }
                Ok(node_proto::NixApplyPhase::Running) => {
                    Fate::Wait("nixos-rebuild running".into())
                }
                Ok(other) => Fate::Fail(format!("nixos-rebuild on {node}: {other:?}")),
                Err(e) if force && unreachable(&e) => Fate::Skip(format!("{e} (force)")),
                Err(e) if unreachable(&e) => Fate::Wait(e),
                Err(e) => Fate::Fail(e),
            }
        }
        rollout::WAIT_ETCD_QUORUM => {
            let leader = health
                .iter()
                .any(|h| h.etcd_leader && voters.contains(&h.node_id));
            if rollout::etcd_quorum(&voters, health) && leader {
                Fate::Done("etcd has quorum and a leader".into())
            } else {
                Fate::Wait("waiting for etcd quorum".into())
            }
        }
        rollout::WAIT_LEADER => {
            let leader = rollout::leader_of(health);
            match (step.node_id.as_str(), leader) {
                (rollout::LEADER, Some(l)) => Fate::Done(format!("leader {l}")),
                (want, Some(l)) if want == l => Fate::Done(format!("leader {l}")),
                (_, l) => Fate::Wait(format!("leader is {}", l.unwrap_or("none"))),
            }
        }
        rollout::WAIT_STREAMING => match health.iter().find(|h| h.node_id == node) {
            Some(h) if h.streaming(spec.max_replication_lag_bytes) => {
                Fate::Done(format!("streaming, lag {} bytes", h.lag_bytes))
            }
            Some(h) if h.is_leader() => Fate::Done("member is the leader".into()),
            Some(h) => Fate::Wait(format!("{node}: {} lag {}", h.state, h.lag_bytes)),
            None => Fate::Wait(format!("no health for {node}")),
        },
        rollout::RELOAD_MEMBER => {
            if !health.iter().any(|h| h.node_id == node && h.patroni_active) {
                return Fate::Skip("patroni is not running".into());
            }
            admin(nodes, &node, req(C::Reload(true)), force).await
        }
        rollout::PATCH_DCS => admin(nodes, &node, req(C::PatchDynamicConfig(true)), force).await,
        rollout::RESTART_MEMBER => {
            if step.detail != "in-place" {
                if let Err(why) = rollout::safe_to_restart(spec, health, &node) {
                    return Fate::Wait(why);
                }
            }
            let fate = admin(nodes, &node, req(C::RestartMember(true)), force).await;
            if matches!(fate, Fate::Done(_)) && !ctx.rolled.contains(&node) {
                ctx.rolled.push(node.clone());
            }
            fate
        }
        rollout::RESTART_ETCD => {
            if let Err(why) = rollout::etcd_restart_allowed(&voters, health, &node) {
                return Fate::Wait(why);
            }
            admin(
                nodes,
                &node,
                req(C::Unit(UnitAction {
                    unit: rollout::UNIT_ETCD.into(),
                    action: "restart".into(),
                })),
                force,
            )
            .await
        }
        rollout::SWITCHOVER => {
            let Some(leader) = rollout::leader_of(health).map(str::to_string) else {
                return Fate::Wait("no leader to switch over from".into());
            };
            if kind != OpKind::Switchover && leader != step.node_id {
                return Fate::Skip(format!(
                    "{} is no longer the leader ({leader} is)",
                    step.node_id
                ));
            }
            let only: Vec<String> = match step.detail.as_str() {
                "rolled" => ctx.rolled.clone(),
                "" => Vec::new(),
                target => vec![target.to_string()],
            };
            let Some(candidate) =
                rollout::switchover_candidate(spec, health, std::slice::from_ref(&leader), &only)
            else {
                return Fate::Wait("no streaming candidate within maximumLagOnFailover".into());
            };
            admin(
                nodes,
                &leader,
                admin_request(cluster, spec, &leader, C::SwitchoverCandidate(candidate)),
                force,
            )
            .await
        }
        rollout::FAILOVER => {
            admin(nodes, &node, req(C::FailoverCandidate(node.clone())), force).await
        }
        rollout::PAUSE | rollout::RESUME => {
            let target = if node.is_empty()
                || !health.iter().any(|h| h.node_id == node && h.patroni_active)
            {
                match health.iter().find(|h| h.patroni_active) {
                    Some(h) => h.node_id.clone(),
                    None => return Fate::Wait("no running Patroni".into()),
                }
            } else {
                node.clone()
            };
            let pause = step.kind == rollout::PAUSE;
            admin(
                nodes,
                &target,
                admin_request(cluster, spec, &target, C::Pause(pause)),
                force,
            )
            .await
        }
        rollout::STOP_UNIT | rollout::START_UNIT => {
            let action = if step.kind == rollout::STOP_UNIT {
                "stop"
            } else {
                "start"
            };
            admin(
                nodes,
                &node,
                admin_request(
                    cluster,
                    &old_spec,
                    &node,
                    C::Unit(UnitAction {
                        unit: step.unit.clone(),
                        action: action.into(),
                    }),
                ),
                force,
            )
            .await
        }
        rollout::STOP_POSTGRES => admin(nodes, &node, req(C::StopPostgres(true)), force).await,
        rollout::REINIT_MEMBER => {
            if rollout::leader_of(health) == Some(node.as_str()) {
                return Fate::Fail(format!(
                    "{node} is the leader; reinit only rebuilds a standby"
                ));
            }
            admin(nodes, &node, req(C::Reinitialize(true)), force).await
        }
        rollout::ENSURE_ROLES => {
            let generated = match role_passwords(db, cluster, spec) {
                Ok(m) => m,
                Err(e) => return Fate::Fail(e),
            };
            admin(
                nodes,
                &node,
                req(C::EnsureRoles(EnsureSql {
                    generated_passwords: generated.into_iter().collect(),
                    drop_roles: ctx.drop_roles.clone(),
                    ..Default::default()
                })),
                force,
            )
            .await
        }
        rollout::ENSURE_DATABASES => {
            admin(
                nodes,
                &node,
                req(C::EnsureDatabases(EnsureSql {
                    drop_databases: ctx.drop_databases.clone(),
                    ..Default::default()
                })),
                force,
            )
            .await
        }
        rollout::ENSURE_EXTENSIONS => {
            admin(
                nodes,
                &node,
                req(C::EnsureExtensions(EnsureSql {
                    update_extensions: step.detail == "update",
                    ..Default::default()
                })),
                force,
            )
            .await
        }
        rollout::ANALYZE => admin(nodes, &node, req(C::Analyze(true)), force).await,
        rollout::PG_UPGRADE_CHECK | rollout::PG_UPGRADE_RUN => {
            let up = PgUpgrade {
                from_package: ctx.from_package.clone(),
                to_package: ctx.to_package.clone(),
            };
            let command = if step.kind == rollout::PG_UPGRADE_CHECK {
                C::PgUpgradeCheck(up)
            } else {
                // From here on the nodes run the new package.
                let mut next = spec.clone();
                next.package = ctx.to_package.clone();
                let mut stored = row.clone();
                stored.spec_json = serde_json::to_string(&next).unwrap_or_default();
                stored.previous_package = ctx.from_package.clone();
                if let Err(e) = db.upsert_pg_cluster(&stored) {
                    return Fate::Fail(e.to_string());
                }
                C::PgUpgradeRun(up)
            };
            admin(nodes, &node, req(command), force).await
        }
        rollout::SET_INITIALIZE_KEY => {
            let mut next = spec.clone();
            if kind == OpKind::RollbackUpgrade && spec.package != ctx.to_package {
                next.package = ctx.to_package.clone();
                let mut stored = row.clone();
                stored.spec_json = serde_json::to_string(&next).unwrap_or_default();
                stored.previous_package = String::new();
                if let Err(e) = db.upsert_pg_cluster(&stored) {
                    return Fate::Fail(e.to_string());
                }
            }
            admin(
                nodes,
                &node,
                admin_request(cluster, &next, &node, C::SetInitializeKey(true)),
                force,
            )
            .await
        }
        rollout::REMOVE_OLD_DATA => {
            let package = match (kind, step.detail.as_str()) {
                (OpKind::RollbackUpgrade, _) => ctx.from_package.clone(),
                _ => row.previous_package.clone(),
            };
            let Some(major) = spec::major_of(&package) else {
                return Fate::Skip("no previous major".into());
            };
            admin(
                nodes,
                &node,
                req(C::RemoveDataMajor(major.to_string())),
                force,
            )
            .await
        }
        rollout::ETCD_MEMBER_ADD => {
            let Some(m) = spec.member(&step.detail) else {
                return Fate::Fail(format!("{} is not in the spec", step.detail));
            };
            let scheme = if spec.tls.disabled { "http" } else { "https" };
            let arg = format!(
                "{}={scheme}://{}:{}",
                m.node_id, m.advertise_address, spec.ports.etcd_peer
            );
            admin(nodes, &node, req(C::EtcdMemberAdd(arg)), false).await
        }
        rollout::ETCD_MEMBER_REMOVE => {
            admin(
                nodes,
                &node,
                req(C::EtcdMemberRemove(step.detail.clone())),
                false,
            )
            .await
        }
        rollout::RELEASE_VOLUME => {
            if ctx.retain_data {
                return Fate::Skip("retainData".into());
            }
            admin(
                nodes,
                &node,
                admin_request(cluster, &old_spec, &node, C::ReleaseVolumes(true)),
                force,
            )
            .await
        }
        rollout::VERIFY => admin(nodes, &node, req(C::Verify(true)), force).await,
        other => Fate::Fail(format!("unknown step kind {other}")),
    }
}

/// Store a planned operation. Refuses while another operation runs.
pub fn start_operation(
    db: &Database,
    cluster: &str,
    kind: OpKind,
    generation: i64,
    steps: &[Step],
    ctx: &OpContext,
    force: bool,
) -> Result<String, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let op = PgOperationRow {
        id: id.clone(),
        cluster_name: cluster.to_string(),
        kind: kind.as_str().to_string(),
        state: "running".into(),
        target_generation: generation,
        steps_json: serde_json::to_string(steps).unwrap_or_default(),
        context_json: serde_json::to_string(ctx).unwrap_or_default(),
        message: String::new(),
        force,
        ..Default::default()
    };
    match db.start_pg_operation(&op).map_err(|e| e.to_string())? {
        Ok(()) => {
            let _ = db.set_pg_cluster_phase(cluster, kind.phase(), "");
            Ok(id)
        }
        Err(active) => Err(format!("operation {active} is still running on {cluster}")),
    }
}

#[cfg(test)]
mod tests;
