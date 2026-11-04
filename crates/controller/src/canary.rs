//! Canary rollouts. The first node of a canary ClusterUpdate runs the target
//! for a soak period while the controller compares it with the other nodes.
//! The rest of the nodes start only after it passes. The canary is never a
//! controller unless every node is one, so a controller still on the old
//! version judges it.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tonic::Request;

use crate::controller_proto::{ClusterUpdateSpec, ClusterUpdateStrategyType};
use crate::db::Database;
use crate::node_client::NodeClients;
use crate::node_proto;

/// Soak of a canary that names none, and of a release on the same NixOS.
pub const DEFAULT_SOAK_SECS: i32 = 3600;
/// Soak of a release that moves to another NixOS release.
pub const BASE_RELEASE_SOAK_SECS: i32 = 86_400;
pub const MAX_SOAK_SECS: i32 = 7 * 86_400;
/// A node label that makes the node the canary of every rollout it is in.
pub const CANARY_LABEL: &str = "kcore.io/update-canary";
/// Guests restart after a reboot; a failure is only declared after this much
/// of the soak, or at its end.
pub const GRACE_SECS: i64 = 300;
const HEARTBEAT_STALE_SECS: i64 = 4 * crate::ha::NODE_HEARTBEAT_INTERVAL_SECS;
/// How much larger the canary's share of stopped guests may be than its
/// peers' before it fails.
const GUEST_MARGIN: f64 = 0.10;
/// How much larger the canary's share of failed agent calls may be than its
/// peers', and how many failures it takes.
const FAULT_MARGIN: f64 = 0.10;
const MIN_FAULTS: u32 = 5;
/// Ceph latency is sampled at most this often and judged over this window.
const CEPH_SAMPLE_EVERY: Duration = Duration::from_secs(60);
const CEPH_WINDOW: Duration = Duration::from_secs(15 * 60);
const MIN_CEPH_SAMPLES: usize = 5;
const CEPH_RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// The canary's OSDs fail when their median commit latency is above both
/// this multiple of the other OSDs' and this many milliseconds more.
const CEPH_FACTOR: f64 = 2.0;
const CEPH_MARGIN_MS: f64 = 20.0;

pub fn is_canary(spec: &ClusterUpdateSpec) -> bool {
    spec.strategy
        .as_ref()
        .is_some_and(|s| s.r#type == ClusterUpdateStrategyType::ClusterUpdateStrategyCanary as i32)
}

pub fn soak_secs(spec: &ClusterUpdateSpec) -> i64 {
    match spec.strategy.as_ref().map(|s| s.canary_soak_seconds) {
        Some(secs) if secs > 0 => i64::from(secs),
        _ => i64::from(DEFAULT_SOAK_SECS),
    }
}

/// The canary among `ordered` (rollout order): the node the strategy names,
/// else a node labelled [`CANARY_LABEL`], else the first. Controllers are
/// only candidates when every node runs one. A rollout of one node has
/// nothing to protect and no canary.
pub fn pick(db: &Database, spec: &ClusterUpdateSpec, ordered: &[String]) -> Option<String> {
    if !is_canary(spec) || ordered.len() < 2 {
        return None;
    }
    let candidates = candidates(db, ordered);
    let named = spec
        .strategy
        .as_ref()
        .map(|s| s.canary_node_id.trim())
        .unwrap_or("");
    if let Some(id) = candidates
        .iter()
        .find(|id| !named.is_empty() && **id == named)
    {
        return Some(id.clone());
    }
    let labelled = candidates.iter().find(|id| {
        db.get_node_labels(id)
            .unwrap_or_default()
            .iter()
            .any(|label| is_canary_label(label))
    });
    labelled.or_else(|| candidates.first()).cloned()
}

/// Refuse a named canary that is not in the rollout, or that runs a
/// controller while some node in the rollout does not.
pub fn check_named(db: &Database, ordered: &[String], named: &str) -> Result<(), String> {
    let named = named.trim();
    if named.is_empty() {
        return Ok(());
    }
    if !ordered.iter().any(|id| id == named) {
        return Err(format!(
            "canary node {named} is not among the nodes to update"
        ));
    }
    if !candidates(db, ordered).iter().any(|id| id == named) {
        return Err(format!(
            "canary node {named} runs a controller; the canary must be a node the controllers judge"
        ));
    }
    Ok(())
}

fn candidates(db: &Database, ordered: &[String]) -> Vec<String> {
    let peers = db.list_controller_peers().unwrap_or_default();
    let workers: Vec<String> = ordered
        .iter()
        .filter(|id| !crate::cluster_update_reconciler::runs_controller(db, &peers, id))
        .cloned()
        .collect();
    if workers.is_empty() {
        ordered.to_vec()
    } else {
        workers
    }
}

fn is_canary_label(label: &str) -> bool {
    match label.strip_prefix(CANARY_LABEL) {
        Some("") => true,
        Some(rest) => rest.strip_prefix('=').is_some_and(|v| v != "false"),
        None => false,
    }
}

/// A day when the release moves to another NixOS release, an hour otherwise.
/// An unknown release on either side counts as the same release.
pub fn default_soak_secs(target_nixos: &str, running_nixos: &str) -> i32 {
    let (target, running) = (target_nixos.trim(), running_nixos.trim());
    if !target.is_empty() && !running.is_empty() && target != running {
        BASE_RELEASE_SOAK_SECS
    } else {
        DEFAULT_SOAK_SECS
    }
}

/// The NixOS release this controller runs, `26.05` for example.
pub fn running_nixos_release() -> String {
    std::fs::read_to_string("/etc/os-release")
        .map(|text| os_release_version(&text))
        .unwrap_or_default()
}

fn os_release_version(text: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("VERSION_ID="))
        .map(|v| v.trim().trim_matches('"').to_string())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeSample {
    /// Approved, not marked not-ready, and heartbeating.
    pub live: bool,
    /// Guests whose desired state is running.
    pub guests_wanted: u32,
    /// Of those, the ones not running.
    pub guests_down: u32,
    /// Controller calls to the node agent during the soak, and the ones the
    /// agent failed.
    pub calls: u32,
    pub call_faults: u32,
}

/// Median commit latency of the canary's OSDs and of every other OSD over
/// the recent samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CephLatency {
    pub canary_ms: f64,
    pub peers_ms: f64,
    pub samples: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(String),
    /// Not enough to compare with. The rollout waits for the operator.
    Unknown(String),
}

/// `calls_since` bounds the agent calls counted; `None` counts none.
pub fn sample(
    db: &Database,
    clients: &NodeClients,
    node_id: &str,
    calls_since: Option<Instant>,
) -> NodeSample {
    let node = db.get_node(node_id).ok().flatten();
    let calls = match (&node, calls_since) {
        (Some(node), Some(since)) => clients.agent_calls().count(&node.address, since),
        _ => Default::default(),
    };
    let live = node.is_some_and(|n| n.approval_status == "approved" && n.status != "not-ready")
        && db
            .node_recently_seen(node_id, HEARTBEAT_STALE_SECS)
            .unwrap_or(false);
    let mut sample = NodeSample {
        live,
        calls: calls.calls,
        call_faults: calls.faults,
        ..Default::default()
    };
    for vm in db.list_vms_for_node(node_id).unwrap_or_default() {
        if !vm.auto_start {
            continue;
        }
        sample.guests_wanted += 1;
        if !matches!(vm.runtime_state.as_str(), "running" | "paused") {
            sample.guests_down += 1;
        }
    }
    sample
}

/// Compare the canary with its peers. `on_target` is false once the canary
/// no longer runs the generation it was confirmed on.
pub fn judge(
    canary: &NodeSample,
    on_target: bool,
    peers: &[NodeSample],
    ceph: Option<&CephLatency>,
) -> Verdict {
    if !on_target {
        return Verdict::Fail("the canary no longer runs the target generation".into());
    }
    let live: Vec<&NodeSample> = peers.iter().filter(|p| p.live).collect();
    if !canary.live {
        return if live.is_empty() {
            Verdict::Unknown("neither the canary nor any other node sends heartbeats".into())
        } else {
            Verdict::Fail("the canary stopped sending heartbeats while other nodes did not".into())
        };
    }
    if live.is_empty() {
        return Verdict::Unknown(
            "no other node sends heartbeats to compare the canary with".into(),
        );
    }
    let share = |down: u32, wanted: u32| {
        if wanted == 0 {
            0.0
        } else {
            f64::from(down) / f64::from(wanted)
        }
    };
    let peer_share = share(
        live.iter().map(|p| p.guests_down).sum(),
        live.iter().map(|p| p.guests_wanted).sum(),
    );
    let canary_share = share(canary.guests_down, canary.guests_wanted);
    if canary.guests_down > 0 && canary_share > peer_share + GUEST_MARGIN {
        return Verdict::Fail(format!(
            "{} of {} guests on the canary are not running, against {:.0}% on the other nodes",
            canary.guests_down,
            canary.guests_wanted,
            peer_share * 100.0
        ));
    }
    let peer_fault_share = share(
        live.iter().map(|p| p.call_faults).sum(),
        live.iter().map(|p| p.calls).sum(),
    );
    let canary_fault_share = share(canary.call_faults, canary.calls);
    if canary.call_faults >= MIN_FAULTS && canary_fault_share > peer_fault_share + FAULT_MARGIN {
        return Verdict::Fail(format!(
            "the canary's agent failed {} of {} controller calls, against {:.0}% on the other nodes",
            canary.call_faults,
            canary.calls,
            peer_fault_share * 100.0
        ));
    }
    if let Some(ceph) = ceph.filter(|c| c.samples >= MIN_CEPH_SAMPLES) {
        let limit = (ceph.peers_ms * CEPH_FACTOR).max(ceph.peers_ms + CEPH_MARGIN_MS);
        if ceph.canary_ms > limit {
            return Verdict::Fail(format!(
                "the canary's OSDs commit in {:.0} ms, the other OSDs in {:.0} ms",
                ceph.canary_ms, ceph.peers_ms
            ));
        }
    }
    Verdict::Pass
}

type CephKey = (String, String);
type CephHistory = VecDeque<(Instant, f64, f64)>;

static CEPH_SAMPLES: LazyLock<Mutex<HashMap<CephKey, CephHistory>>> =
    LazyLock::new(Default::default);

/// Sample Ceph OSD latency for the canary of `update_name`, at most once a
/// minute, and summarize the last 15 minutes. `None` when the canary hosts no
/// OSD or no Ceph member answers. Samples are kept in memory, so a
/// controller restart starts the window again.
pub async fn ceph_latency(
    db: &Database,
    clients: &NodeClients,
    update_name: &str,
    canary_id: &str,
) -> Option<CephLatency> {
    let key = (update_name.to_string(), canary_id.to_string());
    let due = {
        let samples = CEPH_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
        samples
            .get(&key)
            .and_then(|h| h.back())
            .is_none_or(|(at, _, _)| at.elapsed() >= CEPH_SAMPLE_EVERY)
    };
    if due {
        if let Some((canary_ms, peers_ms)) = fetch_ceph_latency(db, clients, canary_id).await {
            let mut samples = CEPH_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
            samples.entry(key.clone()).or_default().push_back((
                Instant::now(),
                canary_ms,
                peers_ms,
            ));
        }
    }
    let mut samples = CEPH_SAMPLES.lock().unwrap_or_else(|e| e.into_inner());
    let history = samples.get_mut(&key)?;
    while history
        .front()
        .is_some_and(|(at, _, _)| at.elapsed() > CEPH_WINDOW)
    {
        history.pop_front();
    }
    summarize(history)
}

/// Drop the Ceph samples of an update whose canary is no longer soaking.
pub fn forget(update_name: &str) {
    CEPH_SAMPLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(name, _), _| name != update_name);
}

fn summarize(history: &CephHistory) -> Option<CephLatency> {
    if history.is_empty() {
        return None;
    }
    let median = |mut values: Vec<f64>| {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    Some(CephLatency {
        canary_ms: median(history.iter().map(|(_, c, _)| *c).collect()),
        peers_ms: median(history.iter().map(|(_, _, p)| *p).collect()),
        samples: history.len(),
    })
}

/// Mean commit latency of the canary's OSDs and of the other OSDs, read
/// from the canary first, then from the other Ceph members.
async fn fetch_ceph_latency(
    db: &Database,
    clients: &NodeClients,
    canary_id: &str,
) -> Option<(f64, f64)> {
    let mut members: Vec<String> = Vec::new();
    for row in db.list_ceph_clusters().unwrap_or_default() {
        let Ok(spec) = crate::ceph_cluster_spec::spec_from_json(&row.spec_json) else {
            continue;
        };
        if spec.nodes.iter().any(|n| n.node_id == canary_id) {
            members.extend(spec.nodes.into_iter().map(|n| n.node_id));
        }
    }
    if members.is_empty() {
        return None;
    }
    members.sort_by_key(|id| id != canary_id);
    members.dedup();
    let host = db
        .get_node(canary_id)
        .ok()
        .flatten()
        .map(|n| n.hostname)
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| canary_id.to_string());
    for member in members {
        let Ok(mut admin) = crate::ceph_cluster_reconciler::admin_for(db, clients, &member).await
        else {
            continue;
        };
        let mut request = Request::new(node_proto::GetCephOsdLatencyRequest {});
        request.set_timeout(CEPH_RPC_TIMEOUT);
        let Ok(resp) = admin.get_ceph_osd_latency(request).await else {
            continue;
        };
        return osd_means(&resp.into_inner().osds, &host);
    }
    None
}

fn osd_means(osds: &[node_proto::CephOsdLatency], canary_host: &str) -> Option<(f64, f64)> {
    let mean = |values: Vec<u32>| {
        (!values.is_empty())
            .then(|| values.iter().map(|v| f64::from(*v)).sum::<f64>() / values.len() as f64)
    };
    let same_host = |host: &str| host.eq_ignore_ascii_case(canary_host);
    let canary = mean(
        osds.iter()
            .filter(|o| same_host(&o.host))
            .map(|o| o.commit_latency_ms)
            .collect(),
    )?;
    let peers = mean(
        osds.iter()
            .filter(|o| !o.host.is_empty() && !same_host(&o.host))
            .map(|o| o.commit_latency_ms)
            .collect(),
    )?;
    Some((canary, peers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_proto::ClusterUpdateStrategy;
    use crate::db::NodeRow;

    fn live(wanted: u32, down: u32) -> NodeSample {
        NodeSample {
            live: true,
            guests_wanted: wanted,
            guests_down: down,
            ..Default::default()
        }
    }

    fn calling(calls: u32, faults: u32) -> NodeSample {
        NodeSample {
            calls,
            call_faults: faults,
            ..live(0, 0)
        }
    }

    fn canary_spec(node: &str) -> ClusterUpdateSpec {
        ClusterUpdateSpec {
            strategy: Some(ClusterUpdateStrategy {
                r#type: ClusterUpdateStrategyType::ClusterUpdateStrategyCanary as i32,
                max_unavailable: 1,
                canary_node_id: node.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn node(id: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: format!("{id}:9091"),
            cpu_cores: 0,
            memory_bytes: 0,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: String::new(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: 0,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    #[test]
    fn a_healthy_canary_passes() {
        assert_eq!(judge(&live(4, 0), true, &[live(4, 0)], None), Verdict::Pass);
        assert_eq!(
            judge(&live(10, 1), true, &[live(10, 1)], None),
            Verdict::Pass,
            "as bad as its peers is not worse"
        );
        assert_eq!(judge(&live(0, 0), true, &[live(3, 0)], None), Verdict::Pass);
    }

    #[test]
    fn a_canary_that_lost_guests_fails() {
        let Verdict::Fail(reason) = judge(&live(4, 2), true, &[live(4, 0), live(2, 0)], None)
        else {
            panic!("expected a failure");
        };
        assert!(reason.contains("2 of 4"), "{reason}");
    }

    #[test]
    fn a_canary_off_its_generation_or_silent_fails() {
        assert!(matches!(
            judge(&live(1, 0), false, &[live(1, 0)], None),
            Verdict::Fail(_)
        ));
        let silent = NodeSample::default();
        assert!(matches!(
            judge(&silent, true, &[live(1, 0)], None),
            Verdict::Fail(_)
        ));
    }

    #[test]
    fn a_canary_whose_agent_fails_calls_fails() {
        let Verdict::Fail(reason) = judge(&calling(20, 6), true, &[calling(40, 1)], None) else {
            panic!("expected a failure");
        };
        assert!(reason.contains("6 of 20"), "{reason}");
        assert_eq!(
            judge(&calling(20, 4), true, &[calling(40, 0)], None),
            Verdict::Pass,
            "a few failures are not a verdict"
        );
        assert_eq!(
            judge(&calling(20, 6), true, &[calling(20, 5)], None),
            Verdict::Pass,
            "as bad as its peers is not worse"
        );
    }

    #[test]
    fn slow_canary_osds_fail_after_enough_samples() {
        let slow = CephLatency {
            canary_ms: 60.0,
            peers_ms: 10.0,
            samples: MIN_CEPH_SAMPLES,
        };
        assert!(matches!(
            judge(&live(0, 0), true, &[live(0, 0)], Some(&slow)),
            Verdict::Fail(_)
        ));
        let early = CephLatency { samples: 2, ..slow };
        assert_eq!(
            judge(&live(0, 0), true, &[live(0, 0)], Some(&early)),
            Verdict::Pass
        );
        let close = CephLatency {
            canary_ms: 25.0,
            ..slow
        };
        assert_eq!(
            judge(&live(0, 0), true, &[live(0, 0)], Some(&close)),
            Verdict::Pass,
            "within 20 ms of the peers"
        );
    }

    #[test]
    fn osd_latency_is_split_by_the_canary_host() {
        let osd = |id: i32, host: &str, ms: u32| node_proto::CephOsdLatency {
            osd_id: id,
            host: host.into(),
            commit_latency_ms: ms,
            apply_latency_ms: ms,
        };
        let osds = [
            osd(0, "Node-A", 30),
            osd(1, "node-a", 10),
            osd(2, "node-b", 4),
            osd(3, "", 900),
        ];
        assert_eq!(osd_means(&osds, "node-a"), Some((20.0, 4.0)));
        assert_eq!(osd_means(&osds, "node-c"), None);
        let mut history = CephHistory::new();
        for (c, p) in [(9.0, 1.0), (1.0, 3.0), (5.0, 2.0)] {
            history.push_back((Instant::now(), c, p));
        }
        assert_eq!(
            summarize(&history),
            Some(CephLatency {
                canary_ms: 5.0,
                peers_ms: 2.0,
                samples: 3
            })
        );
    }

    #[test]
    fn no_live_peer_gives_no_verdict() {
        assert!(matches!(
            judge(&live(1, 0), true, &[], None),
            Verdict::Unknown(_)
        ));
        assert!(matches!(
            judge(&NodeSample::default(), true, &[NodeSample::default()], None),
            Verdict::Unknown(_)
        ));
    }

    #[test]
    fn a_new_nixos_release_soaks_for_a_day() {
        assert_eq!(default_soak_secs("26.11", "26.05"), BASE_RELEASE_SOAK_SECS);
        assert_eq!(default_soak_secs("26.05", "26.05"), DEFAULT_SOAK_SECS);
        assert_eq!(default_soak_secs("", "26.05"), DEFAULT_SOAK_SECS);
        assert_eq!(
            os_release_version("NAME=NixOS\nVERSION_ID=\"26.05\"\n"),
            "26.05"
        );
        let mut spec = canary_spec("");
        assert_eq!(soak_secs(&spec), i64::from(DEFAULT_SOAK_SECS));
        spec.strategy.as_mut().unwrap().canary_soak_seconds = 90;
        assert_eq!(soak_secs(&spec), 90);
    }

    #[test]
    fn the_canary_is_named_then_labelled_then_first() {
        let db = Database::open(":memory:").unwrap();
        for id in ["a", "b", "c"] {
            db.upsert_node(&node(id)).unwrap();
        }
        let ordered: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(pick(&db, &canary_spec(""), &ordered).as_deref(), Some("a"));
        db.upsert_node_labels("c", &[format!("{CANARY_LABEL}=true")])
            .unwrap();
        assert_eq!(pick(&db, &canary_spec(""), &ordered).as_deref(), Some("c"));
        assert_eq!(pick(&db, &canary_spec("b"), &ordered).as_deref(), Some("b"));
        assert_eq!(pick(&db, &canary_spec(""), &ordered[..1]), None);
        let mut plain = canary_spec("");
        plain.strategy.as_mut().unwrap().r#type =
            ClusterUpdateStrategyType::ClusterUpdateStrategyOneAtATime as i32;
        assert_eq!(pick(&db, &plain, &ordered), None);
        assert!(!is_canary_label("kcore.io/update-canary=false"));
        assert!(is_canary_label("kcore.io/update-canary"));
        assert!(!is_canary_label("kcore.io/update-canary-old"));
    }

    #[test]
    fn controllers_are_the_canary_only_when_every_node_is_one() {
        let db = Database::open(":memory:").unwrap();
        for id in ["a", "b", "c"] {
            db.upsert_node(&node(id)).unwrap();
        }
        db.upsert_controller_peer("ctl-a", "a:9090", "DC1").unwrap();
        db.upsert_node_labels("a", &[format!("{CANARY_LABEL}=true")])
            .unwrap();
        let ordered: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            pick(&db, &canary_spec(""), &ordered).as_deref(),
            Some("b"),
            "the label does not make a controller the canary"
        );
        assert_eq!(pick(&db, &canary_spec("a"), &ordered).as_deref(), Some("b"));
        assert!(check_named(&db, &ordered, "a")
            .unwrap_err()
            .contains("controller"));
        assert!(check_named(&db, &ordered, "z").is_err());
        assert!(check_named(&db, &ordered, "c").is_ok());
        let controllers: Vec<String> = vec!["a".into()];
        db.upsert_controller_peer("ctl-b", "b:9090", "DC1").unwrap();
        let both: Vec<String> = vec!["a".into(), "b".into()];
        assert_eq!(pick(&db, &canary_spec(""), &both).as_deref(), Some("a"));
        assert!(check_named(&db, &both, "b").is_ok());
        assert_eq!(pick(&db, &canary_spec(""), &controllers), None);
    }
}
