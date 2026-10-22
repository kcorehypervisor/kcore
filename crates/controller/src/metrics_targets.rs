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

/// `https://<advertise>:<https port>` for each ClickHouse server member.
///
/// Keeper-only members are omitted. A cluster with TLS disabled is omitted:
/// there is no plaintext public scrape. The metrics password stays in
/// `GetClickhouseClusterCredentials` and is not written here.
pub fn render_clickhouse_targets(db: &Database) -> Result<String, String> {
    let clusters = db
        .list_clickhouse_clusters()
        .map_err(|e| format!("listing clickhouse clusters for metrics targets: {e}"))?;
    let mut entries = Vec::new();
    for cluster in clusters {
        let spec = crate::clickhouse_convert::spec_from_json(&cluster.spec_json)?;
        if spec.tls.disabled {
            continue;
        }
        let port = if spec.ports.https > 0 {
            spec.ports.https
        } else {
            8443
        };
        let members = db
            .list_clickhouse_members(&cluster.name)
            .map_err(|e| format!("listing clickhouse members for metrics targets: {e}"))?;
        for member in members {
            if !member.server || member.advertise_address.is_empty() {
                continue;
            }
            let mut labels = BTreeMap::new();
            labels.insert("cluster".to_string(), cluster.name.clone());
            labels.insert("shard".to_string(), member.shard_name.clone());
            labels.insert("replica".to_string(), member.replica_name.clone());
            labels.insert("node".to_string(), member.node_id.clone());
            entries.push(FileSdTarget {
                targets: vec![https_target(&member.advertise_address, port)],
                labels,
            });
        }
    }
    serde_json::to_string_pretty(&entries)
        .map_err(|e| format!("serializing clickhouse file_sd targets: {e}"))
}

fn https_target(address: &str, port: i32) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("https://[{address}]:{port}")
    } else {
        format!("https://{address}:{port}")
    }
}

/// Atomically rewrite `cfg.file_sd_path` from the current node inventory.
pub fn write_node_targets(db: &Database, cfg: &MetricsConfig) -> Result<(), String> {
    let body = render_node_targets(db, cfg.scrape_port)?;
    write_file_sd(Path::new(&cfg.file_sd_path), &body)
}

/// Atomically rewrite the ClickHouse scrape file.
pub fn write_clickhouse_targets(db: &Database, cfg: &MetricsConfig) -> Result<(), String> {
    let body = render_clickhouse_targets(db)?;
    write_file_sd(Path::new(&cfg.clickhouse_file_sd_path), &body)
}

fn write_file_sd(path: &Path, body: &str) -> Result<(), String> {
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
    if let Err(e) = write_clickhouse_targets(&db, &cfg) {
        warn!(error = %e, "initial clickhouse file_sd write failed");
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
            match write_clickhouse_targets(&db, &cfg) {
                Ok(()) => {
                    debug!(path = %cfg.clickhouse_file_sd_path, "refreshed clickhouse file_sd targets");
                }
                Err(e) => warn!(error = %e, "clickhouse file_sd write failed"),
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
            ..MetricsConfig::default()
        };
        write_node_targets(&db, &cfg).expect("write");
        let body = std::fs::read_to_string(&path).expect("read");
        assert!(body.contains("10.0.0.5:9100"), "{body}");
        assert!(!dir.path().join(".nodes.json.tmp").exists());
    }

    #[test]
    fn clickhouse_targets_list_server_https_and_omit_keepers_and_secrets() {
        let db = Database::open(":memory:").expect("db");
        for id in ["node-105", "node-106", "node-108", "node-109", "node-200"] {
            db.upsert_node(&sample_node(id, "127.0.0.1:9", id))
                .expect("node");
        }
        let yaml = r#"
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  shards:
    - replicas:
        - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
        - {nodeId: node-106, advertiseAddress: "2001:db8::106", iface: enp6s0}
  keepers:
    - {nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0}
    - {nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0}
    - {nodeId: node-109, advertiseAddress: 192.168.40.109, iface: enp6s0}
"#;
        let (_name, mut spec) = kcore_clickhouse::parse_manifest(yaml).expect("manifest");
        kcore_clickhouse::normalize(&mut spec);
        db.insert_clickhouse_cluster(&crate::clickhouse_db::ClusterRow {
            name: "analytics".into(),
            generation: 1,
            phase: "ready".into(),
            message: String::new(),
            spec_json: serde_json::to_string(&spec).unwrap(),
            observed_version: String::new(),
            active_operation_id: String::new(),
        })
        .unwrap();
        db.replace_clickhouse_members(
            "analytics",
            &crate::clickhouse_convert::members_from_spec("analytics", &spec),
        )
        .unwrap();
        db.upsert_clickhouse_secrets(
            "analytics",
            &crate::clickhouse_db::SecretRow {
                default_password: "default-secret".into(),
                default_sha256: "digest".into(),
                metrics_password: "metrics-secret-do-not-publish".into(),
                metrics_sha256: "digest".into(),
                interserver_password: "inter".into(),
                keeper_password: "keeper".into(),
                cluster_secret: "cluster".into(),
                ca_cert_pem: String::new(),
                ca_key_pem: String::new(),
            },
        )
        .unwrap();

        let open = r#"
kind: ClickhouseCluster
metadata:
  name: open
spec:
  tls:
    disabled: true
  shards:
    - replicas:
        - {nodeId: node-200, advertiseAddress: 192.168.50.200, iface: enp6s0}
  keepers:
    - {nodeId: node-200, advertiseAddress: 192.168.50.200, iface: enp6s0}
"#;
        let (_name, mut open_spec) = kcore_clickhouse::parse_manifest(open).expect("open");
        kcore_clickhouse::normalize(&mut open_spec);
        db.insert_clickhouse_cluster(&crate::clickhouse_db::ClusterRow {
            name: "open".into(),
            generation: 1,
            phase: "ready".into(),
            message: String::new(),
            spec_json: serde_json::to_string(&open_spec).unwrap(),
            observed_version: String::new(),
            active_operation_id: String::new(),
        })
        .unwrap();
        db.replace_clickhouse_members(
            "open",
            &crate::clickhouse_convert::members_from_spec("open", &open_spec),
        )
        .unwrap();

        let json = render_clickhouse_targets(&db).expect("render");
        assert!(json.contains("https://192.168.40.105:8443"), "{json}");
        assert!(json.contains("https://[2001:db8::106]:8443"), "{json}");
        assert!(json.contains("\"cluster\": \"analytics\""), "{json}");
        assert!(json.contains("\"node\": \"node-105\""), "{json}");
        assert!(json.contains("\"node\": \"node-106\""), "{json}");
        assert!(!json.contains("192.168.40.108"), "{json}");
        assert!(!json.contains("192.168.40.109"), "{json}");
        assert!(!json.contains("192.168.50.200"), "{json}");
        assert!(!json.contains("metrics-secret-do-not-publish"), "{json}");
        assert!(!json.contains("9363"), "{json}");
        assert!(!json.contains("\"open\""), "{json}");
    }
}
