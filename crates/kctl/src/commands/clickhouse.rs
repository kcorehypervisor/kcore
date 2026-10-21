//! `kctl clickhouse` — ClickHouse cluster lifecycle.

use std::fs;

use anyhow::{Context, Result};
use clap::Subcommand;
use kcore_clickhouse::spec::{
    ClusterSpec, DictionarySpec, FunctionSpec, HostVolume, KeeperSpec, NamedCollectionSpec, Ports,
    ProfileSpec, QuotaSpec, ReplicaSpec, ShardSpec, SystemLogs, UserSpec,
};
use kcore_clickhouse::{normalize, parse_manifest};

use crate::client::{self, controller_proto};
use crate::config::ConnectionInfo;

#[derive(Debug, Clone, Subcommand)]
pub enum Action {
    /// Validate a manifest and print the rollout. Does not change the cluster.
    Plan {
        #[arg(short = 'f', long)]
        file: String,
    },
    /// Create the cluster, or update it when the name already exists.
    Apply {
        #[arg(short = 'f', long)]
        file: String,
    },
    Get {
        name: String,
    },
    List,
    Health {
        name: String,
    },
    Credentials {
        name: String,
    },
    Operations {
        name: String,
    },
    Operation {
        id: String,
    },
    Cancel {
        id: String,
    },
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        retain_data: bool,
    },
    Upgrade {
        name: String,
        #[arg(long)]
        package: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        intermediate_package: Option<String>,
    },
    Scale {
        name: String,
        /// shard/replica
        #[arg(long = "remove")]
        remove: Vec<String>,
        /// node=<id>,address=<ip>,iface=<name>,shard=<name>,replica=<name>
        #[arg(long = "add")]
        add: Vec<String>,
        #[arg(long)]
        force: bool,
    },
    ReplaceReplica {
        name: String,
        #[arg(long)]
        shard: String,
        #[arg(long = "old")]
        old_replica: String,
        /// node=<id>,address=<ip>,iface=<name>,replica=<name>
        #[arg(long = "new")]
        new_replica: String,
        #[arg(long)]
        force: bool,
    },
    RestoreReplica {
        name: String,
        #[arg(long)]
        shard: String,
        #[arg(long)]
        replica: String,
        #[arg(long)]
        database: Option<String>,
        #[arg(long)]
        force: bool,
    },
    Backup {
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        location: String,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        backup_name: Option<String>,
        #[arg(long)]
        skip_access: bool,
    },
    Restore {
        name: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        location: String,
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        backup_name: String,
        #[arg(long)]
        allow_non_empty: bool,
        #[arg(long)]
        skip_access: bool,
    },
    Backups {
        name: String,
        #[arg(long, default_value = "disk")]
        kind: String,
        #[arg(long, default_value = "")]
        location: String,
    },
}

pub async fn run(info: &ConnectionInfo, action: &Action) -> Result<()> {
    let action = action.clone();
    match action {
        Action::Plan { file } => {
            let (name, spec) = load_spec(&file)?;
            let mut client = client::controller_client(info).await?;
            let resp = client
                .plan_clickhouse_cluster(controller_proto::PlanClickhouseClusterRequest {
                    name,
                    spec: Some(to_proto(&spec)),
                })
                .await?
                .into_inner();
            println!("action: {}", resp.action);
            print_lines("blocking", &resp.blocking_issues);
            print_lines("warnings", &resp.warnings);
            print_lines("changed", &resp.changed_fields);
            print_lines("steps", &resp.rollout_steps);
        }
        Action::Apply { file } => {
            let (name, spec) = load_spec(&file)?;
            let mut client = client::controller_client(info).await?;
            let exists = client
                .get_clickhouse_cluster(controller_proto::GetClickhouseClusterRequest {
                    name: name.clone(),
                })
                .await
                .is_ok();
            let spec = Some(to_proto(&spec));
            if exists {
                let resp = client
                    .update_clickhouse_cluster(controller_proto::UpdateClickhouseClusterRequest {
                        name,
                        spec,
                        confirm: true,
                    })
                    .await?
                    .into_inner();
                println!("operation {}", resp.operation_id);
            } else {
                let resp = client
                    .create_clickhouse_cluster(controller_proto::CreateClickhouseClusterRequest {
                        name,
                        spec,
                        confirm: true,
                    })
                    .await?
                    .into_inner();
                println!("operation {}", resp.operation_id);
            }
        }
        Action::Get { name } => {
            let mut client = client::controller_client(info).await?;
            let cluster = client
                .get_clickhouse_cluster(controller_proto::GetClickhouseClusterRequest { name })
                .await?
                .into_inner()
                .cluster
                .context("cluster missing")?;
            print_cluster(&cluster);
        }
        Action::List => {
            let mut client = client::controller_client(info).await?;
            let clusters = client
                .list_clickhouse_clusters(controller_proto::ListClickhouseClustersRequest {})
                .await?
                .into_inner()
                .clusters;
            for cluster in clusters {
                print_cluster(&cluster);
            }
        }
        Action::Health { name } => {
            let mut client = client::controller_client(info).await?;
            let health = client
                .get_clickhouse_cluster_health(
                    controller_proto::GetClickhouseClusterHealthRequest { name },
                )
                .await?
                .into_inner();
            println!(
                "{} quorum={} keepers={}/{}",
                health.overall_health,
                health.keeper_quorum,
                health.keeper_members_up,
                health.keeper_members_total
            );
            for replica in health.replicas {
                println!(
                    "replica {} {}/{} active={} readonly={}",
                    replica.node_id,
                    replica.shard_name,
                    replica.replica_name,
                    replica.unit_active,
                    replica.readonly_tables
                );
            }
            for keeper in health.keepers {
                println!(
                    "keeper {} role={} active={}",
                    keeper.node_id, keeper.role, keeper.unit_active
                );
            }
        }
        Action::Credentials { name } => {
            let mut client = client::controller_client(info).await?;
            let creds = client
                .get_clickhouse_cluster_credentials(
                    controller_proto::GetClickhouseClusterCredentialsRequest { name },
                )
                .await?
                .into_inner();
            println!("user {}", creds.default_user);
            println!("password {}", creds.default_password);
            for endpoint in creds.endpoints {
                println!("endpoint {endpoint}");
            }
        }
        Action::Operations { name } => {
            let mut client = client::controller_client(info).await?;
            let ops = client
                .list_clickhouse_operations(controller_proto::ListClickhouseOperationsRequest {
                    cluster_name: name,
                })
                .await?
                .into_inner()
                .operations;
            for op in ops {
                println!("{} {} {} {}", op.id, op.kind, op.state, op.cluster_name);
            }
        }
        Action::Operation { id } => {
            let mut client = client::controller_client(info).await?;
            let op = client
                .get_clickhouse_operation(controller_proto::GetClickhouseOperationRequest { id })
                .await?
                .into_inner()
                .operation
                .context("operation missing")?;
            println!("{} {} {}", op.id, op.kind, op.state);
            for step in op.steps {
                println!(
                    "  {} {} {} {}",
                    step.index, step.kind, step.node_id, step.state
                );
            }
        }
        Action::Cancel { id } => {
            let mut client = client::controller_client(info).await?;
            let op = client
                .cancel_clickhouse_operation(controller_proto::CancelClickhouseOperationRequest {
                    id,
                })
                .await?
                .into_inner()
                .operation
                .context("operation missing")?;
            println!("{} {}", op.id, op.state);
        }
        Action::Delete {
            name,
            force,
            retain_data,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .delete_clickhouse_cluster(controller_proto::DeleteClickhouseClusterRequest {
                    name,
                    confirm: true,
                    force,
                    retain_data,
                })
                .await?
                .into_inner();
            println!("{} operation {}", resp.message, resp.operation_id);
        }
        Action::Upgrade {
            name,
            package,
            force,
            intermediate_package,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .upgrade_clickhouse_cluster(controller_proto::UpgradeClickhouseClusterRequest {
                    name,
                    package,
                    force,
                    confirm: true,
                    intermediate_package: intermediate_package.unwrap_or_default(),
                })
                .await?
                .into_inner();
            println!("operation {}", resp.operation_id);
        }
        Action::Scale {
            name,
            remove,
            add,
            force,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .scale_clickhouse_cluster(controller_proto::ScaleClickhouseClusterRequest {
                    name,
                    add_replicas: add
                        .iter()
                        .map(|raw| parse_add(raw))
                        .collect::<Result<Vec<_>>>()?,
                    remove_replicas: remove,
                    confirm: true,
                    force,
                    ..Default::default()
                })
                .await?
                .into_inner();
            println!("operation {}", resp.operation_id);
        }
        Action::ReplaceReplica {
            name,
            shard,
            old_replica,
            new_replica,
            force,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .replace_clickhouse_replica(controller_proto::ReplaceClickhouseReplicaRequest {
                    name,
                    shard_name: shard,
                    old_replica_name: old_replica,
                    new_replica: Some(parse_replica(&new_replica)?),
                    confirm: true,
                    force,
                })
                .await?
                .into_inner();
            println!("operation {}", resp.operation_id);
        }
        Action::RestoreReplica {
            name,
            shard,
            replica,
            database,
            force,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .restore_clickhouse_replica(controller_proto::RestoreClickhouseReplicaRequest {
                    name,
                    shard_name: shard,
                    replica_name: replica,
                    database: database.unwrap_or_default(),
                    confirm: true,
                    force,
                })
                .await?
                .into_inner();
            println!("operation {}", resp.operation_id);
        }
        Action::Backup {
            name,
            kind,
            location,
            bucket,
            backup_name,
            skip_access,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .backup_clickhouse_cluster(controller_proto::BackupClickhouseClusterRequest {
                    name,
                    target: Some(backup_target(&kind, &location, bucket.as_deref())),
                    backup_name: backup_name.unwrap_or_default(),
                    confirm: true,
                    skip_access,
                    ..Default::default()
                })
                .await?
                .into_inner();
            println!(
                "backup {} operation {}",
                resp.backup_name, resp.operation_id
            );
        }
        Action::Restore {
            name,
            kind,
            location,
            bucket,
            backup_name,
            allow_non_empty,
            skip_access,
        } => {
            let mut client = client::controller_client(info).await?;
            let resp = client
                .restore_clickhouse_cluster(controller_proto::RestoreClickhouseClusterRequest {
                    name,
                    target: Some(backup_target(&kind, &location, bucket.as_deref())),
                    backup_name,
                    allow_non_empty_tables: allow_non_empty,
                    confirm: true,
                    skip_access,
                    ..Default::default()
                })
                .await?
                .into_inner();
            println!("operation {}", resp.operation_id);
        }
        Action::Backups {
            name,
            kind,
            location,
        } => {
            let mut client = client::controller_client(info).await?;
            let backups = client
                .list_clickhouse_backups(controller_proto::ListClickhouseBackupsRequest {
                    name,
                    target: Some(backup_target(&kind, &location, None)),
                })
                .await?
                .into_inner()
                .backups;
            for backup in backups {
                println!(
                    "{} {} {}",
                    backup.backup_name, backup.status, backup.total_size_bytes
                );
            }
        }
    }
    Ok(())
}

fn load_spec(path: &str) -> Result<(String, ClusterSpec)> {
    let text = fs::read_to_string(path).with_context(|| format!("read {path}"))?;
    let (name, mut spec) = parse_manifest(&text).map_err(anyhow::Error::msg)?;
    normalize(&mut spec);
    Ok((name, spec))
}

fn print_lines(label: &str, lines: &[String]) {
    for line in lines {
        println!("{label}: {line}");
    }
}

fn print_cluster(cluster: &controller_proto::ClickhouseCluster) {
    let phase = cluster
        .status
        .as_ref()
        .map(|s| s.phase.as_str())
        .unwrap_or("");
    println!(
        "{} generation={} phase={phase}",
        cluster.name, cluster.generation
    );
}

fn backup_target(
    kind: &str,
    location: &str,
    bucket: Option<&str>,
) -> controller_proto::ClickhouseBackupTarget {
    controller_proto::ClickhouseBackupTarget {
        kind: kind.to_string(),
        location: location.to_string(),
        s3_bucket: bucket.unwrap_or("").to_string(),
        s3_credential_ref: String::new(),
    }
}

fn parse_add(raw: &str) -> Result<controller_proto::ClickhouseScaleReplica> {
    let fields = parse_fields(raw)?;
    Ok(controller_proto::ClickhouseScaleReplica {
        shard_name: fields.get("shard").cloned().unwrap_or_else(|| "1".into()),
        replica: Some(replica_from_fields(&fields)?),
    })
}

fn parse_replica(raw: &str) -> Result<controller_proto::ClickhouseReplicaSpec> {
    replica_from_fields(&parse_fields(raw)?)
}

fn replica_from_fields(
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<controller_proto::ClickhouseReplicaSpec> {
    Ok(controller_proto::ClickhouseReplicaSpec {
        node_id: fields.get("node").cloned().context("node=")?,
        replica_name: fields.get("replica").cloned().unwrap_or_default(),
        advertise_address: fields.get("address").cloned().context("address=")?,
        iface: fields
            .get("iface")
            .cloned()
            .unwrap_or_else(|| "enp6s0".into()),
        ..Default::default()
    })
}

fn parse_fields(raw: &str) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    for part in raw.split(',') {
        let (key, value) = part.split_once('=').context("expected key=value")?;
        out.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(out)
}

fn to_proto(spec: &ClusterSpec) -> controller_proto::ClickhouseClusterSpec {
    controller_proto::ClickhouseClusterSpec {
        package: spec.package.clone(),
        shards: spec.shards.iter().map(shard_to_proto).collect(),
        keepers: spec.keepers.iter().map(keeper_to_proto).collect(),
        users: spec.users.iter().map(user_to_proto).collect(),
        profiles: spec.profiles.iter().map(profile_to_proto).collect(),
        databases: spec
            .databases
            .iter()
            .map(|db| controller_proto::ClickhouseDatabaseSpec {
                name: db.name.clone(),
            })
            .collect(),
        server_settings: spec.server_settings.clone().into_iter().collect(),
        tls: Some(controller_proto::ClickhouseTlsConfig {
            disabled: spec.tls.disabled,
            rotate_before_days: spec.tls.rotate_before_days,
        }),
        client_networks: spec.client_networks.clone(),
        max_replica_delay_seconds: spec.max_replica_delay_seconds,
        step_timeout_seconds: spec.step_timeout_seconds,
        ports: Some(ports_to_proto(&spec.ports)),
        max_server_memory_ratio: spec.max_server_memory_ratio,
        quotas: spec.quotas.iter().map(quota_to_proto).collect(),
        dictionaries: spec.dictionaries.iter().map(dictionary_to_proto).collect(),
        named_collections: spec
            .named_collections
            .iter()
            .map(collection_to_proto)
            .collect(),
        functions: spec.functions.iter().map(function_to_proto).collect(),
        system_logs: Some(logs_to_proto(&spec.system_logs)),
        metrics_public: spec.metrics_public,
    }
}

fn shard_to_proto(shard: &ShardSpec) -> controller_proto::ClickhouseShardSpec {
    controller_proto::ClickhouseShardSpec {
        shard_name: shard.shard_name.clone(),
        replicas: shard.replicas.iter().map(replica_to_proto).collect(),
        weight: shard.weight,
    }
}

fn replica_to_proto(replica: &ReplicaSpec) -> controller_proto::ClickhouseReplicaSpec {
    controller_proto::ClickhouseReplicaSpec {
        node_id: replica.node_id.clone(),
        replica_name: replica.replica_name.clone(),
        advertise_address: replica.advertise_address.clone(),
        iface: replica.iface.clone(),
        data_volume: replica.data_volume.as_ref().map(volume_to_proto),
        datacenter: replica.datacenter.clone(),
        read_only: replica.read_only,
    }
}

fn keeper_to_proto(keeper: &KeeperSpec) -> controller_proto::ClickhouseKeeperSpec {
    controller_proto::ClickhouseKeeperSpec {
        node_id: keeper.node_id.clone(),
        server_id: keeper.server_id,
        advertise_address: keeper.advertise_address.clone(),
        iface: keeper.iface.clone(),
        data_volume: keeper.data_volume.as_ref().map(volume_to_proto),
        datacenter: keeper.datacenter.clone(),
        learner: keeper.learner,
    }
}

fn volume_to_proto(volume: &HostVolume) -> controller_proto::ClickhouseHostVolume {
    controller_proto::ClickhouseHostVolume {
        name: volume.name.clone(),
        storage_class: volume.storage_class.clone(),
        volume_group: volume.volume_group.clone(),
        size_bytes: volume.size_bytes,
        fs_type: volume.fs_type.clone(),
    }
}

fn user_to_proto(user: &UserSpec) -> controller_proto::ClickhouseUserSpec {
    controller_proto::ClickhouseUserSpec {
        name: user.name.clone(),
        password_sha256_hex: user.password_sha256_hex.clone(),
        networks: user.networks.clone(),
        profile: user.profile.clone(),
        quota: user.quota.clone(),
        access_management: user.access_management,
    }
}

fn profile_to_proto(profile: &ProfileSpec) -> controller_proto::ClickhouseSettingsProfile {
    controller_proto::ClickhouseSettingsProfile {
        name: profile.name.clone(),
        settings: profile.settings.clone().into_iter().collect(),
    }
}

fn quota_to_proto(quota: &QuotaSpec) -> controller_proto::ClickhouseQuotaSpec {
    controller_proto::ClickhouseQuotaSpec {
        name: quota.name.clone(),
        intervals: quota
            .intervals
            .iter()
            .map(|interval| controller_proto::ClickhouseQuotaInterval {
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

fn dictionary_to_proto(dict: &DictionarySpec) -> controller_proto::ClickhouseDictionarySpec {
    controller_proto::ClickhouseDictionarySpec {
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
            .map(|attr| controller_proto::ClickhouseDictionaryAttribute {
                name: attr.name.clone(),
                r#type: attr.r#type.clone(),
                nullable: attr.nullable,
            })
            .collect(),
        database: dict.database.clone(),
    }
}

fn collection_to_proto(
    collection: &NamedCollectionSpec,
) -> controller_proto::ClickhouseNamedCollectionSpec {
    controller_proto::ClickhouseNamedCollectionSpec {
        name: collection.name.clone(),
        keys: collection.keys.clone().into_iter().collect(),
        secret_refs: collection.secret_refs.clone().into_iter().collect(),
    }
}

fn function_to_proto(function: &FunctionSpec) -> controller_proto::ClickhouseFunctionSpec {
    controller_proto::ClickhouseFunctionSpec {
        name: function.name.clone(),
        parameters: function.parameters.clone(),
        expression: function.expression.clone(),
    }
}

fn logs_to_proto(logs: &SystemLogs) -> controller_proto::ClickhouseSystemLogs {
    controller_proto::ClickhouseSystemLogs {
        query_log_days: logs.query_log_days,
        part_log_days: logs.part_log_days,
        trace_log_days: logs.trace_log_days,
        text_log_days: logs.text_log_days,
    }
}

fn ports_to_proto(ports: &Ports) -> controller_proto::ClickhousePorts {
    controller_proto::ClickhousePorts {
        tcp: ports.tcp,
        tcp_secure: ports.tcp_secure,
        http: ports.http,
        https: ports.https,
        interserver: ports.interserver,
        keeper: ports.keeper,
        keeper_raft: ports.keeper_raft,
        prometheus: ports.prometheus,
    }
}
