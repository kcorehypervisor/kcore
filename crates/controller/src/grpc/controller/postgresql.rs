//! Handlers for the PostgresqlCluster RPCs.
//!
//! A child of `controller.rs` so it can reach `ControllerService`'s private
//! state; the trait impl there only delegates here. Every mutating RPC stores
//! one operation and returns its id; `postgresql_cluster_engine` executes it.

use std::collections::BTreeSet;

use kcore_postgresql::rollout::{self, OpKind, RolloutRequest};
use kcore_postgresql::spec::{self, ClusterSpec, PlanAction};
use tonic::{Request, Response, Status};

use super::ControllerService;
use crate::auth::OperatorRole;
use crate::controller_proto as pb;
use crate::db::{PgClusterRow, PgMemberRow, PgOperationRow, PgSecretRow};
use crate::postgresql_cluster_engine::{self as engine, GrpcNodes, OpContext, PostgresqlNodes};
use crate::postgresql_cluster_pki as pki;

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}

fn cluster_name(name: &str) -> Result<String, Status> {
    let name = name.trim();
    if !spec::valid_identifier(name) {
        return Err(Status::invalid_argument(format!(
            "cluster name {name:?} must match [a-z_][a-z0-9_]{{0,62}}"
        )));
    }
    Ok(name.to_string())
}

fn require_confirm(confirm: bool) -> Result<(), Status> {
    if confirm {
        Ok(())
    } else {
        Err(Status::failed_precondition(
            "set confirm: true; run the plan first to see what changes",
        ))
    }
}

fn u(v: i32) -> u32 {
    v.max(0) as u32
}

fn volume_from_proto(v: Option<pb::PostgresqlHostVolume>) -> Option<spec::HostVolume> {
    v.filter(|v| !v.name.is_empty()).map(|v| spec::HostVolume {
        name: v.name,
        storage_class: v.storage_class,
        volume_group: v.volume_group,
        size_bytes: v.size_bytes,
        fs_type: v.fs_type,
    })
}

fn volume_to_proto(v: &Option<spec::HostVolume>) -> Option<pb::PostgresqlHostVolume> {
    v.as_ref().map(|v| pb::PostgresqlHostVolume {
        name: v.name.clone(),
        storage_class: v.storage_class.clone(),
        volume_group: v.volume_group.clone(),
        size_bytes: v.size_bytes,
        fs_type: v.fs_type.clone(),
    })
}

pub(crate) fn member_from_proto(m: pb::PostgresqlMemberSpec) -> spec::Member {
    spec::Member {
        node_id: m.node_id.trim().to_string(),
        advertise_address: m.advertise_address.trim().to_string(),
        iface: m.iface.trim().to_string(),
        role: m.role,
        witness: m.witness,
        data_volume: volume_from_proto(m.data_volume),
        wal_volume: volume_from_proto(m.wal_volume),
        etcd_volume: volume_from_proto(m.etcd_volume),
        no_failover: m.no_failover,
        no_load_balance: m.no_load_balance,
        failover_priority: u(m.failover_priority),
        datacenter: m.datacenter,
    }
}

fn member_to_proto(m: &spec::Member) -> pb::PostgresqlMemberSpec {
    pb::PostgresqlMemberSpec {
        node_id: m.node_id.clone(),
        advertise_address: m.advertise_address.clone(),
        iface: m.iface.clone(),
        role: m.role.clone(),
        witness: m.witness,
        data_volume: volume_to_proto(&m.data_volume),
        wal_volume: volume_to_proto(&m.wal_volume),
        etcd_volume: volume_to_proto(&m.etcd_volume),
        no_failover: m.no_failover,
        no_load_balance: m.no_load_balance,
        failover_priority: m.failover_priority as i32,
        datacenter: m.datacenter.clone(),
    }
}

pub(crate) fn spec_from_proto(p: pb::PostgresqlClusterSpec) -> ClusterSpec {
    let r = p.replication.unwrap_or_default();
    let pat = p.patroni.unwrap_or_default();
    let ports = p.ports.unwrap_or_default();
    let port = |v: i32| u16::try_from(v).unwrap_or(0);
    ClusterSpec {
        package: p.package.trim().to_string(),
        members: p.members.into_iter().map(member_from_proto).collect(),
        replication: spec::Replication {
            mode: r.mode,
            sync_count: u(r.sync_count),
            maximum_lag_on_failover_bytes: r.maximum_lag_on_failover_bytes.max(0) as u64,
        },
        patroni: spec::Patroni {
            ttl: u(pat.ttl),
            loop_wait: u(pat.loop_wait),
            retry_timeout: u(pat.retry_timeout),
            watchdog: pat.watchdog,
        },
        vip: p.vip.filter(|v| !v.address.is_empty()).map(|v| spec::Vip {
            address: v.address,
            virtual_router_id: u(v.virtual_router_id),
        }),
        parameters: p.parameters.into_iter().collect(),
        roles: p
            .roles
            .into_iter()
            .map(|r| spec::Role {
                name: r.name,
                login: r.login,
                password_scram: r.password_scram,
                create_db: r.create_db,
                connection_limit: r.connection_limit,
                member_of: r.member_of,
            })
            .collect(),
        databases: p
            .databases
            .into_iter()
            .map(|d| spec::Database {
                name: d.name,
                owner: d.owner,
                extensions: d.extensions,
            })
            .collect(),
        client_networks: p.client_networks,
        tls: spec::Tls {
            disabled: p.tls_disabled,
            rotate_before_days: u(p.tls_rotate_before_days),
        },
        ports: spec::Ports {
            postgres: port(ports.postgres),
            patroni_api: port(ports.patroni_api),
            etcd_client: port(ports.etcd_client),
            etcd_peer: port(ports.etcd_peer),
        },
        memory_limit_bytes: p.memory_limit_bytes.max(0) as u64,
        max_replication_lag_bytes: p.max_replication_lag_bytes.max(0) as u64,
        step_timeout_seconds: u(p.step_timeout_seconds),
        allow_dc_majority: p.allow_dc_majority,
        retain_old_data_days: u(p.retain_old_data_days),
    }
}

pub(crate) fn spec_to_proto(s: &ClusterSpec) -> pb::PostgresqlClusterSpec {
    pb::PostgresqlClusterSpec {
        package: s.package.clone(),
        members: s.members.iter().map(member_to_proto).collect(),
        replication: Some(pb::PostgresqlReplicationSpec {
            mode: s.replication.mode.clone(),
            sync_count: s.replication.sync_count as i32,
            maximum_lag_on_failover_bytes: s.replication.maximum_lag_on_failover_bytes as i64,
        }),
        patroni: Some(pb::PostgresqlPatroniSpec {
            ttl: s.patroni.ttl as i32,
            loop_wait: s.patroni.loop_wait as i32,
            retry_timeout: s.patroni.retry_timeout as i32,
            watchdog: s.patroni.watchdog.clone(),
        }),
        vip: s.vip.as_ref().map(|v| pb::PostgresqlVipSpec {
            address: v.address.clone(),
            virtual_router_id: v.virtual_router_id as i32,
        }),
        parameters: s.parameters.clone().into_iter().collect(),
        roles: s
            .roles
            .iter()
            .map(|r| pb::PostgresqlRoleSpec {
                name: r.name.clone(),
                login: r.login,
                password_scram: r.password_scram.clone(),
                create_db: r.create_db,
                connection_limit: r.connection_limit,
                member_of: r.member_of.clone(),
            })
            .collect(),
        databases: s
            .databases
            .iter()
            .map(|d| pb::PostgresqlDatabaseSpec {
                name: d.name.clone(),
                owner: d.owner.clone(),
                extensions: d.extensions.clone(),
            })
            .collect(),
        client_networks: s.client_networks.clone(),
        tls_disabled: s.tls.disabled,
        tls_rotate_before_days: s.tls.rotate_before_days as i32,
        ports: Some(pb::PostgresqlPorts {
            postgres: i32::from(s.ports.postgres),
            patroni_api: i32::from(s.ports.patroni_api),
            etcd_client: i32::from(s.ports.etcd_client),
            etcd_peer: i32::from(s.ports.etcd_peer),
        }),
        memory_limit_bytes: s.memory_limit_bytes as i64,
        max_replication_lag_bytes: s.max_replication_lag_bytes as i64,
        step_timeout_seconds: s.step_timeout_seconds as i32,
        allow_dc_majority: s.allow_dc_majority,
        retain_old_data_days: s.retain_old_data_days as i32,
    }
}

fn health_to_proto(h: &rollout::MemberHealth) -> pb::PostgresqlMemberHealth {
    pb::PostgresqlMemberHealth {
        node_id: h.node_id.clone(),
        patroni_active: h.patroni_active,
        etcd_active: h.etcd_active,
        role: h.role.clone(),
        state: h.state.clone(),
        timeline: h.timeline,
        lag_bytes: h.lag_bytes,
        pending_restart: h.pending_restart,
        version: h.version.clone(),
        holds_vip: h.holds_vip,
        data_free_bytes: h.data_free_bytes,
        etcd_leader: h.etcd_leader,
        message: h.message.clone(),
    }
}

fn op_to_proto(op: &PgOperationRow) -> pb::PostgresqlOperation {
    let steps: Vec<rollout::Step> = serde_json::from_str(&op.steps_json).unwrap_or_default();
    pb::PostgresqlOperation {
        id: op.id.clone(),
        cluster_name: op.cluster_name.clone(),
        kind: op.kind.clone(),
        state: op.state.clone(),
        target_generation: op.target_generation,
        steps: steps
            .iter()
            .enumerate()
            .map(|(i, s)| pb::PostgresqlOperationStep {
                index: i as i32,
                kind: s.kind.clone(),
                node_id: s.node_id.clone(),
                unit: s.unit.clone(),
                detail: s.detail.clone(),
                state: s.state.clone(),
                message: s.message.clone(),
            })
            .collect(),
        message: op.message.clone(),
        created_at: op.created_at.clone(),
        updated_at: op.updated_at.clone(),
    }
}

fn rpo_line(spec: &ClusterSpec) -> String {
    let standbys = spec.standby_count();
    match spec.replication.mode.as_str() {
        spec::MODE_ASYNC => format!(
            "up to {} bytes of acknowledged commits may be lost on failover (async)",
            spec.replication.maximum_lag_on_failover_bytes
        ),
        mode => format!(
            "RPO 0 for acknowledged commits ({mode}, {} of {standbys} standbys)",
            spec.replication.sync_count
        ),
    }
}

/// Roles and databases that left the spec.
fn dropped(old: &ClusterSpec, new: &ClusterSpec) -> (Vec<String>, Vec<String>) {
    let keep_roles: BTreeSet<&str> = new.roles.iter().map(|r| r.name.as_str()).collect();
    let keep_dbs: BTreeSet<&str> = new.databases.iter().map(|d| d.name.as_str()).collect();
    (
        old.roles
            .iter()
            .filter(|r| !keep_roles.contains(r.name.as_str()))
            .map(|r| r.name.clone())
            .collect(),
        old.databases
            .iter()
            .filter(|d| !keep_dbs.contains(d.name.as_str()))
            .map(|d| d.name.clone())
            .collect(),
    )
}

struct Planned {
    diff: spec::PlanDiff,
    blocking: Vec<String>,
    warnings: Vec<String>,
    node_plans: Vec<pb::PostgresqlNodePlan>,
}

impl ControllerService {
    fn pg_nodes(&self) -> GrpcNodes {
        GrpcNodes::new(self.db.clone(), self.clients.clone())
    }

    fn pg_row(&self, name: &str) -> Result<PgClusterRow, Status> {
        self.db
            .get_pg_cluster(name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("PostgresqlCluster '{name}' not found")))
    }

    fn pg_cluster_to_proto(&self, row: &PgClusterRow) -> Result<pb::PostgresqlCluster, Status> {
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let health = engine::health_of(&self.db, &row.name);
        let streaming = health
            .iter()
            .filter(|h| h.streaming(spec.max_replication_lag_bytes))
            .count();
        Ok(pb::PostgresqlCluster {
            name: row.name.clone(),
            generation: row.generation,
            spec: Some(spec_to_proto(&spec)),
            status: Some(pb::PostgresqlClusterStatus {
                phase: row.phase.clone(),
                message: row.message.clone(),
                observed_generation: row.observed_generation,
                leader_node_id: row.leader_node_id.clone(),
                timeline: row.timeline,
                streaming_standbys: streaming as i32,
                total_members: spec.members.len() as i32,
                running_version: row.observed_version.clone(),
                paused: row.paused,
                active_operation_id: row.active_operation_id.clone(),
                previous_package: row.previous_package.clone(),
                updated_at: row.updated_at.clone(),
            }),
        })
    }

    /// Defaults kcore fills from inventory: the memory limit is 25 % of the
    /// smallest member's RAM.
    fn pg_fill_defaults(&self, spec: &mut ClusterSpec) {
        spec::normalize(spec);
        if spec.memory_limit_bytes == 0 {
            let smallest = spec
                .members
                .iter()
                .filter(|m| !m.witness)
                .filter_map(|m| self.db.get_node(&m.node_id).ok().flatten())
                .map(|n| n.memory_bytes)
                .filter(|b| *b > 0)
                .min();
            if let Some(bytes) = smallest {
                spec.memory_limit_bytes = (bytes / 4) as u64;
            }
        }
    }

    async fn pg_plan(
        &self,
        name: &str,
        spec: &ClusterSpec,
        old: Option<&ClusterSpec>,
        live_preflight: bool,
    ) -> Result<Planned, Status> {
        let issues = spec::validate(name, spec);
        let (mut blocking, mut warnings) = spec::split_issues(&issues);
        let changed_keys: Vec<String> = old
            .map(|o| {
                o.parameters
                    .keys()
                    .chain(spec.parameters.keys())
                    .filter(|k| o.parameters.get(*k) != spec.parameters.get(*k))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let nodes = self.pg_nodes();
        let mut node_plans = Vec::new();
        let mut restart_keys: Vec<String> = Vec::new();
        let voters: Vec<String> = match old {
            Some(_) => engine::voters_of(&self.db, name),
            None => spec.members.iter().map(|m| m.node_id.clone()).collect(),
        };
        for m in &spec.members {
            let mut plan = pb::PostgresqlNodePlan {
                node_id: m.node_id.clone(),
                roles: if m.witness {
                    "etcd".into()
                } else if voters.contains(&m.node_id) {
                    "postgres+etcd".into()
                } else {
                    "postgres".into()
                },
                ..Default::default()
            };
            match self.db.get_node(&m.node_id).map_err(internal)? {
                None => plan
                    .issues
                    .push(format!("node {} is not registered", m.node_id)),
                Some(node)
                    if node.approval_status != "approved" && !node.approval_status.is_empty() =>
                {
                    plan.issues
                        .push(format!("node {} is not approved", m.node_id))
                }
                Some(_) => {}
            }
            if let Some(other) = self
                .db
                .pg_cluster_of_node(&m.node_id)
                .map_err(internal)?
                .filter(|c| c != name)
            {
                plan.issues.push(format!(
                    "node {} belongs to PostgreSQL cluster {other}",
                    m.node_id
                ));
            }
            if let Some(single) = self
                .db
                .get_postgresql_for_node(&m.node_id)
                .map_err(internal)?
            {
                plan.issues.push(format!(
                    "node {} runs the single-node Postgresql '{}'; remove it first",
                    m.node_id, single.name
                ));
            }
            if live_preflight && plan.issues.is_empty() {
                let request = kcore_node_preflight(name, spec, &m.node_id, &voters, &changed_keys);
                match nodes.preflight(&m.node_id, request).await {
                    Ok(resp) => {
                        plan.resolved_version = resp.running_version.clone();
                        for issue in resp.issues {
                            let line = format!("{}: {}: {}", m.node_id, issue.check, issue.message);
                            if issue.blocking {
                                plan.issues.push(line);
                            } else {
                                warnings.push(line);
                            }
                        }
                        for k in resp.restart_parameters {
                            if !restart_keys.contains(&k) {
                                restart_keys.push(k);
                            }
                        }
                    }
                    Err(e) => plan.issues.push(format!("{}: preflight: {e}", m.node_id)),
                }
            }
            plan.preflight_passed = plan.issues.is_empty();
            blocking.extend(plan.issues.iter().cloned());
            node_plans.push(plan);
        }
        let diff = spec::diff(old, spec, &restart_keys);
        let (b, w) = spec::split_issues(&diff.issues);
        blocking.extend(b);
        warnings.extend(w);
        Ok(Planned {
            diff,
            blocking,
            warnings,
            node_plans,
        })
    }

    pub(super) async fn pg_plan_rpc(
        &self,
        request: Request<pb::PlanPostgresqlClusterRequest>,
    ) -> Result<Response<pb::PlanPostgresqlClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        let mut spec = spec_from_proto(
            req.spec
                .ok_or_else(|| Status::invalid_argument("spec is required"))?,
        );
        self.pg_fill_defaults(&mut spec);
        let row = self.db.get_pg_cluster(&name).map_err(internal)?;
        let old = row
            .as_ref()
            .map(|r| engine::spec_from_json(&r.spec_json))
            .transpose()
            .map_err(Status::internal)?;
        let planned = self.pg_plan(&name, &spec, old.as_ref(), true).await?;
        let voters: Vec<String> = match &row {
            Some(_) => engine::voters_of(&self.db, &name),
            None => spec.members.iter().map(|m| m.node_id.clone()).collect(),
        };
        let health = engine::health_of(&self.db, &name);
        let mut blocking = planned.blocking;
        let steps = match planned.diff.action {
            PlanAction::Create => {
                rollout::plan_rollout(&RolloutRequest::new(OpKind::Create, &spec, &voters))
            }
            PlanAction::UpdateReload | PlanAction::UpdateRestart => {
                let mut r = RolloutRequest::new(OpKind::Update, &spec, &voters);
                r.health = &health;
                r.dcs_changed = planned.diff.dcs_changed;
                r.sql_changed = planned.diff.sql_changed;
                r.restart = planned.diff.action == PlanAction::UpdateRestart;
                rollout::plan_rollout(&r)
            }
            PlanAction::Scale => {
                let mut r = RolloutRequest::new(
                    if planned.diff.removed_members.is_empty() {
                        OpKind::ScaleOut
                    } else {
                        OpKind::ScaleIn
                    },
                    &spec,
                    &voters,
                );
                r.health = &health;
                let nodes = if planned.diff.removed_members.is_empty() {
                    planned.diff.added_members.clone()
                } else {
                    planned.diff.removed_members.clone()
                };
                r.nodes = &nodes;
                rollout::plan_rollout(&r)
            }
            PlanAction::UpgradeMajor => {
                blocking.push(
                    "package changed: use UpgradePostgresqlCluster (kctl pg-cluster upgrade)"
                        .into(),
                );
                Vec::new()
            }
            PlanAction::Replace => {
                blocking.push(format!(
                    "immutable fields changed ({}); create a new cluster",
                    planned.diff.changed_fields.join(", ")
                ));
                Vec::new()
            }
            PlanAction::Unchanged => Vec::new(),
        };
        let preview = spec
            .bootstrap_node()
            .and_then(|n| {
                kcore_postgresql::nixgen::MemberFragment::from_spec(
                    &name, 1, &spec, n, &voters, "new", None,
                )
            })
            .map(|f| {
                kcore_postgresql::nixgen::render_member(&f, kcore_postgresql::nixgen::Stage::Full)
            })
            .unwrap_or_default();
        Ok(Response::new(pb::PlanPostgresqlClusterResponse {
            action: planned.diff.action.as_str().to_string(),
            blocking_issues: blocking,
            warnings: planned.warnings,
            changed_fields: planned.diff.changed_fields.clone(),
            changed_parameters: planned
                .diff
                .parameters
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect(),
            node_plans: planned.node_plans,
            rollout_steps: steps.iter().map(rollout::Step::line).collect(),
            rpo: rpo_line(&spec),
            rendered_nix_preview: preview,
        }))
    }

    fn pg_op_response(
        &self,
        name: &str,
        operation_id: String,
        message: String,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        let cluster = match self.db.get_pg_cluster(name).map_err(internal)? {
            Some(row) => Some(self.pg_cluster_to_proto(&row)?),
            None => None,
        };
        Ok(Response::new(pb::PostgresqlClusterOperationResponse {
            cluster,
            operation_id,
            message,
        }))
    }

    fn pg_start(
        &self,
        name: &str,
        kind: OpKind,
        generation: i64,
        steps: &[rollout::Step],
        ctx: &OpContext,
        force: bool,
    ) -> Result<String, Status> {
        engine::start_operation(&self.db, name, kind, generation, steps, ctx, force)
            .map_err(Status::failed_precondition)
    }

    pub(super) async fn pg_create(
        &self,
        request: Request<pb::CreatePostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        if self.db.get_pg_cluster(&name).map_err(internal)?.is_some() {
            return Err(Status::already_exists(format!(
                "PostgresqlCluster '{name}' exists; use UpdatePostgresqlCluster"
            )));
        }
        let mut spec = spec_from_proto(
            req.spec
                .ok_or_else(|| Status::invalid_argument("spec is required"))?,
        );
        self.pg_fill_defaults(&mut spec);
        let planned = self.pg_plan(&name, &spec, None, true).await?;
        if !planned.blocking.is_empty() {
            return Err(Status::failed_precondition(planned.blocking.join("; ")));
        }

        let (ca_cert_pem, ca_key_pem) = pki::generate_ca(&name).map_err(internal)?;
        let secrets = PgSecretRow {
            cluster_name: name.clone(),
            superuser_password: pki::random_password().map_err(internal)?,
            replication_password: pki::random_password().map_err(internal)?,
            rewind_password: pki::random_password().map_err(internal)?,
            restapi_password: pki::random_password().map_err(internal)?,
            ca_cert_pem,
            ca_key_pem,
            role_passwords_json: "{}".into(),
        };
        let spec_json = serde_json::to_string(&spec).map_err(internal)?;
        self.db
            .upsert_pg_cluster(&PgClusterRow {
                name: name.clone(),
                generation: 1,
                phase: "pending".into(),
                spec_json,
                ..Default::default()
            })
            .map_err(internal)?;
        let stored = (|| -> Result<(), Status> {
            self.db.upsert_pg_secrets(&secrets).map_err(internal)?;
            for m in &spec.members {
                self.db
                    .upsert_pg_member(&PgMemberRow {
                        cluster_name: name.clone(),
                        node_id: m.node_id.clone(),
                        etcd_voter: true,
                        ..Default::default()
                    })
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
            }
            Ok(())
        })();
        if let Err(e) = stored {
            let _ = self.db.delete_pg_cluster(&name);
            return Err(e);
        }
        let voters: Vec<String> = spec.members.iter().map(|m| m.node_id.clone()).collect();
        let steps = rollout::plan_rollout(&RolloutRequest::new(OpKind::Create, &spec, &voters));
        let id = self.pg_start(
            &name,
            OpKind::Create,
            1,
            &steps,
            &OpContext::default(),
            false,
        )?;
        self.record_audit(
            &actor,
            "CreatePostgresqlCluster",
            &format!("postgresql_cluster/{name}"),
            id.clone(),
        );
        self.pg_op_response(
            &name,
            id,
            format!(
                "PostgresqlCluster '{name}' created; {} steps queued",
                steps.len()
            ),
        )
    }

    pub(super) async fn pg_update(
        &self,
        request: Request<pb::UpdatePostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        if !row.previous_package.is_empty() {
            return Err(Status::failed_precondition(
                "a major upgrade is not finalized; finalize or roll it back first",
            ));
        }
        let old = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let mut spec = spec_from_proto(
            req.spec
                .ok_or_else(|| Status::invalid_argument("spec is required"))?,
        );
        // The bootstrap role is create-only; keep the stored one.
        for m in &mut spec.members {
            if let Some(prev) = old.member(&m.node_id) {
                m.role = prev.role.clone();
            }
        }
        if spec.memory_limit_bytes == 0 {
            spec.memory_limit_bytes = old.memory_limit_bytes;
        }
        self.pg_fill_defaults(&mut spec);
        let planned = self.pg_plan(&name, &spec, Some(&old), true).await?;
        let mut blocking = planned.blocking;
        let diff = planned.diff;
        let voters = engine::voters_of(&self.db, &name);
        let (drop_roles, drop_dbs) = dropped(&old, &spec);
        if (!drop_roles.is_empty() || !drop_dbs.is_empty()) && !req.force {
            blocking.push(format!(
                "roles {drop_roles:?} and databases {drop_dbs:?} left the spec; set force to drop them"
            ));
        }
        match diff.action {
            PlanAction::Unchanged => {
                return self.pg_op_response(&name, String::new(), "unchanged".into());
            }
            PlanAction::UpgradeMajor => {
                blocking.push("package changed: use UpgradePostgresqlCluster".into())
            }
            PlanAction::Replace => blocking.push(format!(
                "immutable fields changed ({}); create a new cluster",
                diff.changed_fields.join(", ")
            )),
            PlanAction::Scale => {
                for removed in &diff.removed_members {
                    if voters.contains(removed) {
                        blocking.push(format!(
                            "{removed} is an etcd voter; use ReplacePostgresqlMember to move its vote"
                        ));
                    }
                }
                if !diff.added_members.is_empty() && !diff.removed_members.is_empty() {
                    blocking.push("add and remove members in separate requests".into());
                }
            }
            _ => {}
        }
        if !blocking.is_empty() {
            return Err(Status::failed_precondition(blocking.join("; ")));
        }

        let health = engine::health_of(&self.db, &name);
        let generation = row.generation + 1;
        let ctx = OpContext {
            old_spec_json: row.spec_json.clone(),
            drop_roles,
            drop_databases: drop_dbs,
            ..Default::default()
        };
        let (kind, steps) = match diff.action {
            PlanAction::Scale => {
                let (kind, nodes) = if diff.removed_members.is_empty() {
                    (OpKind::ScaleOut, diff.added_members.clone())
                } else {
                    (OpKind::ScaleIn, diff.removed_members.clone())
                };
                let mut r = RolloutRequest::new(kind, &spec, &voters);
                r.health = &health;
                r.nodes = &nodes;
                (kind, rollout::plan_rollout(&r))
            }
            _ => {
                let mut r = RolloutRequest::new(OpKind::Update, &spec, &voters);
                r.health = &health;
                r.dcs_changed = diff.dcs_changed;
                r.sql_changed = diff.sql_changed
                    || !ctx.drop_roles.is_empty()
                    || !ctx.drop_databases.is_empty();
                r.restart = diff.action == PlanAction::UpdateRestart;
                (OpKind::Update, rollout::plan_rollout(&r))
            }
        };
        // ScaleIn keeps the departing members in the stored spec until the
        // operation succeeds is not needed: the engine reads them from
        // old_spec_json. New members are claimed now.
        for added in &diff.added_members {
            self.db
                .upsert_pg_member(&PgMemberRow {
                    cluster_name: name.clone(),
                    node_id: added.clone(),
                    etcd_voter: false,
                    ..Default::default()
                })
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
        }
        let mut stored = row.clone();
        stored.generation = generation;
        stored.spec_json = serde_json::to_string(&spec).map_err(internal)?;
        self.db.upsert_pg_cluster(&stored).map_err(internal)?;
        let id = self.pg_start(&name, kind, generation, &steps, &ctx, req.force)?;
        self.record_audit(
            &actor,
            "UpdatePostgresqlCluster",
            &format!("postgresql_cluster/{name}"),
            id.clone(),
        );
        self.pg_op_response(
            &name,
            id,
            format!(
                "{} at generation {generation}: {}",
                kind.as_str(),
                diff.changed_fields.join(", ")
            ),
        )
    }

    pub(super) async fn pg_scale(
        &self,
        request: Request<pb::ScalePostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let (metadata, extensions, req) = request.into_parts();
        let name = cluster_name(&req.name)?;
        let row = self.pg_row(&name)?;
        let mut spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        spec.members
            .retain(|m| !req.remove_members.contains(&m.node_id));
        spec.members
            .extend(req.add_members.into_iter().map(member_from_proto));
        let update = pb::UpdatePostgresqlClusterRequest {
            name,
            spec: Some(spec_to_proto(&spec)),
            confirm: req.confirm,
            force: req.force,
        };
        self.pg_update(Request::from_parts(metadata, extensions, update))
            .await
    }

    pub(super) async fn pg_replace(
        &self,
        request: Request<pb::ReplacePostgresqlMemberRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        let old_spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let old_member = old_spec
            .member(&req.old_node_id)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("{} is not a member", req.old_node_id)))?;
        let mut new_member = member_from_proto(
            req.new_member
                .ok_or_else(|| Status::invalid_argument("new_member is required"))?,
        );
        new_member.witness = old_member.witness;
        new_member.role = "replica".into();
        let mut spec = old_spec.clone();
        for m in &mut spec.members {
            if m.node_id == old_member.node_id {
                *m = new_member.clone();
            }
        }
        self.pg_fill_defaults(&mut spec);
        let planned = self.pg_plan(&name, &spec, Some(&old_spec), true).await?;
        let blocking: Vec<String> = planned
            .blocking
            .into_iter()
            .filter(|b| !(req.force && b.starts_with(&format!("{}:", old_member.node_id))))
            .collect();
        if !blocking.is_empty() {
            return Err(Status::failed_precondition(blocking.join("; ")));
        }
        let voters = engine::voters_of(&self.db, &name);
        let was_voter = voters.contains(&old_member.node_id);
        let health = engine::health_of(&self.db, &name);
        let nodes = vec![old_member.node_id.clone()];
        let mut r = RolloutRequest::new(OpKind::Replace, &spec, &voters);
        r.health = &health;
        r.nodes = &nodes;
        r.target = &new_member.node_id;
        let steps = rollout::plan_rollout(&r);
        self.db
            .upsert_pg_member(&PgMemberRow {
                cluster_name: name.clone(),
                node_id: new_member.node_id.clone(),
                etcd_voter: was_voter,
                ..Default::default()
            })
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        if was_voter {
            self.db
                .upsert_pg_member(&PgMemberRow {
                    cluster_name: name.clone(),
                    node_id: old_member.node_id.clone(),
                    etcd_voter: false,
                    ..Default::default()
                })
                .map_err(internal)?;
        }
        let generation = row.generation + 1;
        let mut stored = row.clone();
        stored.generation = generation;
        stored.spec_json = serde_json::to_string(&spec).map_err(internal)?;
        self.db.upsert_pg_cluster(&stored).map_err(internal)?;
        let ctx = OpContext {
            old_spec_json: row.spec_json.clone(),
            ..Default::default()
        };
        let id = self.pg_start(&name, OpKind::Replace, generation, &steps, &ctx, req.force)?;
        self.record_audit(
            &actor,
            "ReplacePostgresqlMember",
            &format!("postgresql_cluster/{name}"),
            id.clone(),
        );
        self.pg_op_response(
            &name,
            id,
            format!(
                "replacing {} with {}",
                old_member.node_id, new_member.node_id
            ),
        )
    }

    /// Operations that do not change the spec: one request, one plan.
    async fn pg_simple(
        &self,
        name: &str,
        actor: &str,
        audit: &str,
        kind: OpKind,
        target: &str,
        pause: bool,
        ctx: OpContext,
        force: bool,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        let row = self.pg_row(name)?;
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let voters = engine::voters_of(&self.db, name);
        let health = match self.db.get_pg_cluster(name).map_err(internal)? {
            Some(r) => engine::refresh_health(&self.db, &self.pg_nodes(), &r, None)
                .await
                .unwrap_or_else(|_| engine::health_of(&self.db, name)),
            None => Vec::new(),
        };
        let mut r = RolloutRequest::new(kind, &spec, &voters);
        r.health = &health;
        r.target = target;
        r.pause = pause;
        let steps = rollout::plan_rollout(&r);
        let leader = rollout::leader_of(&health).unwrap_or_default().to_string();
        if !matches!(kind, OpKind::Create | OpKind::Failover) {
            rollout::rollout_invariants(&steps, &leader).map_err(Status::internal)?;
        }
        let id = self.pg_start(name, kind, row.generation, &steps, &ctx, force)?;
        self.record_audit(
            actor,
            audit,
            &format!("postgresql_cluster/{name}"),
            id.clone(),
        );
        self.pg_op_response(
            name,
            id,
            format!("{} queued: {} steps", kind.as_str(), steps.len()),
        )
    }

    pub(super) async fn pg_delete(
        &self,
        request: Request<pb::DeletePostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        let ctx = OpContext {
            old_spec_json: row.spec_json.clone(),
            retain_data: req.retain_data,
            ..Default::default()
        };
        self.pg_simple(
            &name,
            &actor,
            "DeletePostgresqlCluster",
            OpKind::Delete,
            "",
            false,
            ctx,
            req.force,
        )
        .await
    }

    pub(super) async fn pg_reinit(
        &self,
        request: Request<pb::ReinitPostgresqlMemberRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        if row.leader_node_id == req.node_id {
            return Err(Status::failed_precondition(format!(
                "{} is the leader; reinit rebuilds a standby from the leader",
                req.node_id
            )));
        }
        let node = req.node_id.clone();
        self.pg_simple(
            &name,
            &actor,
            "ReinitPostgresqlMember",
            OpKind::Reinit,
            &node,
            false,
            OpContext::default(),
            false,
        )
        .await
    }

    pub(super) async fn pg_upgrade(
        &self,
        request: Request<pb::UpgradePostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        if !row.previous_package.is_empty() {
            return Err(Status::failed_precondition(format!(
                "upgrade from {} is not finalized; finalize or roll it back first",
                row.previous_package
            )));
        }
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let target = if req.package.trim().is_empty() {
            spec.package.clone()
        } else {
            req.package.trim().to_string()
        };
        let (Some(from), Some(to)) = (spec::major_of(&spec.package), spec::major_of(&target))
        else {
            return Err(Status::invalid_argument(format!(
                "package {target} is not a versioned attribute"
            )));
        };
        if !spec::PACKAGES.contains(&target.as_str()) {
            return Err(Status::invalid_argument(format!(
                "package {target} is not supported"
            )));
        }
        if to < from {
            return Err(Status::failed_precondition(
                "PostgreSQL does not downgrade across majors",
            ));
        }
        if to == from {
            return self
                .pg_simple(
                    &name,
                    &actor,
                    "UpgradePostgresqlCluster",
                    OpKind::UpgradeMinor,
                    "",
                    false,
                    OpContext::default(),
                    false,
                )
                .await;
        }
        // Reflink decides whether the upgrade can be rolled back.
        let nodes = self.pg_nodes();
        let voters = engine::voters_of(&self.db, &name);
        let mut warnings = Vec::new();
        for m in spec.postgres_members() {
            let mut preflight = kcore_node_preflight(&name, &spec, &m.node_id, &voters, &[]);
            preflight.upgrade_package = target.clone();
            if let Ok(resp) = nodes.preflight(&m.node_id, preflight).await {
                if !resp.reflink_supported {
                    warnings.push(m.node_id.clone());
                }
            }
        }
        if !warnings.is_empty() && !req.force {
            return Err(Status::failed_precondition(format!(
                "{} cannot clone the data directory (no reflink): pg_upgrade would use --link and the upgrade could not be rolled back; set force to accept",
                warnings.join(", ")
            )));
        }
        let ctx = OpContext {
            from_package: spec.package.clone(),
            to_package: target,
            old_spec_json: row.spec_json.clone(),
            ..Default::default()
        };
        self.pg_simple(
            &name,
            &actor,
            "UpgradePostgresqlCluster",
            OpKind::UpgradeMajor,
            "",
            false,
            ctx,
            false,
        )
        .await
    }

    pub(super) async fn pg_finalize(
        &self,
        request: Request<pb::PostgresqlClusterNameRequest>,
        rollback: bool,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let row = self.pg_row(&name)?;
        if row.previous_package.is_empty() {
            return Err(Status::failed_precondition(
                "no major upgrade is waiting for finalize",
            ));
        }
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        if rollback {
            let ctx = OpContext {
                from_package: spec.package.clone(),
                to_package: row.previous_package.clone(),
                ..Default::default()
            };
            self.pg_simple(
                &name,
                &actor,
                "RollbackPostgresqlUpgrade",
                OpKind::RollbackUpgrade,
                "",
                false,
                ctx,
                false,
            )
            .await
        } else {
            self.pg_simple(
                &name,
                &actor,
                "FinalizePostgresqlUpgrade",
                OpKind::FinalizeUpgrade,
                "",
                false,
                OpContext::default(),
                false,
            )
            .await
        }
    }

    pub(super) async fn pg_switchover(
        &self,
        request: Request<pb::SwitchoverPostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let candidate = req.candidate.trim().to_string();
        let row = self.pg_row(&name)?;
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        if !candidate.is_empty() {
            match spec.member(&candidate) {
                Some(m) if m.witness || m.no_failover => {
                    return Err(Status::failed_precondition(format!(
                        "{candidate} cannot be promoted"
                    )))
                }
                Some(_) => {}
                None => return Err(Status::not_found(format!("{candidate} is not a member"))),
            }
        }
        self.pg_simple(
            &name,
            &actor,
            "SwitchoverPostgresqlCluster",
            OpKind::Switchover,
            &candidate,
            false,
            OpContext::default(),
            false,
        )
        .await
    }

    pub(super) async fn pg_failover(
        &self,
        request: Request<pb::FailoverPostgresqlClusterRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        if !req.force {
            return Err(Status::failed_precondition(
                "manual failover can lose committed data; set force (use switchover while a leader is healthy)",
            ));
        }
        let candidate = req.candidate.trim().to_string();
        let row = self.pg_row(&name)?;
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        if spec.member(&candidate).is_none_or(|m| m.witness) {
            return Err(Status::invalid_argument(
                "candidate must be a PostgreSQL member",
            ));
        }
        let health = engine::refresh_health(&self.db, &self.pg_nodes(), &row, None)
            .await
            .map_err(Status::unavailable)?;
        let voters = engine::voters_of(&self.db, &name);
        if !rollout::etcd_quorum(&voters, &health) {
            return Err(Status::failed_precondition(
                "etcd has no quorum: Patroni cannot write the leader key; replace the lost voters first",
            ));
        }
        self.pg_simple(
            &name,
            &actor,
            "FailoverPostgresqlCluster",
            OpKind::Failover,
            &candidate,
            false,
            OpContext::default(),
            true,
        )
        .await
    }

    pub(super) async fn pg_maintenance(
        &self,
        request: Request<pb::SetPostgresqlMaintenanceRequest>,
    ) -> Result<Response<pb::PostgresqlClusterOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        require_confirm(req.confirm)?;
        let pause = req.enabled;
        self.pg_simple(
            &name,
            &actor,
            "SetPostgresqlMaintenance",
            OpKind::Maintenance,
            "",
            pause,
            OpContext::default(),
            false,
        )
        .await
    }

    pub(super) async fn pg_get(
        &self,
        request: Request<pb::GetPostgresqlClusterRequest>,
    ) -> Result<Response<pb::GetPostgresqlClusterResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = cluster_name(&request.into_inner().name)?;
        let row = self.pg_row(&name)?;
        Ok(Response::new(pb::GetPostgresqlClusterResponse {
            cluster: Some(self.pg_cluster_to_proto(&row)?),
        }))
    }

    pub(super) async fn pg_list(
        &self,
        request: Request<pb::ListPostgresqlClustersRequest>,
    ) -> Result<Response<pb::ListPostgresqlClustersResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let clusters = self
            .db
            .list_pg_clusters()
            .map_err(internal)?
            .iter()
            .map(|row| self.pg_cluster_to_proto(row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(pb::ListPostgresqlClustersResponse {
            clusters,
        }))
    }

    pub(super) async fn pg_health(
        &self,
        request: Request<pb::PostgresqlClusterNameRequest>,
    ) -> Result<Response<pb::GetPostgresqlClusterHealthResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = cluster_name(&request.into_inner().name)?;
        let row = self.pg_row(&name)?;
        let health = engine::refresh_health(&self.db, &self.pg_nodes(), &row, None)
            .await
            .map_err(Status::internal)?;
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let voters = engine::voters_of(&self.db, &name);
        let leader = rollout::leader_of(&health).unwrap_or_default().to_string();
        let overall = match rollout::observed_phase(&spec, &voters, &health) {
            "ready" => "healthy",
            "degraded" => "degraded",
            _ => "critical",
        };
        let up = voters
            .iter()
            .filter(|v| health.iter().any(|h| &h.node_id == *v && h.etcd_active))
            .count();
        Ok(Response::new(pb::GetPostgresqlClusterHealthResponse {
            overall_health: overall.into(),
            timeline: health
                .iter()
                .find(|h| h.node_id == leader)
                .map(|h| h.timeline)
                .unwrap_or(0),
            leader_node_id: leader,
            etcd_quorum: rollout::etcd_quorum(&voters, &health),
            etcd_members_up: up as i32,
            etcd_members_total: voters.len() as i32,
            paused: health.iter().any(|h| h.paused),
            members: health.iter().map(health_to_proto).collect(),
        }))
    }

    pub(super) async fn pg_events(
        &self,
        request: Request<pb::ListPostgresqlClusterEventsRequest>,
    ) -> Result<Response<pb::ListPostgresqlClusterEventsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let req = request.into_inner();
        let name = cluster_name(&req.name)?;
        let limit = if req.limit <= 0 {
            50
        } else {
            req.limit.min(1000)
        };
        let events = self
            .db
            .list_pg_events(&name, i64::from(limit))
            .map_err(internal)?
            .into_iter()
            .map(|e| pb::PostgresqlClusterEvent {
                id: e.id,
                kind: e.kind,
                old_leader: e.old_leader,
                new_leader: e.new_leader,
                timeline: e.timeline,
                message: e.message,
                created_at: e.created_at,
            })
            .collect();
        Ok(Response::new(pb::ListPostgresqlClusterEventsResponse {
            events,
        }))
    }

    pub(super) async fn pg_get_operation(
        &self,
        request: Request<pb::GetPostgresqlOperationRequest>,
    ) -> Result<Response<pb::GetPostgresqlOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let id = request.into_inner().id;
        let op = self
            .db
            .get_pg_operation(&id)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("operation {id} not found")))?;
        Ok(Response::new(pb::GetPostgresqlOperationResponse {
            operation: Some(op_to_proto(&op)),
        }))
    }

    pub(super) async fn pg_list_operations(
        &self,
        request: Request<pb::ListPostgresqlOperationsRequest>,
    ) -> Result<Response<pb::ListPostgresqlOperationsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ReadOnly)?;
        let name = cluster_name(&request.into_inner().cluster_name)?;
        let operations = self
            .db
            .list_pg_operations(&name)
            .map_err(internal)?
            .iter()
            .map(op_to_proto)
            .collect();
        Ok(Response::new(pb::ListPostgresqlOperationsResponse {
            operations,
        }))
    }

    pub(super) async fn pg_cancel_operation(
        &self,
        request: Request<pb::GetPostgresqlOperationRequest>,
    ) -> Result<Response<pb::GetPostgresqlOperationResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let id = request.into_inner().id;
        let op = self
            .db
            .get_pg_operation(&id)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("operation {id} not found")))?;
        if op.state != "running" {
            return Err(Status::failed_precondition(format!(
                "operation is {}",
                op.state
            )));
        }
        self.db
            .update_pg_operation(&id, "cancelled", &op.steps_json, "cancelled by operator")
            .map_err(internal)?;
        let _ = self
            .db
            .finish_pg_operation_on_cluster(&op.cluster_name, None, None);
        let _ = self
            .db
            .set_pg_cluster_phase(&op.cluster_name, "degraded", "operation cancelled");
        self.record_audit(
            &actor,
            "CancelPostgresqlOperation",
            &format!("postgresql_cluster/{}", op.cluster_name),
            id.clone(),
        );
        let op = self
            .db
            .get_pg_operation(&id)
            .map_err(internal)?
            .unwrap_or(op);
        Ok(Response::new(pb::GetPostgresqlOperationResponse {
            operation: Some(op_to_proto(&op)),
        }))
    }

    pub(super) async fn pg_credentials(
        &self,
        request: Request<pb::PostgresqlClusterNameRequest>,
    ) -> Result<Response<pb::GetPostgresqlClusterCredentialsResponse>, Status> {
        self.require_operator(&request, OperatorRole::ClusterAdmin)?;
        let actor = Self::audit_actor(&request);
        let name = cluster_name(&request.into_inner().name)?;
        let row = self.pg_row(&name)?;
        let spec = engine::spec_from_json(&row.spec_json).map_err(Status::internal)?;
        let secrets = self
            .db
            .get_pg_secrets(&name)
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("cluster secrets are missing"))?;
        let roles: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&secrets.role_passwords_json).unwrap_or_default();
        self.record_audit(
            &actor,
            "GetPostgresqlClusterCredentials",
            &format!("postgresql_cluster/{name}"),
            "",
        );
        let (writer, reader) = connection_uris(&spec);
        Ok(Response::new(pb::GetPostgresqlClusterCredentialsResponse {
            superuser: "postgres".into(),
            superuser_password: secrets.superuser_password,
            writer_uri: writer,
            reader_uri: reader,
            ca_pem: secrets.ca_cert_pem,
            roles: roles
                .into_iter()
                .map(|(role, password)| pb::PostgresqlRoleCredential { role, password })
                .collect(),
        }))
    }
}

/// Writer and reader libpq URIs. No user or password: clients add their own.
pub(crate) fn connection_uris(spec: &ClusterSpec) -> (String, String) {
    let port = spec.ports.postgres;
    let ssl = if spec.tls.disabled {
        ""
    } else {
        "&sslmode=verify-full"
    };
    let hosts = |members: Vec<&spec::Member>| {
        members
            .iter()
            .map(|m| format!("{}:{port}", m.advertise_address))
            .collect::<Vec<_>>()
            .join(",")
    };
    let writer = match spec.vip.as_ref().and_then(|v| spec::parse_cidr(&v.address)) {
        Some((vip, _)) => {
            format!("postgresql://{vip}:{port}/postgres?target_session_attrs=read-write{ssl}")
        }
        None => format!(
            "postgresql://{}/postgres?target_session_attrs=read-write{ssl}",
            hosts(spec.postgres_members().collect())
        ),
    };
    let readers: Vec<&spec::Member> = spec
        .postgres_members()
        .filter(|m| !m.no_load_balance)
        .collect();
    let reader = format!(
        "postgresql://{}/postgres?target_session_attrs=prefer-standby&load_balance_hosts=random{ssl}",
        hosts(readers)
    );
    (writer, reader)
}

fn kcore_node_preflight(
    name: &str,
    spec: &ClusterSpec,
    node_id: &str,
    voters: &[String],
    changed: &[String],
) -> crate::node_proto::CheckPostgresqlClusterPreflightRequest {
    crate::node_proto::CheckPostgresqlClusterPreflightRequest {
        cluster_name: name.to_string(),
        spec_json: serde_json::to_string(spec).unwrap_or_default(),
        node_id: node_id.to_string(),
        etcd_voters: voters.to_vec(),
        changed_parameters: changed.to_vec(),
        upgrade_package: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ClusterSpec {
        let (_, mut spec) = spec::parse_manifest(include_str!(
            "../../../../../examples/postgresql-cluster/orders.yaml"
        ))
        .unwrap();
        spec::normalize(&mut spec);
        spec
    }

    #[test]
    fn proto_round_trip_keeps_the_spec() {
        let spec = sample();
        let back = spec_from_proto(spec_to_proto(&spec));
        assert_eq!(back, spec);
    }

    #[test]
    fn uris_use_the_vip_for_writes_and_standbys_for_reads() {
        let spec = sample();
        let (writer, reader) = connection_uris(&spec);
        assert!(writer.starts_with("postgresql://192.168.40.150:5432/"));
        assert!(writer.contains("target_session_attrs=read-write"));
        assert!(reader.contains("192.168.40.106:5432"));
        assert!(reader.contains("load_balance_hosts=random"));
    }

    #[test]
    fn dropped_lists_roles_and_databases_that_left() {
        let old = sample();
        let mut new = old.clone();
        new.roles.clear();
        new.databases.clear();
        let (roles, dbs) = dropped(&old, &new);
        assert_eq!(roles, vec!["app".to_string()]);
        assert_eq!(dbs, vec!["orders".to_string()]);
    }
}
