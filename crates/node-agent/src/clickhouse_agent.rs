//! ClickHouse values, secrets, and SQL for one node.
//!
//! The Nix module lives in the host module set. This agent writes
//! `/etc/nixos/kcore-clickhouse.nix` and files under `/var/lib/kcore-clickhouse`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kcore_clickhouse::nixgen::{
    keeper_data_path, render_disabled, render_fragment, secrets_dir, server_data_path, tls_dir,
    CollectionFragment, DictionaryFragment, Fragment, FunctionFragment, KeeperFragment,
    LogFragment, ProfileFragment, QuotaFragment, QuotaIntervalFragment, RaftPeerFragment,
    RemoteReplica, RemoteShard, ServerFragment, UserFragment,
};
use kcore_clickhouse::ops::{
    backup_access_sql, backup_database_sql, backup_status_sql, create_database_sql,
    create_function_sql, drop_replica_sql, keeper_env, local_parts_sql, reload_config_sql,
    restore_access_sql, restore_database_sql, server_env, set_ttl_sql, verify_clusters_sql,
    BackupTarget, ServerSecrets,
};
use kcore_clickhouse::spec::Ports;

use crate::proto;

pub const VALUES_FILE: &str = "/etc/nixos/kcore-clickhouse.nix";
pub const FLAKE_FILE: &str = "/etc/nixos/flake.nix";
pub const EMPTY_VALUES: &str = "{ ... }:\n{\n}\n";

pub struct Prepared {
    pub nix: String,
    pub cluster: String,
    pub server_env: String,
    pub keeper_env: String,
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub ca_pem: Vec<u8>,
    pub client_password: String,
    pub drop_secrets: bool,
}

pub enum AdminPlan {
    Sql(Vec<String>),
    SyncReplicas { database: String },
    RestoreReadonly { database: String },
}

pub fn prepare(req: &proto::ApplyClickhouseConfigRequest) -> Result<Prepared, String> {
    if req.cluster_name.is_empty() {
        return Err("cluster_name is required".into());
    }
    let nix = if req.enable {
        render_fragment(&fragment_from_apply(req))?
    } else {
        render_disabled().to_string()
    };
    let secrets = req.secrets.clone().unwrap_or_default();
    let users = req.server.as_ref().map(user_digests).unwrap_or_default();
    let material = ServerSecrets {
        default_password_sha256: secrets.default_password_sha256_hex.clone(),
        metrics_password_sha256: secrets.metrics_password_sha256_hex.clone(),
        interserver_password: secrets.interserver_password.clone(),
        cluster_secret: secrets.cluster_secret.clone(),
        keeper_password: keeper_password(&secrets.keeper_secret),
        user_digests: users,
        named_collection_secrets: secrets
            .named_collection_secrets
            .iter()
            .map(|(key, value)| (named_env(key), value.clone()))
            .collect(),
    };
    Ok(Prepared {
        nix,
        cluster: req.cluster_name.clone(),
        server_env: server_env(&material),
        keeper_env: keeper_env(&material),
        cert_pem: secrets.tls_cert_pem,
        key_pem: secrets.tls_key_pem,
        ca_pem: secrets.tls_ca_pem,
        client_password: secrets.default_password,
        drop_secrets: !req.enable,
    })
}

pub fn install(root: &Path, prepared: &Prepared) -> Result<(), String> {
    let values = root.join(VALUES_FILE.trim_start_matches('/'));
    if let Some(parent) = values.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&values, &prepared.nix).map_err(|e| e.to_string())?;
    let flake = root.join(FLAKE_FILE.trim_start_matches('/'));
    if flake.exists() {
        let src = fs::read_to_string(&flake).map_err(|e| e.to_string())?;
        fs::write(&flake, ensure_values_import(&src)).map_err(|e| e.to_string())?;
    }
    let secrets = root.join(secrets_dir(&prepared.cluster).trim_start_matches('/'));
    let tls = root.join(tls_dir(&prepared.cluster).trim_start_matches('/'));
    if prepared.drop_secrets {
        let _ = fs::remove_dir_all(&secrets);
        let _ = fs::remove_dir_all(&tls);
        return Ok(());
    }
    fs::create_dir_all(&secrets).map_err(|e| e.to_string())?;
    fs::create_dir_all(&tls).map_err(|e| e.to_string())?;
    write_private(&secrets.join("server.env"), prepared.server_env.as_bytes())?;
    write_private(&secrets.join("keeper.env"), prepared.keeper_env.as_bytes())?;
    write_private(
        &secrets.join("client.password"),
        prepared.client_password.as_bytes(),
    )?;
    write_private(&tls.join("node.key"), &prepared.key_pem)?;
    fs::write(tls.join("node.crt"), &prepared.cert_pem).map_err(|e| e.to_string())?;
    fs::write(tls.join("ca.crt"), &prepared.ca_pem).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn ensure_values_import(flake: &str) -> String {
    if flake.contains("./kcore-clickhouse.nix") {
        return flake.to_string();
    }
    let needle = "./kcore-ceph.nix";
    if let Some(idx) = flake.find(needle) {
        let mut out = flake.to_string();
        out.insert_str(idx, "./kcore-clickhouse.nix\n        ");
        return out;
    }
    flake.to_string()
}

pub fn preflight(
    req: &proto::CheckClickhousePreflightRequest,
) -> Vec<proto::ClickhousePreflightIssue> {
    let mut issues = Vec::new();
    if req.cluster_name.is_empty() {
        issues.push(issue("error", "cluster", "cluster_name is required"));
    }
    if req.iface.is_empty() || req.iface == "br0" || req.iface.starts_with("kbr-") {
        issues.push(issue(
            "error",
            "iface",
            "iface must be set and must not be br0 or a kbr bridge",
        ));
    }
    if req.advertise_address.parse::<std::net::IpAddr>().is_err() {
        issues.push(issue(
            "error",
            "address",
            "advertise_address must be an IP address",
        ));
    }
    if req.package != "clickhouse" && req.package != "clickhouse-lts" && !req.package.is_empty() {
        issues.push(issue(
            "error",
            "package",
            "package must be clickhouse or clickhouse-lts",
        ));
    }
    let ports = req.ports.unwrap_or_default();
    if !ports_distinct(
        &ports,
        req.tls_enabled,
        req.keeper.is_some(),
        req.server.as_ref().is_some_and(|s| s.metrics_public),
    ) {
        issues.push(issue("error", "port", "ports must be pairwise distinct"));
    }
    if let Some(server) = &req.server {
        if server.shard_name.is_empty() || server.replica_name.is_empty() {
            issues.push(issue(
                "error",
                "server",
                "server shard and replica are required",
            ));
        }
        if !server.data_path.is_empty()
            && !server.data_path.starts_with("/var/lib/kcore-clickhouse/")
        {
            issues.push(issue(
                "error",
                "volume",
                "server data path must be under /var/lib/kcore-clickhouse/",
            ));
        }
    }
    if let Some(keeper) = &req.keeper {
        if keeper.server_id <= 0 {
            issues.push(issue("error", "keeper", "keeper server id is required"));
        }
        let voters = keeper
            .raft_peers
            .iter()
            .filter(|peer| !peer.learner)
            .count();
        if voters == 0 || voters % 2 == 0 {
            issues.push(issue("error", "keeper", "voting Keeper count must be odd"));
        }
        if !keeper
            .raft_peers
            .iter()
            .any(|peer| peer.server_id == keeper.server_id)
        {
            issues.push(issue(
                "error",
                "keeper",
                "server id must be one of the raft peers",
            ));
        }
    }
    if req.server.is_none() && req.keeper.is_none() {
        issues.push(issue(
            "error",
            "role",
            "the node needs a server or a keeper",
        ));
    }
    for volume in [&req.data_volume, &req.keeper_volume].into_iter().flatten() {
        if !volume.name.is_empty() && volume.storage_class != "lvm" && volume.storage_class != "zfs"
        {
            issues.push(issue("error", "volume", "storage class must be lvm or zfs"));
        }
    }
    issues
}

/// Disk space the agent observed for one declared volume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeSpace {
    pub storage_class: String,
    pub pool: String,
    pub name: String,
    pub free_bytes: Option<i64>,
    pub existing_bytes: Option<i64>,
}

/// Host facts gathered on the node. The preflight decision does not shell out.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub ip_addr_json: String,
    pub networkctl_status: String,
    pub lease_file_exists: bool,
    pub listening: Vec<(u16, String)>,
    pub volumes: Vec<VolumeSpace>,
    pub package_version: String,
}

/// Structural preflight plus address, DHCP, ports, and volume room.
pub fn preflight_with_host(
    req: &proto::CheckClickhousePreflightRequest,
    facts: &HostFacts,
) -> Vec<proto::ClickhousePreflightIssue> {
    let mut issues = preflight(req);
    if !req.advertise_address.is_empty() && !req.iface.is_empty() && req.iface != "br0" {
        match crate::ceph_preflight::iface_has_address(
            &facts.ip_addr_json,
            &req.iface,
            &req.advertise_address,
        ) {
            Ok(true) => {}
            Ok(false) => issues.push(issue(
                "error",
                "address",
                "advertise address is not assigned to iface",
            )),
            Err(err) => issues.push(issue("error", "address", &err)),
        }
    }
    if !req.iface.is_empty()
        && crate::ceph_preflight::iface_appears_dhcp(
            &facts.networkctl_status,
            facts.lease_file_exists,
        )
    {
        issues.push(issue("error", "iface", "iface is configured by DHCP"));
    }
    let ports = req.ports.unwrap_or_default();
    let needed = needed_ports(
        &ports,
        req.tls_enabled,
        req.keeper.is_some(),
        req.server.is_some(),
        req.server
            .as_ref()
            .is_some_and(|server| server.metrics_public),
    );
    for port in needed {
        let port_u = u16::try_from(port).unwrap_or(0);
        if let Some((_, owner)) = facts.listening.iter().find(|(open, _)| *open == port_u) {
            if !owner.to_ascii_lowercase().contains("clickhouse") {
                let who = if owner.is_empty() {
                    "an unknown process".to_string()
                } else {
                    owner.clone()
                };
                issues.push(issue(
                    "error",
                    "port",
                    &format!("port {port} is already used by {who}"),
                ));
            }
        }
    }
    for volume in [&req.data_volume, &req.keeper_volume].into_iter().flatten() {
        if volume.name.is_empty() || volume.size_bytes <= 0 {
            continue;
        }
        let found = facts.volumes.iter().find(|row| {
            row.name == volume.name
                && row.pool == volume.volume_group
                && row.storage_class == volume.storage_class
        });
        match found {
            Some(row) if row.existing_bytes == Some(volume.size_bytes) => {}
            Some(row) if row.existing_bytes.is_some() => {
                let have = row.existing_bytes.unwrap_or(0);
                issues.push(issue(
                    "error",
                    "volume",
                    &format!(
                        "volume {}/{} exists at {have} bytes, not {}",
                        volume.volume_group, volume.name, volume.size_bytes
                    ),
                ));
            }
            Some(row) if row.free_bytes.is_some_and(|free| free >= volume.size_bytes) => {}
            Some(row) if row.free_bytes.is_some() => {
                let free = row.free_bytes.unwrap_or(0);
                issues.push(issue(
                    "error",
                    "volume",
                    &format!(
                        "volume group {} has {free} bytes free, short of {} for {}",
                        volume.volume_group, volume.size_bytes, volume.name
                    ),
                ));
            }
            _ => issues.push(issue(
                "error",
                "volume",
                &format!(
                    "free space for {}/{} was not observed",
                    volume.volume_group, volume.name
                ),
            )),
        }
    }
    if facts.package_version.is_empty() {
        issues.push(issue(
            "warning",
            "package",
            "package version was not resolved",
        ));
    }
    issues
}

/// `ss -H -ltnp` rows as `(port, process)`. An empty process means the owner was hidden.
pub fn parse_listening(ss: &str) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    for line in ss.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(local) = line.split_whitespace().nth(3) else {
            continue;
        };
        let Some(port) = local.rsplit(':').next().and_then(|raw| raw.parse().ok()) else {
            continue;
        };
        let owner = line
            .split("((\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap_or("")
            .to_string();
        out.push((port, owner));
    }
    out
}

pub fn lvm_free_bytes(vgs: &str, pool: &str) -> Option<i64> {
    bytes_row(vgs, pool, 0, 1)
}

pub fn lvm_lv_bytes(lvs: &str, pool: &str, name: &str) -> Option<i64> {
    for line in lvs.lines() {
        let cols: Vec<_> = line.split_whitespace().collect();
        if cols.len() >= 3 && cols[0] == name && cols[1] == pool {
            return cols[2].parse().ok();
        }
    }
    None
}

pub fn zpool_free_bytes(listing: &str, pool: &str) -> Option<i64> {
    bytes_row(listing, pool, 0, 1)
}

pub fn zfs_volume_bytes(listing: &str, name: &str) -> Option<i64> {
    bytes_row(listing, name, 0, 1)
}

fn bytes_row(text: &str, key: &str, key_at: usize, value_at: usize) -> Option<i64> {
    for line in text.lines() {
        let cols: Vec<_> = line.split_whitespace().collect();
        if cols.len() > value_at && cols[key_at] == key {
            return cols[value_at].parse().ok();
        }
    }
    None
}

pub fn unit_service(unit: &str) -> Result<&'static str, String> {
    match unit {
        "server" => Ok("clickhouse.service"),
        "keeper" => Ok("clickhouse-keeper.service"),
        other => Err(format!("unit {other} is not server or keeper")),
    }
}

pub fn admin_plan(req: &proto::RunClickhouseAdminRequest) -> Result<AdminPlan, String> {
    use proto::run_clickhouse_admin_request::Command;
    let Some(command) = &req.command else {
        return Err("admin command is required".into());
    };
    let sql = match command {
        Command::SyncReplica(sync) => {
            return Ok(AdminPlan::SyncReplicas {
                database: sync.database.clone(),
            });
        }
        Command::DropReplica(drop) => {
            let databases = if drop.databases.is_empty() {
                vec!["default".to_string()]
            } else {
                drop.databases.clone()
            };
            databases
                .iter()
                .map(|database| drop_replica_sql(&drop.replica_name, database))
                .collect::<Result<Vec<_>, _>>()?
        }
        Command::EnsureDatabase(ensure) => vec![create_database_sql(&ensure.database)?],
        Command::RestoreReplica(restore) => {
            return Ok(AdminPlan::RestoreReadonly {
                database: restore.database.clone(),
            });
        }
        Command::Backup(backup) => statements_for(
            &backup.target,
            &backup.databases,
            &backup.backup_name,
            false,
            false,
        )?,
        Command::Restore(restore) => statements_for(
            &restore.target,
            &restore.databases,
            &restore.backup_name,
            true,
            restore.allow_non_empty_tables,
        )?,
        Command::BackupStatus(status) => vec![backup_status_sql(&status.backup_id)?],
        Command::LocalPartsCount(_) => vec![local_parts_sql().to_string()],
        Command::ReloadConfig(_) => vec![reload_config_sql().to_string()],
        Command::SetSystemLogTtl(ttl) => vec![set_ttl_sql(&ttl.table, ttl.days)?],
        Command::BackupAccess(backup) => vec![backup_access_sql(&target_of(
            backup.target.as_ref(),
            &backup.backup_name,
        )?)?],
        Command::RestoreAccess(restore) => {
            vec![restore_access_sql(&target_of(
                restore.target.as_ref(),
                &restore.backup_name,
            )?)?]
        }
        Command::CreateFunction(function) => {
            vec![create_function_sql(
                &function.name,
                &function.parameters,
                &function.expression,
            )?]
        }
        Command::CreateDictionary(_) => vec![reload_config_sql().to_string()],
        Command::VerifyClusters(_) => vec![verify_clusters_sql().to_string()],
    };
    Ok(AdminPlan::Sql(sql))
}

pub fn client_port(ports: &proto::ClickhousePorts, tls: bool) -> i32 {
    if tls {
        if ports.tcp_secure == 0 {
            9440
        } else {
            ports.tcp_secure
        }
    } else if ports.tcp == 0 {
        9000
    } else {
        ports.tcp
    }
}

fn statements_for(
    target: &Option<proto::ClickhouseBackupTarget>,
    databases: &[String],
    name: &str,
    restore: bool,
    allow_non_empty: bool,
) -> Result<Vec<String>, String> {
    let target = target_of(target.as_ref(), name)?;
    if databases.is_empty() {
        return Err("backup needs at least one database".into());
    }
    databases
        .iter()
        .map(|database| {
            if restore {
                restore_database_sql(database, &target, allow_non_empty)
            } else {
                backup_database_sql(database, &target)
            }
        })
        .collect()
}

fn target_of(
    target: Option<&proto::ClickhouseBackupTarget>,
    name: &str,
) -> Result<BackupTarget, String> {
    let Some(target) = target else {
        return Err("backup target is required".into());
    };
    Ok(BackupTarget {
        kind: target.kind.clone(),
        location: target.location.clone(),
        bucket: target.s3_bucket.clone(),
        name: name.to_string(),
    })
}

fn fragment_from_apply(req: &proto::ApplyClickhouseConfigRequest) -> Fragment {
    let ports_msg = req.ports.unwrap_or_default();
    let ports = Ports {
        tcp: ports_msg.tcp,
        tcp_secure: ports_msg.tcp_secure,
        http: ports_msg.http,
        https: ports_msg.https,
        interserver: ports_msg.interserver,
        keeper: ports_msg.keeper,
        keeper_raft: ports_msg.keeper_raft,
        prometheus: ports_msg.prometheus,
    };
    Fragment {
        cluster_name: req.cluster_name.clone(),
        generation: req.generation,
        package: if req.package.is_empty() {
            "clickhouse".into()
        } else {
            req.package.clone()
        },
        advertise_address: req.advertise_address.clone(),
        iface: req.iface.clone(),
        tls_enabled: req.tls_enabled,
        ports,
        metrics_public: req
            .server
            .as_ref()
            .is_some_and(|server| server.metrics_public),
        max_server_memory_ratio: req
            .server
            .as_ref()
            .map(|server| server.max_server_memory_ratio)
            .unwrap_or(0.5),
        client_networks: req
            .server
            .as_ref()
            .map(|server| server.client_networks.clone())
            .unwrap_or_default(),
        server: req
            .server
            .as_ref()
            .filter(|server| !server.replica_name.is_empty())
            .map(|server| ServerFragment {
                shard: server.shard_name.clone(),
                replica: server.replica_name.clone(),
                read_only: server.read_only,
                data_path: if server.data_path.is_empty() {
                    server_data_path(&req.cluster_name)
                } else {
                    server.data_path.clone()
                },
                remote_shards: server
                    .remote_shards
                    .iter()
                    .map(|shard| RemoteShard {
                        shard: shard.shard_name.clone(),
                        weight: shard.weight,
                        replicas: shard
                            .replicas
                            .iter()
                            .map(|replica| RemoteReplica {
                                replica: replica.replica_name.clone(),
                                host: replica.host.clone(),
                                port: replica.port,
                                secure: replica.secure,
                                priority: if replica.priority == 0 {
                                    1
                                } else {
                                    replica.priority
                                },
                            })
                            .collect(),
                    })
                    .collect(),
                databases: server.databases.iter().map(|db| db.name.clone()).collect(),
                users: server
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
                profiles: server
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
                quotas: server
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
                dictionaries: server
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
                named_collections: server
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
                functions: server
                    .functions
                    .iter()
                    .map(|function| FunctionFragment {
                        name: function.name.clone(),
                        parameters: function.parameters.clone(),
                        expression: function.expression.clone(),
                    })
                    .collect(),
                settings: server
                    .server_settings
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                system_logs: server
                    .system_logs
                    .map(|logs| LogFragment {
                        query_log_days: logs.query_log_days,
                        part_log_days: logs.part_log_days,
                        trace_log_days: logs.trace_log_days,
                        text_log_days: logs.text_log_days,
                    })
                    .unwrap_or_default(),
            }),
        keeper: req
            .keeper
            .as_ref()
            .filter(|keeper| keeper.server_id > 0)
            .map(|keeper| KeeperFragment {
                server_id: keeper.server_id,
                learner: keeper
                    .raft_peers
                    .iter()
                    .any(|peer| peer.server_id == keeper.server_id && peer.learner),
                data_path: if keeper.data_path.is_empty() {
                    keeper_data_path(&req.cluster_name)
                } else {
                    keeper.data_path.clone()
                },
                raft_peers: keeper
                    .raft_peers
                    .iter()
                    .map(|peer| RaftPeerFragment {
                        id: peer.server_id,
                        host: peer.host.clone(),
                        port: peer.port,
                        learner: peer.learner,
                    })
                    .collect(),
            }),
    }
}

fn user_digests(server: &proto::ClickhouseNodeServerConfig) -> Vec<(String, String)> {
    server
        .users
        .iter()
        .filter(|user| !user.password_sha256_hex.is_empty())
        .map(|user| (user.name.clone(), user.password_sha256_hex.clone()))
        .collect()
}

fn keeper_password(secret: &str) -> String {
    secret.strip_prefix("kcore:").unwrap_or(secret).to_string()
}

fn named_env(key: &str) -> String {
    let mut out = String::from("KCORE_CH_NC_");
    for ch in key.chars() {
        if ch == '.' || ch == '-' {
            out.push('_');
        } else {
            out.push(ch.to_ascii_uppercase());
        }
    }
    out
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    fs::write(path, bytes).map_err(|e| e.to_string())?;
    let mut perms = fs::metadata(path).map_err(|e| e.to_string())?.permissions();
    perms.set_mode(0o400);
    fs::set_permissions(path, perms).map_err(|e| e.to_string())?;
    Ok(())
}

fn needed_ports(
    ports: &proto::ClickhousePorts,
    tls: bool,
    keeper: bool,
    server: bool,
    metrics_public: bool,
) -> Vec<i32> {
    let mut values = Vec::new();
    if server {
        if tls {
            values.extend([ports.tcp_secure, ports.https, ports.interserver]);
        } else {
            values.extend([ports.tcp, ports.http, ports.interserver]);
        }
        if metrics_public {
            values.push(ports.prometheus);
        }
    }
    if keeper {
        values.extend([ports.keeper, ports.keeper_raft]);
    }
    values.retain(|port| *port > 0);
    values
}

fn ports_distinct(
    ports: &proto::ClickhousePorts,
    tls: bool,
    keeper: bool,
    metrics_public: bool,
) -> bool {
    let mut values = Vec::new();
    if tls {
        values.extend([ports.tcp_secure, ports.https, ports.interserver]);
    } else {
        values.extend([ports.tcp, ports.http, ports.interserver]);
    }
    if keeper {
        values.extend([ports.keeper, ports.keeper_raft]);
    }
    if metrics_public {
        values.push(ports.prometheus);
    }
    values.retain(|port| *port != 0);
    let before = values.len();
    values.sort_unstable();
    values.dedup();
    values.len() == before
}

fn issue(severity: &str, check: &str, message: &str) -> proto::ClickhousePreflightIssue {
    proto::ClickhousePreflightIssue {
        severity: severity.into(),
        check: check.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_apply() -> proto::ApplyClickhouseConfigRequest {
        proto::ApplyClickhouseConfigRequest {
            cluster_name: "analytics".into(),
            generation: 1,
            enable: true,
            advertise_address: "192.168.40.105".into(),
            iface: "enp6s0".into(),
            package: "clickhouse".into(),
            tls_enabled: true,
            ports: Some(proto::ClickhousePorts {
                tcp_secure: 9440,
                https: 8443,
                interserver: 9010,
                keeper: 9281,
                keeper_raft: 9234,
                prometheus: 9363,
                ..Default::default()
            }),
            server: Some(proto::ClickhouseNodeServerConfig {
                shard_name: "s1".into(),
                replica_name: "r1".into(),
                remote_shards: vec![proto::ClickhouseRemoteShard {
                    shard_name: "s1".into(),
                    weight: 1,
                    replicas: vec![proto::ClickhouseRemoteReplica {
                        replica_name: "r1".into(),
                        host: "192.168.40.105".into(),
                        port: 9440,
                        secure: true,
                        priority: 1,
                    }],
                }],
                ..Default::default()
            }),
            secrets: Some(proto::ClickhouseNodeSecrets {
                default_password_sha256_hex: "ab".into(),
                default_password: "secret".into(),
                interserver_password: "inter".into(),
                keeper_secret: "kcore:keeper".into(),
                cluster_secret: "cluster".into(),
                metrics_password_sha256_hex: "cd".into(),
                tls_cert_pem: b"cert".to_vec(),
                tls_key_pem: b"key".to_vec(),
                tls_ca_pem: b"ca".to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn disabled_apply_is_the_one_liner() {
        let mut req = sample_apply();
        req.enable = false;
        let prepared = prepare(&req).unwrap();
        assert_eq!(prepared.nix, render_disabled());
    }

    #[test]
    fn enabled_fragment_keeps_passwords_out_of_nix() {
        let prepared = prepare(&sample_apply()).unwrap();
        assert!(prepared.nix.contains("analytics"));
        assert!(prepared.nix.contains("secretsDir"));
        assert!(!prepared.nix.contains("\"secret\""));
        assert!(!prepared.nix.contains("kcore:keeper"));
        assert!(prepared.server_env.contains("KCORE_CH_DEFAULT_PASSWORD=ab"));
        assert!(prepared
            .keeper_env
            .contains("KCORE_CH_KEEPER_IDENTITY=kcore:keeper"));
    }

    #[test]
    fn install_writes_mode_0400_secrets() {
        let root = std::env::temp_dir().join(format!("kcore-ch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let prepared = prepare(&sample_apply()).unwrap();
        install(&root, &prepared).unwrap();
        let env = root.join("var/lib/kcore-clickhouse/analytics/secrets/server.env");
        let mode = fs::metadata(&env).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
        assert!(
            !fs::read_to_string(root.join("etc/nixos/kcore-clickhouse.nix"))
                .unwrap()
                .contains("\"secret\"")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn flake_import_is_inserted_once() {
        let flake = "modules = [\n        ./kcore-ceph.nix\n      ];\n";
        let once = ensure_values_import(flake);
        assert!(once.contains("./kcore-clickhouse.nix"));
        assert_eq!(ensure_values_import(&once), once);
    }

    #[test]
    fn preflight_rejects_the_bridge() {
        let req = proto::CheckClickhousePreflightRequest {
            cluster_name: "analytics".into(),
            advertise_address: "192.168.40.105".into(),
            iface: "br0".into(),
            package: "clickhouse".into(),
            server: Some(proto::ClickhouseNodeServerConfig {
                shard_name: "s1".into(),
                replica_name: "r1".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let issues = preflight(&req);
        assert!(issues.iter().any(|issue| issue.check == "iface"));
    }

    fn host_req() -> proto::CheckClickhousePreflightRequest {
        proto::CheckClickhousePreflightRequest {
            cluster_name: "analytics".into(),
            advertise_address: "192.168.40.105".into(),
            iface: "enp6s0".into(),
            package: "clickhouse".into(),
            tls_enabled: true,
            ports: Some(proto::ClickhousePorts {
                tcp_secure: 9440,
                https: 8443,
                interserver: 9010,
                keeper: 9281,
                keeper_raft: 9234,
                prometheus: 9363,
                ..Default::default()
            }),
            server: Some(proto::ClickhouseNodeServerConfig {
                shard_name: "s1".into(),
                replica_name: "r1".into(),
                ..Default::default()
            }),
            data_volume: Some(proto::ClickhouseHostVolume {
                name: "ch-data".into(),
                storage_class: "lvm".into(),
                volume_group: "vg0".into(),
                size_bytes: 1_000,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn good_facts() -> HostFacts {
        HostFacts {
            ip_addr_json: r#"[{"ifname":"enp6s0","addr_info":[{"local":"192.168.40.105"}]}]"#
                .into(),
            networkctl_status: "State: routable".into(),
            lease_file_exists: false,
            listening: vec![(9440, "clickhouse".into())],
            volumes: vec![VolumeSpace {
                storage_class: "lvm".into(),
                pool: "vg0".into(),
                name: "ch-data".into(),
                free_bytes: Some(5_000),
                existing_bytes: None,
            }],
            package_version: "26.2.4.23".into(),
        }
    }

    #[test]
    fn host_preflight_accepts_a_static_address_and_an_owned_port() {
        let issues = preflight_with_host(&host_req(), &good_facts());
        assert!(issues.iter().all(|issue| issue.severity != "error"));
    }

    #[test]
    fn host_preflight_rejects_dhcp_a_foreign_port_and_a_short_volume() {
        let mut facts = good_facts();
        facts.lease_file_exists = true;
        facts.listening = vec![(8443, "nginx".into()), (9010, String::new())];
        facts.volumes[0].free_bytes = Some(10);
        let issues = preflight_with_host(&host_req(), &facts);
        assert!(issues.iter().any(|issue| issue.check == "iface"));
        assert!(issues.iter().any(|issue| issue.message.contains("nginx")));
        assert!(issues
            .iter()
            .any(|issue| issue.message.contains("unknown process")));
        assert!(issues.iter().any(|issue| issue.check == "volume"));
    }

    #[test]
    fn an_unresolved_package_version_is_a_warning() {
        let mut facts = good_facts();
        facts.package_version.clear();
        let issues = preflight_with_host(&host_req(), &facts);
        assert!(issues
            .iter()
            .any(|issue| issue.severity == "warning" && issue.check == "package"));
        assert!(issues.iter().all(|issue| issue.severity != "error"));
    }

    #[test]
    fn an_existing_volume_of_the_same_size_is_room_enough() {
        let mut facts = good_facts();
        facts.volumes[0].free_bytes = Some(0);
        facts.volumes[0].existing_bytes = Some(1_000);
        let issues = preflight_with_host(&host_req(), &facts);
        assert!(issues.iter().all(|issue| issue.check != "volume"));
    }

    #[test]
    fn ss_and_lvm_parsers_read_the_owner_and_the_free_bytes() {
        let rows = parse_listening(
            "LISTEN 0 128 0.0.0.0:9000 0.0.0.0:* users:((\"clickhouse\",pid=1,fd=3))\n",
        );
        assert_eq!(rows, vec![(9000, "clickhouse".into())]);
        assert_eq!(lvm_free_bytes("  vg0  4096\n", "vg0"), Some(4096));
        assert_eq!(
            lvm_lv_bytes("  ch-data vg0 1000\n", "vg0", "ch-data"),
            Some(1000)
        );
    }

    #[test]
    fn backup_sql_uses_the_disk_function() {
        let req = proto::RunClickhouseAdminRequest {
            cluster_name: "analytics".into(),
            command: Some(proto::run_clickhouse_admin_request::Command::Backup(
                proto::run_clickhouse_admin_request::Backup {
                    target: Some(proto::ClickhouseBackupTarget {
                        kind: "disk".into(),
                        location: "/var/lib/kcore-clickhouse/analytics/backups".into(),
                        ..Default::default()
                    }),
                    databases: vec!["analytics".into()],
                    backup_name: "nightly".into(),
                    ..Default::default()
                },
            )),
        };
        match admin_plan(&req).unwrap() {
            AdminPlan::Sql(sql) => {
                assert!(sql[0].contains("BACKUP DATABASE"));
                assert!(sql[0].contains("Disk("));
                assert!(!sql[0].contains("secret"));
            }
            AdminPlan::SyncReplicas { .. } | AdminPlan::RestoreReadonly { .. } => {
                panic!("expected a single SQL statement");
            }
        }
    }
}
