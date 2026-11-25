//! Prometheus text for the Kubernetes clusters, written beside the metrics
//! file_sd targets (`kubernetes.prom`) for a textfile collector or vmagent.
//! The controller has no HTTP metrics endpoint to hang a registry on.
//!
//! Everything comes from the database except etcd health, which the
//! orchestrator samples each tick and leaves in [`record_etcd`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::db::Database;
use crate::kubernetes_orchestrator::ops::NOTICE_DRIFT;
use crate::kubernetes_upgrade;

fn etcd_samples() -> &'static Mutex<HashMap<String, (usize, usize)>> {
    static SAMPLES: OnceLock<Mutex<HashMap<String, (usize, usize)>>> = OnceLock::new();
    SAMPLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Latest etcd sample for a cluster: members answering, members expected.
pub fn record_etcd(cluster: &str, healthy: usize, total: usize) {
    etcd_samples()
        .lock()
        .expect("kubernetes etcd samples")
        .insert(cluster.to_string(), (healthy, total));
}

fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn header(out: &mut String, name: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
}

pub fn collect_kubernetes_metrics(db: &Database) -> String {
    collect_at(db, OffsetDateTime::now_utc())
}

fn collect_at(db: &Database, now: OffsetDateTime) -> String {
    let clusters = db.list_kubernetes_clusters().unwrap_or_default();
    let samples = etcd_samples()
        .lock()
        .expect("kubernetes etcd samples")
        .clone();

    let mut phase = String::new();
    let mut cert = String::new();
    let mut snapshot = String::new();
    let mut healthy = String::new();
    let mut members = String::new();
    let mut upgrade_failed = String::new();
    let mut drift = String::new();

    for cluster in &clusters {
        let name = label(&cluster.name);
        let _ = writeln!(
            phase,
            "kcore_k8s_phase{{cluster=\"{name}\",phase=\"{}\"}} 1",
            label(&cluster.phase)
        );

        if let Ok(stored) = db.kubernetes_pki_leaf_not_after(&cluster.name) {
            if let Ok(not_after) = OffsetDateTime::parse(&stored, &Rfc3339) {
                let days = (not_after - now).as_seconds_f64() / 86_400.0;
                let _ = writeln!(
                    cert,
                    "kcore_k8s_cert_days_remaining{{cluster=\"{name}\"}} {days:.3}"
                );
            }
        }

        if let Ok(snapshots) = db.list_kubernetes_etcd_snapshots(&cluster.name) {
            if let Some(at) = snapshots
                .first()
                .and_then(|s| OffsetDateTime::parse(&s.created_at, &Rfc3339).ok())
            {
                let age = (now - at).whole_seconds().max(0);
                let _ = writeln!(
                    snapshot,
                    "kcore_k8s_last_snapshot_age_seconds{{cluster=\"{name}\"}} {age}"
                );
            }
        }

        let rows = db
            .list_kubernetes_cluster_nodes(&cluster.name)
            .unwrap_or_default();
        let total = rows
            .iter()
            .filter(|r| r.role == "control-plane" && !r.pending_join)
            .count();
        let up = samples
            .get(&cluster.name)
            .map(|(up, _)| *up)
            .unwrap_or(cluster.ready_control_planes.max(0) as usize);
        let _ = writeln!(
            healthy,
            "kcore_k8s_etcd_healthy_members{{cluster=\"{name}\"}} {up}"
        );
        let _ = writeln!(
            members,
            "kcore_k8s_etcd_members{{cluster=\"{name}\"}} {total}"
        );

        let node_failed = db
            .list_kubernetes_node_upgrades(&cluster.name)
            .unwrap_or_default()
            .iter()
            .any(|n| n.upgrade_state == kubernetes_upgrade::STATE_FAILED);
        let failed = node_failed
            || (cluster.phase == "degraded"
                && cluster
                    .status_message
                    .starts_with(kubernetes_upgrade::UPGRADE_FAILED));
        let _ = writeln!(
            upgrade_failed,
            "kcore_k8s_upgrade_failed{{cluster=\"{name}\"}} {}",
            u8::from(failed)
        );

        let pending = cluster.status_message.starts_with(NOTICE_DRIFT);
        let _ = writeln!(
            drift,
            "kcore_k8s_drift_pending{{cluster=\"{name}\"}} {}",
            u8::from(pending)
        );
    }

    let mut out = String::new();
    for (name, help, body) in [
        (
            "kcore_k8s_phase",
            "Cluster phase, 1 for the current phase.",
            &phase,
        ),
        (
            "kcore_k8s_cert_days_remaining",
            "Days until the earliest leaf certificate expires.",
            &cert,
        ),
        (
            "kcore_k8s_last_snapshot_age_seconds",
            "Seconds since the newest etcd snapshot.",
            &snapshot,
        ),
        (
            "kcore_k8s_etcd_healthy_members",
            "etcd members answering health checks.",
            &healthy,
        ),
        (
            "kcore_k8s_etcd_members",
            "etcd members the cluster expects.",
            &members,
        ),
        (
            "kcore_k8s_upgrade_failed",
            "1 when a rolling upgrade stopped on a failure.",
            &upgrade_failed,
        ),
        (
            "kcore_k8s_drift_pending",
            "1 when a node runs configuration that differs from what renders now.",
            &drift,
        ),
    ] {
        header(&mut out, name, help);
        out.push_str(body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{KubernetesClusterConfigRow, KubernetesClusterRow, KubernetesEtcdSnapshotRow};

    fn seed(db: &Database, phase: &str, message: &str) {
        db.upsert_kubernetes_cluster(&KubernetesClusterRow {
            name: "lab".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "10.0.0.1:6443".to_string(),
            cni: "cilium".to_string(),
            phase: phase.to_string(),
            status_message: message.to_string(),
            ready_control_planes: 2,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        db.upsert_kubernetes_cluster_config(&KubernetesClusterConfigRow {
            cluster_name: "lab".to_string(),
            ha_enabled: false,
            vip_address: String::new(),
            vip_interface: String::new(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: String::new(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: String::new(),
        })
        .unwrap();
    }

    #[test]
    fn exposes_phase_drift_and_etcd_for_a_cluster() {
        let db = Database::open(":memory:").unwrap();
        seed(
            &db,
            "ready",
            &format!("{NOTICE_DRIFT} on cp1; auto-reconcile is off"),
        );
        record_etcd("lab", 3, 3);
        let text = collect_kubernetes_metrics(&db);
        assert!(text.contains("kcore_k8s_phase{cluster=\"lab\",phase=\"ready\"} 1"));
        assert!(text.contains("kcore_k8s_drift_pending{cluster=\"lab\"} 1"));
        assert!(text.contains("kcore_k8s_upgrade_failed{cluster=\"lab\"} 0"));
        assert!(text.contains("kcore_k8s_etcd_healthy_members{cluster=\"lab\"} 3"));
        assert!(text.contains("# TYPE kcore_k8s_phase gauge"));
    }

    #[test]
    fn cert_days_and_snapshot_age_come_from_stored_times() {
        let db = Database::open(":memory:").unwrap();
        seed(
            &db,
            "degraded",
            &format!("{} on cp1", kubernetes_upgrade::UPGRADE_FAILED),
        );
        let now = OffsetDateTime::parse("2026-06-01T00:00:00Z", &Rfc3339).unwrap();
        let infos = [crate::kubernetes_pki::NodeInfo {
            node_id: "cp1".into(),
            hostname: "cp1".into(),
            ip_address: "10.0.0.1".into(),
        }];
        let pki = crate::kubernetes_pki::generate_kubernetes_pki(
            "lab",
            "10.0.0.1:6443",
            "10.96.0.0/12",
            &infos,
        )
        .unwrap();
        db.store_kubernetes_pki("lab", &pki).unwrap();
        db.set_kubernetes_pki_leaf_not_after("lab", "2026-06-11T12:00:00Z")
            .unwrap();
        db.insert_kubernetes_etcd_snapshot(&KubernetesEtcdSnapshotRow {
            id: "s1".into(),
            cluster_name: "lab".into(),
            created_at: "2026-05-31T23:00:00Z".into(),
            sha256: "x".into(),
            size_bytes: 1,
            path: "/tmp/s1".into(),
            trigger: "manual".into(),
        })
        .unwrap();
        let text = collect_at(&db, now);
        assert!(
            text.contains("kcore_k8s_cert_days_remaining{cluster=\"lab\"} 10.500"),
            "{text}"
        );
        assert!(text.contains("kcore_k8s_last_snapshot_age_seconds{cluster=\"lab\"} 3600"));
        assert!(text.contains("kcore_k8s_upgrade_failed{cluster=\"lab\"} 1"));
        assert!(text.contains("kcore_k8s_phase{cluster=\"lab\",phase=\"degraded\"} 1"));
    }
}
