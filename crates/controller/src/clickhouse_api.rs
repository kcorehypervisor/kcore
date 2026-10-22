//! ClickHouse cluster RPCs. Handlers persist an operation; the reconciler runs it.

use kcore_clickhouse::{
    diff, normalize, plan_rollout, split_issues, validate, ClusterSpec, OpKind, PlanAction,
    PlanDiff, RolloutRequest, Step,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::clickhouse_convert::{
    members_from_spec, spec_from_json, spec_from_proto, spec_json, spec_to_proto,
};
use crate::clickhouse_db::{CertRow, ClusterRow, OperationRow, SecretRow};
use crate::clickhouse_engine::OpContext;
use crate::clickhouse_pki;
use crate::controller_proto::{
    self, ClickhouseCluster, ClickhouseClusterSpec, ClickhouseClusterStatus,
};
use crate::db::Database;

pub async fn plan(
    db: &Database,
    request: Request<controller_proto::PlanClickhouseClusterRequest>,
) -> Result<Response<controller_proto::PlanClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    let (spec, blocking, warnings) = prepare(&req.name, req.spec.unwrap_or_default())?;
    let existing = db.get_clickhouse_cluster(&req.name).map_err(internal)?;
    let (action, changed, extra_block, extra_warn, steps) = match existing.as_ref() {
        None => {
            let steps = plan_rollout(&rollout(
                None,
                &spec,
                OpKind::Create,
                &PlanDiff {
                    action: PlanAction::Create,
                    changed_fields: vec![],
                    blocking: vec![],
                    warnings: vec![],
                },
            ));
            ("create".to_string(), vec![], Vec::new(), Vec::new(), steps)
        }
        Some(row) => {
            let before = spec_from_json(&row.spec_json).map_err(invalid)?;
            let planned = diff(&before, &spec);
            let kind = kind_of(&planned.action);
            let steps = plan_rollout(&rollout(Some(&before), &spec, kind, &planned));
            let action = action_name(&planned.action);
            (
                action,
                planned.changed_fields,
                planned.blocking,
                planned.warnings,
                steps,
            )
        }
    };
    let mut blocking = blocking;
    blocking.extend(extra_block);
    let member_ids = members_from_spec(&req.name, &spec)
        .into_iter()
        .map(|member| member.node_id)
        .collect::<Vec<_>>();
    blocking.extend(
        db.clickhouse_node_conflicts(&req.name, &member_ids)
            .map_err(internal)?,
    );
    let mut warnings = warnings;
    warnings.extend(extra_warn);
    let preview = members_from_spec(&req.name, &spec)
        .first()
        .and_then(|member| kcore_clickhouse::render_node(&req.name, 1, &spec, &member.node_id).ok())
        .unwrap_or_default();
    let node_plans = members_from_spec(&req.name, &spec)
        .into_iter()
        .map(|member| controller_proto::ClickhouseNodePlan {
            node_id: member.node_id,
            roles: match (member.server, member.keeper) {
                (true, true) => "server+keeper".into(),
                (true, false) => "server".into(),
                _ => "keeper".into(),
            },
            preflight_passed: blocking.is_empty(),
            issues: vec![],
            nix_action: if existing.is_none() {
                "build"
            } else {
                "rebuild"
            }
            .into(),
        })
        .collect();
    Ok(Response::new(
        controller_proto::PlanClickhouseClusterResponse {
            action,
            blocking_issues: blocking,
            warnings,
            changed_fields: changed,
            node_plans,
            rollout_steps: steps.iter().map(step_line).collect(),
            rendered_nix_preview: preview,
        },
    ))
}

pub async fn create(
    db: &Database,
    request: Request<controller_proto::CreateClickhouseClusterRequest>,
) -> Result<Response<controller_proto::CreateClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    if db
        .get_clickhouse_cluster(&req.name)
        .map_err(internal)?
        .is_some()
    {
        return Err(Status::already_exists(format!(
            "clickhouse cluster {} exists",
            req.name
        )));
    }
    let (spec, blocking, _) = prepare(&req.name, req.spec.unwrap_or_default())?;
    if !blocking.is_empty() {
        return Err(invalid(blocking.join("; ")));
    }
    require_nodes(db, &spec)?;
    reject_shared_nodes(db, &req.name, &spec)?;
    let secrets = generate_secrets(&req.name, &spec)?;
    let id = persist_new(
        db,
        &req.name,
        &spec,
        "create",
        OpKind::Create,
        None,
        &OpContext::default(),
        false,
        &secrets,
    )?;
    Ok(Response::new(
        controller_proto::CreateClickhouseClusterResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn update(
    db: &Database,
    request: Request<controller_proto::UpdateClickhouseClusterRequest>,
) -> Result<Response<controller_proto::UpdateClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let (spec, blocking, _) = prepare(&req.name, req.spec.unwrap_or_default())?;
    if !blocking.is_empty() {
        return Err(invalid(blocking.join("; ")));
    }
    let before = load_spec(db, &req.name)?;
    let planned = diff(&before, &spec);
    if matches!(planned.action, PlanAction::Upgrade) {
        return Err(invalid(
            "package changes go through UpgradeClickhouseCluster",
        ));
    }
    if !planned.blocking.is_empty() {
        return Err(invalid(planned.blocking.join("; ")));
    }
    reject_shared_nodes(db, &req.name, &spec)?;
    let id = persist_generation(
        db,
        &req.name,
        &spec,
        "update",
        OpKind::Update,
        Some(&before),
        &planned,
        &OpContext::default(),
        false,
    )?;
    Ok(Response::new(
        controller_proto::UpdateClickhouseClusterResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn get(
    db: &Database,
    request: Request<controller_proto::GetClickhouseClusterRequest>,
) -> Result<Response<controller_proto::GetClickhouseClusterResponse>, Status> {
    let name = request.into_inner().name;
    Ok(Response::new(
        controller_proto::GetClickhouseClusterResponse {
            cluster: Some(load_cluster(db, &name)?),
        },
    ))
}

pub async fn list(
    db: &Database,
    _request: Request<controller_proto::ListClickhouseClustersRequest>,
) -> Result<Response<controller_proto::ListClickhouseClustersResponse>, Status> {
    let clusters = db
        .list_clickhouse_clusters()
        .map_err(internal)?
        .into_iter()
        .map(|row| cluster_message(db, row))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Response::new(
        controller_proto::ListClickhouseClustersResponse { clusters },
    ))
}

pub async fn delete(
    db: &Database,
    request: Request<controller_proto::DeleteClickhouseClusterRequest>,
) -> Result<Response<controller_proto::DeleteClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let spec = load_spec(db, &req.name)?;
    let ctx = OpContext {
        retain_data: req.retain_data,
        ..OpContext::default()
    };
    let id = persist_generation(
        db,
        &req.name,
        &spec,
        "delete",
        OpKind::Delete,
        Some(&spec),
        &empty_diff(),
        &ctx,
        req.force,
    )?;
    Ok(Response::new(
        controller_proto::DeleteClickhouseClusterResponse {
            message: format!("deleting {}", req.name),
            operation_id: id,
        },
    ))
}

pub async fn scale(
    db: &Database,
    request: Request<controller_proto::ScaleClickhouseClusterRequest>,
) -> Result<Response<controller_proto::ScaleClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let before = load_spec(db, &req.name)?;
    let mut after = before.clone();
    let mut removed = Vec::new();
    for shard in req.add_shards {
        after.shards.push(
            crate::clickhouse_convert::spec_from_proto(ClickhouseClusterSpec {
                shards: vec![shard],
                ..ClickhouseClusterSpec::default()
            })
            .shards
            .remove(0),
        );
    }
    for add in req.add_replicas {
        let Some(replica) = add.replica else {
            return Err(invalid("addReplicas needs a replica"));
        };
        let shard = after
            .shards
            .iter_mut()
            .find(|shard| shard.shard_name == add.shard_name);
        let Some(shard) = shard else {
            return Err(invalid(format!("shard {} does not exist", add.shard_name)));
        };
        shard.replicas.push(
            spec_from_proto(ClickhouseClusterSpec {
                shards: vec![controller_proto::ClickhouseShardSpec {
                    replicas: vec![replica],
                    ..Default::default()
                }],
                ..ClickhouseClusterSpec::default()
            })
            .shards
            .remove(0)
            .replicas
            .remove(0),
        );
    }
    for name in &req.remove_shards {
        let Some(index) = after
            .shards
            .iter()
            .position(|shard| shard.shard_name == *name)
        else {
            return Err(invalid(format!("shard {name} does not exist")));
        };
        if !req.force {
            return Err(Status::failed_precondition(format!(
                "removing shard {name} needs force"
            )));
        }
        let shard = after.shards.remove(index);
        for replica in shard.replicas {
            removed.push((
                shard.shard_name.clone(),
                replica.replica_name,
                replica.node_id,
            ));
        }
    }
    for token in &req.remove_replicas {
        let Some((shard_name, replica_name)) = token.split_once('/') else {
            return Err(invalid(format!(
                "removeReplicas entry {token} must be shard/replica"
            )));
        };
        let Some(shard) = after
            .shards
            .iter_mut()
            .find(|shard| shard.shard_name == shard_name)
        else {
            return Err(invalid(format!("shard {shard_name} does not exist")));
        };
        if shard.replicas.len() == 1 && !req.force {
            return Err(Status::failed_precondition(format!(
                "last replica of shard {shard_name} needs force"
            )));
        }
        let Some(index) = shard
            .replicas
            .iter()
            .position(|replica| replica.replica_name == replica_name)
        else {
            return Err(Status::not_found(format!("replica {token} not found")));
        };
        let replica = shard.replicas.remove(index);
        removed.push((
            shard_name.to_string(),
            replica.replica_name,
            replica.node_id,
        ));
    }
    normalize(&mut after);
    let (blocking, _) = split_issues(&validate(&after));
    if !blocking.is_empty() {
        return Err(invalid(blocking.join("; ")));
    }
    require_nodes(db, &after)?;
    reject_shared_nodes(db, &req.name, &after)?;
    let planned = diff(&before, &after);
    if !planned.blocking.is_empty() {
        return Err(invalid(planned.blocking.join("; ")));
    }
    let ctx = OpContext {
        allow_non_empty: req.force,
        ..OpContext::default()
    };
    let mut request = rollout(Some(&before), &after, OpKind::Scale, &planned);
    request.removed_replicas = removed;
    let id = persist_with_request(db, &req.name, &after, "scale", &request, &ctx, req.force)?;
    Ok(Response::new(
        controller_proto::ScaleClickhouseClusterResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn replace(
    db: &Database,
    request: Request<controller_proto::ReplaceClickhouseReplicaRequest>,
) -> Result<Response<controller_proto::ReplaceClickhouseReplicaResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let before = load_spec(db, &req.name)?;
    let mut after = before.clone();
    let new_replica = req
        .new_replica
        .ok_or_else(|| invalid("newReplica is required"))?;
    let mut removed = None;
    for shard in &mut after.shards {
        if shard.shard_name != req.shard_name {
            continue;
        }
        let Some(index) = shard
            .replicas
            .iter()
            .position(|replica| replica.replica_name == req.old_replica_name)
        else {
            continue;
        };
        let old = shard.replicas.remove(index);
        removed = Some((shard.shard_name.clone(), old.replica_name, old.node_id));
        shard.replicas.push(
            spec_from_proto(ClickhouseClusterSpec {
                shards: vec![controller_proto::ClickhouseShardSpec {
                    replicas: vec![new_replica.clone()],
                    ..Default::default()
                }],
                ..ClickhouseClusterSpec::default()
            })
            .shards
            .remove(0)
            .replicas
            .remove(0),
        );
    }
    let Some(removed) = removed else {
        return Err(Status::not_found("replica not found"));
    };
    normalize(&mut after);
    let (blocking, _) = split_issues(&validate(&after));
    if !blocking.is_empty() {
        return Err(invalid(blocking.join("; ")));
    }
    require_nodes(db, &after)?;
    reject_shared_nodes(db, &req.name, &after)?;
    let planned = diff(&before, &after);
    let mut roll = rollout(Some(&before), &after, OpKind::Replace, &planned);
    roll.removed_replicas = vec![removed];
    let id = persist_with_request(
        db,
        &req.name,
        &after,
        "replace",
        &roll,
        &OpContext::default(),
        req.force,
    )?;
    Ok(Response::new(
        controller_proto::ReplaceClickhouseReplicaResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn restore_replica(
    db: &Database,
    request: Request<controller_proto::RestoreClickhouseReplicaRequest>,
) -> Result<Response<controller_proto::RestoreClickhouseReplicaResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let spec = load_spec(db, &req.name)?;
    let node = spec
        .shards
        .iter()
        .flat_map(|shard| shard.replicas.iter().map(move |replica| (shard, replica)))
        .find(|(shard, replica)| {
            shard.shard_name == req.shard_name && replica.replica_name == req.replica_name
        })
        .map(|(_, replica)| replica.node_id.clone())
        .ok_or_else(|| Status::not_found("replica not found"))?;
    let members = db.list_clickhouse_members(&req.name).map_err(internal)?;
    if !req.force {
        let healthy = members
            .iter()
            .find(|member| member.node_id == node)
            .and_then(|member| {
                serde_json::from_str::<kcore_clickhouse::rollout::MemberHealth>(&member.health_json)
                    .ok()
            });
        if healthy.is_some_and(|health| health.keeper_session_ok && health.readonly_tables == 0) {
            return Err(Status::failed_precondition(
                "replica looks healthy; pass force to run SYSTEM RESTORE REPLICA anyway",
            ));
        }
    }
    let planned = empty_diff();
    let mut roll = rollout(Some(&spec), &spec, OpKind::RestoreReplica, &planned);
    roll.restore_node = node;
    let id = persist_with_request(
        db,
        &req.name,
        &spec,
        "restore-replica",
        &roll,
        &OpContext::default(),
        req.force,
    )?;
    Ok(Response::new(
        controller_proto::RestoreClickhouseReplicaResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn upgrade(
    db: &Database,
    request: Request<controller_proto::UpgradeClickhouseClusterRequest>,
) -> Result<Response<controller_proto::UpgradeClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    if req.package != "clickhouse" && req.package != "clickhouse-lts" {
        return Err(invalid("package must be clickhouse or clickhouse-lts"));
    }
    let before = load_spec(db, &req.name)?;
    if before.package == req.package && req.intermediate_package.is_empty() {
        return Err(invalid("package is unchanged"));
    }
    let row = db
        .get_clickhouse_cluster(&req.name)
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("cluster"))?;
    if !req.force && kcore_clickhouse::spec::version_is_lower(&row.observed_version, &req.package) {
        return Err(invalid("downgrade needs force"));
    }
    if let Err(err) = kcore_clickhouse::spec::keeper_window_issue(
        &row.observed_version,
        &req.package,
        if req.intermediate_package.is_empty() {
            None
        } else {
            Some(req.intermediate_package.as_str())
        },
    ) {
        return Err(invalid(err));
    }
    let mut after = before.clone();
    after.package = req.package;
    let planned = empty_diff();
    let mut roll = rollout(Some(&before), &after, OpKind::Upgrade, &planned);
    roll.intermediate = !req.intermediate_package.is_empty();
    let id = persist_with_request(
        db,
        &req.name,
        &after,
        "upgrade",
        &roll,
        &OpContext {
            intermediate: roll.intermediate,
            ..OpContext::default()
        },
        req.force,
    )?;
    Ok(Response::new(
        controller_proto::UpgradeClickhouseClusterResponse {
            cluster: Some(load_cluster(db, &req.name)?),
            operation_id: id,
        },
    ))
}

pub async fn health(
    db: &Database,
    request: Request<controller_proto::GetClickhouseClusterHealthRequest>,
) -> Result<Response<controller_proto::GetClickhouseClusterHealthResponse>, Status> {
    let name = request.into_inner().name;
    let spec = load_spec(db, &name)?;
    let members = db.list_clickhouse_members(&name).map_err(internal)?;
    let mut replicas = Vec::new();
    let mut keepers = Vec::new();
    for member in &members {
        let live: kcore_clickhouse::rollout::MemberHealth =
            serde_json::from_str(&member.health_json).unwrap_or_default();
        if member.server {
            replicas.push(controller_proto::ClickhouseReplicaHealth {
                node_id: member.node_id.clone(),
                shard_name: member.shard_name.clone(),
                replica_name: member.replica_name.clone(),
                unit_active: live.server_active,
                version: live.version.clone(),
                readonly_tables: live.readonly_tables,
                max_absolute_delay_seconds: live.max_delay_seconds,
                keeper_session_ok: live.keeper_session_ok,
                ..Default::default()
            });
        }
        if member.keeper {
            keepers.push(controller_proto::ClickhouseKeeperHealth {
                node_id: member.node_id.clone(),
                server_id: member.server_id,
                unit_active: live.keeper_active,
                role: live.keeper_role.clone(),
                version: live.version.clone(),
                ..Default::default()
            });
        }
    }
    let voting = spec.keepers.iter().filter(|k| !k.learner).count();
    let voting_up = keepers
        .iter()
        .filter(|k| {
            k.unit_active
                && !k.role.is_empty()
                && spec
                    .keepers
                    .iter()
                    .any(|spec_keeper| spec_keeper.node_id == k.node_id && !spec_keeper.learner)
        })
        .count();
    let leaders = keepers.iter().filter(|k| k.role == "leader").count();
    let quorum = leaders == 1 && voting_up > voting / 2;
    let mut shards_without_replica = Vec::new();
    for shard in &spec.shards {
        let writable = shard.replicas.iter().any(|replica| {
            !replica.read_only
                && replicas.iter().any(|health| {
                    health.node_id == replica.node_id
                        && health.unit_active
                        && health.readonly_tables == 0
                })
        });
        if !writable {
            shards_without_replica.push(shard.shard_name.clone());
        }
    }
    let keeper_down = spec.keepers.iter().any(|keeper| {
        !keeper.learner
            && keepers
                .iter()
                .any(|health| health.node_id == keeper.node_id && !health.unit_active)
    });
    let overall = if !shards_without_replica.is_empty() || !quorum {
        "critical"
    } else if replicas
        .iter()
        .any(|r| r.readonly_tables > 0 || !r.unit_active)
        || keeper_down
    {
        "degraded"
    } else {
        "healthy"
    };
    Ok(Response::new(
        controller_proto::GetClickhouseClusterHealthResponse {
            overall_health: overall.into(),
            keeper_quorum: quorum,
            keeper_members_up: voting_up as i32,
            keeper_members_total: voting as i32,
            shards_without_replica,
            replicas,
            keepers,
        },
    ))
}

pub async fn get_operation(
    db: &Database,
    request: Request<controller_proto::GetClickhouseOperationRequest>,
) -> Result<Response<controller_proto::GetClickhouseOperationResponse>, Status> {
    let id = request.into_inner().id;
    let row = db
        .get_clickhouse_operation(&id)
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("operation not found"))?;
    Ok(Response::new(
        controller_proto::GetClickhouseOperationResponse {
            operation: Some(operation_message(row)),
        },
    ))
}

pub async fn list_operations(
    db: &Database,
    request: Request<controller_proto::ListClickhouseOperationsRequest>,
) -> Result<Response<controller_proto::ListClickhouseOperationsResponse>, Status> {
    let name = request.into_inner().cluster_name;
    let operations = db
        .list_clickhouse_operations(&name)
        .map_err(internal)?
        .into_iter()
        .map(operation_message)
        .collect();
    Ok(Response::new(
        controller_proto::ListClickhouseOperationsResponse { operations },
    ))
}

pub async fn cancel(
    db: &Database,
    request: Request<controller_proto::CancelClickhouseOperationRequest>,
) -> Result<Response<controller_proto::CancelClickhouseOperationResponse>, Status> {
    let id = request.into_inner().id;
    let Some(mut row) = db.get_clickhouse_operation(&id).map_err(internal)? else {
        return Err(Status::not_found("operation not found"));
    };
    if row.state == "pending" || row.state == "running" {
        row.state = "cancelled".into();
        row.message = "cancelled before the next step".into();
        db.update_clickhouse_operation(&row).map_err(internal)?;
        if let Some(mut cluster) = db
            .get_clickhouse_cluster(&row.cluster_name)
            .map_err(internal)?
        {
            cluster.active_operation_id.clear();
            cluster.phase = "degraded".into();
            cluster.message = "operation cancelled".into();
            db.update_clickhouse_cluster(&cluster).map_err(internal)?;
        }
    }
    Ok(Response::new(
        controller_proto::CancelClickhouseOperationResponse {
            operation: Some(operation_message(row)),
        },
    ))
}

pub async fn credentials(
    db: &Database,
    request: Request<controller_proto::GetClickhouseClusterCredentialsRequest>,
) -> Result<Response<controller_proto::GetClickhouseClusterCredentialsResponse>, Status> {
    let name = request.into_inner().name;
    let spec = load_spec(db, &name)?;
    let secrets = db
        .get_clickhouse_secrets(&name)
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("secrets not found"))?;
    let port = if spec.tls.disabled {
        spec.ports.tcp
    } else {
        spec.ports.tcp_secure
    };
    let secure = if spec.tls.disabled { "0" } else { "1" };
    let mut endpoints = Vec::new();
    let mut metrics = Vec::new();
    for shard in &spec.shards {
        for replica in &shard.replicas {
            endpoints.push(format!(
                "clickhouse://default@{}:{port}?secure={secure}",
                replica.advertise_address
            ));
            metrics.push(format!(
                "https://{}:{}/clickhouse/metrics",
                replica.advertise_address, spec.ports.https
            ));
        }
    }
    Ok(Response::new(
        controller_proto::GetClickhouseClusterCredentialsResponse {
            default_user: "default".into(),
            default_password: secrets.default_password,
            endpoints,
            ca_pem: secrets.ca_cert_pem,
            metrics_user: "kcore-metrics".into(),
            metrics_password: secrets.metrics_password,
            metrics_urls: metrics,
        },
    ))
}

pub async fn backup(
    db: &Database,
    request: Request<controller_proto::BackupClickhouseClusterRequest>,
) -> Result<Response<controller_proto::BackupClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let spec = load_spec(db, &req.name)?;
    let target = req.target.unwrap_or_default();
    let backup_name = if req.backup_name.is_empty() {
        format!("{}-{}", req.name, uuid::Uuid::new_v4())
    } else {
        req.backup_name
    };
    let ctx = OpContext {
        backup_name: backup_name.clone(),
        skip_access: req.skip_access,
        target_kind: target.kind,
        target_location: target.location,
        target_bucket: target.s3_bucket,
        databases: if req.databases.is_empty() {
            spec.databases.iter().map(|db| db.name.clone()).collect()
        } else {
            req.databases
        },
        ..OpContext::default()
    };
    db.upsert_clickhouse_backup(
        &req.name,
        &backup_name,
        &serde_json::to_string(&ctx).unwrap_or_default(),
        "pending",
    )
    .map_err(internal)?;
    let planned = empty_diff();
    let mut roll = rollout(Some(&spec), &spec, OpKind::Backup, &planned);
    roll.backup_name = backup_name.clone();
    roll.skip_access = ctx.skip_access;
    let id = persist_with_request(db, &req.name, &spec, "backup", &roll, &ctx, false)?;
    Ok(Response::new(
        controller_proto::BackupClickhouseClusterResponse {
            operation_id: id,
            backup_name,
        },
    ))
}

pub async fn restore(
    db: &Database,
    request: Request<controller_proto::RestoreClickhouseClusterRequest>,
) -> Result<Response<controller_proto::RestoreClickhouseClusterResponse>, Status> {
    let req = request.into_inner();
    if !req.confirm {
        return Err(Status::failed_precondition("confirm is required"));
    }
    let spec = load_spec(db, &req.name)?;
    let target = req.target.unwrap_or_default();
    let ctx = OpContext {
        backup_name: req.backup_name.clone(),
        skip_access: req.skip_access,
        allow_non_empty: req.allow_non_empty_tables,
        target_kind: target.kind,
        target_location: target.location,
        target_bucket: target.s3_bucket,
        databases: req.databases,
        ..OpContext::default()
    };
    let planned = empty_diff();
    let mut roll = rollout(Some(&spec), &spec, OpKind::Restore, &planned);
    roll.backup_name = req.backup_name;
    roll.skip_access = ctx.skip_access;
    let id = persist_with_request(db, &req.name, &spec, "restore", &roll, &ctx, false)?;
    Ok(Response::new(
        controller_proto::RestoreClickhouseClusterResponse { operation_id: id },
    ))
}

pub async fn list_backups(
    db: &Database,
    request: Request<controller_proto::ListClickhouseBackupsRequest>,
) -> Result<Response<controller_proto::ListClickhouseBackupsResponse>, Status> {
    let name = request.into_inner().name;
    let backups = db
        .list_clickhouse_backups(&name)
        .map_err(internal)?
        .into_iter()
        .map(
            |(backup_name, status, _)| controller_proto::ClickhouseBackupInfo {
                backup_name,
                status,
                ..Default::default()
            },
        )
        .collect();
    Ok(Response::new(
        controller_proto::ListClickhouseBackupsResponse { backups },
    ))
}

pub(crate) fn prepare(
    name: &str,
    spec: ClickhouseClusterSpec,
) -> Result<(ClusterSpec, Vec<String>, Vec<String>), Status> {
    if name.trim().is_empty() {
        return Err(invalid("name is required"));
    }
    let mut spec = spec_from_proto(spec);
    normalize(&mut spec);
    let (blocking, warnings) = split_issues(&validate(&spec));
    Ok((spec, blocking, warnings))
}

pub(crate) fn reject_shared_nodes(
    db: &Database,
    name: &str,
    spec: &ClusterSpec,
) -> Result<(), Status> {
    let ids = members_from_spec(name, spec)
        .into_iter()
        .map(|member| member.node_id)
        .collect::<Vec<_>>();
    let conflicts = db.clickhouse_node_conflicts(name, &ids).map_err(internal)?;
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(Status::failed_precondition(conflicts.join("; ")))
    }
}

pub(crate) fn require_nodes(db: &Database, spec: &ClusterSpec) -> Result<(), Status> {
    let mut missing = Vec::new();
    for member in members_from_spec("", spec) {
        let known = db.get_node(&member.node_id).map_err(internal)?.is_some();
        if !known {
            missing.push(member.node_id);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Status::failed_precondition(format!(
            "nodes are not registered: {}",
            missing.join(", ")
        )))
    }
}

fn generate_secrets(
    cluster: &str,
    spec: &ClusterSpec,
) -> Result<(SecretRow, Vec<CertRow>), Status> {
    let ca = clickhouse_pki::issue_ca(cluster).map_err(internal)?;
    let mut certs = Vec::new();
    for member in members_from_spec(cluster, spec) {
        let hostname = db_hostname_fallback(&member.node_id);
        let issued = clickhouse_pki::issue_member(
            cluster,
            &member.node_id,
            &hostname,
            &member.advertise_address,
            &ca.cert_pem,
            &ca.key_pem,
        )
        .map_err(internal)?;
        certs.push(CertRow {
            node_id: member.node_id,
            cert_pem: issued.cert_pem,
            key_pem: issued.key_pem,
            not_after_unix: issued.not_after_unix,
        });
    }
    let default_password = random_token();
    let metrics_password = random_token();
    Ok((
        SecretRow {
            default_sha256: sha256_hex(&default_password),
            metrics_sha256: sha256_hex(&metrics_password),
            default_password,
            metrics_password,
            interserver_password: random_token(),
            keeper_password: random_token(),
            cluster_secret: random_token(),
            ca_cert_pem: ca.cert_pem,
            ca_key_pem: ca.key_pem,
        },
        certs,
    ))
}

fn db_hostname_fallback(node_id: &str) -> String {
    node_id.to_string()
}

fn persist_new(
    db: &Database,
    name: &str,
    spec: &ClusterSpec,
    kind: &str,
    op: OpKind,
    before: Option<&ClusterSpec>,
    ctx: &OpContext,
    force: bool,
    secrets: &(SecretRow, Vec<CertRow>),
) -> Result<String, Status> {
    let steps = plan_rollout(&rollout(before, spec, op, &empty_diff()));
    let id = Uuid::new_v4().to_string();
    db.insert_clickhouse_cluster(&ClusterRow {
        name: name.into(),
        generation: 1,
        phase: "bootstrapping".into(),
        message: String::new(),
        spec_json: spec_json(spec).map_err(invalid)?,
        observed_version: String::new(),
        active_operation_id: id.clone(),
    })
    .map_err(internal)?;
    if let Err(err) = assign_new_cluster(db, name, spec, secrets, &id, kind, &steps, ctx, force) {
        let _ = db.delete_clickhouse_cluster(name);
        return Err(err);
    }
    Ok(id)
}

fn assign_new_cluster(
    db: &Database,
    name: &str,
    spec: &ClusterSpec,
    secrets: &(SecretRow, Vec<CertRow>),
    id: &str,
    kind: &str,
    steps: &[Step],
    ctx: &OpContext,
    force: bool,
) -> Result<(), Status> {
    db.replace_clickhouse_members(name, &members_from_spec(name, spec))
        .map_err(internal)?;
    db.upsert_clickhouse_secrets(name, &secrets.0)
        .map_err(internal)?;
    db.replace_clickhouse_certs(name, &secrets.1)
        .map_err(internal)?;
    insert_op(db, name, id, kind, steps, ctx, force)
}

fn persist_generation(
    db: &Database,
    name: &str,
    spec: &ClusterSpec,
    kind: &str,
    op: OpKind,
    before: Option<&ClusterSpec>,
    planned: &PlanDiff,
    ctx: &OpContext,
    force: bool,
) -> Result<String, Status> {
    let request = rollout(before, spec, op, planned);
    persist_with_request(db, name, spec, kind, &request, ctx, force)
}

fn persist_with_request(
    db: &Database,
    name: &str,
    spec: &ClusterSpec,
    kind: &str,
    request: &RolloutRequest<'_>,
    ctx: &OpContext,
    force: bool,
) -> Result<String, Status> {
    if let Some(active) = db.active_clickhouse_operation(name).map_err(internal)? {
        return Err(Status::failed_precondition(format!(
            "operation {} is still active",
            active.id
        )));
    }
    let steps = plan_rollout(request);
    let id = Uuid::new_v4().to_string();
    let Some(mut row) = db.get_clickhouse_cluster(name).map_err(internal)? else {
        return Err(Status::not_found("cluster not found"));
    };
    row.generation += 1;
    row.phase = phase_for(kind).into();
    row.spec_json = spec_json(spec).map_err(invalid)?;
    row.active_operation_id = id.clone();
    db.update_clickhouse_cluster(&row).map_err(internal)?;
    db.replace_clickhouse_members(name, &members_from_spec(name, spec))
        .map_err(internal)?;
    insert_op(db, name, &id, kind, &steps, ctx, force)?;
    Ok(id)
}

fn insert_op(
    db: &Database,
    cluster: &str,
    id: &str,
    kind: &str,
    steps: &[Step],
    ctx: &OpContext,
    force: bool,
) -> Result<(), Status> {
    db.insert_clickhouse_operation(&OperationRow {
        id: id.into(),
        cluster_name: cluster.into(),
        kind: kind.into(),
        state: "pending".into(),
        steps_json: serde_json::to_string(steps).map_err(|e| internal(e.to_string()))?,
        context_json: serde_json::to_string(ctx).unwrap_or_else(|_| "{}".into()),
        message: String::new(),
        force,
    })
    .map_err(internal)
}

fn load_spec(db: &Database, name: &str) -> Result<ClusterSpec, Status> {
    let row = db
        .get_clickhouse_cluster(name)
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("cluster not found"))?;
    spec_from_json(&row.spec_json).map_err(invalid)
}

fn load_cluster(db: &Database, name: &str) -> Result<ClickhouseCluster, Status> {
    let row = db
        .get_clickhouse_cluster(name)
        .map_err(internal)?
        .ok_or_else(|| Status::not_found("cluster not found"))?;
    cluster_message(db, row)
}

fn cluster_message(db: &Database, row: ClusterRow) -> Result<ClickhouseCluster, Status> {
    let spec = spec_from_json(&row.spec_json).map_err(invalid)?;
    let members = db.list_clickhouse_members(&row.name).map_err(internal)?;
    Ok(ClickhouseCluster {
        name: row.name,
        generation: row.generation,
        spec: Some(spec_to_proto(&spec)),
        status: Some(ClickhouseClusterStatus {
            phase: row.phase,
            message: row.message,
            observed_generation: row.generation,
            ready_replicas: members.iter().filter(|m| m.server).count() as i32,
            total_replicas: spec.shards.iter().map(|s| s.replicas.len()).sum::<usize>() as i32,
            ready_keepers: members.iter().filter(|m| m.keeper).count() as i32,
            total_keepers: spec.keepers.len() as i32,
            running_version: row.observed_version,
            active_operation_id: row.active_operation_id,
            ..Default::default()
        }),
    })
}

fn operation_message(row: OperationRow) -> controller_proto::ClickhouseOperation {
    let steps: Vec<Step> = serde_json::from_str(&row.steps_json).unwrap_or_default();
    controller_proto::ClickhouseOperation {
        id: row.id,
        cluster_name: row.cluster_name,
        kind: row.kind,
        state: row.state,
        steps: steps
            .into_iter()
            .enumerate()
            .map(|(index, step)| {
                let message = [step.unit, step.detail, step.message]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                controller_proto::ClickhouseOperationStep {
                    index: index as i32,
                    kind: step.kind,
                    node_id: step.node_id,
                    state: step.state,
                    message,
                    ..Default::default()
                }
            })
            .collect(),
        ..Default::default()
    }
}

fn rollout<'a>(
    before: Option<&'a ClusterSpec>,
    after: &'a ClusterSpec,
    kind: OpKind,
    planned: &'a PlanDiff,
) -> RolloutRequest<'a> {
    RolloutRequest {
        before,
        after,
        kind,
        health: &[],
        diff: Some(planned),
        intermediate: false,
        retain_data: false,
        skip_access: false,
        backup_name: String::new(),
        restore_node: String::new(),
        removed_replicas: vec![],
    }
}

fn empty_diff() -> PlanDiff {
    PlanDiff {
        action: PlanAction::Unchanged,
        changed_fields: vec![],
        blocking: vec![],
        warnings: vec![],
    }
}

fn kind_of(action: &PlanAction) -> OpKind {
    match action {
        PlanAction::Create => OpKind::Create,
        PlanAction::Upgrade => OpKind::Upgrade,
        PlanAction::Scale => OpKind::Scale,
        PlanAction::Replace => OpKind::Replace,
        PlanAction::Update { .. } | PlanAction::Unchanged => OpKind::Update,
    }
}

fn action_name(action: &PlanAction) -> String {
    match action {
        PlanAction::Create => "create",
        PlanAction::Unchanged => "unchanged",
        PlanAction::Update { restart: false } => "update",
        PlanAction::Update { restart: true } => "update",
        PlanAction::Upgrade => "upgrade",
        PlanAction::Scale => "scale",
        PlanAction::Replace => "replace",
    }
    .into()
}

fn phase_for(kind: &str) -> &'static str {
    match kind {
        "create" => "bootstrapping",
        "upgrade" => "upgrading",
        "scale" | "replace" => "scaling",
        "delete" => "deleting",
        _ => "updating",
    }
}

fn step_line(step: &Step) -> String {
    format!(
        "{} {} {} {}",
        step.kind, step.node_id, step.unit, step.detail
    )
}

fn random_token() -> String {
    let mut bytes = [0u8; 24];
    let _ = aws_lc_rs::rand::SystemRandom::new().fill(&mut bytes);
    const ALPH: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    bytes
        .iter()
        .map(|b| ALPH[(*b as usize) % ALPH.len()] as char)
        .collect()
}

fn sha256_hex(value: &str) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, value.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest.as_ref() {
        out.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        out.push(char::from(b"0123456789abcdef"[(byte & 0xf) as usize]));
    }
    out
}

fn internal(err: impl ToString) -> Status {
    Status::internal(err.to_string())
}

fn invalid(err: impl ToString) -> Status {
    Status::invalid_argument(err.to_string())
}

use aws_lc_rs::rand::SecureRandom;

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;

    use crate::db::NodeRow;

    fn node(id: &str) -> NodeRow {
        NodeRow {
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
        }
    }

    fn manifest(name: &str, node_id: &str, address: &str) -> String {
        format!(
            "kind: ClickhouseCluster\nmetadata:\n  name: {name}\nspec:\n  shards:\n    - replicas:\n        - {{nodeId: {node_id}, advertiseAddress: {address}, iface: enp6s0}}\n  keepers:\n    - {{nodeId: {node_id}, advertiseAddress: {address}, iface: enp6s0}}\n"
        )
    }

    async fn create_named(db: &Database, yaml: &str) -> Result<String, Status> {
        let (name, mut spec) = kcore_clickhouse::parse_manifest(yaml).map_err(invalid)?;
        kcore_clickhouse::normalize(&mut spec);
        let response = create(
            db,
            Request::new(controller_proto::CreateClickhouseClusterRequest {
                name: name.clone(),
                spec: Some(spec_to_proto(&spec)),
                confirm: true,
            }),
        )
        .await?;
        Ok(response.into_inner().operation_id)
    }

    #[tokio::test]
    async fn two_clusters_use_different_nodes_and_a_shared_node_is_refused() {
        let db = Database::open(":memory:").unwrap();
        for id in ["node-105", "node-106", "node-107"] {
            db.upsert_node(&node(id)).unwrap();
        }
        create_named(&db, &manifest("analytics", "node-105", "192.168.40.105"))
            .await
            .unwrap();
        create_named(&db, &manifest("billing", "node-106", "192.168.40.106"))
            .await
            .unwrap();
        let names = db
            .list_clickhouse_clusters()
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["analytics".to_string(), "billing".to_string()]);

        let err = create_named(&db, &manifest("stolen", "node-105", "192.168.40.105"))
            .await
            .expect_err("a node has one clickhouse unit");
        assert_eq!(err.code(), Code::FailedPrecondition);
        assert!(err.message().contains("node-105"));
        assert!(err.message().contains("analytics"));
        assert!(db.get_clickhouse_cluster("stolen").unwrap().is_none());

        let (name, mut spec) =
            kcore_clickhouse::parse_manifest(&manifest("stolen", "node-105", "192.168.40.105"))
                .unwrap();
        kcore_clickhouse::normalize(&mut spec);
        let plan = plan(
            &db,
            Request::new(controller_proto::PlanClickhouseClusterRequest {
                name,
                spec: Some(spec_to_proto(&spec)),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(plan
            .blocking_issues
            .iter()
            .any(|issue| issue.contains("already runs ClickHouse cluster analytics")));

        let mut member = db.list_clickhouse_members("analytics").unwrap().remove(0);
        member.health_json = r#"{"server_active":true}"#.into();
        db.replace_clickhouse_members("analytics", &[member.clone()])
            .unwrap();
        member.health_json.clear();
        db.replace_clickhouse_members("analytics", &[member])
            .unwrap();
        assert!(db.list_clickhouse_members("analytics").unwrap()[0]
            .health_json
            .contains("server_active"));

        let mut extra = db.list_clickhouse_members("analytics").unwrap().remove(0);
        extra.node_id = "node-107".into();
        extra.health_json.clear();
        db.replace_clickhouse_members("analytics", &[extra])
            .unwrap();
        assert_eq!(db.list_clickhouse_members("analytics").unwrap().len(), 2);
        db.prune_clickhouse_members("analytics", &["node-105".into()])
            .unwrap();
        let left = db.list_clickhouse_members("analytics").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].node_id, "node-105");
    }
}
