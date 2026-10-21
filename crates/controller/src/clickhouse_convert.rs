//! Proto spec ↔ the pure ClickHouse model. The shared crate does not depend on tonic.

use std::collections::BTreeMap;

use kcore_clickhouse::spec::{
    ClusterSpec, DatabaseSpec, DictionaryAttribute, DictionarySpec, FunctionSpec, HostVolume,
    KeeperSpec, NamedCollectionSpec, Ports, ProfileSpec, QuotaInterval, QuotaSpec, ReplicaSpec,
    ShardSpec, SystemLogs, TlsConfig, UserSpec,
};

use crate::controller_proto;

pub fn spec_from_proto(spec: controller_proto::ClickhouseClusterSpec) -> ClusterSpec {
    ClusterSpec {
        package: spec.package,
        shards: spec.shards.into_iter().map(shard_from_proto).collect(),
        keepers: spec.keepers.into_iter().map(keeper_from_proto).collect(),
        users: spec.users.into_iter().map(user_from_proto).collect(),
        profiles: spec.profiles.into_iter().map(profile_from_proto).collect(),
        databases: spec
            .databases
            .into_iter()
            .map(|db| DatabaseSpec { name: db.name })
            .collect(),
        server_settings: spec.server_settings.into_iter().collect(),
        tls: spec.tls.map(tls_from_proto).unwrap_or_default(),
        client_networks: spec.client_networks,
        max_replica_delay_seconds: spec.max_replica_delay_seconds,
        step_timeout_seconds: spec.step_timeout_seconds,
        ports: spec.ports.map(ports_from_proto).unwrap_or_default(),
        max_server_memory_ratio: spec.max_server_memory_ratio,
        quotas: spec.quotas.into_iter().map(quota_from_proto).collect(),
        dictionaries: spec
            .dictionaries
            .into_iter()
            .map(dictionary_from_proto)
            .collect(),
        named_collections: spec
            .named_collections
            .into_iter()
            .map(collection_from_proto)
            .collect(),
        functions: spec
            .functions
            .into_iter()
            .map(function_from_proto)
            .collect(),
        system_logs: spec.system_logs.map(logs_from_proto).unwrap_or_default(),
        metrics_public: spec.metrics_public,
    }
}

pub fn spec_to_proto(spec: &ClusterSpec) -> controller_proto::ClickhouseClusterSpec {
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

fn shard_from_proto(shard: controller_proto::ClickhouseShardSpec) -> ShardSpec {
    ShardSpec {
        shard_name: shard.shard_name,
        replicas: shard.replicas.into_iter().map(replica_from_proto).collect(),
        weight: shard.weight,
    }
}

fn replica_from_proto(replica: controller_proto::ClickhouseReplicaSpec) -> ReplicaSpec {
    ReplicaSpec {
        node_id: replica.node_id,
        replica_name: replica.replica_name,
        advertise_address: replica.advertise_address,
        iface: replica.iface,
        data_volume: replica.data_volume.and_then(volume_from_proto),
        datacenter: replica.datacenter,
        read_only: replica.read_only,
    }
}

fn keeper_from_proto(keeper: controller_proto::ClickhouseKeeperSpec) -> KeeperSpec {
    KeeperSpec {
        node_id: keeper.node_id,
        server_id: keeper.server_id,
        advertise_address: keeper.advertise_address,
        iface: keeper.iface,
        data_volume: keeper.data_volume.and_then(volume_from_proto),
        datacenter: keeper.datacenter,
        learner: keeper.learner,
    }
}

fn volume_from_proto(volume: controller_proto::ClickhouseHostVolume) -> Option<HostVolume> {
    if volume.name.is_empty() {
        return None;
    }
    Some(HostVolume {
        name: volume.name,
        storage_class: volume.storage_class,
        volume_group: volume.volume_group,
        size_bytes: volume.size_bytes,
        fs_type: volume.fs_type,
    })
}

fn user_from_proto(user: controller_proto::ClickhouseUserSpec) -> UserSpec {
    UserSpec {
        name: user.name,
        password_sha256_hex: user.password_sha256_hex,
        networks: user.networks,
        profile: user.profile,
        quota: user.quota,
        access_management: user.access_management,
    }
}

fn profile_from_proto(profile: controller_proto::ClickhouseSettingsProfile) -> ProfileSpec {
    ProfileSpec {
        name: profile.name,
        settings: profile.settings.into_iter().collect(),
    }
}

fn quota_from_proto(quota: controller_proto::ClickhouseQuotaSpec) -> QuotaSpec {
    QuotaSpec {
        name: quota.name,
        intervals: quota
            .intervals
            .into_iter()
            .map(|interval| QuotaInterval {
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

fn dictionary_from_proto(dict: controller_proto::ClickhouseDictionarySpec) -> DictionarySpec {
    DictionarySpec {
        name: dict.name,
        source: dict.source,
        query: dict.query,
        url: dict.url,
        primary_key: dict.primary_key,
        layout: dict.layout,
        lifetime_seconds: dict.lifetime_seconds,
        attributes: dict
            .attributes
            .into_iter()
            .map(|attr| DictionaryAttribute {
                name: attr.name,
                r#type: attr.r#type,
                nullable: attr.nullable,
            })
            .collect(),
        database: dict.database,
    }
}

fn collection_from_proto(
    collection: controller_proto::ClickhouseNamedCollectionSpec,
) -> NamedCollectionSpec {
    NamedCollectionSpec {
        name: collection.name,
        keys: collection.keys.into_iter().collect(),
        secret_refs: collection.secret_refs.into_iter().collect(),
    }
}

fn function_from_proto(function: controller_proto::ClickhouseFunctionSpec) -> FunctionSpec {
    FunctionSpec {
        name: function.name,
        parameters: function.parameters,
        expression: function.expression,
    }
}

fn logs_from_proto(logs: controller_proto::ClickhouseSystemLogs) -> SystemLogs {
    SystemLogs {
        query_log_days: logs.query_log_days,
        part_log_days: logs.part_log_days,
        trace_log_days: logs.trace_log_days,
        text_log_days: logs.text_log_days,
    }
}

fn tls_from_proto(tls: controller_proto::ClickhouseTlsConfig) -> TlsConfig {
    TlsConfig {
        disabled: tls.disabled,
        rotate_before_days: tls.rotate_before_days,
    }
}

fn ports_from_proto(ports: controller_proto::ClickhousePorts) -> Ports {
    Ports {
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

pub fn members_from_spec(
    cluster: &str,
    spec: &ClusterSpec,
) -> Vec<crate::clickhouse_db::MemberRow> {
    let mut rows: BTreeMap<String, crate::clickhouse_db::MemberRow> = BTreeMap::new();
    for shard in &spec.shards {
        for replica in &shard.replicas {
            rows.insert(
                replica.node_id.clone(),
                crate::clickhouse_db::MemberRow {
                    cluster_name: cluster.into(),
                    node_id: replica.node_id.clone(),
                    server: true,
                    keeper: false,
                    shard_name: shard.shard_name.clone(),
                    replica_name: replica.replica_name.clone(),
                    server_id: 0,
                    learner: false,
                    advertise_address: replica.advertise_address.clone(),
                    iface: replica.iface.clone(),
                    read_only: replica.read_only,
                    health_json: String::new(),
                },
            );
        }
    }
    for keeper in &spec.keepers {
        let row =
            rows.entry(keeper.node_id.clone())
                .or_insert_with(|| crate::clickhouse_db::MemberRow {
                    cluster_name: cluster.into(),
                    node_id: keeper.node_id.clone(),
                    server: false,
                    keeper: false,
                    shard_name: String::new(),
                    replica_name: String::new(),
                    server_id: 0,
                    learner: false,
                    advertise_address: keeper.advertise_address.clone(),
                    iface: keeper.iface.clone(),
                    read_only: false,
                    health_json: String::new(),
                });
        row.keeper = true;
        row.server_id = keeper.server_id;
        row.learner = keeper.learner;
        if row.advertise_address.is_empty() {
            row.advertise_address = keeper.advertise_address.clone();
            row.iface = keeper.iface.clone();
        }
    }
    rows.into_values().collect()
}

pub fn spec_json(spec: &ClusterSpec) -> Result<String, String> {
    serde_json::to_string(spec).map_err(|e| e.to_string())
}

pub fn spec_from_json(raw: &str) -> Result<ClusterSpec, String> {
    serde_json::from_str(raw).map_err(|e| e.to_string())
}
