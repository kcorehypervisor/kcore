//! Quoted ClickHouse statements and parsers for `mntr` and `system.replicas`.
//!
//! The controller never sends SQL. The agent builds each statement here.

use kcore_sanitize::{
    quote_identifier, quote_literal, validate_clickhouse_name, ClickhouseNameClass,
};

const LOG_TABLES: &[&str] = &["query_log", "part_log", "trace_log", "text_log"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mntr {
    pub role: String,
    pub followers: i64,
    pub synced_followers: i64,
    pub znode_count: i64,
    pub outstanding_requests: i64,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaAggregate {
    pub readonly_tables: i32,
    pub max_delay_seconds: i32,
    pub queue_size: i64,
    pub empty: bool,
}

pub fn version_sql() -> &'static str {
    "SELECT version() FORMAT TabSeparated"
}

pub fn replicas_aggregate_sql() -> &'static str {
    "SELECT countIf(is_readonly), ifNull(max(absolute_delay), 0), sum(queue_size) FROM system.replicas FORMAT TabSeparated"
}

pub fn keeper_session_sql() -> &'static str {
    "SELECT count() FROM system.zookeeper WHERE path = '/' FORMAT TabSeparated"
}

pub fn verify_clusters_sql() -> &'static str {
    "SELECT cluster, count() FROM system.clusters GROUP BY cluster FORMAT TabSeparated"
}

pub fn reload_config_sql() -> &'static str {
    "SYSTEM RELOAD CONFIG"
}

pub fn list_replicated_tables_sql(database: &str) -> Result<String, String> {
    if database.is_empty() {
        return Ok("SELECT database, table FROM system.replicas FORMAT TabSeparated".into());
    }
    ident(database, "database")?;
    Ok(format!(
        "SELECT database, table FROM system.replicas WHERE database = {} FORMAT TabSeparated",
        quote_literal(database)
    ))
}

pub fn sync_replica_sql(database: &str, table: &str) -> Result<String, String> {
    ident(database, "database")?;
    ident(table, "table")?;
    Ok(format!(
        "SYSTEM SYNC REPLICA {}.{}",
        quote_identifier(database),
        quote_identifier(table)
    ))
}

pub fn create_database_sql(name: &str) -> Result<String, String> {
    ident(name, "database")?;
    let path = quote_literal(&format!("/clickhouse/databases/{name}"));
    Ok(format!(
        "CREATE DATABASE IF NOT EXISTS {} ENGINE = Replicated({}, '{{shard}}', '{{replica}}')",
        quote_identifier(name),
        path,
    ))
}

pub fn exists_database_sql(name: &str) -> Result<String, String> {
    ident(name, "database")?;
    Ok(format!(
        "SELECT count() FROM system.databases WHERE name = {} FORMAT TabSeparated",
        quote_literal(name)
    ))
}

pub fn drop_replica_sql(replica: &str, database: &str) -> Result<String, String> {
    macro_name(replica, "replica")?;
    ident(database, "database")?;
    Ok(format!(
        "SYSTEM DROP REPLICA {} FROM DATABASE {}",
        quote_literal(replica),
        quote_identifier(database)
    ))
}

pub fn restore_replica_sql(database: &str, table: &str) -> Result<String, String> {
    ident(database, "database")?;
    ident(table, "table")?;
    Ok(format!(
        "SYSTEM RESTORE REPLICA {}.{}",
        quote_identifier(database),
        quote_identifier(table)
    ))
}

pub fn readonly_tables_sql(database: &str) -> Result<String, String> {
    if database.is_empty() {
        return Ok(
            "SELECT database, table FROM system.replicas WHERE is_readonly FORMAT TabSeparated"
                .into(),
        );
    }
    ident(database, "database")?;
    Ok(format!(
        "SELECT database, table FROM system.replicas WHERE is_readonly AND database = {} FORMAT TabSeparated",
        quote_literal(database)
    ))
}

pub fn set_ttl_sql(table: &str, days: i32) -> Result<String, String> {
    if !LOG_TABLES.contains(&table) {
        return Err(format!("system log {table} is not managed"));
    }
    if days <= 0 {
        return Err("ttl days must be positive".into());
    }
    Ok(format!(
        "ALTER TABLE system.{} MODIFY TTL event_date + INTERVAL {days} DAY",
        quote_identifier(table)
    ))
}

pub fn local_parts_sql() -> &'static str {
    "SELECT count() FROM system.parts WHERE active AND database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') FORMAT TabSeparated"
}

pub fn create_function_sql(
    name: &str,
    parameters: &str,
    expression: &str,
) -> Result<String, String> {
    ident(name, "function")?;
    if parameters.contains(';')
        || expression.contains(';')
        || parameters.contains('\n')
        || expression.contains('\n')
    {
        return Err(format!("function {name} must be a single expression"));
    }
    let params = if parameters.is_empty() {
        "()"
    } else {
        parameters
    };
    if !params.starts_with('(') || !params.ends_with(')') {
        return Err(format!("function {name} parameters must be parenthesized"));
    }
    Ok(format!(
        "CREATE FUNCTION IF NOT EXISTS {} AS {params} -> {expression}",
        quote_identifier(name),
        params = params,
        expression = expression,
    ))
}

#[derive(Debug, Clone)]
pub struct BackupTarget {
    pub kind: String,
    pub location: String,
    pub bucket: String,
    pub name: String,
}

pub fn backup_database_sql(database: &str, target: &BackupTarget) -> Result<String, String> {
    ident(database, "database")?;
    Ok(format!(
        "BACKUP DATABASE {} TO {}",
        quote_identifier(database),
        backup_destination(target)?
    ))
}

pub fn backup_access_sql(target: &BackupTarget) -> Result<String, String> {
    let mut access = target.clone();
    if !access.name.ends_with("-access") {
        access.name = format!("{}-access", access.name);
    }
    Ok(format!("BACKUP ACCESS TO {}", backup_destination(&access)?))
}

pub fn restore_database_sql(
    database: &str,
    target: &BackupTarget,
    allow_non_empty: bool,
) -> Result<String, String> {
    ident(database, "database")?;
    let settings = if allow_non_empty {
        " SETTINGS allow_non_empty_tables = 1"
    } else {
        ""
    };
    Ok(format!(
        "RESTORE DATABASE {} FROM {}{settings}",
        quote_identifier(database),
        backup_destination(target)?
    ))
}

pub fn restore_access_sql(target: &BackupTarget) -> Result<String, String> {
    let mut access = target.clone();
    if !access.name.ends_with("-access") {
        access.name = format!("{}-access", access.name);
    }
    Ok(format!(
        "RESTORE ACCESS FROM {}",
        backup_destination(&access)?
    ))
}

pub fn backup_status_sql(backup_id: &str) -> Result<String, String> {
    if backup_id.is_empty() {
        return Ok(
            "SELECT id, status, error FROM system.backups ORDER BY start_time DESC LIMIT 1 FORMAT TabSeparated"
                .into(),
        );
    }
    Ok(format!(
        "SELECT id, status, error FROM system.backups WHERE id = {} FORMAT TabSeparated",
        quote_literal(backup_id)
    ))
}

fn backup_destination(target: &BackupTarget) -> Result<String, String> {
    if target.name.is_empty() || target.name.contains('/') || target.name.contains('\\') {
        return Err("backup name must be a single path segment".into());
    }
    match target.kind.as_str() {
        "disk" => {
            if target.location.is_empty() {
                return Err("disk backup needs a location".into());
            }
            Ok(format!(
                "Disk({}, {})",
                quote_literal("kcore_backups"),
                quote_literal(&target.name)
            ))
        }
        "s3" => {
            if !target.location.starts_with("https://") || target.location.contains('@') {
                return Err("s3 backup location must be an https URL without userinfo".into());
            }
            let bucket = target.bucket.trim_matches('/');
            let url = if bucket.is_empty() {
                format!("{}/{}", target.location.trim_end_matches('/'), target.name)
            } else {
                format!(
                    "{}/{bucket}/{}",
                    target.location.trim_end_matches('/'),
                    target.name
                )
            };
            Ok(format!("S3({})", quote_literal(&url)))
        }
        other => Err(format!("backup kind {other} is not disk or s3")),
    }
}

pub fn server_env(material: &ServerSecrets) -> String {
    let mut out = String::new();
    push_env(
        &mut out,
        "KCORE_CH_DEFAULT_PASSWORD",
        &material.default_password_sha256,
    );
    push_env(
        &mut out,
        "KCORE_CH_METRICS_PASSWORD",
        &material.metrics_password_sha256,
    );
    push_env(
        &mut out,
        "KCORE_CH_INTERSERVER_PASSWORD",
        &material.interserver_password,
    );
    push_env(
        &mut out,
        "KCORE_CH_CLUSTER_SECRET",
        &material.cluster_secret,
    );
    push_env(
        &mut out,
        "KCORE_CH_KEEPER_IDENTITY",
        &format!("kcore:{}", material.keeper_password),
    );
    for (name, digest) in &material.user_digests {
        push_env(&mut out, &user_env(name), digest);
    }
    for (key, value) in &material.named_collection_secrets {
        push_env(&mut out, key, value);
    }
    out
}

pub fn keeper_env(material: &ServerSecrets) -> String {
    let mut out = String::new();
    push_env(
        &mut out,
        "KCORE_CH_KEEPER_IDENTITY",
        &format!("kcore:{}", material.keeper_password),
    );
    out
}

fn user_env(name: &str) -> String {
    crate::nixgen::user_password_env(name)
}

fn push_env(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

#[derive(Debug, Clone, Default)]
pub struct ServerSecrets {
    pub default_password_sha256: String,
    pub metrics_password_sha256: String,
    pub interserver_password: String,
    pub cluster_secret: String,
    pub keeper_password: String,
    pub user_digests: Vec<(String, String)>,
    pub named_collection_secrets: Vec<(String, String)>,
}

pub fn parse_mntr(body: &str) -> Mntr {
    let mut parsed = Mntr {
        role: String::new(),
        followers: 0,
        synced_followers: 0,
        znode_count: 0,
        outstanding_requests: 0,
        ok: false,
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("depth=") || line.starts_with("verify") {
            continue;
        }
        let (key, value) = split_field(line);
        let value = value.trim();
        match key.trim() {
            "zk_server_state" => parsed.role = value.to_string(),
            "zk_followers" => parsed.followers = value.parse().unwrap_or(0),
            "zk_synced_followers" => parsed.synced_followers = value.parse().unwrap_or(0),
            "zk_znode_count" => parsed.znode_count = value.parse().unwrap_or(0),
            "zk_outstanding_requests" => parsed.outstanding_requests = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    parsed.ok = matches!(parsed.role.as_str(), "leader" | "follower" | "observer");
    parsed
}

pub fn parse_replicas_tsv(body: &str) -> ReplicaAggregate {
    let line = body.lines().find(|line| !line.trim().is_empty());
    let Some(line) = line else {
        return ReplicaAggregate {
            readonly_tables: 0,
            max_delay_seconds: 0,
            queue_size: 0,
            empty: true,
        };
    };
    let mut fields = line.split('\t');
    ReplicaAggregate {
        readonly_tables: parse_count(fields.next().unwrap_or("")),
        max_delay_seconds: parse_count(fields.next().unwrap_or("")),
        queue_size: i64::from(parse_count(fields.next().unwrap_or(""))),
        empty: false,
    }
}

pub fn parse_count_text(body: &str) -> i64 {
    i64::from(parse_count(body.lines().next().unwrap_or("").trim()))
}

fn split_field(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\t' || bytes[i] == b' ' || bytes[i] == b'=' {
            return (line[..i].trim(), line[i + 1..].trim());
        }
        i += 1;
    }
    (line, "")
}

fn parse_count(raw: &str) -> i32 {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("null") || raw == "\\N" {
        return 0;
    }
    raw.parse().unwrap_or(0)
}

fn ident(name: &str, label: &str) -> Result<(), String> {
    if !validate_clickhouse_name(name, ClickhouseNameClass::Identifier) {
        return Err(format!("{label} {name} is outside the identifier grammar"));
    }
    Ok(())
}

fn macro_name(name: &str, label: &str) -> Result<(), String> {
    if !validate_clickhouse_name(name, ClickhouseNameClass::Macro) {
        return Err(format!("{label} {name} is outside the macro grammar"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_quotes_and_comments() {
        assert_eq!(quote_literal("a'b`; DROP"), "'a\\'b`; DROP'");
        let sql = drop_replica_sql("node-105", "events").unwrap();
        assert!(sql.contains("'node-105'"));
        assert!(sql.contains("FROM DATABASE `events`"));
        let created = create_database_sql("events").unwrap();
        assert!(created.contains("'{shard}'"));
        assert!(created.contains("'{replica}'"));
        assert!(created.starts_with("CREATE DATABASE IF NOT EXISTS `events`"));
    }

    #[test]
    fn mntr_parses_leader_and_ignores_a_failed_handshake() {
        let leader =
            parse_mntr("zk_server_state\tleader\nzk_synced_followers\t2\nzk_znode_count\t10\n");
        assert!(leader.ok);
        assert_eq!(leader.role, "leader");
        assert_eq!(leader.synced_followers, 2);
        assert_eq!(leader.znode_count, 10);
        let handshake = parse_mntr("CONNECTED(00000003)\n---\nno peer certificate available\n");
        assert!(!handshake.ok);
        assert!(handshake.role.is_empty());
        assert!(!parse_mntr("").ok);
    }

    #[test]
    fn replica_aggregate_treats_null_delay_as_zero() {
        let row = parse_replicas_tsv("1\tNULL\t4\n");
        assert_eq!(row.readonly_tables, 1);
        assert_eq!(row.max_delay_seconds, 0);
        assert_eq!(row.queue_size, 4);
        assert!(!row.empty);
        assert!(parse_replicas_tsv("").empty);
        assert_eq!(parse_replicas_tsv("0\t\\N\t0\n").max_delay_seconds, 0);
    }

    #[test]
    fn backup_refuses_userinfo_and_quotes_the_name() {
        let target = BackupTarget {
            kind: "s3".into(),
            location: "https://s3.example".into(),
            bucket: "bucket".into(),
            name: "nightly".into(),
        };
        let sql = backup_database_sql("events", &target).unwrap();
        assert!(sql.contains("S3('https://s3.example/bucket/nightly')"));
        let bad = BackupTarget {
            location: "https://user:pw@s3.example".into(),
            ..target
        };
        assert!(backup_database_sql("events", &bad).is_err());
    }

    #[test]
    fn env_file_uses_digest_names() {
        let text = server_env(&ServerSecrets {
            default_password_sha256: "ab".into(),
            metrics_password_sha256: "cd".into(),
            interserver_password: "inter".into(),
            cluster_secret: "sec".into(),
            keeper_password: "keep".into(),
            user_digests: vec![("ingest".into(), "ee".into())],
            named_collection_secrets: vec![],
        });
        assert!(text.contains("KCORE_CH_DEFAULT_PASSWORD=ab\n"));
        assert!(text.contains("KCORE_CH_KEEPER_IDENTITY=kcore:keep\n"));
        assert!(text.contains("KCORE_CH_USER_INGEST=ee\n"));
        assert!(!text.contains("password_sha256_hex"));
    }
}
