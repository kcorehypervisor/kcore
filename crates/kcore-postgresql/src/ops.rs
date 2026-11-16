//! Admin SQL, Patroni request bodies and response parsers.
//!
//! The controller never sends SQL. The node agent builds every statement here
//! from validated names, quoting identifiers and literals itself.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::spec::{self, ClusterSpec, Database, Role};

/// `"name"` with embedded quotes doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'value'` with embedded quotes doubled. Backslashes are literal because
/// `standard_conforming_strings` is on (the default since 9.1); an `E''`
/// string is never produced. NUL cannot be sent to PostgreSQL and is dropped.
pub fn quote_literal(value: &str) -> String {
    let cleaned: String = value.chars().filter(|c| *c != '\0').collect();
    format!("'{}'", cleaned.replace('\'', "''"))
}

/// Create the role if missing. Kept free of passwords so the dollar-quoted
/// block never contains operator text other than a validated identifier.
pub fn create_role_if_missing_sql(name: &str) -> String {
    format!(
        "DO $kcore$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {}) THEN CREATE ROLE {}; END IF; END $kcore$;",
        quote_literal(name),
        quote_ident(name)
    )
}

/// Bring an existing role to the spec. `verifier` is a SCRAM-SHA-256
/// verifier; PostgreSQL stores it as-is and never sees a plaintext password.
pub fn alter_role_sql(role: &Role, verifier: Option<&str>) -> String {
    let mut sql = format!(
        "ALTER ROLE {} WITH {} {} CONNECTION LIMIT {}",
        quote_ident(&role.name),
        if role.login { "LOGIN" } else { "NOLOGIN" },
        if role.create_db {
            "CREATEDB"
        } else {
            "NOCREATEDB"
        },
        role.connection_limit.max(-1)
    );
    if let Some(v) = verifier.filter(|v| spec::valid_scram_verifier(v)) {
        sql.push_str(&format!(" PASSWORD {}", quote_literal(v)));
    }
    sql.push(';');
    sql
}

pub fn grant_role_sql(parent: &str, member: &str) -> String {
    format!("GRANT {} TO {};", quote_ident(parent), quote_ident(member))
}

pub fn drop_role_sql(name: &str) -> String {
    format!(
        "REASSIGN OWNED BY {0} TO postgres; DROP OWNED BY {0}; DROP ROLE IF EXISTS {0};",
        quote_ident(name)
    )
}

pub fn database_exists_sql(name: &str) -> String {
    format!(
        "SELECT 1 FROM pg_database WHERE datname = {}",
        quote_literal(name)
    )
}

/// `CREATE DATABASE` cannot run in a transaction block; the agent sends it
/// alone after [`database_exists_sql`] returned no row.
pub fn create_database_sql(db: &Database) -> String {
    format!(
        "CREATE DATABASE {} OWNER {} ENCODING 'UTF8' TEMPLATE template0;",
        quote_ident(&db.name),
        quote_ident(&db.owner)
    )
}

pub fn alter_database_owner_sql(db: &Database) -> String {
    format!(
        "ALTER DATABASE {} OWNER TO {};",
        quote_ident(&db.name),
        quote_ident(&db.owner)
    )
}

pub fn drop_database_sql(name: &str) -> String {
    format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE);",
        quote_ident(name)
    )
}

pub fn create_extension_sql(name: &str) -> String {
    format!("CREATE EXTENSION IF NOT EXISTS {};", quote_ident(name))
}

pub fn update_extension_sql(name: &str) -> String {
    format!("ALTER EXTENSION {} UPDATE;", quote_ident(name))
}

/// `name = 'context'` rows from `pg_settings` for the given keys.
pub fn settings_context_sql(keys: &[String]) -> String {
    let list: Vec<String> = keys.iter().map(|k| quote_literal(k)).collect();
    format!(
        "SELECT name, context FROM pg_settings WHERE name IN ({})",
        if list.is_empty() {
            "''".to_string()
        } else {
            list.join(", ")
        }
    )
}

/// Keys whose context is `postmaster`, from `name|context` lines.
pub fn parse_restart_keys(rows: &str) -> Vec<String> {
    rows.lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(_, ctx)| ctx.trim() == "postmaster")
        .map(|(name, _)| name.trim().to_string())
        .collect()
}

fn kib(bytes: u64) -> String {
    format!("{}kB", (bytes / 1024).max(128))
}

/// PostgreSQL parameters for the dynamic config: kcore's floor, values
/// derived from the memory limit, then the spec's own keys.
pub fn effective_parameters(spec: &ClusterSpec) -> BTreeMap<String, String> {
    let mut params: BTreeMap<String, String> = BTreeMap::new();
    params.insert("wal_level".into(), "replica".into());
    params.insert("hot_standby".into(), "on".into());
    params.insert("wal_log_hints".into(), "on".into());
    params.insert("max_wal_senders".into(), "10".into());
    params.insert("max_replication_slots".into(), "10".into());
    params.insert("wal_keep_size".into(), "1GB".into());
    params.insert("password_encryption".into(), "scram-sha-256".into());
    if spec.memory_limit_bytes > 0 {
        params.insert("shared_buffers".into(), kib(spec.memory_limit_bytes / 4));
        params.insert(
            "effective_cache_size".into(),
            kib(spec.memory_limit_bytes / 10 * 6),
        );
    }
    for (k, v) in &spec.parameters {
        params.insert(k.clone(), v.clone());
    }
    let mut preload: Vec<String> = params
        .get("shared_preload_libraries")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    for lib in spec::preload_libraries(spec) {
        if !preload.contains(&lib) {
            preload.push(lib);
        }
    }
    if preload.is_empty() {
        params.remove("shared_preload_libraries");
    } else {
        params.insert("shared_preload_libraries".into(), preload.join(","));
    }
    params
}

/// Patroni dynamic configuration (`bootstrap.dcs` on create, the target of
/// `PATCH /config` afterwards).
pub fn dynamic_config(spec: &ClusterSpec) -> Value {
    let r = &spec.replication;
    let (sync_mode, strict): (Value, bool) = match r.mode.as_str() {
        spec::MODE_QUORUM => (json!("quorum"), false),
        spec::MODE_SYNC => (json!(true), false),
        spec::MODE_STRICT => (json!(true), true),
        _ => (json!(false), false),
    };
    json!({
        "ttl": spec.patroni.ttl,
        "loop_wait": spec.patroni.loop_wait,
        "retry_timeout": spec.patroni.retry_timeout,
        "maximum_lag_on_failover": r.maximum_lag_on_failover_bytes,
        "synchronous_mode": sync_mode,
        "synchronous_mode_strict": strict,
        "synchronous_node_count": r.sync_count,
        "postgresql": {
            "use_pg_rewind": true,
            "use_slots": true,
            "parameters": effective_parameters(spec),
        },
    })
}

/// `PATCH /config` body taking the cluster from `old` to `new`. Parameters
/// that left the spec are sent as `null`, which Patroni treats as removal.
pub fn dynamic_config_patch(old: &ClusterSpec, new: &ClusterSpec) -> Value {
    let mut body = dynamic_config(new);
    let old_params = effective_parameters(old);
    let new_params = effective_parameters(new);
    if let Some(params) = body
        .get_mut("postgresql")
        .and_then(|p| p.get_mut("parameters"))
        .and_then(Value::as_object_mut)
    {
        for key in old_params.keys() {
            if !new_params.contains_key(key) {
                params.insert(key.clone(), Value::Null);
            }
        }
    }
    body
}

pub fn pause_body(paused: bool) -> Value {
    json!({ "pause": paused })
}

pub fn switchover_body(leader: &str, candidate: &str, scheduled_at: &str) -> Value {
    let mut body = Map::new();
    body.insert("leader".into(), json!(leader));
    if !candidate.is_empty() {
        body.insert("candidate".into(), json!(candidate));
    }
    if !scheduled_at.is_empty() {
        body.insert("scheduled_at".into(), json!(scheduled_at));
    }
    Value::Object(body)
}

pub fn failover_body(candidate: &str) -> Value {
    json!({ "candidate": candidate })
}

/// One member of `GET /cluster`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterMember {
    pub name: String,
    /// leader, standby_leader, sync_standby, quorum_standby, replica
    pub role: String,
    /// running, streaming, starting, stopped, start failed, ...
    pub state: String,
    pub host: String,
    pub timeline: i64,
    /// Bytes behind the leader; -1 when Patroni says "unknown".
    pub lag_bytes: i64,
    pub pending_restart: bool,
    pub nofailover: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClusterView {
    pub members: Vec<ClusterMember>,
    pub paused: bool,
}

impl ClusterView {
    pub fn leader(&self) -> Option<&ClusterMember> {
        self.members.iter().find(|m| m.role == "leader")
    }
}

fn lag_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(-1),
        Some(Value::String(s)) => s.parse().unwrap_or(-1),
        None => 0,
        _ => -1,
    }
}

/// Parse a `GET /cluster` body.
pub fn parse_cluster(body: &str) -> Result<ClusterView, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("GET /cluster: {e}"))?;
    let members = v
        .get("members")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|m| ClusterMember {
            name: m.get("name").and_then(Value::as_str).unwrap_or("").into(),
            role: m.get("role").and_then(Value::as_str).unwrap_or("").into(),
            state: m.get("state").and_then(Value::as_str).unwrap_or("").into(),
            host: m.get("host").and_then(Value::as_str).unwrap_or("").into(),
            timeline: m.get("timeline").and_then(Value::as_i64).unwrap_or(0),
            lag_bytes: if m.get("role").and_then(Value::as_str) == Some("leader") {
                0
            } else {
                lag_of(m.get("lag"))
            },
            pending_restart: m
                .get("pending_restart")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            nofailover: m
                .get("tags")
                .and_then(|t| t.get("nofailover"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
        .collect();
    let paused = v.get("pause").and_then(Value::as_bool).unwrap_or(false);
    Ok(ClusterView { members, paused })
}

/// This member from `GET /patroni`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatroniStatus {
    /// primary or replica (Patroni 4 reports "primary"; older "master")
    pub role: String,
    pub state: String,
    pub timeline: i64,
    pub pending_restart: bool,
    pub paused: bool,
    /// e.g. 170009
    pub server_version: i64,
}

pub fn parse_patroni(body: &str) -> Result<PatroniStatus, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("GET /patroni: {e}"))?;
    let role = match v.get("role").and_then(Value::as_str).unwrap_or("") {
        "master" => "primary",
        other => other,
    };
    Ok(PatroniStatus {
        role: role.into(),
        state: v.get("state").and_then(Value::as_str).unwrap_or("").into(),
        timeline: v.get("timeline").and_then(Value::as_i64).unwrap_or(0),
        pending_restart: v
            .get("pending_restart")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        paused: v.get("pause").and_then(Value::as_bool).unwrap_or(false),
        server_version: v.get("server_version").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// `170009` to `17.9`.
pub fn format_server_version(num: i64) -> String {
    if num <= 0 {
        return String::new();
    }
    format!("{}.{}", num / 10000, num % 10000)
}

/// `etcdctl endpoint status -w json` for one endpoint: `(is_leader, raft_index)`.
pub fn parse_etcd_status(body: &str) -> Result<(bool, i64), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("etcdctl: {e}"))?;
    let entry = v.as_array().and_then(|a| a.first()).unwrap_or(&v);
    let status = entry.get("Status").unwrap_or(entry);
    let member = status
        .get("header")
        .and_then(|h| h.get("member_id"))
        .and_then(Value::as_u64);
    let leader = status.get("leader").and_then(Value::as_u64);
    let index = status.get("raftIndex").and_then(Value::as_i64).unwrap_or(0);
    Ok((member.is_some() && member == leader, index))
}

/// `Database system identifier:` from `pg_controldata` output.
pub fn parse_system_identifier(controldata: &str) -> Option<String> {
    controldata.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == "Database system identifier").then(|| v.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::tests::sample;

    #[test]
    fn quoting_doubles_quotes() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal("a\0b"), "'ab'");
        assert_eq!(
            quote_literal("x'; DROP TABLE t; --"),
            "'x''; DROP TABLE t; --'"
        );
    }

    #[test]
    fn role_sql_never_embeds_a_password_in_the_do_block() {
        let (_, spec) = sample();
        let role = &spec.roles[0];
        let create = create_role_if_missing_sql(&role.name);
        assert!(create.contains("CREATE ROLE \"app\""));
        let verifier = "SCRAM-SHA-256$4096:c2FsdA==$c3RvcmVk:c2VydmVy";
        let alter = alter_role_sql(role, Some(verifier));
        assert!(alter.starts_with("ALTER ROLE \"app\" WITH LOGIN NOCREATEDB CONNECTION LIMIT -1"));
        assert!(alter.contains(&format!("PASSWORD '{verifier}'")));
        assert!(!alter_role_sql(role, Some("plaintext")).contains("PASSWORD"));
    }

    #[test]
    fn database_and_extension_sql() {
        let (_, spec) = sample();
        assert_eq!(
            create_database_sql(&spec.databases[0]),
            "CREATE DATABASE \"orders\" OWNER \"app\" ENCODING 'UTF8' TEMPLATE template0;"
        );
        assert_eq!(
            create_extension_sql("uuid-ossp"),
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\";"
        );
        assert_eq!(
            settings_context_sql(&["work_mem".into(), "shared_buffers".into()]),
            "SELECT name, context FROM pg_settings WHERE name IN ('work_mem', 'shared_buffers')"
        );
        assert_eq!(
            parse_restart_keys("work_mem|user\nshared_buffers|postmaster\n"),
            vec!["shared_buffers"]
        );
    }

    #[test]
    fn dynamic_config_carries_mode_parameters_and_preloads() {
        let (_, mut spec) = sample();
        spec.memory_limit_bytes = 32 << 30;
        let dcs = dynamic_config(&spec);
        assert_eq!(dcs["synchronous_mode"], json!("quorum"));
        assert_eq!(dcs["synchronous_node_count"], json!(1));
        let params = &dcs["postgresql"]["parameters"];
        assert_eq!(params["max_connections"], json!("300"));
        assert_eq!(params["shared_buffers"], json!("8388608kB"));
        assert_eq!(
            params["shared_preload_libraries"],
            json!("pg_stat_statements")
        );
        spec.replication.mode = "strict".into();
        let dcs = dynamic_config(&spec);
        assert_eq!(dcs["synchronous_mode"], json!(true));
        assert_eq!(dcs["synchronous_mode_strict"], json!(true));
    }

    #[test]
    fn patch_removes_parameters_that_left_the_spec() {
        let (_, old) = sample();
        let mut new = old.clone();
        new.parameters.remove("work_mem");
        let patch = dynamic_config_patch(&old, &new);
        assert_eq!(patch["postgresql"]["parameters"]["work_mem"], Value::Null);
    }

    const CLUSTER: &str = r#"{"members":[
      {"name":"node-105","role":"leader","state":"running","host":"192.168.40.105","port":5432,"timeline":3},
      {"name":"node-106","role":"quorum_standby","state":"streaming","host":"192.168.40.106","timeline":3,"lag":0},
      {"name":"node-108","role":"replica","state":"streaming","host":"192.168.40.108","timeline":3,"lag":"unknown","pending_restart":true,"tags":{"nofailover":true}}
    ],"pause":true,"scope":"orders"}"#;

    #[test]
    fn parses_cluster_view() {
        let view = parse_cluster(CLUSTER).unwrap();
        assert!(view.paused);
        assert_eq!(view.leader().unwrap().name, "node-105");
        assert_eq!(view.members[1].role, "quorum_standby");
        assert_eq!(view.members[2].lag_bytes, -1);
        assert!(view.members[2].pending_restart && view.members[2].nofailover);
    }

    #[test]
    fn parses_patroni_and_etcd_status() {
        let p = parse_patroni(
            r#"{"state":"running","role":"master","server_version":170009,"timeline":2,"pending_restart":true}"#,
        )
        .unwrap();
        assert_eq!(p.role, "primary");
        assert_eq!(format_server_version(p.server_version), "17.9");
        let etcd = r#"[{"Endpoint":"https://127.0.0.1:2479","Status":{"header":{"member_id":42},"leader":42,"raftIndex":9}}]"#;
        assert_eq!(parse_etcd_status(etcd).unwrap(), (true, 9));
        assert_eq!(
            parse_system_identifier(
                "pg_control version number:  1300\nDatabase system identifier:           7299\n"
            ),
            Some("7299".into())
        );
    }

    proptest::proptest! {
        #[test]
        fn quoted_literal_has_only_paired_quotes(s in "\\PC{0,64}") {
            let q = quote_literal(&s);
            let inner = &q[1..q.len() - 1];
            proptest::prop_assert_eq!(inner.replace("''", "").matches('\'').count(), 0);
        }

        #[test]
        fn quoted_ident_has_only_paired_quotes(s in "\\PC{0,64}") {
            let q = quote_ident(&s);
            let inner = &q[1..q.len() - 1];
            proptest::prop_assert_eq!(inner.replace("\"\"", "").matches('"').count(), 0);
        }
    }
}
