//! Nix values fragment for `/etc/nixos/kcore-clickhouse.nix`.
//!
//! Secrets are environment variable names. The hex digests and passwords
//! never appear in this text.

use std::fmt::Write as _;

use kcore_sanitize::{nix_escape, validate_clickhouse_name, ClickhouseNameClass};

use crate::spec::{ClusterSpec, KeeperSpec, Ports, ReplicaSpec, ShardSpec};

pub fn cluster_root(name: &str) -> String {
    format!("/var/lib/kcore-clickhouse/{name}")
}

pub fn secrets_dir(name: &str) -> String {
    format!("{}/secrets", cluster_root(name))
}

pub fn tls_dir(name: &str) -> String {
    format!("{}/tls", cluster_root(name))
}

pub fn server_data_path(name: &str) -> String {
    format!("{}/data", cluster_root(name))
}

pub fn keeper_data_path(name: &str) -> String {
    format!("{}/keeper", cluster_root(name))
}

/// One declared data disk for a ClickHouse role on a node.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeMount {
    pub role: String,
    pub name: String,
    pub storage_class: String,
    pub volume_group: String,
    pub size_bytes: i64,
    pub fs_type: String,
    pub mount_point: String,
}

/// Controller volume row name. The LV or zvol name stays `mount.name`.
pub fn controller_volume_name(cluster: &str, node_id: &str, role: &str) -> String {
    format!("clickhouse-{cluster}-{node_id}-{role}")
}

/// Volumes this node should have mounted for `role` (`server`, `keeper`, or `both`).
pub fn volumes_for_node(
    cluster: &str,
    spec: &ClusterSpec,
    node_id: &str,
    role: &str,
) -> Vec<VolumeMount> {
    let mut out = Vec::new();
    let want_server = role.is_empty() || role == "both" || role == "server";
    let want_keeper = role.is_empty() || role == "both" || role == "keeper";
    if want_server {
        for shard in &spec.shards {
            for replica in &shard.replicas {
                if replica.node_id == node_id {
                    push_volume(
                        &mut out,
                        replica.data_volume.as_ref(),
                        "server",
                        &server_data_path(cluster),
                    );
                }
            }
        }
    }
    if want_keeper {
        for keeper in &spec.keepers {
            if keeper.node_id == node_id {
                push_volume(
                    &mut out,
                    keeper.data_volume.as_ref(),
                    "keeper",
                    &keeper_data_path(cluster),
                );
            }
        }
    }
    out
}

fn push_volume(
    out: &mut Vec<VolumeMount>,
    volume: Option<&crate::spec::HostVolume>,
    role: &str,
    mount_point: &str,
) {
    let Some(volume) = volume else {
        return;
    };
    if volume.name.is_empty() {
        return;
    }
    let fs_type = if volume.fs_type.is_empty() {
        "xfs".to_string()
    } else {
        volume.fs_type.clone()
    };
    out.push(VolumeMount {
        role: role.to_string(),
        name: volume.name.clone(),
        storage_class: volume.storage_class.clone(),
        volume_group: volume.volume_group.clone(),
        size_bytes: volume.size_bytes,
        fs_type,
        mount_point: mount_point.to_string(),
    });
}

pub fn user_password_env(name: &str) -> String {
    let mut upper = String::new();
    for c in name.chars() {
        if c == '-' {
            upper.push('_');
        } else {
            upper.push(c.to_ascii_uppercase());
        }
    }
    format!("KCORE_CH_USER_{upper}")
}

pub fn render_disabled() -> &'static str {
    "{ ... }:\n{\n  kcore.clickhouse.enable = false;\n}\n"
}

#[derive(Debug, Clone)]
pub struct Fragment {
    pub cluster_name: String,
    pub generation: i64,
    pub package: String,
    pub advertise_address: String,
    pub iface: String,
    pub tls_enabled: bool,
    pub ports: Ports,
    pub metrics_public: bool,
    pub max_server_memory_ratio: f64,
    pub client_networks: Vec<String>,
    pub server: Option<ServerFragment>,
    pub keeper: Option<KeeperFragment>,
}

#[derive(Debug, Clone)]
pub struct ServerFragment {
    pub shard: String,
    pub replica: String,
    pub read_only: bool,
    pub data_path: String,
    pub remote_shards: Vec<RemoteShard>,
    pub databases: Vec<String>,
    pub users: Vec<UserFragment>,
    pub profiles: Vec<ProfileFragment>,
    pub quotas: Vec<QuotaFragment>,
    pub dictionaries: Vec<DictionaryFragment>,
    pub named_collections: Vec<CollectionFragment>,
    pub functions: Vec<FunctionFragment>,
    pub settings: Vec<(String, String)>,
    pub system_logs: LogFragment,
}

#[derive(Debug, Clone)]
pub struct RemoteShard {
    pub shard: String,
    pub weight: i32,
    pub replicas: Vec<RemoteReplica>,
}

#[derive(Debug, Clone)]
pub struct RemoteReplica {
    pub replica: String,
    pub host: String,
    pub port: i32,
    pub secure: bool,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct UserFragment {
    pub name: String,
    pub networks: Vec<String>,
    pub profile: String,
    pub quota: String,
    pub access_management: bool,
}

#[derive(Debug, Clone)]
pub struct ProfileFragment {
    pub name: String,
    pub settings: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct QuotaFragment {
    pub name: String,
    pub intervals: Vec<QuotaIntervalFragment>,
}

#[derive(Debug, Clone)]
pub struct QuotaIntervalFragment {
    pub duration_seconds: i32,
    pub queries: i64,
    pub errors: i64,
    pub result_rows: i64,
    pub read_rows: i64,
    pub execution_time_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct DictionaryFragment {
    pub name: String,
    pub source: String,
    pub query: String,
    pub url: String,
    pub primary_key: String,
    pub layout: String,
    pub lifetime_seconds: i32,
    pub database: String,
    pub attributes: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone)]
pub struct CollectionFragment {
    pub name: String,
    pub keys: Vec<(String, String)>,
    pub secret_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FunctionFragment {
    pub name: String,
    pub parameters: String,
    pub expression: String,
}

#[derive(Debug, Clone, Default)]
pub struct LogFragment {
    pub query_log_days: i32,
    pub part_log_days: i32,
    pub trace_log_days: i32,
    pub text_log_days: i32,
}

#[derive(Debug, Clone)]
pub struct KeeperFragment {
    pub server_id: i32,
    pub learner: bool,
    pub data_path: String,
    pub raft_peers: Vec<RaftPeerFragment>,
}

#[derive(Debug, Clone)]
pub struct RaftPeerFragment {
    pub id: i32,
    pub host: String,
    pub port: i32,
    pub learner: bool,
}

pub fn render_node(
    cluster: &str,
    generation: i64,
    spec: &ClusterSpec,
    node_id: &str,
) -> Result<String, String> {
    let replica = find_replica(spec, node_id);
    let keeper = spec.keepers.iter().find(|k| k.node_id == node_id);
    if replica.is_none() && keeper.is_none() {
        return Err(format!("node {node_id} is not a member of {cluster}"));
    }
    let (advertise, iface) = match (replica, keeper) {
        (Some((_, replica)), Some(keeper)) => {
            if replica.advertise_address != keeper.advertise_address
                || replica.iface != keeper.iface
            {
                return Err(format!("node {node_id} is advertised on two interfaces"));
            }
            (replica.advertise_address.clone(), replica.iface.clone())
        }
        (Some((_, replica)), None) => (replica.advertise_address.clone(), replica.iface.clone()),
        (None, Some(keeper)) => (keeper.advertise_address.clone(), keeper.iface.clone()),
        (None, None) => return Err(format!("node {node_id} is not a member of {cluster}")),
    };
    let client_port = if spec.tls.disabled {
        spec.ports.tcp
    } else {
        spec.ports.tcp_secure
    };
    let fragment = Fragment {
        cluster_name: cluster.to_string(),
        generation,
        package: spec.package.clone(),
        advertise_address: advertise,
        iface,
        tls_enabled: !spec.tls.disabled,
        ports: spec.ports.clone(),
        metrics_public: spec.metrics_public,
        max_server_memory_ratio: spec.max_server_memory_ratio,
        client_networks: spec.client_networks.clone(),
        server: replica.map(|(shard, replica)| server_fragment(spec, shard, replica, client_port)),
        keeper: keeper.map(|keeper| keeper_fragment(spec, keeper)),
    };
    render_fragment(&fragment)
}

fn server_fragment(
    spec: &ClusterSpec,
    shard: &ShardSpec,
    replica: &ReplicaSpec,
    client_port: i32,
) -> ServerFragment {
    ServerFragment {
        shard: shard.shard_name.clone(),
        replica: replica.replica_name.clone(),
        read_only: replica.read_only,
        data_path: String::new(),
        remote_shards: spec
            .shards
            .iter()
            .map(|shard| RemoteShard {
                shard: shard.shard_name.clone(),
                weight: shard.weight,
                replicas: shard
                    .replicas
                    .iter()
                    .map(|replica| RemoteReplica {
                        replica: replica.replica_name.clone(),
                        host: replica.advertise_address.clone(),
                        port: client_port,
                        secure: !spec.tls.disabled,
                        priority: if replica.read_only { 100 } else { 1 },
                    })
                    .collect(),
            })
            .collect(),
        databases: spec.databases.iter().map(|db| db.name.clone()).collect(),
        users: spec
            .users
            .iter()
            .map(|user| UserFragment {
                name: user.name.clone(),
                networks: user.networks.clone(),
                profile: user.profile.clone(),
                quota: user.quota.clone(),
                access_management: user.access_management,
            })
            .collect(),
        profiles: spec
            .profiles
            .iter()
            .map(|profile| ProfileFragment {
                name: profile.name.clone(),
                settings: profile
                    .settings
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            })
            .collect(),
        quotas: spec
            .quotas
            .iter()
            .map(|quota| QuotaFragment {
                name: quota.name.clone(),
                intervals: quota
                    .intervals
                    .iter()
                    .map(|interval| QuotaIntervalFragment {
                        duration_seconds: interval.duration_seconds,
                        queries: interval.queries,
                        errors: interval.errors,
                        result_rows: interval.result_rows,
                        read_rows: interval.read_rows,
                        execution_time_seconds: interval.execution_time_seconds,
                    })
                    .collect(),
            })
            .collect(),
        dictionaries: spec
            .dictionaries
            .iter()
            .map(|dict| DictionaryFragment {
                name: dict.name.clone(),
                source: dict.source.clone(),
                query: dict.query.clone(),
                url: dict.url.clone(),
                primary_key: dict.primary_key.clone(),
                layout: dict.layout.clone(),
                lifetime_seconds: dict.lifetime_seconds,
                database: dict.database.clone(),
                attributes: dict
                    .attributes
                    .iter()
                    .map(|attr| (attr.name.clone(), attr.r#type.clone(), attr.nullable))
                    .collect(),
            })
            .collect(),
        named_collections: spec
            .named_collections
            .iter()
            .map(|collection| CollectionFragment {
                name: collection.name.clone(),
                keys: collection
                    .keys
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                secret_refs: collection.secret_refs.keys().cloned().collect(),
            })
            .collect(),
        functions: spec
            .functions
            .iter()
            .map(|function| FunctionFragment {
                name: function.name.clone(),
                parameters: function.parameters.clone(),
                expression: function.expression.clone(),
            })
            .collect(),
        settings: spec
            .server_settings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        system_logs: LogFragment {
            query_log_days: spec.system_logs.query_log_days,
            part_log_days: spec.system_logs.part_log_days,
            trace_log_days: spec.system_logs.trace_log_days,
            text_log_days: spec.system_logs.text_log_days,
        },
    }
}

fn keeper_fragment(spec: &ClusterSpec, keeper: &KeeperSpec) -> KeeperFragment {
    KeeperFragment {
        server_id: keeper.server_id,
        learner: keeper.learner,
        data_path: String::new(),
        raft_peers: spec
            .keepers
            .iter()
            .map(|peer| RaftPeerFragment {
                id: peer.server_id,
                host: peer.advertise_address.clone(),
                port: spec.ports.keeper_raft,
                learner: peer.learner,
            })
            .collect(),
    }
}

fn find_replica<'a>(
    spec: &'a ClusterSpec,
    node_id: &str,
) -> Option<(&'a ShardSpec, &'a ReplicaSpec)> {
    spec.shards.iter().find_map(|shard| {
        shard
            .replicas
            .iter()
            .find(|replica| replica.node_id == node_id)
            .map(|replica| (shard, replica))
    })
}

pub fn render_fragment(fragment: &Fragment) -> Result<String, String> {
    check_name(
        &fragment.cluster_name,
        ClickhouseNameClass::Identifier,
        "cluster",
    )?;
    if fragment.package != "clickhouse" && fragment.package != "clickhouse-lts" {
        return Err(format!(
            "package {} is not clickhouse or clickhouse-lts",
            fragment.package
        ));
    }
    let root = cluster_root(&fragment.cluster_name);
    let mut out = String::new();
    writeln!(out, "{{ ... }}:").map_err(|e| e.to_string())?;
    writeln!(out, "{{").map_err(|e| e.to_string())?;
    writeln!(out, "  kcore.clickhouse = {{").map_err(|e| e.to_string())?;
    writeln!(out, "    enable = true;").map_err(|e| e.to_string())?;
    writeln!(
        out,
        "    clusterName = {};",
        nix_str(&fragment.cluster_name)
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "    generation = {};", fragment.generation).map_err(|e| e.to_string())?;
    writeln!(out, "    package = {};", nix_str(&fragment.package)).map_err(|e| e.to_string())?;
    writeln!(
        out,
        "    advertiseAddress = {};",
        nix_str(&fragment.advertise_address)
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "    iface = {};", nix_str(&fragment.iface)).map_err(|e| e.to_string())?;
    writeln!(
        out,
        "    secretsDir = {};",
        nix_str(&secrets_dir(&fragment.cluster_name))
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "    tls = {{").map_err(|e| e.to_string())?;
    writeln!(out, "      enable = {};", nix_bool(fragment.tls_enabled))
        .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      certFile = {};",
        nix_str(&format!("{root}/tls/node.crt"))
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      keyFile = {};",
        nix_str(&format!("{root}/tls/node.key"))
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      caFile = {};",
        nix_str(&format!("{root}/tls/ca.crt"))
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "    }};").map_err(|e| e.to_string())?;
    write_ports(&mut out, &fragment.ports)?;
    if let Some(server) = &fragment.server {
        write_server(&mut out, fragment, server)?;
    } else {
        writeln!(out, "    server.enable = false;").map_err(|e| e.to_string())?;
    }
    if let Some(keeper) = &fragment.keeper {
        write_keeper(&mut out, &fragment.cluster_name, keeper)?;
    } else {
        writeln!(out, "    keeper.enable = false;").map_err(|e| e.to_string())?;
    }
    writeln!(out, "  }};").map_err(|e| e.to_string())?;
    writeln!(out, "}}").map_err(|e| e.to_string())?;
    Ok(out)
}

fn write_ports(out: &mut String, ports: &Ports) -> Result<(), String> {
    writeln!(out, "    ports = {{").map_err(|e| e.to_string())?;
    writeln!(out, "      tcp = {};", ports.tcp).map_err(|e| e.to_string())?;
    writeln!(out, "      tcpSecure = {};", ports.tcp_secure).map_err(|e| e.to_string())?;
    writeln!(out, "      http = {};", ports.http).map_err(|e| e.to_string())?;
    writeln!(out, "      https = {};", ports.https).map_err(|e| e.to_string())?;
    writeln!(out, "      interserver = {};", ports.interserver).map_err(|e| e.to_string())?;
    writeln!(out, "      keeper = {};", ports.keeper).map_err(|e| e.to_string())?;
    writeln!(out, "      keeperRaft = {};", ports.keeper_raft).map_err(|e| e.to_string())?;
    writeln!(out, "      prometheus = {};", ports.prometheus).map_err(|e| e.to_string())?;
    writeln!(out, "    }};").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_server(
    out: &mut String,
    fragment: &Fragment,
    server: &ServerFragment,
) -> Result<(), String> {
    check_name(&server.shard, ClickhouseNameClass::Macro, "shard")?;
    check_name(&server.replica, ClickhouseNameClass::Macro, "replica")?;
    let data = if server.data_path.is_empty() {
        server_data_path(&fragment.cluster_name)
    } else {
        server.data_path.clone()
    };
    writeln!(out, "    server = {{").map_err(|e| e.to_string())?;
    writeln!(out, "      enable = true;").map_err(|e| e.to_string())?;
    writeln!(out, "      shard = {};", nix_str(&server.shard)).map_err(|e| e.to_string())?;
    writeln!(out, "      replica = {};", nix_str(&server.replica)).map_err(|e| e.to_string())?;
    writeln!(out, "      readOnly = {};", nix_bool(server.read_only)).map_err(|e| e.to_string())?;
    writeln!(out, "      dataPath = {};", nix_str(&data)).map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      maxServerMemoryRatio = {};",
        format_ratio(fragment.max_server_memory_ratio)
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      metricsPublic = {};",
        nix_bool(fragment.metrics_public)
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "      clientNetworks = {};",
        nix_list(&fragment.client_networks)?
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "      databases = {};", nix_list(&server.databases)?)
        .map_err(|e| e.to_string())?;
    writeln!(out, "      remoteServers = [").map_err(|e| e.to_string())?;
    for shard in &server.remote_shards {
        check_name(&shard.shard, ClickhouseNameClass::Macro, "shard")?;
        writeln!(out, "        {{").map_err(|e| e.to_string())?;
        writeln!(out, "          shard = {};", nix_str(&shard.shard)).map_err(|e| e.to_string())?;
        writeln!(out, "          weight = {};", shard.weight).map_err(|e| e.to_string())?;
        writeln!(out, "          internalReplication = true;").map_err(|e| e.to_string())?;
        writeln!(out, "          replicas = [").map_err(|e| e.to_string())?;
        for replica in &shard.replicas {
            check_name(&replica.replica, ClickhouseNameClass::Macro, "replica")?;
            writeln!(
                out,
                "            {{ replica = {}; host = {}; port = {}; secure = {}; priority = {}; }}",
                nix_str(&replica.replica),
                nix_str(&replica.host),
                replica.port,
                nix_bool(replica.secure),
                replica.priority
            )
            .map_err(|e| e.to_string())?;
        }
        writeln!(out, "          ];").map_err(|e| e.to_string())?;
        writeln!(out, "        }}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    write_users(out, server)?;
    write_profiles(out, server)?;
    write_settings(out, "      settings", &server.settings)?;
    writeln!(out, "      systemLogs = {{").map_err(|e| e.to_string())?;
    writeln!(
        out,
        "        queryLogDays = {};",
        server.system_logs.query_log_days
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "        partLogDays = {};",
        server.system_logs.part_log_days
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "        traceLogDays = {};",
        server.system_logs.trace_log_days
    )
    .map_err(|e| e.to_string())?;
    writeln!(
        out,
        "        textLogDays = {};",
        server.system_logs.text_log_days
    )
    .map_err(|e| e.to_string())?;
    writeln!(out, "      }};").map_err(|e| e.to_string())?;
    if !server.quotas.is_empty() {
        write_quotas(out, server)?;
    }
    if !server.dictionaries.is_empty() {
        write_dictionaries(out, server)?;
    }
    if !server.named_collections.is_empty() {
        write_collections(out, server)?;
    }
    if !server.functions.is_empty() {
        write_functions(out, server)?;
    }
    writeln!(out, "    }};").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_users(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      users = [").map_err(|e| e.to_string())?;
    for user in &server.users {
        check_name(&user.name, ClickhouseNameClass::Identifier, "user")?;
        let profile = if user.profile.is_empty() {
            "default"
        } else {
            user.profile.as_str()
        };
        let quota = if user.quota.is_empty() {
            "default"
        } else {
            user.quota.as_str()
        };
        writeln!(
            out,
            "        {{ name = {}; passwordEnv = {}; networks = {}; profile = {}; quota = {}; accessManagement = {}; }}",
            nix_str(&user.name),
            nix_str(&user_password_env(&user.name)),
            nix_list(&user.networks)?,
            nix_str(profile),
            nix_str(quota),
            nix_bool(user.access_management)
        )
        .map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_profiles(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      profiles = [").map_err(|e| e.to_string())?;
    for profile in &server.profiles {
        check_name(&profile.name, ClickhouseNameClass::Identifier, "profile")?;
        writeln!(out, "        {{").map_err(|e| e.to_string())?;
        writeln!(out, "          name = {};", nix_str(&profile.name)).map_err(|e| e.to_string())?;
        write_settings(out, "          settings", &profile.settings)?;
        writeln!(out, "        }}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_settings(
    out: &mut String,
    label: &str,
    settings: &[(String, String)],
) -> Result<(), String> {
    writeln!(out, "{label} = {{").map_err(|e| e.to_string())?;
    for (key, value) in settings {
        check_name(key, ClickhouseNameClass::SettingKey, "setting")?;
        if value.contains('\n') || value.contains('\0') {
            return Err(format!("setting {key} value is not a single line"));
        }
        writeln!(out, "        {key} = {};", nix_str(value)).map_err(|e| e.to_string())?;
    }
    writeln!(out, "      }};").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_quotas(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      quotas = [").map_err(|e| e.to_string())?;
    for quota in &server.quotas {
        check_name(&quota.name, ClickhouseNameClass::Identifier, "quota")?;
        writeln!(out, "        {{").map_err(|e| e.to_string())?;
        writeln!(out, "          name = {};", nix_str(&quota.name)).map_err(|e| e.to_string())?;
        writeln!(out, "          intervals = [").map_err(|e| e.to_string())?;
        for interval in &quota.intervals {
            writeln!(
                out,
                "            {{ durationSeconds = {}; queries = {}; errors = {}; resultRows = {}; readRows = {}; executionTimeSeconds = {}; }}",
                interval.duration_seconds,
                interval.queries,
                interval.errors,
                interval.result_rows,
                interval.read_rows,
                interval.execution_time_seconds
            )
            .map_err(|e| e.to_string())?;
        }
        writeln!(out, "          ];").map_err(|e| e.to_string())?;
        writeln!(out, "        }}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_dictionaries(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      dictionaries = [").map_err(|e| e.to_string())?;
    for dict in &server.dictionaries {
        check_name(&dict.name, ClickhouseNameClass::Identifier, "dictionary")?;
        writeln!(out, "        {{").map_err(|e| e.to_string())?;
        writeln!(out, "          name = {};", nix_str(&dict.name)).map_err(|e| e.to_string())?;
        writeln!(out, "          source = {};", nix_str(&dict.source))
            .map_err(|e| e.to_string())?;
        writeln!(out, "          query = {};", nix_str(&dict.query)).map_err(|e| e.to_string())?;
        writeln!(out, "          url = {};", nix_str(&dict.url)).map_err(|e| e.to_string())?;
        writeln!(
            out,
            "          primaryKey = {};",
            nix_str(&dict.primary_key)
        )
        .map_err(|e| e.to_string())?;
        writeln!(out, "          layout = {};", nix_str(&dict.layout))
            .map_err(|e| e.to_string())?;
        writeln!(
            out,
            "          lifetimeSeconds = {};",
            dict.lifetime_seconds
        )
        .map_err(|e| e.to_string())?;
        writeln!(out, "          database = {};", nix_str(&dict.database))
            .map_err(|e| e.to_string())?;
        writeln!(out, "          attributes = [").map_err(|e| e.to_string())?;
        for (name, ty, nullable) in &dict.attributes {
            writeln!(
                out,
                "            {{ name = {}; type = {}; nullable = {}; }}",
                nix_str(name),
                nix_str(ty),
                nix_bool(*nullable)
            )
            .map_err(|e| e.to_string())?;
        }
        writeln!(out, "          ];").map_err(|e| e.to_string())?;
        writeln!(out, "        }}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_collections(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      namedCollections = [").map_err(|e| e.to_string())?;
    for collection in &server.named_collections {
        check_name(
            &collection.name,
            ClickhouseNameClass::Identifier,
            "collection",
        )?;
        writeln!(out, "        {{").map_err(|e| e.to_string())?;
        writeln!(out, "          name = {};", nix_str(&collection.name))
            .map_err(|e| e.to_string())?;
        write_settings(out, "          keys", &collection.keys)?;
        writeln!(out, "          secretRefs = [").map_err(|e| e.to_string())?;
        for key in &collection.secret_refs {
            writeln!(out, "            {}", nix_str(key)).map_err(|e| e.to_string())?;
        }
        writeln!(out, "          ];").map_err(|e| e.to_string())?;
        writeln!(out, "        }}").map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_functions(out: &mut String, server: &ServerFragment) -> Result<(), String> {
    writeln!(out, "      functions = [").map_err(|e| e.to_string())?;
    for function in &server.functions {
        check_name(&function.name, ClickhouseNameClass::Identifier, "function")?;
        writeln!(
            out,
            "        {{ name = {}; parameters = {}; expression = {}; }}",
            nix_str(&function.name),
            nix_str(&function.parameters),
            nix_str(&function.expression)
        )
        .map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    Ok(())
}

fn write_keeper(out: &mut String, cluster: &str, keeper: &KeeperFragment) -> Result<(), String> {
    let data = if keeper.data_path.is_empty() {
        keeper_data_path(cluster)
    } else {
        keeper.data_path.clone()
    };
    writeln!(out, "    keeper = {{").map_err(|e| e.to_string())?;
    writeln!(out, "      enable = true;").map_err(|e| e.to_string())?;
    writeln!(out, "      serverId = {};", keeper.server_id).map_err(|e| e.to_string())?;
    writeln!(out, "      learner = {};", nix_bool(keeper.learner)).map_err(|e| e.to_string())?;
    writeln!(out, "      dataPath = {};", nix_str(&data)).map_err(|e| e.to_string())?;
    writeln!(out, "      raftPeers = [").map_err(|e| e.to_string())?;
    for peer in &keeper.raft_peers {
        writeln!(
            out,
            "        {{ id = {}; host = {}; port = {}; learner = {}; }}",
            peer.id,
            nix_str(&peer.host),
            peer.port,
            nix_bool(peer.learner)
        )
        .map_err(|e| e.to_string())?;
    }
    writeln!(out, "      ];").map_err(|e| e.to_string())?;
    writeln!(out, "    }};").map_err(|e| e.to_string())?;
    Ok(())
}

fn nix_str(value: &str) -> String {
    format!("\"{}\"", nix_escape(value))
}

fn nix_bool(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn nix_list(values: &[String]) -> Result<String, String> {
    let mut out = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        check_scalar(value)?;
        out.push_str(&nix_str(value));
    }
    out.push(']');
    Ok(out)
}

fn check_scalar(value: &str) -> Result<(), String> {
    if value.contains('\n') || value.contains('\0') {
        return Err("nix scalar is not a single line".into());
    }
    Ok(())
}

fn check_name(name: &str, class: ClickhouseNameClass, label: &str) -> Result<(), String> {
    if !validate_clickhouse_name(name, class) {
        return Err(format!("{label} {name} is outside its grammar"));
    }
    Ok(())
}

fn format_ratio(value: f64) -> String {
    let text = format!("{value:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".into()
    } else {
        trimmed.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests::two_by_two;

    #[test]
    fn disabled_fragment_is_exact() {
        assert_eq!(
            render_disabled(),
            "{ ... }:\n{\n  kcore.clickhouse.enable = false;\n}\n"
        );
    }

    #[test]
    fn member_fragment_escapes_and_omits_secrets() {
        let spec = two_by_two();
        let text = render_node("analytics", 3, &spec, "node-105").unwrap();
        assert!(text.contains("clusterName = \"analytics\";"));
        assert!(text.contains("generation = 3;"));
        assert!(text.contains("replica = \"node-105\";"));
        assert!(text.contains("shard = \"1\";"));
        assert!(text.contains("priority = 1;"));
        assert!(text.contains("serverId = 1;"));
        assert!(text.contains("learner = false;"));
        assert!(!text.contains("@from_env"));
        assert!(!text.contains("password"));
        assert!(text.contains("secretsDir = \"/var/lib/kcore-clickhouse/analytics/secrets\";"));
        assert!(text.contains("databases = [\"events\"];"));
    }

    #[test]
    fn setting_quotes_are_escaped() {
        let mut spec = two_by_two();
        spec.server_settings
            .insert("max_concurrent_queries".into(), "a\"b${c}".into());
        let text = render_node("analytics", 1, &spec, "node-106").unwrap();
        assert!(text.contains("max_concurrent_queries = \"a\\\"b\\${c}\";"));
    }

    #[test]
    fn server_without_keeper_disables_keeper() {
        let spec = two_by_two();
        let text = render_node("analytics", 1, &spec, "node-109").unwrap();
        assert!(text.contains("shard = \"2\";"));
        assert!(text.contains("replica = \"node-109\";"));
        assert!(text.contains("keeper.enable = false;"));
    }

    #[test]
    fn volumes_follow_the_role_and_default_to_xfs() {
        let mut spec = two_by_two();
        spec.shards[0].replicas[0].data_volume = Some(crate::spec::HostVolume {
            name: "ch-analytics".into(),
            storage_class: "lvm".into(),
            volume_group: "vg0".into(),
            size_bytes: 10,
            fs_type: String::new(),
        });
        spec.keepers[0].data_volume = Some(crate::spec::HostVolume {
            name: "ch-keeper".into(),
            storage_class: "lvm".into(),
            volume_group: "vg0".into(),
            size_bytes: 2,
            fs_type: "ext4".into(),
        });
        let server = volumes_for_node("analytics", &spec, "node-105", "server");
        assert_eq!(server.len(), 1);
        assert_eq!(server[0].mount_point, server_data_path("analytics"));
        assert_eq!(server[0].fs_type, "xfs");
        let keeper = volumes_for_node("analytics", &spec, "node-105", "keeper");
        assert_eq!(keeper.len(), 1);
        assert_eq!(keeper[0].fs_type, "ext4");
        assert_eq!(keeper[0].mount_point, keeper_data_path("analytics"));
        assert!(volumes_for_node("analytics", &spec, "node-106", "both").is_empty());
        assert_eq!(
            controller_volume_name("analytics", "node-105", "server"),
            "clickhouse-analytics-node-105-server"
        );
    }
}
