//! Ceph gates for a ClusterUpdate. A node that is a CephCluster member only
//! starts once its OSDs are `ok-to-stop`, and `noout` is held from then until
//! the node has finished, so a reboot does not start a rebalance.
//!
//! `noout` is cluster-wide. The reconciler holds it per (update, cluster)
//! while any member of that cluster is mid-update, and clears it on the first
//! tick after the last one settles.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use tonic::{Code, Request};
use tracing::warn;

use crate::ceph_cluster_spec;
use crate::db::{ClusterUpdateNodeRow, Database};
use crate::node_client::NodeClients;
use crate::node_proto;

const CEPH_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// (update name, cluster name) pairs this controller has set `noout` for.
static HELD: LazyLock<Mutex<HashSet<(String, String)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Phases between the start of prepare and the node being done. `pending`
/// is not one of them: the gate runs while the node is still pending.
pub(crate) fn in_flight(phase: &str) -> bool {
    matches!(
        phase,
        "prepared"
            | "drained"
            | "waiting_ready"
            | "confirming"
            | "reboot_required"
            | "rebooting"
            | "rolling_back"
    )
}

/// The CephCluster `node_id` is a member of, with every member id.
fn member_cluster(db: &Database, node_id: &str) -> Option<(String, Vec<String>)> {
    for row in db.list_ceph_clusters().ok()? {
        let Ok(spec) = ceph_cluster_spec::spec_from_json(&row.spec_json) else {
            continue;
        };
        if spec.nodes.iter().any(|n| n.node_id == node_id) {
            let members = spec.nodes.into_iter().map(|n| n.node_id).collect();
            return Some((row.name, members));
        }
    }
    None
}

/// Members in the order to try them: those not being updated first.
pub(crate) fn survivor_order(members: &[String], busy: &[String]) -> Vec<String> {
    let mut order: Vec<String> = members
        .iter()
        .filter(|m| !busy.contains(m))
        .cloned()
        .collect();
    order.extend(members.iter().filter(|m| busy.contains(m)).cloned());
    order
}

/// Rollout order: mon/full members before osd-only, preserving relative order.
pub fn mon_first_rollout_order(
    spec: &crate::controller_proto::CephClusterSpec,
    node_ids: &[String],
) -> Vec<String> {
    let role_of = |id: &str| -> String {
        spec.nodes
            .iter()
            .find(|n| n.node_id == id)
            .map(|n| ceph_cluster_spec::normalize_role(&n.role))
            .unwrap_or_else(|| "full".into())
    };
    let mut mon_like = Vec::new();
    let mut osd_only = Vec::new();
    for id in node_ids {
        if role_of(id) == "osd" {
            osd_only.push(id.clone());
        } else {
            mon_like.push(id.clone());
        }
    }
    mon_like.extend(osd_only);
    mon_like
}

/// Wait until a survivor reports every PG active+clean (or timeout).
pub async fn wait_active_clean(
    db: &Database,
    clients: &NodeClients,
    survivors: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = "no Ceph member reachable for active+clean".to_string();
    while tokio::time::Instant::now() < deadline {
        for member in survivors {
            let mut admin =
                match crate::ceph_cluster_reconciler::admin_for(db, clients, member).await {
                    Ok(a) => a,
                    Err(e) => {
                        last = format!("{member}: {e}");
                        continue;
                    }
                };
            let mut request = Request::new(node_proto::GetCephHealthRequest {
                cluster_name: String::new(),
            });
            request.set_timeout(CEPH_RPC_TIMEOUT);
            match admin.get_ceph_health(request).await {
                Ok(resp) => {
                    let h = resp.into_inner();
                    if h.pgs_not_clean == 0 {
                        return Ok(());
                    }
                    last = format!(
                        "{member}: {} PGs not active+clean (health {})",
                        h.pgs_not_clean, h.health_status
                    );
                }
                Err(e) => last = format!("{member}: {}", e.message()),
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Err(format!("timed out waiting for active+clean: {last}"))
}

async fn set_noout(
    db: &Database,
    clients: &NodeClients,
    order: &[String],
    set: bool,
) -> Result<(), String> {
    let (set_flags, unset_flags) = if set {
        (vec!["noout".to_string()], Vec::new())
    } else {
        (Vec::new(), vec!["noout".to_string()])
    };
    let verb = if set { "set" } else { "unset" };
    let mut last = format!("no Ceph member reachable to {verb} noout");
    for member in order {
        let mut admin = match crate::ceph_cluster_reconciler::admin_for(db, clients, member).await {
            Ok(admin) => admin,
            Err(e) => {
                last = format!("{member}: {e}");
                continue;
            }
        };
        let mut request = Request::new(node_proto::SetCephFlagsRequest {
            set_flags: set_flags.clone(),
            unset_flags: unset_flags.clone(),
        });
        request.set_timeout(CEPH_RPC_TIMEOUT);
        match admin.set_ceph_flags(request).await {
            Ok(resp) if resp.get_ref().success => return Ok(()),
            Ok(resp) => last = format!("{member}: {}", resp.get_ref().message),
            Err(e) => last = format!("{member}: {}", e.message()),
        }
    }
    Err(format!("ceph osd {verb} noout: {last}"))
}

/// `Ok(())` when the host's OSDs may stop, or when no member can say (an
/// agent without the RPC must not wedge every update).
async fn check_ok_to_stop(
    db: &Database,
    clients: &NodeClients,
    order: &[String],
    host: &str,
) -> Result<(), String> {
    let mut last = "no Ceph member reachable to check ok-to-stop".to_string();
    for member in order {
        let mut admin = match crate::ceph_cluster_reconciler::admin_for(db, clients, member).await {
            Ok(admin) => admin,
            Err(e) => {
                last = format!("{member}: {e}");
                continue;
            }
        };
        let mut request = Request::new(node_proto::CheckCephOkToStopRequest {
            host: host.to_string(),
        });
        request.set_timeout(CEPH_RPC_TIMEOUT);
        match admin.check_ceph_ok_to_stop(request).await {
            Ok(resp) => {
                let resp = resp.into_inner();
                return if resp.ok {
                    Ok(())
                } else {
                    Err(format!("ceph osd ok-to-stop: {}", resp.message))
                };
            }
            Err(e) if e.code() == Code::Unimplemented => {
                warn!(%member, "node agent has no CheckCephOkToStop; skipping the check");
                return Ok(());
            }
            Err(e) => last = format!("{member}: {}", e.message()),
        }
    }
    Err(last)
}

/// Run before PrepareSystemUpdate. Not a Ceph member: nothing to do. A
/// member waits (the caller records the error and retries next tick) until
/// its OSDs are ok-to-stop, then `noout` is set.
pub(crate) async fn before_prepare(
    db: &Database,
    clients: &NodeClients,
    update_name: &str,
    node_id: &str,
) -> Result<(), String> {
    let Some((cluster, members)) = member_cluster(db, node_id) else {
        return Ok(());
    };
    let order = survivor_order(&members, &[node_id.to_string()]);
    let host = db
        .get_node(node_id)
        .ok()
        .flatten()
        .map(|n| n.hostname)
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| node_id.to_string());
    check_ok_to_stop(db, clients, &order, &host).await?;
    // Do not reboot the next member while PGs are still recovering.
    wait_active_clean(db, clients, &order, Duration::from_secs(120)).await?;
    let key = (update_name.to_string(), cluster);
    let held = HELD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&key);
    if !held {
        set_noout(db, clients, &order, true).await?;
        HELD.lock().unwrap_or_else(|e| e.into_inner()).insert(key);
    }
    Ok(())
}

/// Keep `noout` in step with the nodes: set while a member is mid-update
/// (this also re-asserts it after a controller restart), cleared once none
/// is. A failed call is retried on the next tick.
pub(crate) async fn sweep(
    db: &Database,
    clients: &NodeClients,
    update_name: &str,
    nodes: &[ClusterUpdateNodeRow],
) {
    let mut clusters: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
    for row in nodes {
        let Some((cluster, members)) = member_cluster(db, &row.node_id) else {
            continue;
        };
        let entry = clusters.entry(cluster).or_insert((members, Vec::new()));
        if in_flight(&row.phase) {
            entry.1.push(row.node_id.clone());
        }
    }
    for (cluster, (members, busy)) in clusters {
        let key = (update_name.to_string(), cluster.clone());
        let held = HELD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key);
        let order = survivor_order(&members, &busy);
        if !busy.is_empty() && !held {
            match set_noout(db, clients, &order, true).await {
                Ok(()) => {
                    HELD.lock().unwrap_or_else(|e| e.into_inner()).insert(key);
                }
                Err(e) => warn!(%cluster, error = %e, "cluster update: could not hold noout"),
            }
        } else if busy.is_empty() && held {
            match set_noout(db, clients, &order, false).await {
                Ok(()) => {
                    HELD.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                }
                Err(e) => warn!(%cluster, error = %e, "cluster update: could not clear noout"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_phases_between_prepare_and_done_hold_noout() {
        for phase in [
            "prepared",
            "drained",
            "waiting_ready",
            "confirming",
            "reboot_required",
            "rebooting",
            "rolling_back",
        ] {
            assert!(in_flight(phase), "{phase}");
        }
        for phase in [
            "pending",
            "bootstrapping",
            "soaking",
            "awaiting_promotion",
            "succeeded",
            "failed",
            "cancelled",
        ] {
            assert!(!in_flight(phase), "{phase}");
        }
    }

    #[test]
    fn the_node_being_updated_is_asked_last() {
        let members: Vec<String> = ["n1", "n2", "n3"].map(String::from).to_vec();
        assert_eq!(
            survivor_order(&members, &["n1".to_string()]),
            ["n2", "n3", "n1"].map(String::from)
        );
        assert_eq!(survivor_order(&members, &[]), members);
    }

    #[test]
    fn a_node_outside_every_cluster_has_no_gate() {
        let db = Database::open(":memory:").expect("db");
        assert!(member_cluster(&db, "n1").is_none());
    }

    #[test]
    fn mon_first_puts_osd_only_last() {
        let spec = crate::controller_proto::CephClusterSpec {
            nodes: vec![
                crate::controller_proto::CephClusterNodeSpec {
                    node_id: "a".into(),
                    role: "osd".into(),
                    ..Default::default()
                },
                crate::controller_proto::CephClusterNodeSpec {
                    node_id: "b".into(),
                    role: "mon".into(),
                    ..Default::default()
                },
                crate::controller_proto::CephClusterNodeSpec {
                    node_id: "c".into(),
                    role: "full".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let order = mon_first_rollout_order(&spec, &["a".into(), "b".into(), "c".into()]);
        assert_eq!(order, vec!["b", "c", "a"]);
    }
}
