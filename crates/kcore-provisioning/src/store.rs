//! SQLite catalog. The Nix seed fills an empty database and does not erase leases.

use crate::model::{Catalog, DnsRecord, Network};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Seed {
    pub upstream: Vec<String>,
    pub authoritative: String,
    #[serde(default)]
    pub networks: Vec<Network>,
    #[serde(default)]
    pub records: Vec<DnsRecord>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS networks (
                name TEXT PRIMARY KEY,
                body TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS reservations (
                network TEXT NOT NULL,
                mac TEXT NOT NULL,
                body TEXT NOT NULL,
                PRIMARY KEY (network, mac)
            );
            CREATE TABLE IF NOT EXISTS records (
                zone TEXT NOT NULL,
                name TEXT NOT NULL,
                rrtype TEXT NOT NULL,
                content TEXT NOT NULL,
                ttl INTEGER NOT NULL,
                PRIMARY KEY (name, rrtype, content)
            );
            ",
        )?;
        Ok(Self { conn })
    }

    pub fn load(&self) -> Result<Catalog> {
        let upstream = self
            .meta("upstream")?
            .map(|v| serde_json::from_str(&v))
            .transpose()?
            .unwrap_or_else(|| vec!["1.1.1.1".into(), "8.8.8.8".into()]);
        let authoritative = self
            .meta("authoritative")?
            .unwrap_or_else(|| "127.0.0.1:5300".into());
        let mut networks = Vec::new();
        let mut stmt = self
            .conn
            .prepare("SELECT body FROM networks ORDER BY name")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            networks.push(serde_json::from_str(&row?)?);
        }
        let mut reservations = Vec::new();
        let mut stmt = self
            .conn
            .prepare("SELECT body FROM reservations ORDER BY network, mac")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            reservations.push(serde_json::from_str(&row?)?);
        }
        let mut records = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT zone, name, rrtype, content, ttl FROM records ORDER BY zone, name, rrtype",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(DnsRecord {
                zone: row.get(0)?,
                name: row.get(1)?,
                rrtype: row.get(2)?,
                content: row.get(3)?,
                ttl: row.get(4)?,
            })
        })?;
        for row in rows {
            records.push(row?);
        }
        Ok(Catalog {
            upstream,
            authoritative,
            networks,
            reservations,
            records,
        })
    }

    pub fn save(&mut self, catalog: &Catalog) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('upstream', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![serde_json::to_string(&catalog.upstream)?],
        )?;
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('authoritative', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![catalog.authoritative],
        )?;
        tx.execute("DELETE FROM networks", [])?;
        tx.execute("DELETE FROM reservations", [])?;
        tx.execute("DELETE FROM records", [])?;
        for network in &catalog.networks {
            tx.execute(
                "INSERT INTO networks(name, body) VALUES(?1, ?2)",
                params![network.name, serde_json::to_string(network)?],
            )?;
        }
        for reservation in &catalog.reservations {
            tx.execute(
                "INSERT INTO reservations(network, mac, body) VALUES(?1, ?2, ?3)",
                params![
                    reservation.network,
                    reservation.mac_address.to_ascii_lowercase(),
                    serde_json::to_string(reservation)?
                ],
            )?;
        }
        for record in &catalog.records {
            tx.execute(
                "INSERT INTO records(zone, name, rrtype, content, ttl) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    record.zone,
                    record.name,
                    record.rrtype,
                    record.content,
                    record.ttl
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert seed networks only when the database has none.
    /// Upstream forwarders always follow the seed so a rebuild can change them.
    pub fn apply_seed(&mut self, seed: &Seed) -> Result<bool> {
        let mut catalog = self.load()?;
        let inserted = catalog.networks.is_empty();
        catalog.upstream = seed.upstream.clone();
        catalog.authoritative = seed.authoritative.clone();
        if inserted {
            catalog.networks = seed.networks.clone();
        }
        for record in &seed.records {
            if !catalog.records.iter().any(|have| {
                have.name == record.name
                    && have.rrtype == record.rrtype
                    && have.content == record.content
            }) {
                catalog.records.push(record.clone());
            }
        }
        catalog.validate().map_err(|e| anyhow::anyhow!(e))?;
        self.save(&catalog)?;
        Ok(inserted)
    }

    fn meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::sample_catalog;

    #[test]
    fn seed_does_not_clobber_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        let mut store = Store::open(&path).unwrap();
        let seed = Seed {
            upstream: vec!["1.1.1.1".into(), "8.8.8.8".into()],
            authoritative: "127.0.0.1:5300".into(),
            networks: sample_catalog().networks,
            records: vec![],
        };
        assert!(store.apply_seed(&seed).unwrap());
        let mut catalog = store.load().unwrap();
        catalog.reservations = sample_catalog().reservations;
        store.save(&catalog).unwrap();
        let seed = Seed {
            upstream: vec!["9.9.9.9".into()],
            authoritative: "127.0.0.1:5300".into(),
            networks: vec![],
            records: vec![],
        };
        assert!(!store.apply_seed(&seed).unwrap());
        let loaded = store.load().unwrap();
        assert_eq!(loaded.reservations.len(), 1);
        assert_eq!(loaded.upstream, vec!["9.9.9.9".to_string()]);
        assert_eq!(loaded.networks.len(), 2);
    }
}
