//! Write Prometheus `file_sd` targets for VictoriaMetrics from the node inventory.

use crate::config::MetricsConfig;
use crate::db::Database;
use crate::grpc::helpers::grpc_address_host;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

#[derive(Debug, Serialize, PartialEq, Eq)]
struct FileSdTarget {
    targets: Vec<String>,
    labels: BTreeMap<String, String>,
}

/// Build the JSON body VictoriaMetrics scrapes via `file_sd_configs`.
pub fn render_node_targets(db: &Database, scrape_port: u16) -> Result<String, String> {
    let nodes = db
        .list_nodes()
        .map_err(|e| format!("listing nodes for metrics targets: {e}"))?;

    let mut entries: Vec<FileSdTarget> = Vec::new();
    for node in nodes {
        let host = grpc_address_host(&node.address);
        if host.is_empty() || host == "0.0.0.0" || host == "::" {
            continue;
        }
        let target = if host.contains(':') && !host.starts_with('[') {
            // Bare IPv6 host — bracket it for the scrape URL host:port form.
            format!("[{host}]:{scrape_port}")
        } else {
            format!("{host}:{scrape_port}")
        };
        let mut labels = BTreeMap::new();
        labels.insert("node_id".to_string(), node.id.clone());
        if !node.hostname.is_empty() {
            labels.insert("hostname".to_string(), node.hostname.clone());
        }
        if !node.dc_id.is_empty() {
            labels.insert("dc_id".to_string(), node.dc_id.clone());
        }
        entries.push(FileSdTarget {
            targets: vec![target],
            labels,
        });
    }

    serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("serializing metrics file_sd targets: {e}"))
}

/// Atomically rewrite `cfg.file_sd_path` from the current node inventory.
pub fn write_node_targets(db: &Database, cfg: &MetricsConfig) -> Result<(), String> {
    let body = render_node_targets(db, cfg.scrape_port)?;
    let path = Path::new(&cfg.file_sd_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating metrics file_sd dir {}: {e}", parent.display()))?;
    }

    let tmp = temp_path_beside(path);
    std::fs::write(&tmp, format!("{body}\n"))
        .map_err(|e| format!("writing metrics file_sd temp {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("replacing metrics file_sd {}: {e}", path.display()))?;
    Ok(())
}

fn temp_path_beside(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("nodes.json");
    tmp.set_file_name(format!(".{name}.tmp"));
    tmp
}

/// Periodically rewrite scrape targets so new/removed nodes show up without a rebuild.
pub fn spawn_file_sd_writer(db: Database, cfg: MetricsConfig) {
    if !cfg.enabled {
        info!("metrics file_sd writer disabled (metrics.enabled: false)");
        return;
    }

    info!(
        path = %cfg.file_sd_path,
        scrape_port = cfg.scrape_port,
        refresh_secs = cfg.refresh_interval_secs,
        "metrics file_sd writer enabled"
    );

    if let Err(e) = write_node_targets(&db, &cfg) {
        warn!(error = %e, "initial metrics file_sd write failed");
    }

    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(cfg.refresh_interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            match write_node_targets(&db, &cfg) {
                Ok(()) => debug!(path = %cfg.file_sd_path, "refreshed metrics file_sd targets"),
                Err(e) => warn!(error = %e, "metrics file_sd write failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Database, NodeRow};

    fn sample_node(id: &str, address: &str, hostname: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: hostname.into(),
            address: address.into(),
            cpu_cores: 4,
            memory_bytes: 8 << 30,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: "br0".into(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: 30,
            luks_method: "tpm2".into(),
            dc_id: "DC1".into(),
        }
    }

    #[test]
    fn render_includes_host_port_and_labels() {
        let db = Database::open(":memory:").expect("db");
        db.upsert_node(&sample_node("n1", "192.168.40.10:9091", "kvm-a"))
            .expect("upsert");
        db.upsert_node(&sample_node("n2", "192.168.40.11:9091", "kvm-b"))
            .expect("upsert");

        let json = render_node_targets(&db, 9100).expect("render");
        assert!(json.contains("192.168.40.10:9100"), "{json}");
        assert!(json.contains("192.168.40.11:9100"), "{json}");
        assert!(json.contains("\"node_id\": \"n1\""), "{json}");
        assert!(json.contains("\"hostname\": \"kvm-a\""), "{json}");
        assert!(json.contains("\"dc_id\": \"DC1\""), "{json}");
    }

    #[test]
    fn render_skips_wildcard_binds() {
        let db = Database::open(":memory:").expect("db");
        db.upsert_node(&sample_node("bad", "0.0.0.0:9091", "nope"))
            .expect("upsert");
        let json = render_node_targets(&db, 9100).expect("render");
        assert_eq!(json.trim(), "[]");
    }

    #[test]
    fn write_node_targets_atomic_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("nodes.json");
        let db = Database::open(":memory:").expect("db");
        db.upsert_node(&sample_node("n1", "10.0.0.5:9091", "node-1"))
            .expect("upsert");

        let cfg = MetricsConfig {
            enabled: true,
            file_sd_path: path.to_string_lossy().into(),
            scrape_port: 9100,
            refresh_interval_secs: 30,
        };
        write_node_targets(&db, &cfg).expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("10.0.0.5:9100"), "{body}");
        assert!(!dir.path().join(".nodes.json.tmp").exists());
    }
}
