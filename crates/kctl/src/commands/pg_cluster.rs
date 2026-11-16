//! `kctl pg-cluster` subcommands for PostgresqlCluster.
//!
//! Manifests are parsed with `kcore_postgresql::spec::parse_manifest`, the same
//! parser the controller's tests use; kctl converts to the proto spec and
//! never interprets fields itself.

use anyhow::{bail, Context, Result};
use kcore_postgresql::spec::{self, ClusterSpec};

use crate::client::{self, controller_proto as pb};
use crate::config::ConnectionInfo;

fn volume(v: &Option<spec::HostVolume>) -> Option<pb::PostgresqlHostVolume> {
    v.as_ref().map(|v| pb::PostgresqlHostVolume {
        name: v.name.clone(),
        storage_class: v.storage_class.clone(),
        volume_group: v.volume_group.clone(),
        size_bytes: v.size_bytes,
        fs_type: v.fs_type.clone(),
    })
}

pub fn member_to_proto(m: &spec::Member) -> pb::PostgresqlMemberSpec {
    pb::PostgresqlMemberSpec {
        node_id: m.node_id.clone(),
        advertise_address: m.advertise_address.clone(),
        iface: m.iface.clone(),
        role: m.role.clone(),
        witness: m.witness,
        data_volume: volume(&m.data_volume),
        wal_volume: volume(&m.wal_volume),
        etcd_volume: volume(&m.etcd_volume),
        no_failover: m.no_failover,
        no_load_balance: m.no_load_balance,
        failover_priority: m.failover_priority as i32,
        datacenter: m.datacenter.clone(),
    }
}

/// Unset fields stay zero so the controller fills its defaults.
pub fn spec_to_proto(s: &ClusterSpec) -> pb::PostgresqlClusterSpec {
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

pub fn read_manifest(file: &str) -> Result<(String, ClusterSpec)> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    spec::parse_manifest(&text).map_err(|e| anyhow::anyhow!("{file}: {e}"))
}

/// One member mapping (the shape of an entry under `spec.members`).
pub fn read_member(file: &str) -> Result<spec::Member> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    serde_yaml::from_str(&text).with_context(|| format!("{file}: not a PostgreSQL member"))
}

pub fn plan_lines(name: &str, plan: &pb::PlanPostgresqlClusterResponse) -> Vec<String> {
    let mut out = vec![format!("PostgresqlCluster {name}: {}", plan.action)];
    if !plan.changed_fields.is_empty() {
        out.push(format!("Changed: {}", plan.changed_fields.join(", ")));
    }
    for p in &plan.changed_parameters {
        out.push(format!("  parameter {p}"));
    }
    out.push(format!("Replication: {}", plan.rpo));
    out.push("Members:".into());
    for n in &plan.node_plans {
        out.push(format!(
            "  {:<14} {:<14} {}{}",
            n.node_id,
            n.roles,
            if n.preflight_passed { "ok" } else { "BLOCKED" },
            if n.resolved_version.is_empty() {
                String::new()
            } else {
                format!(" (running {})", n.resolved_version)
            }
        ));
    }
    if !plan.rollout_steps.is_empty() {
        out.push("Steps:".into());
        for (i, s) in plan.rollout_steps.iter().enumerate() {
            out.push(format!("  {:>2}. {s}", i + 1));
        }
    }
    for w in &plan.warnings {
        out.push(format!("WARNING: {w}"));
    }
    for b in &plan.blocking_issues {
        out.push(format!("BLOCKING: {b}"));
    }
    out
}

async fn plan(
    info: &ConnectionInfo,
    name: &str,
    spec: &ClusterSpec,
) -> Result<pb::PlanPostgresqlClusterResponse> {
    let mut c = client::controller_client(info).await?;
    Ok(c.plan_postgresql_cluster(pb::PlanPostgresqlClusterRequest {
        name: name.to_string(),
        spec: Some(spec_to_proto(spec)),
    })
    .await
    .context("PlanPostgresqlCluster")?
    .into_inner())
}

pub async fn plan_from_file(info: &ConnectionInfo, file: &str) -> Result<()> {
    let (name, spec) = read_manifest(file)?;
    let plan = plan(info, &name, &spec).await?;
    for line in plan_lines(&name, &plan) {
        println!("{line}");
    }
    if !plan.blocking_issues.is_empty() {
        bail!("plan has {} blocking issue(s)", plan.blocking_issues.len());
    }
    Ok(())
}

fn print_op(resp: &pb::PostgresqlClusterOperationResponse) {
    if !resp.message.is_empty() {
        println!("{}", resp.message);
    }
    if !resp.operation_id.is_empty() {
        println!("Operation: {}", resp.operation_id);
        println!(
            "Follow it with: kctl pg-cluster operation {}",
            resp.operation_id
        );
    }
}

/// Create the cluster, or update it when it exists. Refused without a clean
/// plan.
pub async fn apply_from_file(
    info: &ConnectionInfo,
    file: &str,
    confirm: bool,
    force: bool,
) -> Result<()> {
    let (name, spec) = read_manifest(file)?;
    let plan = plan(info, &name, &spec).await?;
    for line in plan_lines(&name, &plan) {
        println!("{line}");
    }
    if !plan.blocking_issues.is_empty() {
        bail!(
            "cannot apply: {} blocking issue(s)",
            plan.blocking_issues.len()
        );
    }
    if plan.action == "unchanged" {
        println!("Nothing to do.");
        return Ok(());
    }
    if !confirm {
        bail!("re-run with --confirm to apply this plan");
    }
    let mut c = client::controller_client(info).await?;
    let resp = if plan.action == "create" {
        c.create_postgresql_cluster(pb::CreatePostgresqlClusterRequest {
            name: name.clone(),
            spec: Some(spec_to_proto(&spec)),
            confirm,
        })
        .await
        .context("CreatePostgresqlCluster")?
    } else {
        c.update_postgresql_cluster(pb::UpdatePostgresqlClusterRequest {
            name: name.clone(),
            spec: Some(spec_to_proto(&spec)),
            confirm,
            force,
        })
        .await
        .context("UpdatePostgresqlCluster")?
    }
    .into_inner();
    print_op(&resp);
    Ok(())
}

pub async fn list(info: &ConnectionInfo) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let resp = c
        .list_postgresql_clusters(pb::ListPostgresqlClustersRequest {})
        .await
        .context("ListPostgresqlClusters")?
        .into_inner();
    if resp.clusters.is_empty() {
        println!("No PostgresqlClusters");
        return Ok(());
    }
    println!(
        "{:<20} {:<15} {:<12} {:<9} {:<10} {:<6}",
        "NAME", "PHASE", "LEADER", "MEMBERS", "VERSION", "GEN"
    );
    for cl in resp.clusters {
        let status = cl.status.unwrap_or_default();
        println!(
            "{:<20} {:<15} {:<12} {:<9} {:<10} {:<6}",
            cl.name,
            status.phase,
            status.leader_node_id,
            format!("{}/{}", status.streaming_standbys + 1, status.total_members),
            status.running_version,
            cl.generation
        );
    }
    Ok(())
}

pub async fn get(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let cl = c
        .get_postgresql_cluster(pb::GetPostgresqlClusterRequest { name: name.into() })
        .await
        .context("GetPostgresqlCluster")?
        .into_inner()
        .cluster
        .ok_or_else(|| anyhow::anyhow!("PostgresqlCluster '{name}' not found"))?;
    let spec = cl.spec.unwrap_or_default();
    let status = cl.status.unwrap_or_default();
    println!("PostgresqlCluster: {}", cl.name);
    println!(
        "Generation: {} (observed {})",
        cl.generation, status.observed_generation
    );
    println!(
        "Phase: {}{}",
        status.phase,
        if status.message.is_empty() {
            String::new()
        } else {
            format!(" ({})", status.message)
        }
    );
    println!(
        "Package: {}  running {}",
        spec.package, status.running_version
    );
    if !status.previous_package.is_empty() {
        println!("Upgrade pending finalize: from {}", status.previous_package);
    }
    println!(
        "Leader: {}  timeline {}",
        status.leader_node_id, status.timeline
    );
    println!("Paused: {}", status.paused);
    if !status.active_operation_id.is_empty() {
        println!("Active operation: {}", status.active_operation_id);
    }
    if let Some(r) = spec.replication {
        println!("Replication: {} (sync count {})", r.mode, r.sync_count);
    }
    if let Some(v) = spec.vip {
        println!("Writer VIP: {}", v.address);
    }
    println!("Members:");
    for m in spec.members {
        println!(
            "  {:<14} {:<16} {}{}{}",
            m.node_id,
            m.advertise_address,
            m.iface,
            if m.witness { " witness" } else { "" },
            if m.no_failover { " nofailover" } else { "" }
        );
    }
    Ok(())
}

pub async fn health(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let h = c
        .get_postgresql_cluster_health(pb::PostgresqlClusterNameRequest {
            name: name.into(),
            confirm: false,
        })
        .await
        .context("GetPostgresqlClusterHealth")?
        .into_inner();
    println!(
        "{}: leader {} timeline {}  etcd {}/{}{}{}",
        h.overall_health,
        h.leader_node_id,
        h.timeline,
        h.etcd_members_up,
        h.etcd_members_total,
        if h.etcd_quorum { "" } else { " NO QUORUM" },
        if h.paused { "  PAUSED" } else { "" }
    );
    println!(
        "{:<14} {:<16} {:<12} {:>12} {:<8} {:<5} {:<5}",
        "NODE", "ROLE", "STATE", "LAG", "VERSION", "ETCD", "VIP"
    );
    for m in h.members {
        println!(
            "{:<14} {:<16} {:<12} {:>12} {:<8} {:<5} {:<5}{}",
            m.node_id,
            if m.role.is_empty() { "-" } else { &m.role },
            if m.state.is_empty() { "-" } else { &m.state },
            if m.lag_bytes < 0 {
                "unknown".to_string()
            } else {
                m.lag_bytes.to_string()
            },
            m.version,
            if m.etcd_leader {
                "lead"
            } else if m.etcd_active {
                "up"
            } else {
                "down"
            },
            if m.holds_vip { "yes" } else { "" },
            if m.message.is_empty() {
                String::new()
            } else {
                format!("  {}", m.message)
            }
        );
    }
    Ok(())
}

pub async fn events(info: &ConnectionInfo, name: &str, limit: i32) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let resp = c
        .list_postgresql_cluster_events(pb::ListPostgresqlClusterEventsRequest {
            name: name.into(),
            limit,
        })
        .await
        .context("ListPostgresqlClusterEvents")?
        .into_inner();
    for e in resp.events {
        println!(
            "{}  {:<10} {} -> {}  timeline {}  {}",
            e.created_at, e.kind, e.old_leader, e.new_leader, e.timeline, e.message
        );
    }
    Ok(())
}

pub async fn credentials(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let r = c
        .get_postgresql_cluster_credentials(pb::PostgresqlClusterNameRequest {
            name: name.into(),
            confirm: false,
        })
        .await
        .context("GetPostgresqlClusterCredentials")?
        .into_inner();
    println!("Superuser: {}", r.superuser);
    println!("Superuser password: {}", r.superuser_password);
    println!("Writer: {}", r.writer_uri);
    println!("Reader: {}", r.reader_uri);
    for role in r.roles {
        println!("Role {} password: {}", role.role, role.password);
    }
    println!(
        "CA certificate (save as root.crt, use sslrootcert=):\n{}",
        r.ca_pem
    );
    Ok(())
}

pub fn operation_lines(op: &pb::PostgresqlOperation) -> Vec<String> {
    let mut out = vec![format!(
        "Operation {} {} on {}: {}{}",
        op.id,
        op.kind,
        op.cluster_name,
        op.state,
        if op.message.is_empty() {
            String::new()
        } else {
            format!(" ({})", op.message)
        }
    )];
    for s in &op.steps {
        let mut line = format!("  {:>2}. [{:<9}] {}", s.index + 1, s.state, s.kind);
        if !s.node_id.is_empty() {
            line.push_str(&format!(" {}", s.node_id));
        }
        if !s.unit.is_empty() {
            line.push_str(&format!(" [{}]", s.unit));
        }
        if !s.detail.is_empty() {
            line.push_str(&format!(" ({})", s.detail));
        }
        if !s.message.is_empty() {
            line.push_str(&format!(" - {}", s.message));
        }
        out.push(line);
    }
    out
}

pub async fn operation(info: &ConnectionInfo, id: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let op = c
        .get_postgresql_operation(pb::GetPostgresqlOperationRequest { id: id.into() })
        .await
        .context("GetPostgresqlOperation")?
        .into_inner()
        .operation
        .unwrap_or_default();
    for line in operation_lines(&op) {
        println!("{line}");
    }
    Ok(())
}

pub async fn operations(info: &ConnectionInfo, name: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let resp = c
        .list_postgresql_operations(pb::ListPostgresqlOperationsRequest {
            cluster_name: name.into(),
        })
        .await
        .context("ListPostgresqlOperations")?
        .into_inner();
    for op in resp.operations {
        println!(
            "{}  {:<17} {:<10} {}",
            op.created_at, op.kind, op.state, op.id
        );
    }
    Ok(())
}

pub async fn cancel(info: &ConnectionInfo, id: &str) -> Result<()> {
    let mut c = client::controller_client(info).await?;
    let op = c
        .cancel_postgresql_operation(pb::GetPostgresqlOperationRequest { id: id.into() })
        .await
        .context("CancelPostgresqlOperation")?
        .into_inner()
        .operation
        .unwrap_or_default();
    println!("Operation {} is {}", op.id, op.state);
    Ok(())
}

/// The mutating verbs that only name the cluster.
pub enum Verb {
    Switchover {
        candidate: String,
    },
    Failover {
        candidate: String,
        force: bool,
    },
    Maintenance {
        enabled: bool,
    },
    Reinit {
        node: String,
    },
    Upgrade {
        package: String,
        force: bool,
    },
    Finalize,
    Rollback,
    Delete {
        force: bool,
        retain_data: bool,
    },
    Scale {
        add: Option<String>,
        remove: Vec<String>,
        force: bool,
    },
    Replace {
        old: String,
        file: String,
        force: bool,
    },
}

pub async fn run(info: &ConnectionInfo, name: &str, verb: Verb, confirm: bool) -> Result<()> {
    if !confirm {
        bail!("this changes the cluster; re-run with --confirm");
    }
    let mut c = client::controller_client(info).await?;
    let name = name.to_string();
    let resp = match verb {
        Verb::Switchover { candidate } => c
            .switchover_postgresql_cluster(pb::SwitchoverPostgresqlClusterRequest {
                name,
                candidate,
                confirm,
            })
            .await
            .context("SwitchoverPostgresqlCluster")?,
        Verb::Failover { candidate, force } => c
            .failover_postgresql_cluster(pb::FailoverPostgresqlClusterRequest {
                name,
                candidate,
                confirm,
                force,
            })
            .await
            .context("FailoverPostgresqlCluster")?,
        Verb::Maintenance { enabled } => c
            .set_postgresql_maintenance(pb::SetPostgresqlMaintenanceRequest {
                name,
                enabled,
                confirm,
            })
            .await
            .context("SetPostgresqlMaintenance")?,
        Verb::Reinit { node } => c
            .reinit_postgresql_member(pb::ReinitPostgresqlMemberRequest {
                name,
                node_id: node,
                confirm,
            })
            .await
            .context("ReinitPostgresqlMember")?,
        Verb::Upgrade { package, force } => c
            .upgrade_postgresql_cluster(pb::UpgradePostgresqlClusterRequest {
                name,
                package,
                confirm,
                force,
            })
            .await
            .context("UpgradePostgresqlCluster")?,
        Verb::Finalize => c
            .finalize_postgresql_upgrade(pb::PostgresqlClusterNameRequest { name, confirm })
            .await
            .context("FinalizePostgresqlUpgrade")?,
        Verb::Rollback => c
            .rollback_postgresql_upgrade(pb::PostgresqlClusterNameRequest { name, confirm })
            .await
            .context("RollbackPostgresqlUpgrade")?,
        Verb::Delete { force, retain_data } => c
            .delete_postgresql_cluster(pb::DeletePostgresqlClusterRequest {
                name,
                confirm,
                force,
                retain_data,
            })
            .await
            .context("DeletePostgresqlCluster")?,
        Verb::Scale { add, remove, force } => {
            let add_members = match add {
                Some(file) => vec![member_to_proto(&read_member(&file)?)],
                None => Vec::new(),
            };
            c.scale_postgresql_cluster(pb::ScalePostgresqlClusterRequest {
                name,
                add_members,
                remove_members: remove,
                confirm,
                force,
            })
            .await
            .context("ScalePostgresqlCluster")?
        }
        Verb::Replace { old, file, force } => c
            .replace_postgresql_member(pb::ReplacePostgresqlMemberRequest {
                name,
                old_node_id: old,
                new_member: Some(member_to_proto(&read_member(&file)?)),
                confirm,
                force,
            })
            .await
            .context("ReplacePostgresqlMember")?,
    }
    .into_inner();
    print_op(&resp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/postgresql-cluster/orders.yaml"
    );

    #[test]
    fn example_manifest_converts_to_proto() {
        let (name, spec) = read_manifest(EXAMPLE).unwrap();
        assert_eq!(name, "orders");
        let proto = spec_to_proto(&spec);
        assert_eq!(proto.members.len(), 3);
        assert_eq!(proto.members[0].role, "primary");
        assert_eq!(proto.vip.unwrap().address, "192.168.40.150/24");
        assert_eq!(proto.parameters["max_connections"], "300");
        // Unset numbers stay zero for the controller's defaults.
        assert_eq!(proto.ports.unwrap().postgres, 0);
    }

    #[test]
    fn a_member_file_parses_in_both_cases() {
        let dir = std::env::temp_dir().join(format!("kctl-pg-member-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.yaml");
        std::fs::write(
            &path,
            "nodeId: node-110\nadvertiseAddress: 10.0.0.10\niface: eth0\n",
        )
        .unwrap();
        let m = read_member(path.to_str().unwrap()).unwrap();
        assert_eq!(m.node_id, "node-110");
        std::fs::write(&path, "nodeId: node-110\nbogus: 1\n").unwrap();
        assert!(read_member(path.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plan_lines_show_blocking_and_steps() {
        let plan = pb::PlanPostgresqlClusterResponse {
            action: "create".into(),
            blocking_issues: vec!["node-105: port 5432 is in use".into()],
            rollout_steps: vec!["apply-nix node-105 (etcd-only)".into()],
            rpo: "RPO 0".into(),
            ..Default::default()
        };
        let lines = plan_lines("orders", &plan);
        assert_eq!(lines[0], "PostgresqlCluster orders: create");
        assert!(lines.iter().any(|l| l.contains("1. apply-nix node-105")));
        assert!(lines.last().unwrap().starts_with("BLOCKING"));
    }

    #[test]
    fn operation_lines_number_steps() {
        let op = pb::PostgresqlOperation {
            id: "op1".into(),
            kind: "update".into(),
            cluster_name: "orders".into(),
            state: "running".into(),
            steps: vec![pb::PostgresqlOperationStep {
                index: 0,
                kind: "restart-member".into(),
                node_id: "node-106".into(),
                unit: "patroni".into(),
                state: "running".into(),
                message: "waiting".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let lines = operation_lines(&op);
        assert_eq!(
            lines[1],
            "   1. [running  ] restart-member node-106 [patroni] - waiting"
        );
    }
}
