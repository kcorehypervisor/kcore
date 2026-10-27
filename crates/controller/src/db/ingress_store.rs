//! Ingress gateways, routes, WAF policies and WAF audit events.
//!
//! Specs are stored as the canonical JSON documents (see `ingress_spec`); only
//! the columns that are filtered on are lifted out. Status is a JSON blob the
//! reconciler rewrites each tick.

use rusqlite::{params, OptionalExtension, Row};

use super::Database;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressGatewayRow {
    pub name: String,
    pub generation: i64,
    pub spec_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngressGatewayStatusRow {
    pub name: String,
    pub observed_generation: i64,
    /// `pending`, `applying`, `ready`, `degraded` or `failed`.
    pub phase: String,
    pub message: String,
    /// JSON array of per-node status objects.
    pub nodes_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressRouteRow {
    pub name: String,
    pub gateway: String,
    pub generation: i64,
    pub spec_json: String,
    /// Empty for operator routes, `k8s:<cluster>/<ns>/<ingress>` for synthesised ones.
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IngressRouteStatusRow {
    pub name: String,
    pub phase: String,
    pub message: String,
    /// JSON object: certs, upstreams, last WAF block.
    pub status_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WafPolicyRow {
    pub name: String,
    pub generation: i64,
    pub spec_json: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WafEventRow {
    pub id: i64,
    pub gateway: String,
    pub route: String,
    pub node_id: String,
    pub rule_id: i32,
    pub severity: String,
    pub client_ip: String,
    pub host: String,
    pub uri: String,
    /// `detected` or `blocked`.
    pub action: String,
    pub tx_id: String,
    pub raw_json: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default)]
pub struct WafEventFilter {
    pub gateway: String,
    pub route: String,
    pub rule_id: i32,
    pub client_ip: String,
    pub since_unix: i64,
    pub limit: i64,
}

pub const INGRESS_TABLES: &str = "CREATE TABLE IF NOT EXISTS ingress_gateways (
        name TEXT PRIMARY KEY,
        generation INTEGER NOT NULL DEFAULT 1,
        spec_json TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS ingress_gateway_status (
        name TEXT PRIMARY KEY REFERENCES ingress_gateways(name) ON DELETE CASCADE,
        observed_generation INTEGER NOT NULL DEFAULT 0,
        phase TEXT NOT NULL DEFAULT 'pending',
        message TEXT NOT NULL DEFAULT '',
        nodes_json TEXT NOT NULL DEFAULT '[]'
    );
    CREATE TABLE IF NOT EXISTS ingress_routes (
        name TEXT PRIMARY KEY,
        gateway TEXT NOT NULL,
        generation INTEGER NOT NULL DEFAULT 1,
        spec_json TEXT NOT NULL,
        owner TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_ingress_routes_gateway ON ingress_routes(gateway);
    CREATE TABLE IF NOT EXISTS ingress_route_status (
        name TEXT PRIMARY KEY REFERENCES ingress_routes(name) ON DELETE CASCADE,
        phase TEXT NOT NULL DEFAULT 'pending',
        message TEXT NOT NULL DEFAULT '',
        status_json TEXT NOT NULL DEFAULT '{}'
    );
    CREATE TABLE IF NOT EXISTS waf_policies (
        name TEXT PRIMARY KEY,
        generation INTEGER NOT NULL DEFAULT 1,
        spec_json TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE IF NOT EXISTS waf_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        gateway TEXT NOT NULL DEFAULT '',
        route TEXT NOT NULL DEFAULT '',
        node_id TEXT NOT NULL DEFAULT '',
        rule_id INTEGER NOT NULL DEFAULT 0,
        severity TEXT NOT NULL DEFAULT '',
        client_ip TEXT NOT NULL DEFAULT '',
        host TEXT NOT NULL DEFAULT '',
        uri TEXT NOT NULL DEFAULT '',
        action TEXT NOT NULL DEFAULT '',
        tx_id TEXT NOT NULL DEFAULT '',
        raw_json TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_waf_events_route ON waf_events(route, created_at);
    CREATE INDEX IF NOT EXISTS idx_waf_events_rule ON waf_events(rule_id, created_at);";

pub const INGRESS_TABLES_DOWN: &str = "DROP TABLE IF EXISTS waf_events;
DROP TABLE IF EXISTS waf_policies;
DROP TABLE IF EXISTS ingress_route_status;
DROP TABLE IF EXISTS ingress_routes;
DROP TABLE IF EXISTS ingress_gateway_status;
DROP TABLE IF EXISTS ingress_gateways;";

fn gateway_row(r: &Row<'_>) -> rusqlite::Result<IngressGatewayRow> {
    Ok(IngressGatewayRow {
        name: r.get(0)?,
        generation: r.get(1)?,
        spec_json: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

fn route_row(r: &Row<'_>) -> rusqlite::Result<IngressRouteRow> {
    Ok(IngressRouteRow {
        name: r.get(0)?,
        gateway: r.get(1)?,
        generation: r.get(2)?,
        spec_json: r.get(3)?,
        owner: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

fn policy_row(r: &Row<'_>) -> rusqlite::Result<WafPolicyRow> {
    Ok(WafPolicyRow {
        name: r.get(0)?,
        generation: r.get(1)?,
        spec_json: r.get(2)?,
        created_at: r.get(3)?,
        updated_at: r.get(4)?,
    })
}

const GATEWAY_COLS: &str = "name, generation, spec_json, created_at, updated_at";
const ROUTE_COLS: &str = "name, gateway, generation, spec_json, owner, created_at, updated_at";

impl Database {
    // ------------------------------------------------------------- gateways

    pub fn upsert_ingress_gateway(&self, row: &IngressGatewayRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ingress_gateways (name, generation, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, COALESCE(NULLIF(?4, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               generation = excluded.generation,
               spec_json = excluded.spec_json,
               updated_at = datetime('now')",
            params![row.name, row.generation, row.spec_json, row.created_at],
        )?;
        Ok(())
    }

    pub fn get_ingress_gateway(
        &self,
        name: &str,
    ) -> Result<Option<IngressGatewayRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {GATEWAY_COLS} FROM ingress_gateways WHERE name = ?1"),
            params![name],
            gateway_row,
        )
        .optional()
    }

    pub fn list_ingress_gateways(&self) -> Result<Vec<IngressGatewayRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {GATEWAY_COLS} FROM ingress_gateways ORDER BY name"
        ))?;
        let rows = stmt.query_map([], gateway_row)?;
        rows.collect()
    }

    pub fn delete_ingress_gateway(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute(
            "DELETE FROM ingress_gateways WHERE name = ?1",
            params![name],
        )? > 0)
    }

    pub fn upsert_ingress_gateway_status(
        &self,
        row: &IngressGatewayStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ingress_gateway_status (name, observed_generation, phase, message, nodes_json)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
               observed_generation = excluded.observed_generation,
               phase = excluded.phase,
               message = excluded.message,
               nodes_json = excluded.nodes_json",
            params![
                row.name,
                row.observed_generation,
                row.phase,
                row.message,
                row.nodes_json
            ],
        )?;
        Ok(())
    }

    pub fn get_ingress_gateway_status(
        &self,
        name: &str,
    ) -> Result<Option<IngressGatewayStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT name, observed_generation, phase, message, nodes_json
             FROM ingress_gateway_status WHERE name = ?1",
            params![name],
            |r| {
                Ok(IngressGatewayStatusRow {
                    name: r.get(0)?,
                    observed_generation: r.get(1)?,
                    phase: r.get(2)?,
                    message: r.get(3)?,
                    nodes_json: r.get(4)?,
                })
            },
        )
        .optional()
    }

    // --------------------------------------------------------------- routes

    pub fn upsert_ingress_route(&self, row: &IngressRouteRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ingress_routes (name, gateway, generation, spec_json, owner, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(NULLIF(?6, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               gateway = excluded.gateway,
               generation = excluded.generation,
               spec_json = excluded.spec_json,
               owner = excluded.owner,
               updated_at = datetime('now')",
            params![
                row.name,
                row.gateway,
                row.generation,
                row.spec_json,
                row.owner,
                row.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_ingress_route(
        &self,
        name: &str,
    ) -> Result<Option<IngressRouteRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {ROUTE_COLS} FROM ingress_routes WHERE name = ?1"),
            params![name],
            route_row,
        )
        .optional()
    }

    /// All routes, or only those of `gateway` when it is non-empty.
    pub fn list_ingress_routes(
        &self,
        gateway: &str,
    ) -> Result<Vec<IngressRouteRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ROUTE_COLS} FROM ingress_routes
             WHERE ?1 = '' OR gateway = ?1 ORDER BY name"
        ))?;
        let rows = stmt.query_map(params![gateway], route_row)?;
        rows.collect()
    }

    pub fn delete_ingress_route(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM ingress_routes WHERE name = ?1", params![name])? > 0)
    }

    pub fn upsert_ingress_route_status(
        &self,
        row: &IngressRouteStatusRow,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO ingress_route_status (name, phase, message, status_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO UPDATE SET
               phase = excluded.phase,
               message = excluded.message,
               status_json = excluded.status_json",
            params![row.name, row.phase, row.message, row.status_json],
        )?;
        Ok(())
    }

    pub fn get_ingress_route_status(
        &self,
        name: &str,
    ) -> Result<Option<IngressRouteStatusRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT name, phase, message, status_json FROM ingress_route_status WHERE name = ?1",
            params![name],
            |r| {
                Ok(IngressRouteStatusRow {
                    name: r.get(0)?,
                    phase: r.get(1)?,
                    message: r.get(2)?,
                    status_json: r.get(3)?,
                })
            },
        )
        .optional()
    }

    // ---------------------------------------------------------- WAF policies

    pub fn upsert_waf_policy(&self, row: &WafPolicyRow) -> Result<(), rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO waf_policies (name, generation, spec_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, COALESCE(NULLIF(?4, ''), datetime('now')), datetime('now'))
             ON CONFLICT(name) DO UPDATE SET
               generation = excluded.generation,
               spec_json = excluded.spec_json,
               updated_at = datetime('now')",
            params![row.name, row.generation, row.spec_json, row.created_at],
        )?;
        Ok(())
    }

    pub fn get_waf_policy(&self, name: &str) -> Result<Option<WafPolicyRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.query_row(
            &format!("SELECT {GATEWAY_COLS} FROM waf_policies WHERE name = ?1"),
            params![name],
            policy_row,
        )
        .optional()
    }

    pub fn list_waf_policies(&self) -> Result<Vec<WafPolicyRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {GATEWAY_COLS} FROM waf_policies ORDER BY name"
        ))?;
        let rows = stmt.query_map([], policy_row)?;
        rows.collect()
    }

    pub fn delete_waf_policy(&self, name: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.lock_conn()?;
        Ok(conn.execute("DELETE FROM waf_policies WHERE name = ?1", params![name])? > 0)
    }

    // ------------------------------------------------------------ WAF events

    pub fn insert_waf_events(&self, events: &[WafEventRow]) -> Result<usize, rusqlite::Error> {
        let mut conn = self.lock_conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO waf_events
                 (gateway, route, node_id, rule_id, severity, client_ip, host, uri, action, tx_id, raw_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for e in events {
                stmt.execute(params![
                    e.gateway,
                    e.route,
                    e.node_id,
                    e.rule_id,
                    e.severity,
                    e.client_ip,
                    e.host,
                    e.uri,
                    e.action,
                    e.tx_id,
                    e.raw_json,
                    e.created_at
                ])?;
            }
        }
        tx.commit()?;
        Ok(events.len())
    }

    pub fn list_waf_events(&self, f: &WafEventFilter) -> Result<Vec<WafEventRow>, rusqlite::Error> {
        let conn = self.lock_conn()?;
        let limit = if f.limit <= 0 { 200 } else { f.limit.min(5000) };
        let mut stmt = conn.prepare(
            "SELECT id, gateway, route, node_id, rule_id, severity, client_ip, host, uri, action, tx_id, raw_json, created_at
             FROM waf_events
             WHERE (?1 = '' OR gateway = ?1)
               AND (?2 = '' OR route = ?2)
               AND (?3 = 0 OR rule_id = ?3)
               AND (?4 = '' OR client_ip = ?4)
               AND created_at >= ?5
             ORDER BY created_at DESC, id DESC
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![
                f.gateway,
                f.route,
                f.rule_id,
                f.client_ip,
                f.since_unix,
                limit
            ],
            |r| {
                Ok(WafEventRow {
                    id: r.get(0)?,
                    gateway: r.get(1)?,
                    route: r.get(2)?,
                    node_id: r.get(3)?,
                    rule_id: r.get(4)?,
                    severity: r.get(5)?,
                    client_ip: r.get(6)?,
                    host: r.get(7)?,
                    uri: r.get(8)?,
                    action: r.get(9)?,
                    tx_id: r.get(10)?,
                    raw_json: r.get(11)?,
                    created_at: r.get(12)?,
                })
            },
        )?;
        rows.collect()
    }

    /// Deletes events older than `before_unix`; returns how many went.
    pub fn prune_waf_events(&self, before_unix: i64) -> Result<usize, rusqlite::Error> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM waf_events WHERE created_at < ?1",
            params![before_unix],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Database {
        Database::open(":memory:").expect("in-memory db")
    }

    #[test]
    fn gateway_route_policy_round_trip() {
        let db = db();
        let gw = IngressGatewayRow {
            name: "edge".into(),
            generation: 1,
            spec_json: "{}".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_ingress_gateway(&gw).unwrap();
        assert_eq!(
            db.get_ingress_gateway("edge").unwrap().unwrap().generation,
            1
        );

        let route = IngressRouteRow {
            name: "shop".into(),
            gateway: "edge".into(),
            generation: 1,
            spec_json: "{}".into(),
            owner: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_ingress_route(&route).unwrap();
        assert_eq!(db.list_ingress_routes("edge").unwrap().len(), 1);
        assert!(db.list_ingress_routes("other").unwrap().is_empty());
        assert_eq!(db.list_ingress_routes("").unwrap().len(), 1);

        db.upsert_ingress_route_status(&IngressRouteStatusRow {
            name: "shop".into(),
            phase: "ready".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            db.get_ingress_route_status("shop").unwrap().unwrap().phase,
            "ready"
        );

        // Deleting the route cascades to its status.
        assert!(db.delete_ingress_route("shop").unwrap());
        assert!(db.get_ingress_route_status("shop").unwrap().is_none());

        let p = WafPolicyRow {
            name: "crs".into(),
            generation: 1,
            spec_json: "{}".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_waf_policy(&p).unwrap();
        assert_eq!(db.list_waf_policies().unwrap().len(), 1);
        assert!(db.delete_waf_policy("crs").unwrap());
    }

    #[test]
    fn waf_events_filter_and_prune() {
        let db = db();
        let ev = |route: &str, rule: i32, at: i64| WafEventRow {
            gateway: "edge".into(),
            route: route.into(),
            rule_id: rule,
            client_ip: "198.51.100.7".into(),
            action: "blocked".into(),
            created_at: at,
            ..Default::default()
        };
        db.insert_waf_events(&[
            ev("shop", 942100, 100),
            ev("shop", 941100, 200),
            ev("blog", 942100, 300),
        ])
        .unwrap();
        let all = db.list_waf_events(&WafEventFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].route, "blog", "newest first");
        let shop = db
            .list_waf_events(&WafEventFilter {
                route: "shop".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(shop.len(), 2);
        let rule = db
            .list_waf_events(&WafEventFilter {
                rule_id: 942100,
                since_unix: 150,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rule.len(), 1);
        assert_eq!(db.prune_waf_events(250).unwrap(), 2);
        assert_eq!(
            db.list_waf_events(&WafEventFilter::default())
                .unwrap()
                .len(),
            1
        );
    }
}
