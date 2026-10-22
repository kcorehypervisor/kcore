//! ClickHouse cluster rows. Secrets sit in this database the same way Ceph
//! dashboard credentials do: the controller store is the secret store.

use rusqlite::{params, OptionalExtension};

use crate::db::Database;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS clickhouse_clusters (
    name TEXT PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 1,
    phase TEXT NOT NULL DEFAULT 'pending',
    message TEXT NOT NULL DEFAULT '',
    spec_json TEXT NOT NULL,
    observed_version TEXT NOT NULL DEFAULT '',
    active_operation_id TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS clickhouse_cluster_members (
    cluster_name TEXT NOT NULL REFERENCES clickhouse_clusters(name) ON DELETE CASCADE,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    server INTEGER NOT NULL DEFAULT 0,
    keeper INTEGER NOT NULL DEFAULT 0,
    shard_name TEXT NOT NULL DEFAULT '',
    replica_name TEXT NOT NULL DEFAULT '',
    server_id INTEGER NOT NULL DEFAULT 0,
    learner INTEGER NOT NULL DEFAULT 0,
    advertise_address TEXT NOT NULL DEFAULT '',
    iface TEXT NOT NULL DEFAULT '',
    read_only INTEGER NOT NULL DEFAULT 0,
    health_json TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (cluster_name, node_id)
);
CREATE TABLE IF NOT EXISTS clickhouse_cluster_secrets (
    cluster_name TEXT PRIMARY KEY REFERENCES clickhouse_clusters(name) ON DELETE CASCADE,
    default_password_enc TEXT NOT NULL,
    default_sha256_enc TEXT NOT NULL,
    metrics_password_enc TEXT NOT NULL,
    metrics_sha256_enc TEXT NOT NULL,
    interserver_password_enc TEXT NOT NULL,
    keeper_password_enc TEXT NOT NULL,
    cluster_secret_enc TEXT NOT NULL,
    ca_cert_pem TEXT NOT NULL,
    ca_key_pem TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clickhouse_member_certs (
    cluster_name TEXT NOT NULL REFERENCES clickhouse_clusters(name) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    cert_pem TEXT NOT NULL,
    key_pem TEXT NOT NULL,
    not_after_unix INTEGER NOT NULL,
    PRIMARY KEY (cluster_name, node_id)
);
CREATE TABLE IF NOT EXISTS clickhouse_operations (
    id TEXT PRIMARY KEY,
    cluster_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    steps_json TEXT NOT NULL,
    context_json TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    force INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_clickhouse_operations_cluster ON clickhouse_operations(cluster_name, state);
CREATE TABLE IF NOT EXISTS clickhouse_backups (
    cluster_name TEXT NOT NULL,
    backup_name TEXT NOT NULL,
    target_json TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY (cluster_name, backup_name)
);
";

#[derive(Debug, Clone)]
pub struct ClusterRow {
    pub name: String,
    pub generation: i64,
    pub phase: String,
    pub message: String,
    pub spec_json: String,
    pub observed_version: String,
    pub active_operation_id: String,
}

#[derive(Debug, Clone)]
pub struct MemberRow {
    pub cluster_name: String,
    pub node_id: String,
    pub server: bool,
    pub keeper: bool,
    pub shard_name: String,
    pub replica_name: String,
    pub server_id: i32,
    pub learner: bool,
    pub advertise_address: String,
    pub iface: String,
    pub read_only: bool,
    pub health_json: String,
}

#[derive(Debug, Clone)]
pub struct SecretRow {
    pub default_password: String,
    pub default_sha256: String,
    pub metrics_password: String,
    pub metrics_sha256: String,
    pub interserver_password: String,
    pub keeper_password: String,
    pub cluster_secret: String,
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
}

#[derive(Debug, Clone)]
pub struct CertRow {
    pub node_id: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after_unix: i64,
}

#[derive(Debug, Clone)]
pub struct OperationRow {
    pub id: String,
    pub cluster_name: String,
    pub kind: String,
    pub state: String,
    pub steps_json: String,
    pub context_json: String,
    pub message: String,
    pub force: bool,
}

impl Database {
    pub fn insert_clickhouse_cluster(&self, row: &ClusterRow) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO clickhouse_clusters (name, generation, phase, message, spec_json, observed_version, active_operation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.name,
                row.generation,
                row.phase,
                row.message,
                row.spec_json,
                row.observed_version,
                row.active_operation_id
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn update_clickhouse_cluster(&self, row: &ClusterRow) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE clickhouse_clusters
             SET generation = ?2, phase = ?3, message = ?4, spec_json = ?5,
                 observed_version = ?6, active_operation_id = ?7, updated_at = datetime('now')
             WHERE name = ?1",
            params![
                row.name,
                row.generation,
                row.phase,
                row.message,
                row.spec_json,
                row.observed_version,
                row.active_operation_id
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_clickhouse_cluster(&self, name: &str) -> Result<Option<ClusterRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT name, generation, phase, message, spec_json, observed_version, active_operation_id
             FROM clickhouse_clusters WHERE name = ?1",
            params![name],
            |row| {
                Ok(ClusterRow {
                    name: row.get(0)?,
                    generation: row.get(1)?,
                    phase: row.get(2)?,
                    message: row.get(3)?,
                    spec_json: row.get(4)?,
                    observed_version: row.get(5)?,
                    active_operation_id: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn list_clickhouse_clusters(&self) -> Result<Vec<ClusterRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT name, generation, phase, message, spec_json, observed_version, active_operation_id
                 FROM clickhouse_clusters ORDER BY name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ClusterRow {
                    name: row.get(0)?,
                    generation: row.get(1)?,
                    phase: row.get(2)?,
                    message: row.get(3)?,
                    spec_json: row.get(4)?,
                    observed_version: row.get(5)?,
                    active_operation_id: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn delete_clickhouse_cluster(&self, name: &str) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM clickhouse_clusters WHERE name = ?1",
            params![name],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn replace_clickhouse_members(
        &self,
        cluster: &str,
        members: &[MemberRow],
    ) -> Result<(), String> {
        let mut conn = self.lock_conn().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        for member in members {
            let other: Option<String> = tx
                .query_row(
                    "SELECT cluster_name FROM clickhouse_cluster_members
                     WHERE node_id = ?1 AND cluster_name != ?2",
                    params![member.node_id, cluster],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some(other) = other {
                return Err(node_in_use(&member.node_id, &other));
            }
        }
        let prior = {
            let mut stmt = tx
                .prepare(
                    "SELECT node_id, health_json FROM clickhouse_cluster_members
                     WHERE cluster_name = ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![cluster], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            let mut prior = std::collections::HashMap::new();
            for row in rows {
                let (node_id, health) = row.map_err(|e| e.to_string())?;
                prior.insert(node_id, health);
            }
            prior
        };
        for member in members {
            let health = if member.health_json.is_empty() {
                prior.get(&member.node_id).cloned().unwrap_or_default()
            } else {
                member.health_json.clone()
            };
            tx.execute(
                "INSERT INTO clickhouse_cluster_members (
                    cluster_name, node_id, server, keeper, shard_name, replica_name, server_id,
                    learner, advertise_address, iface, read_only, health_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(cluster_name, node_id) DO UPDATE SET
                    server = excluded.server,
                    keeper = excluded.keeper,
                    shard_name = excluded.shard_name,
                    replica_name = excluded.replica_name,
                    server_id = excluded.server_id,
                    learner = excluded.learner,
                    advertise_address = excluded.advertise_address,
                    iface = excluded.iface,
                    read_only = excluded.read_only,
                    health_json = excluded.health_json",
                params![
                    cluster,
                    member.node_id,
                    i32::from(member.server),
                    i32::from(member.keeper),
                    member.shard_name,
                    member.replica_name,
                    member.server_id,
                    i32::from(member.learner),
                    member.advertise_address,
                    member.iface,
                    i32::from(member.read_only),
                    health
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Drop member rows that are no longer in the spec. Departing nodes stay
    /// claimed until the operation that disables them has succeeded, so a
    /// second cluster cannot take the node while its unit still belongs to this one.
    pub fn prune_clickhouse_members(&self, cluster: &str, keep: &[String]) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        if keep.is_empty() {
            conn.execute(
                "DELETE FROM clickhouse_cluster_members WHERE cluster_name = ?1",
                params![cluster],
            )
            .map_err(|e| e.to_string())?;
            return Ok(());
        }
        let placeholders = std::iter::repeat_n("?", keep.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "DELETE FROM clickhouse_cluster_members
             WHERE cluster_name = ?1 AND node_id NOT IN ({placeholders})"
        );
        let mut args = Vec::with_capacity(keep.len() + 1);
        args.push(cluster.to_string());
        args.extend(keep.iter().cloned());
        conn.execute(&sql, rusqlite::params_from_iter(args))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clickhouse_node_conflicts(
        &self,
        cluster: &str,
        node_ids: &[String],
    ) -> Result<Vec<String>, String> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let placeholders = std::iter::repeat_n("?", node_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT node_id, cluster_name FROM clickhouse_cluster_members
             WHERE cluster_name != ?1 AND node_id IN ({placeholders})
             ORDER BY node_id"
        );
        let mut args = Vec::with_capacity(node_ids.len() + 1);
        args.push(cluster.to_string());
        args.extend(node_ids.iter().cloned());
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut conflicts = Vec::new();
        for row in rows {
            let (node_id, other) = row.map_err(|e| e.to_string())?;
            conflicts.push(node_in_use(&node_id, &other));
        }
        Ok(conflicts)
    }

    pub fn list_clickhouse_members(&self, cluster: &str) -> Result<Vec<MemberRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT cluster_name, node_id, server, keeper, shard_name, replica_name, server_id,
                        learner, advertise_address, iface, read_only, health_json
                 FROM clickhouse_cluster_members WHERE cluster_name = ?1 ORDER BY node_id",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![cluster], member_from_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn clickhouse_memberships_for_node(&self, node_id: &str) -> Result<Vec<MemberRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT cluster_name, node_id, server, keeper, shard_name, replica_name, server_id,
                        learner, advertise_address, iface, read_only, health_json
                 FROM clickhouse_cluster_members WHERE node_id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![node_id], member_from_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn set_clickhouse_member_health(
        &self,
        cluster: &str,
        node_id: &str,
        health_json: &str,
    ) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE clickhouse_cluster_members SET health_json = ?3 WHERE cluster_name = ?1 AND node_id = ?2",
            params![cluster, node_id, health_json],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn upsert_clickhouse_secrets(
        &self,
        cluster: &str,
        secrets: &SecretRow,
    ) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO clickhouse_cluster_secrets (
                cluster_name, default_password_enc, default_sha256_enc, metrics_password_enc,
                metrics_sha256_enc, interserver_password_enc, keeper_password_enc, cluster_secret_enc,
                ca_cert_pem, ca_key_pem
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(cluster_name) DO UPDATE SET
                default_password_enc = excluded.default_password_enc,
                default_sha256_enc = excluded.default_sha256_enc,
                metrics_password_enc = excluded.metrics_password_enc,
                metrics_sha256_enc = excluded.metrics_sha256_enc,
                interserver_password_enc = excluded.interserver_password_enc,
                keeper_password_enc = excluded.keeper_password_enc,
                cluster_secret_enc = excluded.cluster_secret_enc,
                ca_cert_pem = excluded.ca_cert_pem,
                ca_key_pem = excluded.ca_key_pem",
            params![
                cluster,
                secrets.default_password,
                secrets.default_sha256,
                secrets.metrics_password,
                secrets.metrics_sha256,
                secrets.interserver_password,
                secrets.keeper_password,
                secrets.cluster_secret,
                secrets.ca_cert_pem,
                secrets.ca_key_pem
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_clickhouse_secrets(&self, cluster: &str) -> Result<Option<SecretRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT default_password_enc, default_sha256_enc, metrics_password_enc, metrics_sha256_enc,
                    interserver_password_enc, keeper_password_enc, cluster_secret_enc, ca_cert_pem, ca_key_pem
             FROM clickhouse_cluster_secrets WHERE cluster_name = ?1",
            params![cluster],
            |row| {
                Ok(SecretRow {
                    default_password: row.get(0)?,
                    default_sha256: row.get(1)?,
                    metrics_password: row.get(2)?,
                    metrics_sha256: row.get(3)?,
                    interserver_password: row.get(4)?,
                    keeper_password: row.get(5)?,
                    cluster_secret: row.get(6)?,
                    ca_cert_pem: row.get(7)?,
                    ca_key_pem: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn replace_clickhouse_certs(&self, cluster: &str, certs: &[CertRow]) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "DELETE FROM clickhouse_member_certs WHERE cluster_name = ?1",
            params![cluster],
        )
        .map_err(|e| e.to_string())?;
        for cert in certs {
            conn.execute(
                "INSERT INTO clickhouse_member_certs (cluster_name, node_id, cert_pem, key_pem, not_after_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![cluster, cert.node_id, cert.cert_pem, cert.key_pem, cert.not_after_unix],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn list_clickhouse_certs(&self, cluster: &str) -> Result<Vec<CertRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT node_id, cert_pem, key_pem, not_after_unix FROM clickhouse_member_certs WHERE cluster_name = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![cluster], |row| {
                Ok(CertRow {
                    node_id: row.get(0)?,
                    cert_pem: row.get(1)?,
                    key_pem: row.get(2)?,
                    not_after_unix: row.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn upsert_clickhouse_cert(&self, cluster: &str, cert: &CertRow) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO clickhouse_member_certs (cluster_name, node_id, cert_pem, key_pem, not_after_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(cluster_name, node_id) DO UPDATE SET
               cert_pem = excluded.cert_pem,
               key_pem = excluded.key_pem,
               not_after_unix = excluded.not_after_unix",
            params![
                cluster,
                cert.node_id,
                cert.cert_pem,
                cert.key_pem,
                cert.not_after_unix
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn active_clickhouse_operation(
        &self,
        cluster: &str,
    ) -> Result<Option<OperationRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT id, cluster_name, kind, state, steps_json, context_json, message, force
             FROM clickhouse_operations
             WHERE cluster_name = ?1 AND state IN ('pending', 'running')
             ORDER BY created_at DESC LIMIT 1",
            params![cluster],
            operation_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn insert_clickhouse_operation(&self, row: &OperationRow) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO clickhouse_operations (id, cluster_name, kind, state, steps_json, context_json, message, force)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id,
                row.cluster_name,
                row.kind,
                row.state,
                row.steps_json,
                row.context_json,
                row.message,
                i32::from(row.force)
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn update_clickhouse_operation(&self, row: &OperationRow) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE clickhouse_operations
             SET state = ?2, steps_json = ?3, context_json = ?4, message = ?5, force = ?6,
                 updated_at = datetime('now')
             WHERE id = ?1",
            params![
                row.id,
                row.state,
                row.steps_json,
                row.context_json,
                row.message,
                i32::from(row.force)
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_clickhouse_operation(&self, id: &str) -> Result<Option<OperationRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.query_row(
            "SELECT id, cluster_name, kind, state, steps_json, context_json, message, force
             FROM clickhouse_operations WHERE id = ?1",
            params![id],
            operation_from_row,
        )
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn list_clickhouse_operations(&self, cluster: &str) -> Result<Vec<OperationRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, cluster_name, kind, state, steps_json, context_json, message, force
                 FROM clickhouse_operations WHERE cluster_name = ?1 ORDER BY created_at DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![cluster], operation_from_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn list_active_clickhouse_operations(&self) -> Result<Vec<OperationRow>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT id, cluster_name, kind, state, steps_json, context_json, message, force
                 FROM clickhouse_operations WHERE state IN ('pending', 'running') ORDER BY created_at",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], operation_from_row)
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn upsert_clickhouse_backup(
        &self,
        cluster: &str,
        name: &str,
        target_json: &str,
        status: &str,
    ) -> Result<(), String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO clickhouse_backups (cluster_name, backup_name, target_json, status)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cluster_name, backup_name) DO UPDATE SET status = excluded.status, target_json = excluded.target_json",
            params![cluster, name, target_json, status],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_clickhouse_backups(
        &self,
        cluster: &str,
    ) -> Result<Vec<(String, String, String)>, String> {
        let conn = self.lock_conn().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT backup_name, status, target_json FROM clickhouse_backups WHERE cluster_name = ?1 ORDER BY backup_name",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![cluster], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }
}

fn member_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemberRow> {
    let server: i32 = row.get(2)?;
    let keeper: i32 = row.get(3)?;
    let learner: i32 = row.get(7)?;
    let read_only: i32 = row.get(10)?;
    Ok(MemberRow {
        cluster_name: row.get(0)?,
        node_id: row.get(1)?,
        server: server != 0,
        keeper: keeper != 0,
        shard_name: row.get(4)?,
        replica_name: row.get(5)?,
        server_id: row.get(6)?,
        learner: learner != 0,
        advertise_address: row.get(8)?,
        iface: row.get(9)?,
        read_only: read_only != 0,
        health_json: row.get(11)?,
    })
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRow> {
    let force: i32 = row.get(7)?;
    Ok(OperationRow {
        id: row.get(0)?,
        cluster_name: row.get(1)?,
        kind: row.get(2)?,
        state: row.get(3)?,
        steps_json: row.get(4)?,
        context_json: row.get(5)?,
        message: row.get(6)?,
        force: force != 0,
    })
}

fn node_in_use(node: &str, other: &str) -> String {
    format!("node {node} already runs ClickHouse cluster {other}")
}

/// Block a host rollout when this node is a ClickHouse member and taking it
/// down would drop the last healthy writable replica or Keeper quorum.
pub fn take_down_block(db: &Database, node_id: &str) -> Result<(), String> {
    let memberships = db.clickhouse_memberships_for_node(node_id)?;
    if memberships.is_empty() {
        return Ok(());
    }
    for membership in memberships {
        let cluster = db
            .get_clickhouse_cluster(&membership.cluster_name)?
            .ok_or_else(|| format!("clickhouse cluster {} disappeared", membership.cluster_name))?;
        let spec: kcore_clickhouse::ClusterSpec =
            serde_json::from_str(&cluster.spec_json).map_err(|e| e.to_string())?;
        let members = db.list_clickhouse_members(&membership.cluster_name)?;
        if members.iter().any(|member| member.health_json.is_empty()) {
            return Err(format!(
                "clickhouse health for {} has not been observed",
                membership.cluster_name
            ));
        }
        let mut health = Vec::new();
        for member in &members {
            let mut row: kcore_clickhouse::rollout::MemberHealth =
                serde_json::from_str(&member.health_json).map_err(|e| e.to_string())?;
            row.node_id = member.node_id.clone();
            health.push(row);
        }
        kcore_clickhouse::safe_to_take_down(&spec, node_id, &health)?;
    }
    Ok(())
}
