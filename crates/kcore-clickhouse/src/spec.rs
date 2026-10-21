//! Declarative ClickHouse cluster spec: defaults, validation, and diff.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use kcore_sanitize::{validate_clickhouse_name, ClickhouseNameClass};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

pub const DEFAULT_PACKAGE: &str = "clickhouse";
pub const DEFAULT_DELAY_SECONDS: i32 = 30;
pub const DEFAULT_STEP_TIMEOUT_SECONDS: i32 = 600;
pub const DEFAULT_MEMORY_RATIO: f64 = 0.5;
pub const MAX_MEMORY_RATIO: f64 = 0.8;

pub const RESTART_SETTINGS: &[&str] = &[
    "compiled_expression_cache_size",
    "mark_cache_size",
    "max_connections",
    "max_server_memory_usage",
    "mmap_cache_size",
    "uncompressed_cache_size",
];

pub const OWNED_SETTINGS: &[&str] = &[
    "access_control_path",
    "dictionaries_config",
    "format_schema_path",
    "http_handlers",
    "http_port",
    "https_port",
    "include_from",
    "interserver_http_credentials",
    "interserver_http_port",
    "interserver_https_host",
    "interserver_https_port",
    "listen_host",
    "macros",
    "max_server_memory_usage_to_ram_ratio",
    "named_collections",
    "openSSL",
    "openssl",
    "part_log",
    "path",
    "prometheus",
    "query_log",
    "remote_servers",
    "tcp_port",
    "tcp_port_secure",
    "text_log",
    "tmp_path",
    "trace_log",
    "user_defined_executable_functions",
    "user_directories",
    "user_files_path",
    "users_config",
    "zookeeper",
];

/// A ClickHouse package version as `(major, minor)`.
pub type PackageVersion = (u32, u32);

/// Keeper on-disk feature flags. A jump from below `min_step` to `introduced`
/// or newer needs an intermediate package inside that window.
pub const KEEPER_FLAGS: &[(PackageVersion, PackageVersion)] = &[((25, 7), (24, 9))];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostVolume {
    pub name: String,
    pub storage_class: String,
    pub volume_group: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub fs_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaSpec {
    pub node_id: String,
    #[serde(default)]
    pub replica_name: String,
    pub advertise_address: String,
    pub iface: String,
    #[serde(default)]
    pub data_volume: Option<HostVolume>,
    #[serde(default)]
    pub datacenter: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShardSpec {
    #[serde(default)]
    pub shard_name: String,
    pub replicas: Vec<ReplicaSpec>,
    #[serde(default)]
    pub weight: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeeperSpec {
    pub node_id: String,
    #[serde(default)]
    pub server_id: i32,
    pub advertise_address: String,
    pub iface: String,
    #[serde(default)]
    pub data_volume: Option<HostVolume>,
    #[serde(default)]
    pub datacenter: String,
    #[serde(default)]
    pub learner: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserSpec {
    pub name: String,
    pub password_sha256_hex: String,
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub quota: String,
    #[serde(default)]
    pub access_management: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProfileSpec {
    pub name: String,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabaseSpec {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaInterval {
    pub duration_seconds: i32,
    #[serde(default)]
    pub queries: i64,
    #[serde(default)]
    pub errors: i64,
    #[serde(default)]
    pub result_rows: i64,
    #[serde(default)]
    pub read_rows: i64,
    #[serde(default)]
    pub execution_time_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaSpec {
    pub name: String,
    #[serde(default)]
    pub intervals: Vec<QuotaInterval>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DictionaryAttribute {
    pub name: String,
    pub r#type: String,
    #[serde(default)]
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DictionarySpec {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub url: String,
    pub primary_key: String,
    pub layout: String,
    #[serde(default)]
    pub lifetime_seconds: i32,
    #[serde(default)]
    pub attributes: Vec<DictionaryAttribute>,
    #[serde(default)]
    pub database: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamedCollectionSpec {
    pub name: String,
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    #[serde(default)]
    pub secret_refs: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FunctionSpec {
    pub name: String,
    #[serde(default)]
    pub parameters: String,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SystemLogs {
    #[serde(default)]
    pub query_log_days: i32,
    #[serde(default)]
    pub part_log_days: i32,
    #[serde(default)]
    pub trace_log_days: i32,
    #[serde(default)]
    pub text_log_days: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub rotate_before_days: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ports {
    #[serde(default)]
    pub tcp: i32,
    #[serde(default)]
    pub tcp_secure: i32,
    #[serde(default)]
    pub http: i32,
    #[serde(default)]
    pub https: i32,
    #[serde(default)]
    pub interserver: i32,
    #[serde(default)]
    pub keeper: i32,
    #[serde(default)]
    pub keeper_raft: i32,
    #[serde(default)]
    pub prometheus: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClusterSpec {
    #[serde(default)]
    pub package: String,
    pub shards: Vec<ShardSpec>,
    pub keepers: Vec<KeeperSpec>,
    #[serde(default)]
    pub users: Vec<UserSpec>,
    #[serde(default)]
    pub profiles: Vec<ProfileSpec>,
    #[serde(default)]
    pub databases: Vec<DatabaseSpec>,
    #[serde(default)]
    pub server_settings: BTreeMap<String, String>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub client_networks: Vec<String>,
    #[serde(default)]
    pub max_replica_delay_seconds: i32,
    #[serde(default)]
    pub step_timeout_seconds: i32,
    #[serde(default)]
    pub ports: Ports,
    #[serde(default)]
    pub max_server_memory_ratio: f64,
    #[serde(default)]
    pub quotas: Vec<QuotaSpec>,
    #[serde(default)]
    pub dictionaries: Vec<DictionarySpec>,
    #[serde(default)]
    pub named_collections: Vec<NamedCollectionSpec>,
    #[serde(default)]
    pub functions: Vec<FunctionSpec>,
    #[serde(default)]
    pub system_logs: SystemLogs,
    #[serde(default)]
    pub metrics_public: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    Create,
    Unchanged,
    Update { restart: bool },
    Upgrade,
    Scale,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiff {
    pub action: PlanAction,
    pub changed_fields: Vec<String>,
    pub blocking: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

pub fn parse_manifest(yaml: &str) -> Result<(String, ClusterSpec), String> {
    let value: Value = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
    let value = camelize(value, false);
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if kind != "ClickhouseCluster" {
        return Err(format!("expected kind ClickhouseCluster, got {kind}"));
    }
    let name = value
        .get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return Err("metadata.name is required".into());
    }
    if !validate_clickhouse_name(&name, ClickhouseNameClass::Identifier) {
        return Err(format!("cluster name {name} is not an identifier"));
    }
    let spec = value.get("spec").cloned().unwrap_or(Value::Null);
    let spec: ClusterSpec = serde_yaml::from_value(spec).map_err(|e| e.to_string())?;
    Ok((name, spec))
}

pub fn normalize(spec: &mut ClusterSpec) {
    if spec.package.is_empty() {
        spec.package = DEFAULT_PACKAGE.into();
    }
    if spec.max_replica_delay_seconds == 0 {
        spec.max_replica_delay_seconds = DEFAULT_DELAY_SECONDS;
    }
    if spec.step_timeout_seconds == 0 {
        spec.step_timeout_seconds = DEFAULT_STEP_TIMEOUT_SECONDS;
    }
    if spec.max_server_memory_ratio == 0.0 {
        spec.max_server_memory_ratio = DEFAULT_MEMORY_RATIO;
    }
    if spec.tls.rotate_before_days == 0 {
        spec.tls.rotate_before_days = 30;
    }
    fill_ports(&mut spec.ports, spec.tls.disabled);
    if spec.system_logs.query_log_days == 0 {
        spec.system_logs.query_log_days = 14;
    }
    if spec.system_logs.part_log_days == 0 {
        spec.system_logs.part_log_days = 14;
    }
    if spec.system_logs.trace_log_days == 0 {
        spec.system_logs.trace_log_days = 3;
    }
    if spec.system_logs.text_log_days == 0 {
        spec.system_logs.text_log_days = 7;
    }
    for (index, shard) in spec.shards.iter_mut().enumerate() {
        if shard.shard_name.is_empty() {
            shard.shard_name = (index + 1).to_string();
        }
        if shard.weight == 0 {
            shard.weight = 1;
        }
        for replica in &mut shard.replicas {
            if replica.replica_name.is_empty() {
                replica.replica_name = replica.node_id.clone();
            }
            if let Some(volume) = &mut replica.data_volume {
                if volume.fs_type.is_empty() {
                    volume.fs_type = "xfs".into();
                }
            }
        }
    }
    for (index, keeper) in spec.keepers.iter_mut().enumerate() {
        if keeper.server_id == 0 {
            keeper.server_id = (index + 1) as i32;
        }
        if let Some(volume) = &mut keeper.data_volume {
            if volume.fs_type.is_empty() {
                volume.fs_type = "xfs".into();
            }
        }
    }
}

pub fn fill_ports(ports: &mut Ports, tls_disabled: bool) {
    if ports.tcp == 0 {
        ports.tcp = 9000;
    }
    if ports.tcp_secure == 0 {
        ports.tcp_secure = 9440;
    }
    if ports.http == 0 {
        ports.http = 8123;
    }
    if ports.https == 0 {
        ports.https = 8443;
    }
    if ports.interserver == 0 {
        ports.interserver = if tls_disabled { 9009 } else { 9010 };
    }
    if ports.keeper == 0 {
        ports.keeper = if tls_disabled { 9181 } else { 9281 };
    }
    if ports.keeper_raft == 0 {
        ports.keeper_raft = 9234;
    }
    if ports.prometheus == 0 {
        ports.prometheus = 9363;
    }
}

pub fn validate(spec: &ClusterSpec) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut push = |path: &str, message: String| {
        issues.push(Issue {
            path: path.into(),
            message,
        })
    };
    if spec.package != "clickhouse" && spec.package != "clickhouse-lts" {
        push(
            "package",
            format!(
                "package {} is not clickhouse or clickhouse-lts",
                spec.package
            ),
        );
    }
    if spec.shards.is_empty() {
        push("shards", "at least one shard is required".into());
    }
    if spec.max_server_memory_ratio <= 0.0 || spec.max_server_memory_ratio > MAX_MEMORY_RATIO {
        push(
            "maxServerMemoryRatio",
            format!("must be in (0, {MAX_MEMORY_RATIO}]"),
        );
    }
    if spec.max_replica_delay_seconds <= 0 {
        push("maxReplicaDelaySeconds", "must be positive".into());
    }
    if spec.step_timeout_seconds <= 0 {
        push("stepTimeoutSeconds", "must be positive".into());
    }
    let voting: Vec<_> = spec.keepers.iter().filter(|k| !k.learner).collect();
    if voting.is_empty() || ![1, 3, 5].contains(&voting.len()) {
        push("keepers", "voting Keeper members must be 1, 3, or 5".into());
    }
    let mut server_ids = BTreeSet::new();
    let mut keeper_nodes = BTreeSet::new();
    for keeper in &spec.keepers {
        if !server_ids.insert(keeper.server_id) {
            push(
                "keepers",
                format!("duplicate serverId {}", keeper.server_id),
            );
        }
        if !keeper_nodes.insert(keeper.node_id.clone()) {
            push(
                "keepers",
                format!("duplicate keeper node {}", keeper.node_id),
            );
        }
        check_endpoint(
            &keeper.node_id,
            &keeper.advertise_address,
            &keeper.iface,
            "keepers",
            &mut push,
        );
        if let Some(volume) = &keeper.data_volume {
            check_volume(volume, "keepers", &mut push);
        }
    }
    let mut replica_nodes = BTreeSet::new();
    let mut shard_names = BTreeSet::new();
    let mut any_dc = false;
    let mut missing_dc = false;
    for shard in &spec.shards {
        if !validate_clickhouse_name(&shard.shard_name, ClickhouseNameClass::Macro) {
            push(
                "shards",
                format!(
                    "shard name {} is outside the macro grammar",
                    shard.shard_name
                ),
            );
        }
        if !shard_names.insert(shard.shard_name.clone()) {
            push("shards", format!("duplicate shard {}", shard.shard_name));
        }
        if shard.replicas.is_empty() {
            push(
                "shards",
                format!("shard {} has no replicas", shard.shard_name),
            );
        }
        if !shard.replicas.iter().any(|r| !r.read_only) {
            push(
                "shards",
                format!("shard {} has no writable replica", shard.shard_name),
            );
        }
        let mut replica_names = BTreeSet::new();
        for replica in &shard.replicas {
            if !replica_nodes.insert(replica.node_id.clone()) {
                push(
                    "shards",
                    format!("node {} hosts more than one server", replica.node_id),
                );
            }
            if !validate_clickhouse_name(&replica.replica_name, ClickhouseNameClass::Macro) {
                push(
                    "shards",
                    format!(
                        "replica name {} is outside the macro grammar",
                        replica.replica_name
                    ),
                );
            }
            if !replica_names.insert(replica.replica_name.clone()) {
                push(
                    "shards",
                    format!(
                        "duplicate replica {} in shard {}",
                        replica.replica_name, shard.shard_name
                    ),
                );
            }
            check_endpoint(
                &replica.node_id,
                &replica.advertise_address,
                &replica.iface,
                "shards",
                &mut push,
            );
            if let Some(volume) = &replica.data_volume {
                check_volume(volume, "shards", &mut push);
            }
            if replica.datacenter.is_empty() {
                missing_dc = true;
            } else {
                any_dc = true;
            }
        }
    }
    for keeper in &spec.keepers {
        if keeper.datacenter.is_empty() {
            missing_dc = true;
        } else {
            any_dc = true;
        }
    }
    if any_dc && missing_dc {
        push(
            "datacenter",
            "every replica and keeper must set datacenter when any member does".into(),
        );
    }
    if any_dc {
        let mut by_dc: BTreeMap<&str, usize> = BTreeMap::new();
        for keeper in voting.iter() {
            *by_dc.entry(keeper.datacenter.as_str()).or_default() += 1;
        }
        let majority = voting.len() / 2 + 1;
        if by_dc
            .values()
            .any(|count| *count >= majority && voting.len() > 1)
        {
            push(
                "keepers",
                "one datacenter holds a majority of voting Keepers".into(),
            );
        }
        for shard in &spec.shards {
            let writable: Vec<_> = shard.replicas.iter().filter(|r| !r.read_only).collect();
            let dcs: BTreeSet<_> = writable.iter().map(|r| r.datacenter.as_str()).collect();
            if writable.len() > 1 && dcs.len() == 1 {
                push(
                    "shards",
                    format!(
                        "warning: every writable replica of shard {} sits in one datacenter",
                        shard.shard_name
                    ),
                );
            }
        }
    }
    for shard in &spec.shards {
        for replica in &shard.replicas {
            if let Some(keeper) = spec.keepers.iter().find(|k| k.node_id == replica.node_id) {
                if keeper.iface != replica.iface
                    || keeper.advertise_address != replica.advertise_address
                {
                    push(
                        "shards",
                        format!("node {} is advertised on two interfaces", replica.node_id),
                    );
                }
                if let (Some(server_vol), Some(keeper_vol)) =
                    (&replica.data_volume, &keeper.data_volume)
                {
                    if server_vol.name == keeper_vol.name {
                        push(
                            "dataVolume",
                            format!(
                                "node {} reuses volume {} for server and Keeper",
                                replica.node_id, server_vol.name
                            ),
                        );
                    }
                }
            }
        }
    }
    let mut names = BTreeSet::new();
    for user in &spec.users {
        if user.name == "default" || user.name == "kcore-metrics" {
            push("users", format!("user {} is reserved", user.name));
        }
        if !validate_clickhouse_name(&user.name, ClickhouseNameClass::Identifier) {
            push(
                "users",
                format!("user {} is outside the identifier grammar", user.name),
            );
        }
        if !names.insert(user.name.clone()) {
            push("users", format!("duplicate user {}", user.name));
        }
        if user.password_sha256_hex.len() != 64
            || !user
                .password_sha256_hex
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        {
            push(
                "users",
                format!("user {} password must be a sha256 hex digest", user.name),
            );
        }
        for net in &user.networks {
            if net.parse::<IpOrCidr>().is_err() {
                push(
                    "users",
                    format!("user {} network {net} is not an IP or CIDR", user.name),
                );
            }
        }
    }
    for profile in &spec.profiles {
        if !validate_clickhouse_name(&profile.name, ClickhouseNameClass::Identifier) {
            push(
                "profiles",
                format!("profile {} is outside the identifier grammar", profile.name),
            );
        }
        check_settings(&profile.settings, "profiles", &mut push);
    }
    for quota in &spec.quotas {
        if quota.name == "default" {
            push("quotas", "quota default is reserved".into());
        }
        if !validate_clickhouse_name(&quota.name, ClickhouseNameClass::Identifier) {
            push(
                "quotas",
                format!("quota {} is outside the identifier grammar", quota.name),
            );
        }
    }
    for database in &spec.databases {
        if !validate_clickhouse_name(&database.name, ClickhouseNameClass::Identifier) {
            push(
                "databases",
                format!(
                    "database {} is outside the identifier grammar",
                    database.name
                ),
            );
        }
    }
    for key in spec.server_settings.keys() {
        if OWNED_SETTINGS.contains(&key.as_str()) {
            push("serverSettings", format!("kcore owns setting {key}"));
        }
    }
    check_settings(&spec.server_settings, "serverSettings", &mut push);
    for dict in &spec.dictionaries {
        if dict.source == "executable" || dict.source == "executable_pool" {
            push(
                "dictionaries",
                format!("dictionary {} source {} is refused", dict.name, dict.source),
            );
        } else if dict.source != "clickhouse" && dict.source != "https" {
            push(
                "dictionaries",
                format!(
                    "dictionary {} source must be clickhouse or https",
                    dict.name
                ),
            );
        }
        if !matches!(dict.layout.as_str(), "flat" | "hashed" | "cache") {
            push(
                "dictionaries",
                format!(
                    "dictionary {} layout must be flat, hashed, or cache",
                    dict.name
                ),
            );
        }
        if dict.source == "https" && (dict.url.contains('@') || !dict.url.starts_with("https://")) {
            push(
                "dictionaries",
                format!(
                    "dictionary {} url must be https without userinfo",
                    dict.name
                ),
            );
        }
        if !dict.database.is_empty()
            && !validate_clickhouse_name(&dict.database, ClickhouseNameClass::Identifier)
        {
            push(
                "dictionaries",
                format!(
                    "dictionary database {} is outside the identifier grammar",
                    dict.database
                ),
            );
        }
    }
    for collection in &spec.named_collections {
        if !validate_clickhouse_name(&collection.name, ClickhouseNameClass::Identifier) {
            push(
                "namedCollections",
                format!(
                    "collection {} is outside the identifier grammar",
                    collection.name
                ),
            );
        }
        for key in collection.keys.keys() {
            if collection.secret_refs.contains_key(key) || looks_secret(key) {
                push(
                    "namedCollections",
                    format!(
                        "collection {} key {key} looks like a secret and must be a secretRef",
                        collection.name
                    ),
                );
            }
        }
    }
    for function in &spec.functions {
        if !validate_clickhouse_name(&function.name, ClickhouseNameClass::Identifier) {
            push(
                "functions",
                format!(
                    "function {} is outside the identifier grammar",
                    function.name
                ),
            );
        }
        if function.expression.contains(';') {
            push(
                "functions",
                format!(
                    "function {} expression must be a single expression",
                    function.name
                ),
            );
        }
    }
    let ports = [
        spec.ports.tcp,
        spec.ports.tcp_secure,
        spec.ports.http,
        spec.ports.https,
        spec.ports.interserver,
        spec.ports.keeper,
        spec.ports.keeper_raft,
        spec.ports.prometheus,
    ];
    if ports.iter().any(|p| *p <= 0 || *p > 65535)
        || ports.iter().collect::<BTreeSet<_>>().len() != ports.len()
    {
        push("ports", "ports must be distinct and in 1..=65535".into());
    }
    for net in &spec.client_networks {
        if net.parse::<IpOrCidr>().is_err() {
            push("clientNetworks", format!("{net} is not an IP or CIDR"));
        }
    }
    issues
}

fn check_endpoint(
    node: &str,
    address: &str,
    iface: &str,
    path: &str,
    push: &mut impl FnMut(&str, String),
) {
    if node.is_empty() {
        push(path, "nodeId is required".into());
    }
    if address.parse::<IpAddr>().is_err() {
        push(
            path,
            format!("advertise address {address} on {node} is not an IP"),
        );
    }
    if iface.is_empty() || iface == "br0" || iface.starts_with("kbr-") || iface.contains('/') {
        push(path, format!("iface {iface} on {node} is not a usable NIC"));
    }
}

fn check_volume(volume: &HostVolume, path: &str, push: &mut impl FnMut(&str, String)) {
    if !validate_clickhouse_name(&volume.name, ClickhouseNameClass::Macro) {
        push(
            path,
            format!("volume {} is outside the macro grammar", volume.name),
        );
    }
    if volume.storage_class != "lvm" && volume.storage_class != "zfs" {
        push(
            path,
            format!("volume {} storageClass must be lvm or zfs", volume.name),
        );
    }
    if volume.volume_group.is_empty() || volume.volume_group.contains('/') {
        push(
            path,
            format!("volume {} volumeGroup is required", volume.name),
        );
    }
    if volume.size_bytes <= 0 {
        push(
            path,
            format!("volume {} sizeBytes must be positive", volume.name),
        );
    }
    if volume.fs_type != "xfs" && volume.fs_type != "ext4" {
        push(
            path,
            format!("volume {} fsType must be xfs or ext4", volume.name),
        );
    }
}

fn check_settings(
    settings: &BTreeMap<String, String>,
    path: &str,
    push: &mut impl FnMut(&str, String),
) {
    for (key, value) in settings {
        if !validate_clickhouse_name(key, ClickhouseNameClass::SettingKey) {
            push(
                path,
                format!("setting {key} is outside the setting grammar"),
            );
        }
        if value.contains('\n') || value.contains('\0') {
            push(path, format!("setting {key} value is not a single line"));
        }
    }
}

fn looks_secret(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("password")
        || lower.contains("secret")
        || lower.contains("access_key")
        || lower.contains("token")
}

/// Minimal IP or CIDR check without pulling a new crate.
struct IpOrCidr;

impl std::str::FromStr for IpOrCidr {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        if let Some((ip, prefix)) = s.split_once('/') {
            ip.parse::<IpAddr>().map_err(|_| ())?;
            let bits: u8 = prefix.parse().map_err(|_| ())?;
            let max = if ip.contains(':') { 128 } else { 32 };
            if bits > max {
                return Err(());
            }
        } else {
            s.parse::<IpAddr>().map_err(|_| ())?;
        }
        Ok(IpOrCidr)
    }
}

pub fn diff(before: &ClusterSpec, after: &ClusterSpec) -> PlanDiff {
    let mut changed = Vec::new();
    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    let mut replace = false;
    let mut scale = false;
    let mut restart = false;
    if before.package != after.package {
        changed.push("package".into());
    }
    if before.tls.disabled != after.tls.disabled {
        changed.push("tls.disabled".into());
        replace = true;
        blocking.push("tls.disabled is immutable".into());
    }
    if before.ports != after.ports {
        changed.push("ports".into());
        replace = true;
        blocking.push("ports are immutable".into());
    }
    if keeper_identity(before) != keeper_identity(after) {
        changed.push("keepers".into());
        replace = true;
        blocking.push("Keeper membership is immutable in v1".into());
    }
    let before_replicas = replica_index(before);
    let after_replicas = replica_index(after);
    if before_replicas.keys().collect::<BTreeSet<_>>()
        != after_replicas.keys().collect::<BTreeSet<_>>()
        || before
            .shards
            .iter()
            .map(|s| &s.shard_name)
            .collect::<BTreeSet<_>>()
            != after
                .shards
                .iter()
                .map(|s| &s.shard_name)
                .collect::<BTreeSet<_>>()
    {
        changed.push("shards".into());
        scale = true;
    }
    for (node, prev) in &before_replicas {
        if let Some(next) = after_replicas.get(node) {
            if prev.replica_name != next.replica_name || prev.shard_name != next.shard_name {
                replace = true;
                blocking.push(format!("replica identity of {node} is immutable"));
                changed.push(format!("shards.{node}.replicaName"));
            }
            if prev.read_only != next.read_only {
                changed.push(format!("shards.{node}.readOnly"));
            }
            if prev.weight != next.weight {
                changed.push(format!("shards.{node}.weight"));
            }
            if prev.data_volume != next.data_volume {
                changed.push(format!("shards.{node}.dataVolume"));
                replace = true;
                blocking.push(format!("dataVolume of {node} is a replace"));
            }
        }
    }
    if before.users != after.users {
        changed.push("users".into());
    }
    if before.profiles != after.profiles {
        changed.push("profiles".into());
    }
    if before.quotas != after.quotas {
        changed.push("quotas".into());
    }
    if before.databases != after.databases {
        changed.push("databases".into());
    }
    if before.dictionaries != after.dictionaries {
        changed.push("dictionaries".into());
    }
    if before.named_collections != after.named_collections {
        changed.push("namedCollections".into());
    }
    if before.functions != after.functions {
        changed.push("functions".into());
    }
    if before.client_networks != after.client_networks {
        changed.push("clientNetworks".into());
    }
    if before.system_logs != after.system_logs {
        changed.push("systemLogs".into());
    }
    if before.metrics_public != after.metrics_public {
        changed.push("metricsPublic".into());
        if after.metrics_public {
            warnings.push("prometheus 9363 has no authentication".into());
        }
    }
    if (before.max_server_memory_ratio - after.max_server_memory_ratio).abs() > f64::EPSILON {
        changed.push("maxServerMemoryRatio".into());
        restart = true;
    }
    if before.max_replica_delay_seconds != after.max_replica_delay_seconds {
        changed.push("maxReplicaDelaySeconds".into());
    }
    if before.step_timeout_seconds != after.step_timeout_seconds {
        changed.push("stepTimeoutSeconds".into());
    }
    for (key, value) in &after.server_settings {
        if before.server_settings.get(key) != Some(value) {
            changed.push(format!("serverSettings.{key}"));
            if RESTART_SETTINGS.contains(&key.as_str()) {
                restart = true;
            }
        }
    }
    for key in before.server_settings.keys() {
        if !after.server_settings.contains_key(key) {
            changed.push(format!("serverSettings.{key}"));
            if RESTART_SETTINGS.contains(&key.as_str()) {
                restart = true;
            }
        }
    }
    let action = if replace {
        PlanAction::Replace
    } else if before.package != after.package {
        if scale {
            blocking.push("change the package with upgrade before scaling".into());
        }
        PlanAction::Upgrade
    } else if scale {
        PlanAction::Scale
    } else if changed.is_empty() {
        PlanAction::Unchanged
    } else {
        PlanAction::Update { restart }
    };
    PlanDiff {
        action,
        changed_fields: changed,
        blocking,
        warnings,
    }
}

/// Split validation output. Messages that start with `warning:` do not block a plan.
pub fn split_issues(issues: &[Issue]) -> (Vec<String>, Vec<String>) {
    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    for issue in issues {
        let line = format!("{}: {}", issue.path, issue.message);
        if issue.message.starts_with("warning:") {
            warnings.push(line);
        } else {
            blocking.push(line);
        }
    }
    (blocking, warnings)
}

#[derive(Clone, PartialEq, Eq)]
struct ReplicaKey {
    shard_name: String,
    replica_name: String,
    read_only: bool,
    weight: i32,
    data_volume: String,
}

fn replica_index(spec: &ClusterSpec) -> BTreeMap<String, ReplicaKey> {
    let mut out = BTreeMap::new();
    for shard in &spec.shards {
        for replica in &shard.replicas {
            out.insert(
                replica.node_id.clone(),
                ReplicaKey {
                    shard_name: shard.shard_name.clone(),
                    replica_name: replica.replica_name.clone(),
                    read_only: replica.read_only,
                    weight: shard.weight,
                    data_volume: volume_key(&replica.data_volume),
                },
            );
        }
    }
    out
}

fn volume_key(volume: &Option<HostVolume>) -> String {
    volume
        .as_ref()
        .map(|v| {
            format!(
                "{}:{}:{}:{}:{}",
                v.name, v.storage_class, v.volume_group, v.size_bytes, v.fs_type
            )
        })
        .unwrap_or_default()
}

fn keeper_identity(spec: &ClusterSpec) -> Vec<(String, i32, String, String, bool)> {
    spec.keepers
        .iter()
        .map(|k| {
            (
                k.node_id.clone(),
                k.server_id,
                k.advertise_address.clone(),
                k.iface.clone(),
                k.learner,
            )
        })
        .collect()
}

pub fn parse_version(raw: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = raw.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    let build = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch, build))
}

pub fn version_is_lower(running: &str, target: &str) -> bool {
    match (parse_version(running), parse_version(target)) {
        (Some(a), Some(b)) => b < a,
        _ => false,
    }
}

pub fn version_at_least(version: &str, major: u32, minor: u32) -> bool {
    parse_version(version).is_some_and(|v| v.0 > major || (v.0 == major && v.1 >= minor))
}

/// Refuse a Keeper feature-flag jump that has no intermediate package in the window.
pub fn keeper_window_issue(
    running: &str,
    target: &str,
    intermediate: Option<&str>,
) -> Result<(), String> {
    let Some(running_v) = parse_version(running) else {
        return Ok(());
    };
    let Some(target_v) = parse_version(target) else {
        return Ok(());
    };
    for ((flag_major, flag_minor), (step_major, step_minor)) in KEEPER_FLAGS {
        let below_step =
            running_v.0 < *step_major || (running_v.0 == *step_major && running_v.1 < *step_minor);
        let target_reaches =
            target_v.0 > *flag_major || (target_v.0 == *flag_major && target_v.1 >= *flag_minor);
        if !(below_step && target_reaches) {
            continue;
        }
        let Some(intermediate) = intermediate.filter(|s| !s.is_empty()) else {
            return Err(format!(
                "Keeper jump crosses {flag_major}.{flag_minor}; set intermediatePackage inside {step_major}.{step_minor}"
            ));
        };
        let Some(mid) = parse_version(intermediate) else {
            return Err("intermediatePackage version is not a dotted version".into());
        };
        let mid_ok = (mid.0 > *step_major || (mid.0 == *step_major && mid.1 >= *step_minor))
            && (mid.0 < *flag_major || (mid.0 == *flag_major && mid.1 < *flag_minor));
        if !mid_ok {
            return Err(format!(
                "intermediatePackage must resolve inside [{step_major}.{step_minor}, {flag_major}.{flag_minor})"
            ));
        }
    }
    Ok(())
}

fn camelize(value: Value, preserve: bool) -> Value {
    match value {
        Value::Mapping(map) => {
            let mut out = serde_yaml::Mapping::new();
            for (key, child) in map {
                let raw = key.as_str().unwrap_or("").to_string();
                let renamed = if preserve {
                    raw.clone()
                } else {
                    snake_to_camel(&raw)
                };
                let next_preserve = preserve
                    || matches!(
                        renamed.as_str(),
                        "settings" | "serverSettings" | "keys" | "secretRefs"
                    );
                out.insert(Value::String(renamed), camelize(child, next_preserve));
            }
            Value::Mapping(out)
        }
        Value::Sequence(items) => Value::Sequence(
            items
                .into_iter()
                .map(|item| camelize(item, preserve))
                .collect(),
        ),
        other => other,
    }
}

fn snake_to_camel(key: &str) -> String {
    if !key.contains('_') {
        return key.to_string();
    }
    let mut out = String::new();
    let mut upper = false;
    for ch in key.chars() {
        if ch == '_' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn replica(node: &str, addr: &str) -> ReplicaSpec {
        ReplicaSpec {
            node_id: node.into(),
            replica_name: String::new(),
            advertise_address: addr.into(),
            iface: "enp6s0".into(),
            data_volume: None,
            datacenter: String::new(),
            read_only: false,
        }
    }

    fn keeper(node: &str, addr: &str) -> KeeperSpec {
        KeeperSpec {
            node_id: node.into(),
            server_id: 0,
            advertise_address: addr.into(),
            iface: "enp6s0".into(),
            data_volume: None,
            datacenter: String::new(),
            learner: false,
        }
    }

    pub fn two_by_two() -> ClusterSpec {
        let mut spec = ClusterSpec {
            package: String::new(),
            shards: vec![
                ShardSpec {
                    shard_name: String::new(),
                    replicas: vec![
                        replica("node-105", "192.168.40.105"),
                        replica("node-106", "192.168.40.106"),
                    ],
                    weight: 0,
                },
                ShardSpec {
                    shard_name: String::new(),
                    replicas: vec![
                        replica("node-108", "192.168.40.108"),
                        replica("node-109", "192.168.40.109"),
                    ],
                    weight: 0,
                },
            ],
            keepers: vec![
                keeper("node-105", "192.168.40.105"),
                keeper("node-106", "192.168.40.106"),
                keeper("node-108", "192.168.40.108"),
            ],
            users: vec![],
            profiles: vec![],
            databases: vec![DatabaseSpec {
                name: "events".into(),
            }],
            server_settings: BTreeMap::new(),
            tls: TlsConfig::default(),
            client_networks: vec!["192.168.40.0/24".into()],
            max_replica_delay_seconds: 0,
            step_timeout_seconds: 0,
            ports: Ports::default(),
            max_server_memory_ratio: 0.0,
            quotas: vec![],
            dictionaries: vec![],
            named_collections: vec![],
            functions: vec![],
            system_logs: SystemLogs::default(),
            metrics_public: false,
        };
        normalize(&mut spec);
        spec
    }

    #[test]
    fn defaults_fill_names_ports_and_timeouts() {
        let spec = two_by_two();
        assert_eq!(spec.package, "clickhouse");
        assert_eq!(spec.shards[0].shard_name, "1");
        assert_eq!(spec.shards[0].replicas[0].replica_name, "node-105");
        assert_eq!(spec.keepers[2].server_id, 3);
        assert_eq!(spec.ports.tcp_secure, 9440);
        assert_eq!(spec.ports.keeper, 9281);
        assert_eq!(spec.max_replica_delay_seconds, 30);
        assert_eq!(spec.system_logs.trace_log_days, 3);
        assert!(
            validate(&spec).is_empty(),
            "{:?}",
            validate(&spec)
                .iter()
                .map(|i| &i.message)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn validation_catches_the_design_errors() {
        let mut spec = two_by_two();
        spec.keepers.pop();
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("1, 3, or 5")));
        spec = two_by_two();
        spec.keepers[0].learner = true;
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("1, 3, or 5")));
        spec = two_by_two();
        for keeper in &mut spec.keepers {
            keeper.datacenter = "a".into();
        }
        for shard in &mut spec.shards {
            for replica in &mut shard.replicas {
                replica.datacenter = "a".into();
            }
        }
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("majority")));
        spec = two_by_two();
        spec.shards[0].replicas[0].read_only = true;
        spec.shards[0].replicas[1].read_only = true;
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("no writable")));
        spec = two_by_two();
        spec.shards[0].replicas[0].datacenter = "a".into();
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("every replica")));
        spec = two_by_two();
        spec.users.push(UserSpec {
            name: "default".into(),
            password_sha256_hex: "ab".repeat(32),
            networks: vec![],
            profile: String::new(),
            quota: String::new(),
            access_management: false,
        });
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("reserved")));
        spec = two_by_two();
        spec.server_settings
            .insert("listen_host".into(), "0.0.0.0".into());
        assert!(validate(&spec).iter().any(|i| i.message.contains("owns")));
        spec = two_by_two();
        spec.databases.push(DatabaseSpec {
            name: "node-105".into(),
        });
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("database")));
        spec = two_by_two();
        spec.shards[0].replicas[0].iface = "eth0".into();
        assert!(validate(&spec)
            .iter()
            .any(|i| i.message.contains("two interfaces")));
    }

    #[test]
    fn snake_case_manifest_parses_and_unknown_fields_fail() {
        let yaml = r#"
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  shards:
    - replicas:
        - node_id: node-105
          advertise_address: 192.168.40.105
          iface: enp6s0
  keepers:
    - node_id: node-105
      advertise_address: 192.168.40.105
      iface: enp6s0
"#;
        let (name, mut spec) = parse_manifest(yaml).unwrap();
        assert_eq!(name, "analytics");
        normalize(&mut spec);
        assert_eq!(spec.shards[0].replicas[0].node_id, "node-105");
        let bad = yaml.replace("iface: enp6s0", "iface: enp6s0\n          nope: 1");
        assert!(parse_manifest(&bad).is_err());
    }

    #[test]
    fn diff_classifies_reload_restart_upgrade_scale_and_replace() {
        let base = two_by_two();
        assert_eq!(diff(&base, &base).action, PlanAction::Unchanged);
        let mut reload = base.clone();
        reload
            .server_settings
            .insert("max_concurrent_queries".into(), "200".into());
        assert_eq!(
            diff(&base, &reload).action,
            PlanAction::Update { restart: false }
        );
        let mut restart = base.clone();
        restart
            .server_settings
            .insert("max_connections".into(), "128".into());
        assert_eq!(
            diff(&base, &restart).action,
            PlanAction::Update { restart: true }
        );
        let mut upgrade = base.clone();
        upgrade.package = "clickhouse-lts".into();
        assert_eq!(diff(&base, &upgrade).action, PlanAction::Upgrade);
        let mut scale = base.clone();
        scale.shards[0]
            .replicas
            .push(replica("node-110", "192.168.40.110"));
        normalize(&mut scale);
        assert_eq!(diff(&base, &scale).action, PlanAction::Scale);
        let mut replaced = base.clone();
        replaced.keepers[0].learner = true;
        assert_eq!(diff(&base, &replaced).action, PlanAction::Replace);
        assert!(version_is_lower("26.9.1.1", "26.2.4.23"));
        assert!(keeper_window_issue("26.2.4.23", "26.9.1.1", None).is_ok());
        assert!(keeper_window_issue("24.1.1.1", "26.2.1.1", None).is_err());
        assert!(keeper_window_issue("24.1.1.1", "26.2.1.1", Some("24.9.1.1")).is_ok());
    }
}
