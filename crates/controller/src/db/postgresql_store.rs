//! PostgreSQL clusters, members, secrets, certificates, operations and
//! events (schema 72).
//!
//! The spec is the normalized `kcore_postgresql::ClusterSpec` as JSON.
//! Generated secrets sit in this database the same way Ceph dashboard
//! credentials and Slurm munge keys do: the controller store is the secret
//! store. A node belongs to at most one cluster (`node_id` is the members'
//! primary key).

use rusqlite::{params, OptionalExtension, Row};

use super::Database;

pub const POSTGRESQL_TABLES: &str = "
CREATE TABLE IF NOT EXISTS postgresql_clusters (
    name TEXT PRIMARY KEY,
    generation INTEGER NOT NULL DEFAULT 1,
    observed_generation INTEGER NOT NULL DEFAULT 0,
    phase TEXT NOT NULL DEFAULT 'pending',
    message TEXT NOT NULL DEFAULT '',
    spec_json TEXT NOT NULL,
    previous_package TEXT NOT NULL DEFAULT '',
    leader_node_id TEXT NOT NULL DEFAULT '',
    timeline INTEGER NOT NULL DEFAULT 0,
    observed_version TEXT NOT NULL DEFAULT '',
    paused INTEGER NOT NULL DEFAULT 0,
    active_operation_id TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS postgresql_cluster_members (
    node_id TEXT PRIMARY KEY,
    cluster_name TEXT NOT NULL REFERENCES postgresql_clusters(name) ON DELETE CASCADE,
    etcd_voter INTEGER NOT NULL DEFAULT 0,
    applied_generation INTEGER NOT NULL DEFAULT 0,
    health_json TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_postgresql_cluster_members_cluster ON postgresql_cluster_members(cluster_name);
CREATE TABLE IF NOT EXISTS postgresql_cluster_secrets (
    cluster_name TEXT PRIMARY KEY REFERENCES postgresql_clusters(name) ON DELETE CASCADE,
    superuser_password TEXT NOT NULL,
    replication_password TEXT NOT NULL,
    rewind_password TEXT NOT NULL,
    restapi_password TEXT NOT NULL,
    ca_cert_pem TEXT NOT NULL,
    ca_key_pem TEXT NOT NULL,
    role_passwords_json TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS postgresql_member_certs (
    cluster_name TEXT NOT NULL REFERENCES postgresql_clusters(name) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    cert_pem TEXT NOT NULL,
    key_pem TEXT NOT NULL,
    not_after_unix INTEGER NOT NULL,
    PRIMARY KEY (cluster_name, node_id)
);
CREATE TABLE IF NOT EXISTS postgresql_operations (
    id TEXT PRIMARY KEY,
    cluster_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    target_generation INTEGER NOT NULL DEFAULT 0,
    steps_json TEXT NOT NULL,
    context_json TEXT NOT NULL DEFAULT '{}',
    message TEXT NOT NULL DEFAULT '',
    force INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_postgresql_operations_cluster ON postgresql_operations(cluster_name, state);
CREATE TABLE IF NOT EXISTS postgresql_cluster_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    cluster_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    old_leader TEXT NOT NULL DEFAULT '',
    new_leader TEXT NOT NULL DEFAULT '',
    timeline INTEGER NOT NULL DEFAULT 0,
    message TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_postgresql_cluster_events_cluster ON postgresql_cluster_events(cluster_name, id);
";

pub const POSTGRESQL_TABLES_DOWN: &str = "DROP TABLE IF EXISTS postgresql_cluster_events;
DROP TABLE IF EXISTS postgresql_operations;
DROP TABLE IF EXISTS postgresql_member_certs;
DROP TABLE IF EXISTS postgresql_cluster_secrets;
DROP TABLE IF EXISTS postgresql_cluster_members;
DROP TABLE IF EXISTS postgresql_clusters;";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgClusterRow {
    pub name: String,
    pub generation: i64,
    pub observed_generation: i64,
    pub phase: String,
    pub message: String,
    pub spec_json: String,
    pub previous_package: String,
    pub leader_node_id: String,
    pub timeline: i64,
    pub observed_version: String,
    pub paused: bool,
    pub active_operation_id: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgMemberRow {
    pub cluster_name: String,
    pub node_id: String,
    pub etcd_voter: bool,
    pub applied_generation: i64,
    pub health_json: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgSecretRow {
    pub cluster_name: String,
    pub superuser_password: String,
    pub replication_password: String,
    pub rewind_password: String,
    pub restapi_password: String,
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub role_passwords_json: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgCertRow {
    pub cluster_name: String,
    pub node_id: String,
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgOperationRow {
    pub id: String,
    pub cluster_name: String,
    pub kind: String,
    pub state: String,
    pub target_generation: i64,
    pub steps_json: String,
    pub context_json: String,
    pub message: String,
    pub force: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgEventRow {
    pub id: i64,
    pub cluster_name: String,
    pub kind: String,
    pub old_leader: String,
    pub new_leader: String,
    pub timeline: i64,
    pub message: String,
    pub created_at: String,
}

const CLUSTER_COLUMNS: &str = "name, generation, observed_generation, phase, message, spec_json,
    previous_package, leader_node_id, timeline, observed_version, paused, active_operation_id,
    created_at, updated_at";

fn cluster_row(r: &Row<'_>) -> rusqlite::Result<PgClusterRow> {
    Ok(PgClusterRow {
        name: r.get(0)?,
        generation: r.get(1)?,
        observed_generation: r.get(2)?,
        phase: r.get(3)?,
        message: r.get(4)?,
        spec_json: r.get(5)?,
        previous_package: r.get(6)?,
        leader_node_id: r.get(7)?,
        timeline: r.get(8)?,
        observed_version: r.get(9)?,
        paused: r.get::<_, i64>(10)? != 0,
        active_operation_id: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

const OP_COLUMNS: &str = "id, cluster_name, kind, state, target_generation, steps_json,
    context_json, message, force, created_at, updated_at";

fn op_row(r: &Row<'_>) -> rusqlite::Result<PgOperationRow> {
    Ok(PgOperationRow {
        id: r.get(0)?,
        cluster_name: r.get(1)?,
        kind: r.get(2)?,
        state: r.get(3)?,
        target_generation: r.get(4)?,
        steps_json: r.get(5)?,
        context_json: r.get(6)?,
        message: r.get(7)?,
        force: r.get::<_, i64>(8)? != 0,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

impl Database {
    pub fn get_pg_cluster(&self, name: &str) -> Result<Option<PgClusterRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {CLUSTER_COLUMNS} FROM postgresql_clusters WHERE name=?1"),
            params![name],
            cluster_row,
        )
        .optional()
    }

    pub fn list_pg_clusters(&self) -> Result<Vec<PgClusterRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {CLUSTER_COLUMNS} FROM postgresql_clusters ORDER BY name"
        ))?;
        let rows = stmt.query_map([], cluster_row)?;
        rows.collect()
    }

    /// Insert or replace the cluster row with its spec, generation and phase.
    /// Live columns (leader, timeline, version) are kept on update.
    pub fn upsert_pg_cluster(&self, row: &PgClusterRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_clusters (name, generation, observed_generation, phase, message,
                spec_json, previous_package, active_operation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(name) DO UPDATE SET
               generation=excluded.generation,
               observed_generation=excluded.observed_generation,
               phase=excluded.phase,
               message=excluded.message,
               spec_json=excluded.spec_json,
               previous_package=excluded.previous_package,
               active_operation_id=excluded.active_operation_id,
               updated_at=datetime('now')",
            params![
                row.name,
                row.generation,
                row.observed_generation,
                row.phase,
                row.message,
                row.spec_json,
                row.previous_package,
                row.active_operation_id
            ],
        )?;
        Ok(())
    }

    pub fn set_pg_cluster_phase(
        &self,
        name: &str,
        phase: &str,
        message: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_clusters SET phase=?2, message=?3, updated_at=datetime('now') WHERE name=?1",
            params![name, phase, message],
        )?;
        Ok(())
    }

    pub fn set_pg_cluster_live(
        &self,
        name: &str,
        leader: &str,
        timeline: i64,
        version: &str,
        paused: bool,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_clusters SET leader_node_id=?2, timeline=?3, observed_version=?4,
                paused=?5, updated_at=datetime('now') WHERE name=?1",
            params![name, leader, timeline, version, paused as i64],
        )?;
        Ok(())
    }

    pub fn finish_pg_operation_on_cluster(
        &self,
        name: &str,
        observed_generation: Option<i64>,
        previous_package: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_clusters SET active_operation_id='',
                observed_generation=COALESCE(?2, observed_generation),
                previous_package=COALESCE(?3, previous_package),
                updated_at=datetime('now') WHERE name=?1",
            params![name, observed_generation, previous_package],
        )?;
        Ok(())
    }

    pub fn delete_pg_cluster(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM postgresql_cluster_members WHERE cluster_name=?1",
            params![name],
        )?;
        Ok(conn.execute(
            "DELETE FROM postgresql_clusters WHERE name=?1",
            params![name],
        )? > 0)
    }

    pub fn list_pg_members(&self, cluster: &str) -> Result<Vec<PgMemberRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT cluster_name, node_id, etcd_voter, applied_generation, health_json
             FROM postgresql_cluster_members WHERE cluster_name=?1 ORDER BY node_id",
        )?;
        let rows = stmt.query_map(params![cluster], |r| {
            Ok(PgMemberRow {
                cluster_name: r.get(0)?,
                node_id: r.get(1)?,
                etcd_voter: r.get::<_, i64>(2)? != 0,
                applied_generation: r.get(3)?,
                health_json: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Cluster a node belongs to, if any.
    pub fn pg_cluster_of_node(&self, node_id: &str) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT cluster_name FROM postgresql_cluster_members WHERE node_id=?1",
            params![node_id],
            |r| r.get(0),
        )
        .optional()
    }

    /// Claim a node for a cluster. Fails (unique key) when another cluster
    /// holds it.
    pub fn upsert_pg_member(&self, row: &PgMemberRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        let other: Option<String> = conn
            .query_row(
                "SELECT cluster_name FROM postgresql_cluster_members WHERE node_id=?1",
                params![row.node_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = other.filter(|o| *o != row.cluster_name) {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(19),
                Some(format!(
                    "node {} belongs to PostgreSQL cluster {other}",
                    row.node_id
                )),
            ));
        }
        conn.execute(
            "INSERT INTO postgresql_cluster_members (node_id, cluster_name, etcd_voter, applied_generation, health_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(node_id) DO UPDATE SET
               etcd_voter=excluded.etcd_voter,
               applied_generation=MAX(postgresql_cluster_members.applied_generation, excluded.applied_generation),
               updated_at=datetime('now')",
            params![
                row.node_id,
                row.cluster_name,
                row.etcd_voter as i64,
                row.applied_generation,
                row.health_json
            ],
        )?;
        Ok(())
    }

    pub fn set_pg_member_health(
        &self,
        node_id: &str,
        health_json: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_cluster_members SET health_json=?2, updated_at=datetime('now') WHERE node_id=?1",
            params![node_id, health_json],
        )?;
        Ok(())
    }

    pub fn set_pg_member_generation(
        &self,
        node_id: &str,
        generation: i64,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_cluster_members SET applied_generation=?2, updated_at=datetime('now') WHERE node_id=?1",
            params![node_id, generation],
        )?;
        Ok(())
    }

    pub fn delete_pg_member(&self, cluster: &str, node_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM postgresql_cluster_members WHERE cluster_name=?1 AND node_id=?2",
            params![cluster, node_id],
        )?;
        conn.execute(
            "DELETE FROM postgresql_member_certs WHERE cluster_name=?1 AND node_id=?2",
            params![cluster, node_id],
        )?;
        Ok(())
    }

    pub fn get_pg_secrets(&self, cluster: &str) -> Result<Option<PgSecretRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT cluster_name, superuser_password, replication_password, rewind_password,
                restapi_password, ca_cert_pem, ca_key_pem, role_passwords_json
             FROM postgresql_cluster_secrets WHERE cluster_name=?1",
            params![cluster],
            |r| {
                Ok(PgSecretRow {
                    cluster_name: r.get(0)?,
                    superuser_password: r.get(1)?,
                    replication_password: r.get(2)?,
                    rewind_password: r.get(3)?,
                    restapi_password: r.get(4)?,
                    ca_cert_pem: r.get(5)?,
                    ca_key_pem: r.get(6)?,
                    role_passwords_json: r.get(7)?,
                })
            },
        )
        .optional()
    }

    pub fn upsert_pg_secrets(&self, row: &PgSecretRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_cluster_secrets (cluster_name, superuser_password,
                replication_password, rewind_password, restapi_password, ca_cert_pem, ca_key_pem,
                role_passwords_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(cluster_name) DO UPDATE SET role_passwords_json=excluded.role_passwords_json",
            params![
                row.cluster_name,
                row.superuser_password,
                row.replication_password,
                row.rewind_password,
                row.restapi_password,
                row.ca_cert_pem,
                row.ca_key_pem,
                row.role_passwords_json
            ],
        )?;
        Ok(())
    }

    pub fn get_pg_cert(
        &self,
        cluster: &str,
        node_id: &str,
    ) -> Result<Option<PgCertRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT cluster_name, node_id, cert_pem, key_pem, not_after_unix
             FROM postgresql_member_certs WHERE cluster_name=?1 AND node_id=?2",
            params![cluster, node_id],
            |r| {
                Ok(PgCertRow {
                    cluster_name: r.get(0)?,
                    node_id: r.get(1)?,
                    cert_pem: r.get(2)?,
                    key_pem: r.get(3)?,
                    not_after_unix: r.get(4)?,
                })
            },
        )
        .optional()
    }

    pub fn upsert_pg_cert(&self, row: &PgCertRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_member_certs (cluster_name, node_id, cert_pem, key_pem, not_after_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(cluster_name, node_id) DO UPDATE SET
               cert_pem=excluded.cert_pem, key_pem=excluded.key_pem, not_after_unix=excluded.not_after_unix",
            params![row.cluster_name, row.node_id, row.cert_pem, row.key_pem, row.not_after_unix],
        )?;
        Ok(())
    }

    /// Store a new operation and mark it active on the cluster, unless another
    /// operation is still running.
    pub fn start_pg_operation(
        &self,
        op: &PgOperationRow,
    ) -> Result<Result<(), String>, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        let active: Option<String> = tx
            .query_row(
                "SELECT id FROM postgresql_operations WHERE cluster_name=?1 AND state='running' LIMIT 1",
                params![op.cluster_name],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(active) = active {
            return Ok(Err(active));
        }
        tx.execute(
            "INSERT INTO postgresql_operations (id, cluster_name, kind, state, target_generation,
                steps_json, context_json, message, force)
             VALUES (?1, ?2, ?3, 'running', ?4, ?5, ?6, ?7, ?8)",
            params![
                op.id,
                op.cluster_name,
                op.kind,
                op.target_generation,
                op.steps_json,
                op.context_json,
                op.message,
                op.force as i64
            ],
        )?;
        tx.execute(
            "UPDATE postgresql_clusters SET active_operation_id=?2, updated_at=datetime('now') WHERE name=?1",
            params![op.cluster_name, op.id],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    pub fn get_pg_operation(&self, id: &str) -> Result<Option<PgOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {OP_COLUMNS} FROM postgresql_operations WHERE id=?1"),
            params![id],
            op_row,
        )
        .optional()
    }

    pub fn list_pg_operations(
        &self,
        cluster: &str,
    ) -> Result<Vec<PgOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {OP_COLUMNS} FROM postgresql_operations WHERE cluster_name=?1 ORDER BY created_at DESC, id"
        ))?;
        let rows = stmt.query_map(params![cluster], op_row)?;
        rows.collect()
    }

    pub fn list_running_pg_operations(&self) -> Result<Vec<PgOperationRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {OP_COLUMNS} FROM postgresql_operations WHERE state='running' ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], op_row)?;
        rows.collect()
    }

    pub fn update_pg_operation(
        &self,
        id: &str,
        state: &str,
        steps_json: &str,
        message: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_operations SET state=?2, steps_json=?3, message=?4, updated_at=datetime('now')
             WHERE id=?1",
            params![id, state, steps_json, message],
        )?;
        Ok(())
    }

    pub fn set_pg_operation_context(
        &self,
        id: &str,
        context_json: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE postgresql_operations SET context_json=?2 WHERE id=?1",
            params![id, context_json],
        )?;
        Ok(())
    }

    pub fn append_pg_event(
        &self,
        cluster: &str,
        kind: &str,
        old_leader: &str,
        new_leader: &str,
        timeline: i64,
        message: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO postgresql_cluster_events (cluster_name, kind, old_leader, new_leader, timeline, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![cluster, kind, old_leader, new_leader, timeline, message],
        )?;
        Ok(())
    }

    pub fn list_pg_events(
        &self,
        cluster: &str,
        limit: i64,
    ) -> Result<Vec<PgEventRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, cluster_name, kind, old_leader, new_leader, timeline, message, created_at
             FROM postgresql_cluster_events WHERE cluster_name=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cluster, limit], |r| {
            Ok(PgEventRow {
                id: r.get(0)?,
                cluster_name: r.get(1)?,
                kind: r.get(2)?,
                old_leader: r.get(3)?,
                new_leader: r.get(4)?,
                timeline: r.get(5)?,
                message: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open(":memory:").unwrap()
    }

    fn cluster(name: &str) -> PgClusterRow {
        PgClusterRow {
            name: name.into(),
            generation: 1,
            phase: "pending".into(),
            spec_json: "{}".into(),
            ..Default::default()
        }
    }

    #[test]
    fn cluster_member_and_secret_round_trip() {
        let db = db();
        db.upsert_pg_cluster(&cluster("orders")).unwrap();
        db.upsert_pg_member(&PgMemberRow {
            cluster_name: "orders".into(),
            node_id: "node-105".into(),
            etcd_voter: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            db.pg_cluster_of_node("node-105").unwrap().as_deref(),
            Some("orders")
        );
        db.upsert_pg_secrets(&PgSecretRow {
            cluster_name: "orders".into(),
            superuser_password: "s".into(),
            role_passwords_json: "{}".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            db.get_pg_secrets("orders")
                .unwrap()
                .unwrap()
                .superuser_password,
            "s"
        );
        db.set_pg_cluster_live("orders", "node-105", 3, "17.9", false)
            .unwrap();
        let row = db.get_pg_cluster("orders").unwrap().unwrap();
        assert_eq!((row.leader_node_id.as_str(), row.timeline), ("node-105", 3));
        assert!(db.delete_pg_cluster("orders").unwrap());
        assert!(db.get_pg_secrets("orders").unwrap().is_none());
        assert!(db.pg_cluster_of_node("node-105").unwrap().is_none());
    }

    #[test]
    fn a_node_belongs_to_one_cluster() {
        let db = db();
        db.upsert_pg_cluster(&cluster("a")).unwrap();
        db.upsert_pg_cluster(&cluster("b")).unwrap();
        let member = |c: &str| PgMemberRow {
            cluster_name: c.into(),
            node_id: "node-1".into(),
            ..Default::default()
        };
        db.upsert_pg_member(&member("a")).unwrap();
        assert!(db.upsert_pg_member(&member("b")).is_err());
    }

    #[test]
    fn one_running_operation_per_cluster() {
        let db = db();
        db.upsert_pg_cluster(&cluster("orders")).unwrap();
        let op = |id: &str| PgOperationRow {
            id: id.into(),
            cluster_name: "orders".into(),
            kind: "update".into(),
            steps_json: "[]".into(),
            context_json: "{}".into(),
            ..Default::default()
        };
        assert!(db.start_pg_operation(&op("op1")).unwrap().is_ok());
        assert_eq!(
            db.start_pg_operation(&op("op2")).unwrap(),
            Err("op1".into())
        );
        db.update_pg_operation("op1", "succeeded", "[]", "")
            .unwrap();
        assert!(db.start_pg_operation(&op("op2")).unwrap().is_ok());
        assert_eq!(db.list_running_pg_operations().unwrap().len(), 1);
        assert_eq!(
            db.get_pg_cluster("orders")
                .unwrap()
                .unwrap()
                .active_operation_id,
            "op2"
        );
    }

    #[test]
    fn events_are_listed_newest_first() {
        let db = db();
        db.append_pg_event("orders", "failover", "a", "b", 2, "")
            .unwrap();
        db.append_pg_event("orders", "failover", "b", "c", 3, "")
            .unwrap();
        let events = db.list_pg_events("orders", 10).unwrap();
        assert_eq!(events[0].new_leader, "c");
    }
}
