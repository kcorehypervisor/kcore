//! Declarative PostgreSQL cluster spec: manifest parsing, defaults,
//! validation, and diff.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use serde_yaml::Value;

pub const KIND: &str = "PostgresqlCluster";
pub const DEFAULT_PACKAGE: &str = "postgresql_17";
pub const PACKAGES: &[&str] = &[
    "postgresql_14",
    "postgresql_15",
    "postgresql_16",
    "postgresql_17",
    "postgresql_18",
];
pub const DEFAULT_TTL: u32 = 30;
pub const DEFAULT_LOOP_WAIT: u32 = 10;
pub const DEFAULT_RETRY_TIMEOUT: u32 = 10;
pub const DEFAULT_MAX_LAG_ON_FAILOVER: u64 = 1_048_576;
pub const DEFAULT_MAX_REPLICATION_LAG: u64 = 16 * 1_048_576;
pub const DEFAULT_STEP_TIMEOUT_SECONDS: u32 = 600;
pub const DEFAULT_ROTATE_BEFORE_DAYS: u32 = 30;
pub const DEFAULT_RETAIN_OLD_DATA_DAYS: u32 = 7;
pub const DEFAULT_PORTS: Ports = Ports {
    postgres: 5432,
    patroni_api: 8008,
    etcd_client: 2479,
    etcd_peer: 2480,
};

pub const MODE_QUORUM: &str = "quorum";
pub const MODE_SYNC: &str = "sync";
pub const MODE_STRICT: &str = "strict";
pub const MODE_ASYNC: &str = "async";

pub const RESERVED_ROLES: &[&str] = &["postgres", "replicator", "rewind", "kcore", "public"];

/// Parameters kcore renders itself. The spec may not set them.
pub const OWNED_PARAMETERS: &[&str] = &[
    "archive_command",
    "archive_mode",
    "cluster_name",
    "config_file",
    "data_directory",
    "hba_file",
    "hot_standby",
    "ident_file",
    "listen_addresses",
    "password_encryption",
    "port",
    "primary_conninfo",
    "primary_slot_name",
    "promote_trigger_file",
    "restore_command",
    "unix_socket_directories",
    "wal_log_hints",
];

/// Parameters whose `pg_settings.context` is `postmaster`: a change needs a
/// restart. The agent's live answer wins; this is the fallback when no member
/// answers.
pub const RESTART_PARAMETERS: &[&str] = &[
    "autovacuum_max_workers",
    "huge_pages",
    "max_connections",
    "max_files_per_process",
    "max_locks_per_transaction",
    "max_logical_replication_workers",
    "max_pred_locks_per_transaction",
    "max_prepared_transactions",
    "max_replication_slots",
    "max_wal_senders",
    "max_worker_processes",
    "shared_buffers",
    "shared_preload_libraries",
    "superuser_reserved_connections",
    "track_commit_timestamp",
    "wal_buffers",
    "wal_level",
];

/// Extensions kcore knows how to provide: `(name, nixpkgs attribute in
/// postgresqlPackages or "" for contrib, needs shared_preload_libraries)`.
pub const EXTENSIONS: &[(&str, &str, bool)] = &[
    ("amcheck", "", false),
    ("btree_gin", "", false),
    ("btree_gist", "", false),
    ("citext", "", false),
    ("fuzzystrmatch", "", false),
    ("hstore", "", false),
    ("intarray", "", false),
    ("ltree", "", false),
    ("pg_buffercache", "", false),
    ("pg_cron", "pg_cron", true),
    ("pg_partman", "pg_partman", false),
    ("pg_prewarm", "", false),
    ("pg_stat_statements", "", true),
    ("pg_trgm", "", false),
    ("pgaudit", "pgaudit", true),
    ("pgcrypto", "", false),
    ("pgstattuple", "", false),
    ("postgis", "postgis", false),
    ("postgres_fdw", "", false),
    ("tablefunc", "", false),
    ("unaccent", "", false),
    ("uuid-ossp", "", false),
    ("vector", "pgvector", false),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct HostVolume {
    pub name: String,
    pub storage_class: String,
    pub volume_group: String,
    pub size_bytes: i64,
    pub fs_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Member {
    pub node_id: String,
    pub advertise_address: String,
    pub iface: String,
    /// "primary" marks the bootstrap node. Ignored after create.
    pub role: String,
    pub witness: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_volume: Option<HostVolume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wal_volume: Option<HostVolume>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etcd_volume: Option<HostVolume>,
    pub no_failover: bool,
    pub no_load_balance: bool,
    pub failover_priority: u32,
    pub datacenter: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Replication {
    pub mode: String,
    pub sync_count: u32,
    pub maximum_lag_on_failover_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Patroni {
    pub ttl: u32,
    pub loop_wait: u32,
    pub retry_timeout: u32,
    pub watchdog: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Vip {
    /// `a.b.c.d/nn`
    pub address: String,
    pub virtual_router_id: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Role {
    pub name: String,
    pub login: bool,
    /// A SCRAM-SHA-256 verifier. Empty with `login` means kcore generates a
    /// password and returns it from GetPostgresqlClusterCredentials.
    pub password_scram: String,
    pub create_db: bool,
    pub connection_limit: i32,
    pub member_of: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Database {
    pub name: String,
    pub owner: String,
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Tls {
    pub disabled: bool,
    pub rotate_before_days: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Ports {
    pub postgres: u16,
    pub patroni_api: u16,
    pub etcd_client: u16,
    pub etcd_peer: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct ClusterSpec {
    pub package: String,
    pub members: Vec<Member>,
    pub replication: Replication,
    pub patroni: Patroni,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vip: Option<Vip>,
    pub parameters: BTreeMap<String, String>,
    pub roles: Vec<Role>,
    pub databases: Vec<Database>,
    pub client_networks: Vec<String>,
    pub tls: Tls,
    pub ports: Ports,
    /// MemoryMax on patroni.service. 0: the controller fills it from the
    /// smallest member's RAM at create.
    pub memory_limit_bytes: u64,
    pub max_replication_lag_bytes: u64,
    pub step_timeout_seconds: u32,
    pub allow_dc_majority: bool,
    pub retain_old_data_days: u32,
}

impl ClusterSpec {
    pub fn postgres_members(&self) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(|m| !m.witness)
    }

    pub fn member(&self, node_id: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.node_id == node_id)
    }

    /// The bootstrap member: the one marked `primary`, else the first
    /// PostgreSQL member.
    pub fn bootstrap_node(&self) -> Option<&str> {
        self.postgres_members()
            .find(|m| m.role == "primary")
            .or_else(|| self.postgres_members().next())
            .map(|m| m.node_id.as_str())
    }

    pub fn standby_count(&self) -> usize {
        self.postgres_members().count().saturating_sub(1)
    }

    pub fn major(&self) -> u32 {
        major_of(&self.package).unwrap_or(0)
    }
}

pub fn major_of(package: &str) -> Option<u32> {
    package.strip_prefix("postgresql_")?.parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub blocking: bool,
    pub message: String,
}

impl Issue {
    fn error(message: impl Into<String>) -> Self {
        Self {
            blocking: true,
            message: message.into(),
        }
    }

    fn warning(message: impl Into<String>) -> Self {
        Self {
            blocking: false,
            message: message.into(),
        }
    }
}

pub fn split_issues(issues: &[Issue]) -> (Vec<String>, Vec<String>) {
    let mut blocking = Vec::new();
    let mut warnings = Vec::new();
    for issue in issues {
        if issue.blocking {
            blocking.push(issue.message.clone());
        } else {
            warnings.push(issue.message.clone());
        }
    }
    (blocking, warnings)
}

/// `kind: PostgresqlCluster` YAML to `(name, spec)`. snake_case keys are
/// accepted and turned into camelCase, except inside `parameters`, whose keys
/// are PostgreSQL names.
pub fn parse_manifest(yaml: &str) -> Result<(String, ClusterSpec), String> {
    let value: Value = serde_yaml::from_str(yaml).map_err(|e| e.to_string())?;
    let value = camelize(value);
    let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind != KIND {
        return Err(format!("expected kind {KIND}, got {kind}"));
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
    if !valid_identifier(&name) {
        return Err(format!(
            "cluster name {name} must match [a-z_][a-z0-9_]{{0,62}}"
        ));
    }
    let spec = value.get("spec").cloned().unwrap_or(Value::Null);
    let spec: ClusterSpec = serde_yaml::from_value(spec).map_err(|e| e.to_string())?;
    Ok((name, spec))
}

fn camelize(value: Value) -> Value {
    match value {
        Value::Mapping(map) => {
            let mut out = serde_yaml::Mapping::new();
            for (key, child) in map {
                let (key, keep) = match key {
                    Value::String(s) => {
                        let camel = to_camel(&s);
                        let keep = camel == "parameters";
                        (Value::String(camel), keep)
                    }
                    other => (other, false),
                };
                out.insert(key, if keep { child } else { camelize(child) });
            }
            Value::Mapping(out)
        }
        Value::Sequence(items) => Value::Sequence(items.into_iter().map(camelize).collect()),
        other => other,
    }
}

fn to_camel(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut upper = false;
    for ch in s.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn normalize(spec: &mut ClusterSpec) {
    if spec.package.is_empty() {
        spec.package = DEFAULT_PACKAGE.into();
    }
    let standbys = spec.standby_count();
    if spec.replication.mode.is_empty() {
        spec.replication.mode = if standbys >= 2 {
            MODE_QUORUM
        } else {
            MODE_ASYNC
        }
        .into();
    }
    if spec.replication.sync_count == 0 {
        spec.replication.sync_count = 1;
    }
    if spec.replication.maximum_lag_on_failover_bytes == 0 {
        spec.replication.maximum_lag_on_failover_bytes = DEFAULT_MAX_LAG_ON_FAILOVER;
    }
    if spec.patroni.ttl == 0 {
        spec.patroni.ttl = DEFAULT_TTL;
    }
    if spec.patroni.loop_wait == 0 {
        spec.patroni.loop_wait = DEFAULT_LOOP_WAIT;
    }
    if spec.patroni.retry_timeout == 0 {
        spec.patroni.retry_timeout = DEFAULT_RETRY_TIMEOUT;
    }
    if spec.patroni.watchdog.is_empty() {
        spec.patroni.watchdog = "off".into();
    }
    if spec.tls.rotate_before_days == 0 {
        spec.tls.rotate_before_days = DEFAULT_ROTATE_BEFORE_DAYS;
    }
    let ports = &mut spec.ports;
    if ports.postgres == 0 {
        ports.postgres = DEFAULT_PORTS.postgres;
    }
    if ports.patroni_api == 0 {
        ports.patroni_api = DEFAULT_PORTS.patroni_api;
    }
    if ports.etcd_client == 0 {
        ports.etcd_client = DEFAULT_PORTS.etcd_client;
    }
    if ports.etcd_peer == 0 {
        ports.etcd_peer = DEFAULT_PORTS.etcd_peer;
    }
    if spec.max_replication_lag_bytes == 0 {
        spec.max_replication_lag_bytes = DEFAULT_MAX_REPLICATION_LAG;
    }
    if spec.step_timeout_seconds == 0 {
        spec.step_timeout_seconds = DEFAULT_STEP_TIMEOUT_SECONDS;
    }
    if spec.retain_old_data_days == 0 {
        spec.retain_old_data_days = DEFAULT_RETAIN_OLD_DATA_DAYS;
    }
    if let Some(vip) = spec.vip.as_mut() {
        if vip.virtual_router_id == 0 {
            vip.virtual_router_id = 61;
        }
    }
    for member in &mut spec.members {
        if member.failover_priority == 0 {
            member.failover_priority = 1;
        }
        if member.role.is_empty() && !member.witness {
            member.role = "replica".into();
        }
        for volume in [
            member.data_volume.as_mut(),
            member.wal_volume.as_mut(),
            member.etcd_volume.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            if volume.fs_type.is_empty() {
                volume.fs_type = "xfs".into();
            }
            if volume.storage_class.is_empty() {
                volume.storage_class = "lvm".into();
            }
        }
    }
    if !spec.members.iter().any(|m| m.role == "primary") {
        if let Some(first) = spec.members.iter_mut().find(|m| !m.witness) {
            first.role = "primary".into();
        }
    }
    for role in &mut spec.roles {
        if role.connection_limit == 0 {
            role.connection_limit = -1;
        }
    }
    for db in &mut spec.databases {
        if db.owner.is_empty() {
            db.owner = "postgres".into();
        }
    }
}

/// Unquoted PostgreSQL identifier kcore accepts for cluster, role and
/// database names.
pub fn valid_identifier(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

pub fn valid_parameter_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'.')
}

pub fn valid_parameter_value(value: &str) -> bool {
    value.len() <= 1024 && !value.chars().any(|c| c.is_control())
}

/// Node ids and interface names: `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`.
pub fn valid_node_token(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn valid_scram_verifier(s: &str) -> bool {
    s.starts_with("SCRAM-SHA-256$")
        && s.len() < 512
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "$:+/=-".contains(c))
}

pub fn extension(name: &str) -> Option<(&'static str, bool)> {
    EXTENSIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, attr, preload)| (*attr, *preload))
}

/// Nixpkgs attributes the module adds with `withPackages`.
pub fn extension_packages(spec: &ClusterSpec) -> Vec<String> {
    let mut out = BTreeSet::new();
    for db in &spec.databases {
        for ext in &db.extensions {
            if let Some((attr, _)) = extension(ext) {
                if !attr.is_empty() {
                    out.insert(attr.to_string());
                }
            }
        }
    }
    out.into_iter().collect()
}

/// Libraries the declared extensions need preloaded.
pub fn preload_libraries(spec: &ClusterSpec) -> Vec<String> {
    let mut out = BTreeSet::new();
    for db in &spec.databases {
        for ext in &db.extensions {
            if let Some((_, true)) = extension(ext) {
                out.insert(ext.clone());
            }
        }
    }
    out.into_iter().collect()
}

pub fn parse_ipv4(s: &str) -> Option<Ipv4Addr> {
    s.parse().ok()
}

/// `a.b.c.d` or `a.b.c.d/nn`.
pub fn parse_cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    match s.split_once('/') {
        Some((addr, len)) => {
            let len: u8 = len.parse().ok()?;
            (len <= 32).then_some(())?;
            Some((addr.parse().ok()?, len))
        }
        None => Some((s.parse().ok()?, 32)),
    }
}

fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, prefix: u8) -> bool {
    if prefix == 0 {
        return true;
    }
    let mask = u32::MAX << (32 - u32::from(prefix));
    (u32::from(a) & mask) == (u32::from(b) & mask)
}

/// Every problem with a normalized spec. Blocking issues refuse plan, create
/// and update; warnings are printed.
pub fn validate(name: &str, spec: &ClusterSpec) -> Vec<Issue> {
    let mut issues = Vec::new();
    if !valid_identifier(name) {
        issues.push(Issue::error(format!(
            "cluster name {name} must match [a-z_][a-z0-9_]{{0,62}}"
        )));
    }
    if spec.package == "postgresql" {
        issues.push(Issue::error(
            "package must be a versioned attribute (postgresql_14 .. postgresql_18): the unversioned alias would turn a pin move into a major upgrade",
        ));
    } else if !PACKAGES.contains(&spec.package.as_str()) {
        issues.push(Issue::error(format!(
            "package {} is not one of {}",
            spec.package,
            PACKAGES.join(", ")
        )));
    }

    validate_members(spec, &mut issues);
    validate_replication(spec, &mut issues);
    validate_parameters(spec, &mut issues);
    validate_roles_and_databases(spec, &mut issues);

    let p = &spec.patroni;
    if p.loop_wait + 2 * p.retry_timeout > p.ttl {
        issues.push(Issue::error(format!(
            "patroni: loopWait + 2*retryTimeout ({}) must not exceed ttl ({})",
            p.loop_wait + 2 * p.retry_timeout,
            p.ttl
        )));
    }
    match p.watchdog.as_str() {
        "off" => {}
        "automatic" | "required" => issues.push(Issue::warning(
            "patroni.watchdog: a watchdog reset reboots the whole host, guests included",
        )),
        other => issues.push(Issue::error(format!(
            "patroni.watchdog {other} is not off, automatic or required"
        ))),
    }

    let ports = [
        spec.ports.postgres,
        spec.ports.patroni_api,
        spec.ports.etcd_client,
        spec.ports.etcd_peer,
    ];
    let distinct: BTreeSet<_> = ports.iter().collect();
    if distinct.len() != ports.len() || ports.contains(&0) {
        issues.push(Issue::error("ports must be non-zero and pairwise distinct"));
    }
    if [spec.ports.etcd_client, spec.ports.etcd_peer]
        .iter()
        .any(|p| *p == 2379 || *p == 2380)
    {
        issues.push(Issue::warning(
            "etcd ports 2379/2380 collide with a Kubernetes control-plane member on the same node",
        ));
    }

    for net in &spec.client_networks {
        if parse_cidr(net).is_none() {
            issues.push(Issue::error(format!(
                "clientNetworks entry {net} is not an IPv4 address or CIDR"
            )));
        } else if net.ends_with("/0") {
            issues.push(Issue::warning(format!(
                "clientNetworks entry {net} opens PostgreSQL to every address"
            )));
        }
    }

    if let Some(vip) = &spec.vip {
        match parse_cidr(&vip.address) {
            Some((addr, prefix)) if vip.address.contains('/') => {
                for m in spec.postgres_members() {
                    if let Some(member) = parse_ipv4(&m.advertise_address) {
                        if !same_subnet(addr, member, prefix) {
                            issues.push(Issue::error(format!(
                                "vip {} is not on the subnet of {} ({})",
                                vip.address, m.node_id, m.advertise_address
                            )));
                        }
                    }
                }
                if spec
                    .members
                    .iter()
                    .any(|m| m.advertise_address == addr.to_string())
                {
                    issues.push(Issue::error("vip address is a member address"));
                }
            }
            _ => issues.push(Issue::error(format!(
                "vip.address {} must be an IPv4 CIDR like 192.168.40.150/24",
                vip.address
            ))),
        }
        if !(1..=255).contains(&vip.virtual_router_id) {
            issues.push(Issue::error("vip.virtualRouterId must be 1..255"));
        }
    }
    issues
}

fn validate_members(spec: &ClusterSpec, issues: &mut Vec<Issue>) {
    let pg: Vec<&Member> = spec.postgres_members().collect();
    if pg.is_empty() {
        issues.push(Issue::error("at least one PostgreSQL member is required"));
        return;
    }
    if pg.len() > 5 {
        issues.push(Issue::error(
            "at most five PostgreSQL members are supported",
        ));
    }
    if pg.len() == 1 {
        issues.push(Issue::warning(
            "one PostgreSQL member has no failover; every restart is downtime",
        ));
    }
    let voters = spec.members.len();
    if voters == 2 || voters == 4 {
        issues.push(Issue::error(format!(
            "{voters} members give an even etcd vote; add a witness member"
        )));
    }
    if voters > 5 {
        issues.push(Issue::error(
            "at most five etcd voters; members added later with scale join as non-voters",
        ));
    }
    let primaries = spec.members.iter().filter(|m| m.role == "primary").count();
    if primaries > 1 {
        issues.push(Issue::error("only one member may have role primary"));
    }
    if spec
        .members
        .iter()
        .any(|m| m.witness && m.role == "primary")
    {
        issues.push(Issue::error("a witness cannot be the bootstrap primary"));
    }
    let mut ids = BTreeSet::new();
    let mut addrs = BTreeSet::new();
    for m in &spec.members {
        if !valid_node_token(&m.node_id) {
            issues.push(Issue::error(format!(
                "node id {:?} is not valid",
                m.node_id
            )));
        }
        if !ids.insert(m.node_id.clone()) {
            issues.push(Issue::error(format!("node {} is listed twice", m.node_id)));
        }
        if parse_ipv4(&m.advertise_address).is_none() {
            issues.push(Issue::error(format!(
                "{}: advertiseAddress {:?} is not an IPv4 address",
                m.node_id, m.advertise_address
            )));
        } else if !addrs.insert(m.advertise_address.clone()) {
            issues.push(Issue::error(format!(
                "address {} is used by two members",
                m.advertise_address
            )));
        }
        if !valid_node_token(&m.iface) {
            issues.push(Issue::error(format!("{}: iface is required", m.node_id)));
        } else if m.iface == "br0" || m.iface.starts_with("kbr-") {
            issues.push(Issue::error(format!(
                "{}: iface {} is a VM bridge",
                m.node_id, m.iface
            )));
        }
        if !matches!(m.role.as_str(), "" | "primary" | "replica") {
            issues.push(Issue::error(format!(
                "{}: role {} is not primary or replica",
                m.node_id, m.role
            )));
        }
        let mut names = BTreeSet::new();
        for (label, volume) in [
            ("dataVolume", &m.data_volume),
            ("walVolume", &m.wal_volume),
            ("etcdVolume", &m.etcd_volume),
        ] {
            let Some(v) = volume else { continue };
            if m.witness && label != "etcdVolume" {
                issues.push(Issue::error(format!(
                    "{}: a witness has no {label}",
                    m.node_id
                )));
            }
            if !valid_node_token(&v.name) || !valid_node_token(&v.volume_group) {
                issues.push(Issue::error(format!(
                    "{}: {label} needs a valid name and volumeGroup",
                    m.node_id
                )));
            }
            if !matches!(v.storage_class.as_str(), "lvm" | "zfs") {
                issues.push(Issue::error(format!(
                    "{}: {label}.storageClass must be lvm or zfs",
                    m.node_id
                )));
            }
            if !matches!(v.fs_type.as_str(), "xfs" | "ext4") {
                issues.push(Issue::error(format!(
                    "{}: {label}.fsType must be xfs or ext4",
                    m.node_id
                )));
            }
            if v.size_bytes < 1 << 30 {
                issues.push(Issue::error(format!(
                    "{}: {label} must be at least 1 GiB",
                    m.node_id
                )));
            }
            if !names.insert(v.name.clone()) {
                issues.push(Issue::error(format!(
                    "{}: {label} reuses volume name {}",
                    m.node_id, v.name
                )));
            }
        }
        if m.data_volume.as_ref().is_some_and(|v| v.fs_type == "ext4") {
            issues.push(Issue::warning(format!(
                "{}: ext4 has no reflink; a major upgrade will use pg_upgrade --link and cannot be rolled back",
                m.node_id
            )));
        }
    }
    if pg.iter().all(|m| m.no_failover) && pg.len() > 1 {
        issues.push(Issue::error(
            "no member can be promoted: every member has noFailover",
        ));
    }
    if pg.iter().any(|m| m.role == "primary" && m.no_failover) {
        issues.push(Issue::warning(
            "the bootstrap primary has noFailover; it will not be re-promoted after a failover",
        ));
    }

    let labelled = spec
        .members
        .iter()
        .filter(|m| !m.datacenter.is_empty())
        .count();
    if labelled != 0 && labelled != spec.members.len() {
        issues.push(Issue::error(
            "datacenter is set on some members only; set it on every member or none",
        ));
    }
    if labelled == spec.members.len() && labelled > 0 {
        let mut per_dc: BTreeMap<&str, usize> = BTreeMap::new();
        for m in &spec.members {
            *per_dc.entry(m.datacenter.as_str()).or_default() += 1;
        }
        if per_dc.len() > 1 && !spec.allow_dc_majority {
            for (dc, count) in &per_dc {
                if count * 2 > spec.members.len() {
                    issues.push(Issue::error(format!(
                        "datacenter {dc} holds a majority of etcd voters; losing it stops failover (set allowDcMajority to accept)"
                    )));
                }
            }
        }
    }
}

fn validate_replication(spec: &ClusterSpec, issues: &mut Vec<Issue>) {
    let standbys = spec.standby_count() as u32;
    let mode = spec.replication.mode.as_str();
    match mode {
        MODE_QUORUM | MODE_SYNC | MODE_STRICT => {
            if standbys == 0 {
                issues.push(Issue::error(format!(
                    "replication.mode {mode} needs at least one standby"
                )));
            } else if spec.replication.sync_count > standbys {
                issues.push(Issue::error(format!(
                    "replication.syncCount {} exceeds the {standbys} standbys",
                    spec.replication.sync_count
                )));
            } else if mode != MODE_STRICT && spec.replication.sync_count == standbys {
                issues.push(Issue::warning(
                    "syncCount equals the standby count: losing one standby drops the cluster to asynchronous commits",
                ));
            }
        }
        MODE_ASYNC => issues.push(Issue::warning(format!(
            "replication.mode async: a failover can lose up to {} bytes of acknowledged commits",
            spec.replication.maximum_lag_on_failover_bytes
        ))),
        other => issues.push(Issue::error(format!(
            "replication.mode {other} is not quorum, sync, strict or async"
        ))),
    }
}

fn validate_parameters(spec: &ClusterSpec, issues: &mut Vec<Issue>) {
    for (key, value) in &spec.parameters {
        if !valid_parameter_key(key) {
            issues.push(Issue::error(format!(
                "parameter {key:?} is not a valid name"
            )));
            continue;
        }
        if !valid_parameter_value(value) {
            issues.push(Issue::error(format!(
                "parameter {key} has an invalid value"
            )));
        }
        if OWNED_PARAMETERS.contains(&key.as_str()) || key.starts_with("ssl") {
            issues.push(Issue::error(format!("parameter {key} is owned by kcore")));
        }
        if key == "wal_level" && value != "replica" && value != "logical" {
            issues.push(Issue::error("wal_level may only be replica or logical"));
        }
    }
}

fn validate_roles_and_databases(spec: &ClusterSpec, issues: &mut Vec<Issue>) {
    let mut roles = BTreeSet::new();
    for role in &spec.roles {
        if !valid_identifier(&role.name) || role.name.starts_with("pg_") {
            issues.push(Issue::error(format!(
                "role name {:?} is not valid",
                role.name
            )));
        }
        if RESERVED_ROLES.contains(&role.name.as_str()) {
            issues.push(Issue::error(format!("role {} is reserved", role.name)));
        }
        if !roles.insert(role.name.as_str()) {
            issues.push(Issue::error(format!("role {} is listed twice", role.name)));
        }
        if !role.password_scram.is_empty() && !valid_scram_verifier(&role.password_scram) {
            issues.push(Issue::error(format!(
                "role {}: passwordScram must be a SCRAM-SHA-256 verifier, not a password",
                role.name
            )));
        }
        if role.connection_limit < -1 {
            issues.push(Issue::error(format!(
                "role {}: connectionLimit must be -1 or more",
                role.name
            )));
        }
    }
    for role in &spec.roles {
        for parent in &role.member_of {
            if !roles.contains(parent.as_str()) {
                issues.push(Issue::error(format!(
                    "role {} is a member of unknown role {parent}",
                    role.name
                )));
            }
        }
    }
    let mut dbs = BTreeSet::new();
    for db in &spec.databases {
        if !valid_identifier(&db.name)
            || matches!(db.name.as_str(), "postgres" | "template0" | "template1")
        {
            issues.push(Issue::error(format!(
                "database name {:?} is not valid",
                db.name
            )));
        }
        if !dbs.insert(db.name.as_str()) {
            issues.push(Issue::error(format!(
                "database {} is listed twice",
                db.name
            )));
        }
        if db.owner != "postgres" && !roles.contains(db.owner.as_str()) {
            issues.push(Issue::error(format!(
                "database {} is owned by unknown role {}",
                db.name, db.owner
            )));
        }
        for ext in &db.extensions {
            if extension(ext).is_none() {
                issues.push(Issue::error(format!(
                    "database {}: extension {ext} is not one kcore provides",
                    db.name
                )));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Create,
    Unchanged,
    /// Nix apply plus a dynamic-config patch or SQL; no restart.
    UpdateReload,
    /// A rolling restart, leader last after a switchover.
    UpdateRestart,
    UpgradeMajor,
    /// Members added or removed.
    Scale,
    /// An immutable field changed; a new cluster is the path.
    Replace,
}

impl PlanAction {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanAction::Create => "create",
            PlanAction::Unchanged => "unchanged",
            PlanAction::UpdateReload => "update",
            PlanAction::UpdateRestart => "update",
            PlanAction::UpgradeMajor => "upgrade-major",
            PlanAction::Scale => "scale",
            PlanAction::Replace => "replace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDiff {
    pub action: PlanAction,
    pub changed_fields: Vec<String>,
    /// Parameter name and "reload" or "restart".
    pub parameters: Vec<(String, String)>,
    pub added_members: Vec<String>,
    pub removed_members: Vec<String>,
    /// Dynamic config (DCS) changed.
    pub dcs_changed: bool,
    /// Roles, databases or extensions changed.
    pub sql_changed: bool,
    pub issues: Vec<Issue>,
}

/// Classify the change from `old` to `new`. Both are normalized. `restart`
/// names the parameters a live member reported as postmaster-context; empty
/// uses [`RESTART_PARAMETERS`].
pub fn diff(old: Option<&ClusterSpec>, new: &ClusterSpec, restart: &[String]) -> PlanDiff {
    let mut out = PlanDiff {
        action: PlanAction::Create,
        changed_fields: Vec::new(),
        parameters: Vec::new(),
        added_members: Vec::new(),
        removed_members: Vec::new(),
        dcs_changed: false,
        sql_changed: false,
        issues: Vec::new(),
    };
    let Some(old) = old else {
        return out;
    };
    let mut fields = Vec::new();
    let mut replace = false;
    let mut restart_needed = false;

    if old.tls.disabled != new.tls.disabled {
        fields.push("tls.disabled".to_string());
        replace = true;
    }
    if old.ports != new.ports {
        fields.push("ports".to_string());
        replace = true;
    }
    let upgrade = old.package != new.package;
    if upgrade {
        fields.push("package".to_string());
        if new.major() < old.major() {
            out.issues.push(Issue::error(format!(
                "package {} is an older major than {}; PostgreSQL does not downgrade",
                new.package, old.package
            )));
        }
    }

    let old_ids: BTreeSet<&str> = old.members.iter().map(|m| m.node_id.as_str()).collect();
    let new_ids: BTreeSet<&str> = new.members.iter().map(|m| m.node_id.as_str()).collect();
    out.added_members = new_ids
        .difference(&old_ids)
        .map(|s| s.to_string())
        .collect();
    out.removed_members = old_ids
        .difference(&new_ids)
        .map(|s| s.to_string())
        .collect();
    if !out.added_members.is_empty() || !out.removed_members.is_empty() {
        fields.push("members".to_string());
    }
    for m in &new.members {
        let Some(prev) = old.member(&m.node_id) else {
            continue;
        };
        let path = |f: &str| format!("members[{}].{f}", m.node_id);
        if prev.advertise_address != m.advertise_address || prev.iface != m.iface {
            fields.push(path("advertiseAddress"));
            replace = true;
        }
        if prev.witness != m.witness {
            fields.push(path("witness"));
            replace = true;
        }
        if prev.data_volume != m.data_volume
            || prev.wal_volume != m.wal_volume
            || prev.etcd_volume != m.etcd_volume
        {
            fields.push(path("volumes"));
            replace = true;
        }
        if prev.no_failover != m.no_failover
            || prev.no_load_balance != m.no_load_balance
            || prev.failover_priority != m.failover_priority
        {
            fields.push(path("tags"));
        }
        if prev.datacenter != m.datacenter {
            fields.push(path("datacenter"));
        }
    }

    if old.replication != new.replication {
        fields.push("replication".to_string());
        out.dcs_changed = true;
    }
    if old.patroni.ttl != new.patroni.ttl
        || old.patroni.loop_wait != new.patroni.loop_wait
        || old.patroni.retry_timeout != new.patroni.retry_timeout
    {
        fields.push("patroni".to_string());
        out.dcs_changed = true;
    }
    if old.patroni.watchdog != new.patroni.watchdog {
        fields.push("patroni.watchdog".to_string());
        restart_needed = true;
    }
    let keys: BTreeSet<&String> = old.parameters.keys().chain(new.parameters.keys()).collect();
    for key in keys {
        if old.parameters.get(key) != new.parameters.get(key) {
            let class = if restart.is_empty() {
                RESTART_PARAMETERS.contains(&key.as_str())
            } else {
                restart.iter().any(|k| k == key)
            };
            out.parameters.push((
                key.clone(),
                if class { "restart" } else { "reload" }.to_string(),
            ));
            restart_needed |= class;
            out.dcs_changed = true;
        }
    }
    if !out.parameters.is_empty() {
        fields.push("parameters".to_string());
    }
    if preload_libraries(old) != preload_libraries(new) {
        fields.push("parameters.shared_preload_libraries".to_string());
        restart_needed = true;
        out.dcs_changed = true;
    }
    if extension_packages(old) != extension_packages(new) {
        fields.push("extensionPackages".to_string());
    }
    if old.memory_limit_bytes != new.memory_limit_bytes {
        fields.push("memoryLimitBytes".to_string());
        restart_needed = true;
        out.dcs_changed = true;
    }
    if old.roles != new.roles {
        fields.push("roles".to_string());
        out.sql_changed = true;
    }
    if old.databases != new.databases {
        fields.push("databases".to_string());
        out.sql_changed = true;
    }
    if old.client_networks != new.client_networks {
        fields.push("clientNetworks".to_string());
    }
    if old.vip != new.vip {
        fields.push("vip".to_string());
    }
    if old.tls.rotate_before_days != new.tls.rotate_before_days
        || old.max_replication_lag_bytes != new.max_replication_lag_bytes
        || old.step_timeout_seconds != new.step_timeout_seconds
        || old.allow_dc_majority != new.allow_dc_majority
        || old.retain_old_data_days != new.retain_old_data_days
    {
        fields.push("controller settings".to_string());
    }

    out.action = if replace {
        PlanAction::Replace
    } else if upgrade {
        PlanAction::UpgradeMajor
    } else if !out.added_members.is_empty() || !out.removed_members.is_empty() {
        PlanAction::Scale
    } else if fields.is_empty() {
        PlanAction::Unchanged
    } else if restart_needed {
        PlanAction::UpdateRestart
    } else {
        PlanAction::UpdateReload
    };
    if upgrade && fields.len() > 1 {
        out.issues.push(Issue::error(
            "a package change cannot be combined with other changes; upgrade first",
        ));
    }
    if out.action == PlanAction::Scale
        && fields
            .iter()
            .any(|f| f != "members" && !f.starts_with("members["))
    {
        out.issues.push(Issue::error(
            "adding or removing members cannot be combined with other changes",
        ));
    }
    out.changed_fields = fields;
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const MANIFEST: &str = r#"
kind: PostgresqlCluster
metadata:
  name: orders
spec:
  package: postgresql_17
  members:
    - nodeId: node-105
      role: primary
      advertiseAddress: 192.168.40.105
      iface: enp6s0
      dataVolume: { name: pg-orders, storageClass: lvm, volumeGroup: vg0, sizeBytes: 214748364800, fsType: xfs }
    - node_id: node-106
      advertise_address: 192.168.40.106
      iface: enp6s0
    - nodeId: node-108
      advertiseAddress: 192.168.40.108
      iface: enp6s0
  vip:
    address: 192.168.40.150/24
  clientNetworks: [192.168.40.0/24]
  parameters:
    max_connections: "300"
    work_mem: "16MB"
  roles:
    - name: app
      login: true
  databases:
    - name: orders
      owner: app
      extensions: [pg_stat_statements, pgcrypto]
"#;

    pub(crate) fn sample() -> (String, ClusterSpec) {
        let (name, mut spec) = parse_manifest(MANIFEST).unwrap();
        normalize(&mut spec);
        (name, spec)
    }

    #[test]
    fn manifest_accepts_snake_case_and_keeps_parameter_keys() {
        let (name, spec) = sample();
        assert_eq!(name, "orders");
        assert_eq!(spec.members[1].node_id, "node-106");
        assert_eq!(spec.parameters["max_connections"], "300");
        assert_eq!(spec.replication.mode, MODE_QUORUM);
        assert_eq!(spec.bootstrap_node(), Some("node-105"));
        assert!(
            validate(&name, &spec).iter().all(|i| !i.blocking),
            "{:?}",
            validate(&name, &spec)
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_wrong_kind() {
        let bad = MANIFEST.replace(
            "iface: enp6s0\n      dataVolume",
            "ifacee: x\n      dataVolume",
        );
        assert!(parse_manifest(&bad).is_err());
        assert!(parse_manifest(&MANIFEST.replace("PostgresqlCluster", "Postgresql")).is_err());
    }

    #[test]
    fn defaults_follow_standby_count() {
        let (_, mut spec) = sample();
        spec.members.truncate(1);
        spec.replication.mode.clear();
        normalize(&mut spec);
        assert_eq!(spec.replication.mode, MODE_ASYNC);
        assert_eq!(spec.ports, DEFAULT_PORTS);
        assert_eq!(spec.patroni.ttl, 30);
    }

    fn blocking(spec: &ClusterSpec) -> Vec<String> {
        validate("orders", spec)
            .into_iter()
            .filter(|i| i.blocking)
            .map(|i| i.message)
            .collect()
    }

    #[test]
    fn two_members_need_a_witness() {
        let (_, mut spec) = sample();
        spec.members.pop();
        spec.replication.mode = MODE_SYNC.into();
        assert!(blocking(&spec).iter().any(|m| m.contains("witness")));
        spec.members.push(Member {
            node_id: "node-107".into(),
            advertise_address: "192.168.40.107".into(),
            iface: "enp6s0".into(),
            witness: true,
            ..Default::default()
        });
        assert!(blocking(&spec).is_empty(), "{:?}", blocking(&spec));
    }

    #[test]
    fn owned_parameters_and_bad_values_are_refused() {
        let (_, mut spec) = sample();
        spec.parameters
            .insert("listen_addresses".into(), "*".into());
        spec.parameters.insert("ssl_ciphers".into(), "x".into());
        spec.parameters.insert("wal_level".into(), "minimal".into());
        spec.parameters.insert("Bad Key".into(), "1".into());
        spec.parameters.insert("work_mem".into(), "1\nMB".into());
        let errors = blocking(&spec);
        assert!(errors.iter().any(|m| m.contains("listen_addresses")));
        assert!(errors.iter().any(|m| m.contains("ssl_ciphers")));
        assert!(errors.iter().any(|m| m.contains("wal_level")));
        assert!(errors.iter().any(|m| m.contains("Bad Key")));
        assert!(errors.iter().any(|m| m.contains("work_mem")));
    }

    #[test]
    fn names_and_injection_strings_are_refused() {
        let (_, mut spec) = sample();
        spec.roles[0].name = "app\"; DROP".into();
        spec.databases[0].name = "postgres".into();
        spec.databases[0].extensions.push("plpython3u".into());
        spec.members[0].node_id = "node 105".into();
        spec.members[1].iface = "br0".into();
        let errors = blocking(&spec);
        assert!(errors.iter().any(|m| m.contains("role name")));
        assert!(errors.iter().any(|m| m.contains("database name")));
        assert!(errors.iter().any(|m| m.contains("plpython3u")));
        assert!(errors.iter().any(|m| m.contains("node id")));
        assert!(errors.iter().any(|m| m.contains("VM bridge")));
        assert!(!valid_identifier("node-105"));
        assert!(valid_node_token("node-105"));
        assert!(!valid_scram_verifier("hunter2"));
        assert!(valid_scram_verifier(
            "SCRAM-SHA-256$4096:c2FsdA==$c3RvcmVk:c2VydmVy"
        ));
    }

    #[test]
    fn unversioned_package_and_vip_off_subnet_are_refused() {
        let (_, mut spec) = sample();
        spec.package = "postgresql".into();
        spec.vip = Some(Vip {
            address: "10.0.0.5/24".into(),
            virtual_router_id: 61,
        });
        let errors = blocking(&spec);
        assert!(errors.iter().any(|m| m.contains("versioned")));
        assert!(errors.iter().any(|m| m.contains("subnet")));
    }

    #[test]
    fn datacenter_majority_is_refused_unless_accepted() {
        let (_, mut spec) = sample();
        spec.members[0].datacenter = "dc1".into();
        spec.members[1].datacenter = "dc1".into();
        assert!(blocking(&spec)
            .iter()
            .any(|m| m.contains("some members only")));
        spec.members[2].datacenter = "dc2".into();
        assert!(blocking(&spec).iter().any(|m| m.contains("majority")));
        spec.allow_dc_majority = true;
        assert!(blocking(&spec).is_empty());
    }

    #[test]
    fn diff_classifies_changes() {
        let (_, old) = sample();
        assert_eq!(diff(None, &old, &[]).action, PlanAction::Create);
        assert_eq!(diff(Some(&old), &old, &[]).action, PlanAction::Unchanged);

        let mut new = old.clone();
        new.parameters.insert("work_mem".into(), "32MB".into());
        let d = diff(Some(&old), &new, &[]);
        assert_eq!(d.action, PlanAction::UpdateReload);
        assert!(d.dcs_changed);
        assert_eq!(d.parameters, vec![("work_mem".into(), "reload".into())]);

        new.parameters
            .insert("max_connections".into(), "400".into());
        assert_eq!(
            diff(Some(&old), &new, &[]).action,
            PlanAction::UpdateRestart
        );
        // A live answer overrides the static table.
        assert_eq!(
            diff(Some(&old), &new, &["work_mem".into()]).action,
            PlanAction::UpdateRestart
        );

        let mut roles = old.clone();
        roles.roles[0].create_db = true;
        let d = diff(Some(&old), &roles, &[]);
        assert_eq!(d.action, PlanAction::UpdateReload);
        assert!(d.sql_changed && !d.dcs_changed);

        let mut ext = old.clone();
        ext.databases[0].extensions.push("pg_cron".into());
        assert_eq!(
            diff(Some(&old), &ext, &[]).action,
            PlanAction::UpdateRestart
        );

        let mut up = old.clone();
        up.package = "postgresql_18".into();
        let d = diff(Some(&old), &up, &[]);
        assert_eq!(d.action, PlanAction::UpgradeMajor);
        assert!(d.issues.is_empty());
        up.package = "postgresql_16".into();
        assert!(diff(Some(&old), &up, &[]).issues.iter().any(|i| i.blocking));

        let mut ports = old.clone();
        ports.ports.postgres = 5433;
        assert_eq!(diff(Some(&old), &ports, &[]).action, PlanAction::Replace);

        let mut scale = old.clone();
        scale.members.push(Member {
            node_id: "node-110".into(),
            advertise_address: "192.168.40.110".into(),
            iface: "enp6s0".into(),
            ..Default::default()
        });
        let d = diff(Some(&old), &scale, &[]);
        assert_eq!(d.action, PlanAction::Scale);
        assert_eq!(d.added_members, vec!["node-110".to_string()]);
        scale.parameters.insert("work_mem".into(), "1MB".into());
        assert!(diff(Some(&old), &scale, &[])
            .issues
            .iter()
            .any(|i| i.blocking));
    }

    #[test]
    fn bootstrap_role_change_after_create_is_not_a_diff_on_its_own() {
        let (_, old) = sample();
        let mut new = old.clone();
        new.members[0].role = "replica".into();
        new.members[1].role = "primary".into();
        assert_eq!(diff(Some(&old), &new, &[]).action, PlanAction::Unchanged);
    }

    #[test]
    fn extensions_map_to_packages_and_preloads() {
        let (_, mut spec) = sample();
        spec.databases[0].extensions.push("vector".into());
        spec.databases[0].extensions.push("pg_cron".into());
        assert_eq!(extension_packages(&spec), vec!["pg_cron", "pgvector"]);
        assert_eq!(
            preload_libraries(&spec),
            vec!["pg_cron", "pg_stat_statements"]
        );
    }

    proptest::proptest! {
        #[test]
        fn diff_of_a_spec_with_itself_is_unchanged(work_mem in "[0-9]{1,4}MB", mode in "(quorum|sync|async)") {
            let (_, mut spec) = sample();
            spec.parameters.insert("work_mem".into(), work_mem);
            spec.replication.mode = mode;
            proptest::prop_assert_eq!(diff(Some(&spec), &spec, &[]).action, PlanAction::Unchanged);
        }

        #[test]
        fn identifiers_never_contain_quotes(name in "\\PC{0,80}") {
            if valid_identifier(&name) {
                proptest::prop_assert!(!name.contains('"') && !name.contains('\'') && !name.contains(' '));
            }
        }
    }
}
