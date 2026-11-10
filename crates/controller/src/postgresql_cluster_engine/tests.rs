//! Engine scenarios against a fake node agent that simulates Patroni and
//! etcd: who leads, who streams, what each admin call does.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::*;
use crate::db::{PgClusterRow, PgMemberRow, PgSecretRow};

#[derive(Debug, Clone, Default)]
struct Member {
    etcd: bool,
    patroni: bool,
    role: String,
    state: String,
    lag: i64,
    timeline: i64,
    paused: bool,
}

#[derive(Default)]
struct Fake {
    members: Mutex<BTreeMap<String, Member>>,
    /// (node, command, role of the node when the call arrived)
    calls: Mutex<Vec<(String, String, String)>>,
    unreachable: Mutex<Vec<String>>,
    /// Promote this node right after the named node restarts.
    failover_after_restart: Mutex<Option<(String, String)>>,
}

impl Fake {
    fn new(nodes: &[&str]) -> Self {
        let fake = Fake::default();
        {
            let mut m = fake.members.lock().unwrap();
            for n in nodes {
                m.insert(n.to_string(), Member::default());
            }
        }
        fake
    }

    fn running(nodes: &[&str], leader: &str) -> Self {
        let fake = Fake::new(nodes);
        {
            let mut m = fake.members.lock().unwrap();
            for (name, member) in m.iter_mut() {
                member.etcd = true;
                member.patroni = true;
                member.timeline = 1;
                if name == leader {
                    member.role = "leader".into();
                    member.state = "running".into();
                } else {
                    member.role = "replica".into();
                    member.state = "streaming".into();
                }
            }
        }
        fake
    }

    fn leader(&self) -> Option<String> {
        self.members
            .lock()
            .unwrap()
            .iter()
            .find(|(_, m)| m.role == "leader")
            .map(|(n, _)| n.clone())
    }

    fn promote(&self, node: &str) {
        let mut m = self.members.lock().unwrap();
        for (name, member) in m.iter_mut() {
            if name == node {
                member.role = "leader".into();
                member.state = "running".into();
                member.timeline += 1;
            } else if member.patroni {
                member.role = "replica".into();
                member.state = "streaming".into();
                member.timeline += 1;
            }
        }
    }

    fn calls(&self, command: &str) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, c, _)| c == command)
            .map(|(n, _, role)| (n.clone(), role.clone()))
            .collect()
    }

    fn record(&self, node: &str, command: &str) {
        let role = self
            .members
            .lock()
            .unwrap()
            .get(node)
            .map(|m| m.role.clone())
            .unwrap_or_default();
        self.calls
            .lock()
            .unwrap()
            .push((node.to_string(), command.to_string(), role));
    }

    fn check(&self, node: &str) -> Result<(), String> {
        if self.unreachable.lock().unwrap().iter().any(|n| n == node) {
            return Err(format!("unreachable: {node}"));
        }
        Ok(())
    }
}

impl PostgresqlNodes for Fake {
    fn preflight<'a>(
        &'a self,
        _node_id: &'a str,
        _request: node_proto::CheckPostgresqlClusterPreflightRequest,
    ) -> BoxFuture<'a, Result<node_proto::CheckPostgresqlClusterPreflightResponse, String>> {
        Box::pin(async move {
            Ok(node_proto::CheckPostgresqlClusterPreflightResponse {
                ok: true,
                reflink_supported: true,
                ..Default::default()
            })
        })
    }

    fn apply<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::ApplyPostgresqlClusterConfigRequest,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            self.check(node_id)?;
            self.record(node_id, &format!("apply:{}", request.stage));
            let has_leader = self.leader().is_some();
            let mut m = self.members.lock().unwrap();
            let member = m.entry(node_id.to_string()).or_default();
            match request.stage.as_str() {
                "etcd-only" => member.etcd = true,
                "full" | "join" => {
                    member.etcd = true;
                    if !member.patroni {
                        member.patroni = true;
                        member.timeline = 1;
                        if has_leader {
                            member.role = "replica".into();
                            member.state = "streaming".into();
                        } else {
                            member.role = "leader".into();
                            member.state = "running".into();
                        }
                    }
                }
                "disable" => *member = Member::default(),
                _ => {}
            }
            Ok(format!("pg-{node_id}"))
        })
    }

    fn nix_status<'a>(
        &'a self,
        node_id: &'a str,
        _apply_id: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::NixApplyPhase, String>> {
        Box::pin(async move {
            self.check(node_id)?;
            Ok(node_proto::NixApplyPhase::Succeeded)
        })
    }

    fn status<'a>(
        &'a self,
        node_id: &'a str,
        _cluster: &'a str,
    ) -> BoxFuture<'a, Result<node_proto::GetPostgresqlClusterStatusResponse, String>> {
        Box::pin(async move {
            self.check(node_id)?;
            let m = self.members.lock().unwrap();
            let me = m.get(node_id).cloned().unwrap_or_default();
            let view = m
                .iter()
                .filter(|(_, v)| v.patroni)
                .map(|(n, v)| node_proto::PostgresqlClusterMemberStatus {
                    name: n.clone(),
                    role: v.role.clone(),
                    state: v.state.clone(),
                    timeline: v.timeline,
                    lag_bytes: v.lag,
                    ..Default::default()
                })
                .collect();
            Ok(node_proto::GetPostgresqlClusterStatusResponse {
                patroni_active: me.patroni,
                etcd_active: me.etcd,
                etcd_leader: me.etcd && node_id == "node-105",
                role: if me.role == "leader" {
                    "primary".into()
                } else {
                    "replica".into()
                },
                state: me.state.clone(),
                timeline: me.timeline,
                paused: me.paused,
                version: "17.9".into(),
                members: if me.patroni { view } else { Vec::new() },
                ..Default::default()
            })
        })
    }

    fn admin<'a>(
        &'a self,
        node_id: &'a str,
        request: node_proto::RunPostgresqlAdminRequest,
    ) -> BoxFuture<'a, Result<node_proto::RunPostgresqlAdminResponse, String>> {
        use node_proto::run_postgresql_admin_request::Command as C;
        Box::pin(async move {
            self.check(node_id)?;
            let command = request.command.clone().unwrap();
            let name = match &command {
                C::SwitchoverCandidate(_) => "switchover",
                C::FailoverCandidate(_) => "failover",
                C::RestartMember(_) => "restart",
                C::Reinitialize(_) => "reinit",
                C::Pause(true) => "pause",
                C::Pause(false) => "resume",
                C::PatchDynamicConfig(_) => "patch",
                C::Reload(_) => "reload",
                C::Unit(u) => {
                    if u.action == "stop" {
                        "stop"
                    } else {
                        "start"
                    }
                }
                C::EnsureRoles(_) => "ensure-roles",
                C::EnsureDatabases(_) => "ensure-databases",
                C::EnsureExtensions(_) => "ensure-extensions",
                C::StopPostgres(_) => "stop-postgres",
                _ => "other",
            };
            self.record(node_id, name);
            match command {
                C::SwitchoverCandidate(c) | C::FailoverCandidate(c) => self.promote(&c),
                C::RestartMember(_) => {
                    let promote = self
                        .failover_after_restart
                        .lock()
                        .unwrap()
                        .take_if(|(after, _)| after == node_id);
                    if let Some((_, to)) = promote {
                        self.promote(&to);
                    }
                }
                C::Pause(p) => {
                    for m in self.members.lock().unwrap().values_mut() {
                        m.paused = p;
                    }
                }
                C::Unit(u) if u.unit == "patroni" => {
                    let mut m = self.members.lock().unwrap();
                    if let Some(member) = m.get_mut(node_id) {
                        member.patroni = u.action != "stop";
                        member.state = match (u.action.as_str(), member.role.as_str()) {
                            ("stop", _) => "stopped".into(),
                            (_, "leader") => "running".into(),
                            _ => "streaming".into(),
                        };
                    }
                }
                _ => {}
            }
            Ok(node_proto::RunPostgresqlAdminResponse {
                ok: true,
                message: name.into(),
                result: String::new(),
            })
        })
    }
}

const MANIFEST: &str = include_str!("../../../../examples/postgresql-cluster/orders.yaml");
const NODES: [&str; 3] = ["node-105", "node-106", "node-108"];

fn setup(db: &Database, generation: i64) -> ClusterSpec {
    let (_, mut spec) = spec::parse_manifest(MANIFEST).unwrap();
    spec::normalize(&mut spec);
    spec.memory_limit_bytes = 8 << 30;
    let (ca, key) = pki::generate_ca("orders").unwrap();
    db.upsert_pg_cluster(&PgClusterRow {
        name: "orders".into(),
        generation,
        phase: "pending".into(),
        spec_json: serde_json::to_string(&spec).unwrap(),
        ..Default::default()
    })
    .unwrap();
    db.upsert_pg_secrets(&PgSecretRow {
        cluster_name: "orders".into(),
        superuser_password: "su".into(),
        replication_password: "re".into(),
        rewind_password: "rw".into(),
        restapi_password: "ra".into(),
        ca_cert_pem: ca,
        ca_key_pem: key,
        role_passwords_json: "{}".into(),
    })
    .unwrap();
    for n in NODES {
        db.upsert_pg_member(&PgMemberRow {
            cluster_name: "orders".into(),
            node_id: n.into(),
            etcd_voter: true,
            ..Default::default()
        })
        .unwrap();
    }
    spec
}

fn voters() -> Vec<String> {
    NODES.iter().map(|s| s.to_string()).collect()
}

async fn run_to_end(db: &Database, fake: &Fake, id: &str) -> PgOperationRow {
    for _ in 0..20 {
        drive(db, fake, id).await.unwrap();
        let op = db.get_pg_operation(id).unwrap().unwrap();
        if op.state != "running" {
            return op;
        }
    }
    db.get_pg_operation(id).unwrap().unwrap()
}

fn start(db: &Database, kind: OpKind, steps: &[Step], ctx: &OpContext, force: bool) -> String {
    start_operation(db, "orders", kind, 2, steps, ctx, force).unwrap()
}

#[tokio::test]
async fn create_brings_up_etcd_then_the_bootstrap_member_then_standbys() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 1);
    let fake = Fake::new(&NODES);
    let steps = rollout::plan_rollout(&rollout::RolloutRequest::new(
        OpKind::Create,
        &spec,
        &voters(),
    ));
    let id = start(&db, OpKind::Create, &steps, &OpContext::default(), false);
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded", "{}", op.message);

    let applies = fake.calls.lock().unwrap().clone();
    let first_full = applies
        .iter()
        .position(|(_, c, _)| c == "apply:full")
        .unwrap();
    let etcd_only: Vec<usize> = applies
        .iter()
        .enumerate()
        .filter(|(_, (_, c, _))| c == "apply:etcd-only")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(etcd_only.len(), 3);
    assert!(etcd_only.iter().all(|i| *i < first_full));
    assert_eq!(applies[first_full].0, "node-105");
    assert_eq!(fake.leader().as_deref(), Some("node-105"));
    assert_eq!(
        fake.calls("ensure-roles"),
        vec![("node-105".into(), "leader".into())]
    );

    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    assert_eq!(row.phase, "ready");
    assert_eq!(row.leader_node_id, "node-105");
    assert!(row.active_operation_id.is_empty());
    // Login role without a verifier got a generated password.
    let secrets = db.get_pg_secrets("orders").unwrap().unwrap();
    assert!(secrets.role_passwords_json.contains("\"app\""));
    // Every member got a certificate.
    assert!(db.get_pg_cert("orders", "node-108").unwrap().is_some());
}

#[tokio::test]
async fn rolling_update_never_restarts_the_leader_and_ends_with_the_old_one() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 2);
    let fake = Fake::running(&NODES, "node-105");
    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    let health = refresh_health(&db, &fake, &row, None).await.unwrap();
    let v = voters_of(&db, "orders");
    let mut req = rollout::RolloutRequest::new(OpKind::Update, &spec, &v);
    req.health = &health;
    req.restart = true;
    req.dcs_changed = true;
    let steps = rollout::plan_rollout(&req);
    let id = start(&db, OpKind::Update, &steps, &OpContext::default(), false);
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded", "{}", op.message);
    let restarts = fake.calls("restart");
    assert_eq!(restarts.len(), 3);
    assert!(
        restarts.iter().all(|(_, role)| role != "leader"),
        "{restarts:?}"
    );
    assert_eq!(restarts.last().unwrap().0, "node-105");
    assert_eq!(fake.calls("switchover").len(), 1);
    assert_eq!(fake.calls("patch").len(), 1);
}

#[tokio::test]
async fn an_unplanned_failover_mid_roll_is_followed_not_fought() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 2);
    let fake = Fake::running(&NODES, "node-105");
    // Patroni promotes node-106 right after node-106 is restarted.
    *fake.failover_after_restart.lock().unwrap() = Some(("node-106".into(), "node-106".into()));
    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    let health = refresh_health(&db, &fake, &row, None).await.unwrap();
    let v = voters_of(&db, "orders");
    let mut req = rollout::RolloutRequest::new(OpKind::Update, &spec, &v);
    req.health = &health;
    req.restart = true;
    let steps = rollout::plan_rollout(&req);
    let id = start(&db, OpKind::Update, &steps, &OpContext::default(), false);
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded", "{}", op.message);
    let restarts = fake.calls("restart");
    assert!(
        restarts.iter().all(|(_, role)| role != "leader"),
        "{restarts:?}"
    );
    // The planned switchover away from node-105 was skipped: it no longer led.
    assert!(fake.calls("switchover").is_empty());
    assert_eq!(fake.leader().as_deref(), Some("node-106"));
    // The move happened during an operation: a switchover event, no failover.
    let events = db.list_pg_events("orders", 10).unwrap();
    assert!(events.iter().all(|e| e.kind == EVENT_SWITCHOVER));
}

#[tokio::test]
async fn reload_only_update_patches_once_and_restarts_nothing() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 2);
    let fake = Fake::running(&NODES, "node-105");
    let v = voters();
    let mut req = rollout::RolloutRequest::new(OpKind::Update, &spec, &v);
    req.dcs_changed = true;
    req.sql_changed = true;
    let steps = rollout::plan_rollout(&req);
    let id = start(&db, OpKind::Update, &steps, &OpContext::default(), false);
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded");
    assert!(fake.calls("restart").is_empty());
    assert_eq!(
        fake.calls("patch"),
        vec![("node-105".into(), "leader".into())]
    );
    assert_eq!(fake.calls("reload").len(), 3);
    assert_eq!(
        db.get_pg_cluster("orders")
            .unwrap()
            .unwrap()
            .observed_generation,
        2
    );
}

#[tokio::test]
async fn a_second_operation_is_refused_while_one_runs() {
    let db = Database::open(":memory:").unwrap();
    let _ = setup(&db, 1);
    let steps = vec![Step::new(rollout::WARNING, "", "", "x")];
    let first = start(&db, OpKind::Update, &steps, &OpContext::default(), false);
    let second = start_operation(
        &db,
        "orders",
        OpKind::Switchover,
        1,
        &steps,
        &OpContext::default(),
        false,
    );
    assert!(second.unwrap_err().contains(&first));
}

#[tokio::test]
async fn unreachable_member_waits_without_force_and_is_skipped_with_force() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 2);
    let fake = Fake::running(&NODES, "node-105");
    fake.unreachable.lock().unwrap().push("node-108".into());
    let ctx = OpContext {
        old_spec_json: serde_json::to_string(&spec).unwrap(),
        retain_data: true,
        ..Default::default()
    };
    let v = voters();
    let mut req = rollout::RolloutRequest::new(OpKind::Delete, &spec, &v);
    req.leader = Some("node-105");
    let steps = rollout::plan_rollout(&req);
    let id = start(&db, OpKind::Delete, &steps, &ctx, false);
    drive(&db, &fake, &id).await.unwrap();
    let op = db.get_pg_operation(&id).unwrap().unwrap();
    assert_eq!(op.state, "running");
    assert!(op.steps_json.contains("unreachable"));
    db.update_pg_operation(&id, "cancelled", &op.steps_json, "")
        .unwrap();

    let id = start(&db, OpKind::Delete, &steps, &ctx, true);
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded", "{}", op.message);
    assert!(db.get_pg_cluster("orders").unwrap().is_none());
    assert!(op.steps_json.contains("skipped"));
}

#[tokio::test]
async fn failover_without_an_operation_is_recorded() {
    let db = Database::open(":memory:").unwrap();
    let _ = setup(&db, 1);
    let fake = Fake::running(&NODES, "node-105");
    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    refresh_health(&db, &fake, &row, None).await.unwrap();
    fake.promote("node-108");
    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    refresh_health(&db, &fake, &row, None).await.unwrap();
    let events = db.list_pg_events("orders", 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EVENT_FAILOVER);
    assert_eq!(
        (events[0].old_leader.as_str(), events[0].new_leader.as_str()),
        ("node-105", "node-108")
    );
    assert_eq!(
        db.get_pg_cluster("orders").unwrap().unwrap().leader_node_id,
        "node-108"
    );
}

#[tokio::test]
async fn major_upgrade_runs_on_the_old_leader_and_records_the_previous_package() {
    let db = Database::open(":memory:").unwrap();
    let spec = setup(&db, 2);
    let fake = Fake::running(&NODES, "node-106");
    let ctx = OpContext {
        from_package: "postgresql_17".into(),
        to_package: "postgresql_18".into(),
        old_spec_json: serde_json::to_string(&spec).unwrap(),
        ..Default::default()
    };
    let v = voters();
    let mut req = rollout::RolloutRequest::new(OpKind::UpgradeMajor, &spec, &v);
    req.leader = Some("node-106");
    let steps = rollout::plan_rollout(&req);
    let id = start(&db, OpKind::UpgradeMajor, &steps, &ctx, false);
    // Patroni stopped everywhere: the fake member goes back up on start-unit.
    let op = run_to_end(&db, &fake, &id).await;
    assert_eq!(op.state, "succeeded", "{}", op.message);
    let stops = fake.calls("stop-postgres");
    assert_eq!(stops.last().unwrap().0, "node-106");
    assert!(fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, c, _)| c.starts_with("apply:"))
        .all(|(_, c, _)| c == "apply:upgrade"));
    let row = db.get_pg_cluster("orders").unwrap().unwrap();
    assert_eq!(row.previous_package, "postgresql_17");
    assert!(row.spec_json.contains("postgresql_18"));
}

#[test]
fn merge_health_prefers_the_leader_view() {
    let (_, mut spec) = spec::parse_manifest(MANIFEST).unwrap();
    spec::normalize(&mut spec);
    let mut statuses = BTreeMap::new();
    statuses.insert(
        "node-105".to_string(),
        Ok(node_proto::GetPostgresqlClusterStatusResponse {
            patroni_active: true,
            etcd_active: true,
            role: "primary".into(),
            members: vec![
                node_proto::PostgresqlClusterMemberStatus {
                    name: "node-105".into(),
                    role: "leader".into(),
                    state: "running".into(),
                    ..Default::default()
                },
                node_proto::PostgresqlClusterMemberStatus {
                    name: "node-106".into(),
                    role: "replica".into(),
                    state: "streaming".into(),
                    lag_bytes: 42,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
    );
    statuses.insert("node-108".to_string(), Err("unreachable: node-108".into()));
    let health = merge_health(&spec, &statuses);
    assert_eq!(rollout::leader_of(&health), Some("node-105"));
    assert_eq!(health[1].lag_bytes, 42);
    assert_eq!(health[2].lag_bytes, -1);
    assert!(health[2].message.contains("unreachable"));
}
