use std::collections::HashSet;
use std::path::Path;

use crate::controller_proto::{CephClusterNodeSpec, CephClusterSpec};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSpec {
    fsid: String,
    public_network: String,
    cluster_network: String,
    size: i32,
    min_size: i32,
    #[serde(default)]
    force_wipe: bool,
    /// OSD dm-crypt. Missing in pre-E1 JSON → false (legacy plain OSDs).
    /// New clusters set true via kctl / create path.
    #[serde(default)]
    encrypt_osds: bool,
    nodes: Vec<StoredNode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredNode {
    node_id: String,
    mon_addr: String,
    cluster_addr: String,
    public_iface: String,
    cluster_iface: String,
    osd_device: String,
    #[serde(default)]
    osd_devices: Vec<String>,
}

pub fn validate_spec(spec: &CephClusterSpec) -> Result<(), String> {
    if spec.nodes.is_empty() {
        return Err("spec.nodes must contain at least one node".into());
    }
    if spec.public_network.trim().is_empty() || spec.cluster_network.trim().is_empty() {
        return Err("publicNetwork and clusterNetwork are required".into());
    }
    if spec.size <= 0 || spec.min_size <= 0 || spec.size < spec.min_size {
        return Err("size must be >= minSize and both must be positive".into());
    }
    let mut ids = HashSet::new();
    for node in &spec.nodes {
        if node.node_id.trim().is_empty() || !ids.insert(node.node_id.trim()) {
            return Err(format!(
                "nodeId must be non-empty and unique: {}",
                node.node_id
            ));
        }
        let devices = osd_devices(node);
        if devices.is_empty() {
            return Err(format!("osdDevice for {} is required", node.node_id));
        }
        for device in &devices {
            if !Path::new(device).is_absolute() {
                return Err(format!(
                    "osdDevice for {} must be absolute: {device}",
                    node.node_id
                ));
            }
        }
        if node.public_iface.trim().is_empty() || node.cluster_iface.trim().is_empty() {
            return Err(format!(
                "publicIface and clusterIface are required for {}",
                node.node_id
            ));
        }
    }
    Ok(())
}

/// Loud plan notice for encryptOsds (immutable after create).
pub fn encrypt_osds_notice(encrypt: bool) -> String {
    if encrypt {
        "encryptOsds=true (DEFAULT): every OSD will be LUKS-encrypted via ceph-volume --dmcrypt. \
This is immutable after create — set encryptOsds: false explicitly only for labs."
            .into()
    } else {
        "encryptOsds=false: OSD disks will NOT be encrypted. Immutable after create.".into()
    }
}

/// Kernel paths this node should have OSDs on.
///
/// `osd_device` is the original single-disk field. `osd_devices` adds more
/// without dropping it, so a manifest can grow a second disk in place.
pub fn osd_devices(node: &CephClusterNodeSpec) -> Vec<String> {
    let mut out = Vec::new();
    let primary = node.osd_device.trim();
    if !primary.is_empty() {
        out.push(primary.to_string());
    }
    for device in &node.osd_devices {
        let device = device.trim();
        if !device.is_empty() && !out.iter().any(|existing| existing == device) {
            out.push(device.to_string());
        }
    }
    out
}

pub fn spec_to_json(spec: &CephClusterSpec) -> Result<String, serde_json::Error> {
    serde_json::to_string(&StoredSpec {
        fsid: spec.fsid.clone(),
        public_network: spec.public_network.clone(),
        cluster_network: spec.cluster_network.clone(),
        size: spec.size,
        min_size: spec.min_size,
        force_wipe: spec.force_wipe,
        encrypt_osds: spec.encrypt_osds,
        nodes: spec
            .nodes
            .iter()
            .map(|n| StoredNode {
                node_id: n.node_id.clone(),
                mon_addr: n.mon_addr.clone(),
                cluster_addr: n.cluster_addr.clone(),
                public_iface: n.public_iface.clone(),
                cluster_iface: n.cluster_iface.clone(),
                osd_device: n.osd_device.clone(),
                osd_devices: n.osd_devices.clone(),
            })
            .collect(),
    })
}

pub fn spec_from_json(json: &str) -> Result<CephClusterSpec, serde_json::Error> {
    let s: StoredSpec = serde_json::from_str(json)?;
    Ok(CephClusterSpec {
        fsid: s.fsid,
        public_network: s.public_network,
        cluster_network: s.cluster_network,
        size: s.size,
        min_size: s.min_size,
        force_wipe: s.force_wipe,
        encrypt_osds: s.encrypt_osds,
        nodes: s
            .nodes
            .into_iter()
            .map(|n| CephClusterNodeSpec {
                node_id: n.node_id,
                mon_addr: n.mon_addr,
                cluster_addr: n.cluster_addr,
                public_iface: n.public_iface,
                cluster_iface: n.cluster_iface,
                osd_device: n.osd_device,
                osd_devices: n.osd_devices,
            })
            .collect(),
    })
}

/// Node ids present in `old` but not in `new`: the members an apply retires.
pub fn removed_members(old: &CephClusterSpec, new: &CephClusterSpec) -> Vec<String> {
    old.nodes
        .iter()
        .filter(|n| !new.nodes.iter().any(|m| m.node_id == n.node_id))
        .map(|n| n.node_id.clone())
        .collect()
}

/// What the cluster looked like when the operator asked to retire a member.
pub struct RemovalContext<'a> {
    /// `ceph -s` health of the cluster right now; `None` when no survivor
    /// could be asked (treated as not healthy: a blind removal is refused).
    pub health_status: Option<&'a str>,
    /// Members whose earlier removal the reconciler has not finished.
    pub pending_removals: &'a [String],
}

/// Verdict on retiring the members that `new` drops from `old`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemovalPlan {
    pub removed: Vec<String>,
    /// Blocking: the apply is refused while any of these hold.
    pub issues: Vec<String>,
    /// Shown to the operator; the apply proceeds.
    pub warnings: Vec<String>,
}

/// Each replica lives on a distinct host (the default CRUSH rule), so the
/// survivors must still number at least `size`; the cluster must be
/// HEALTH_OK so every PG is fully replicated before one copy leaves; and
/// removals go one at a time so the drain of one host finishes before the
/// next starts.
pub fn plan_member_removal(
    old: &CephClusterSpec,
    new: &CephClusterSpec,
    ctx: &RemovalContext<'_>,
) -> RemovalPlan {
    let removed = removed_members(old, new);
    if removed.is_empty() {
        return RemovalPlan::default();
    }
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    let leaving = removed.join(", ");
    if removed.len() > 1 {
        issues.push(format!(
            "remove one member per apply so each host drains before the next leaves; this apply removes {}: {leaving}",
            removed.len()
        ));
    }
    let remaining = new.nodes.len();
    if (remaining as i64) < i64::from(new.size) {
        issues.push(format!(
            "removing {leaving} leaves {remaining} host(s) for size {}: every replica needs its own host, so lower size (and minSize) first or add a member",
            new.size
        ));
    }
    if remaining == 2 {
        warnings.push(
            "2 monitors cannot survive losing either one (quorum needs a majority); plan to go back to 3+ members or down to 1"
                .to_string(),
        );
    }
    match ctx.health_status {
        Some("HEALTH_OK") => {}
        Some(status) => issues.push(format!(
            "cluster is {status}; wait for HEALTH_OK so every placement group is fully replicated before a host leaves"
        )),
        None => issues.push(
            "could not read ceph health from a surviving member; refusing to remove a host blind".to_string(),
        ),
    }
    let still_pending: Vec<&str> = ctx
        .pending_removals
        .iter()
        .filter(|id| !removed.contains(id))
        .map(String::as_str)
        .collect();
    if !still_pending.is_empty() {
        issues.push(format!(
            "removal of {} is still in progress; wait for it to finish",
            still_pending.join(", ")
        ));
    }
    warnings.push(format!(
        "{leaving} will be drained (OSDs marked out, wait for every PG to be active+clean), then its OSDs purged and its monitor removed"
    ));
    RemovalPlan {
        removed,
        issues,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_node(id: &str) -> CephClusterNodeSpec {
        CephClusterNodeSpec {
            node_id: id.into(),
            mon_addr: "10.10.0.11:6789".into(),
            cluster_addr: "10.20.0.11".into(),
            public_iface: "eth1".into(),
            cluster_iface: "eth2".into(),
            osd_device: "/dev/nvme0n1".into(),
            osd_devices: Vec::new(),
        }
    }

    fn valid_spec() -> CephClusterSpec {
        CephClusterSpec {
            fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
            public_network: "10.10.0.0/24".into(),
            cluster_network: "10.20.0.0/24".into(),
            size: 3,
            min_size: 2,
            force_wipe: false,
            encrypt_osds: true,
            nodes: vec![
                valid_node("dell-1"),
                valid_node("dell-2"),
                valid_node("dell-3"),
            ],
        }
    }

    #[test]
    fn validate_accepts_three_node_lab_spec() {
        validate_spec(&valid_spec()).expect("valid");
    }

    #[test]
    fn validate_rejects_empty_nodes() {
        let mut spec = valid_spec();
        spec.nodes.clear();
        let err = validate_spec(&spec).expect_err("empty nodes");
        assert!(err.contains("nodes"));
    }

    #[test]
    fn validate_rejects_empty_networks() {
        let mut spec = valid_spec();
        spec.public_network = "  ".into();
        assert!(validate_spec(&spec).is_err());
        spec = valid_spec();
        spec.cluster_network.clear();
        assert!(validate_spec(&spec).is_err());
    }

    #[test]
    fn validate_rejects_size_less_than_min_size() {
        let mut spec = valid_spec();
        spec.size = 1;
        spec.min_size = 2;
        let err = validate_spec(&spec).expect_err("size < minSize");
        assert!(err.contains("size"));
    }

    #[test]
    fn validate_rejects_non_positive_replication() {
        let mut spec = valid_spec();
        spec.size = 0;
        spec.min_size = 0;
        assert!(validate_spec(&spec).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_or_empty_node_id() {
        let mut spec = valid_spec();
        spec.nodes[1].node_id = "dell-1".into();
        assert!(validate_spec(&spec).unwrap_err().contains("unique"));
        spec = valid_spec();
        spec.nodes[0].node_id = "  ".into();
        assert!(validate_spec(&spec).is_err());
    }

    #[test]
    fn validate_rejects_relative_osd_device() {
        let mut spec = valid_spec();
        spec.nodes[0].osd_device = "nvme0n1".into();
        let err = validate_spec(&spec).expect_err("relative osd");
        assert!(err.contains("absolute"));
    }

    #[test]
    fn encrypt_osds_notice_is_loud_for_default_true() {
        let notice = encrypt_osds_notice(true);
        assert!(notice.contains("encryptOsds=true"));
        assert!(notice.to_ascii_lowercase().contains("immutable"));
        let plain = encrypt_osds_notice(false);
        assert!(plain.contains("encryptOsds=false"));
    }

    #[test]
    fn validate_accepts_by_id_osd_device() {
        let mut spec = valid_spec();
        spec.nodes[0].osd_device = "/dev/disk/by-id/ata-Micron_1300_SATA_512GB_2144329E98F2".into();
        validate_spec(&spec).expect("by-id path");
    }

    #[test]
    fn osd_devices_keeps_the_primary_and_appends_more() {
        let mut node = valid_node("dell-1");
        node.osd_devices = vec!["/dev/nvme1n1".into(), "/dev/nvme0n1".into()];
        assert_eq!(
            osd_devices(&node),
            vec!["/dev/nvme0n1".to_string(), "/dev/nvme1n1".to_string()]
        );
        let mut spec = valid_spec();
        spec.nodes[0] = node;
        validate_spec(&spec).expect("two disks");
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert_eq!(
            back.nodes[0].osd_devices,
            vec!["/dev/nvme1n1".to_string(), "/dev/nvme0n1".to_string()]
        );
    }

    #[test]
    fn json_round_trip_preserves_camel_case_fields() {
        let spec = valid_spec();
        let json = spec_to_json(&spec).expect("encode");
        assert!(json.contains("\"publicNetwork\""));
        assert!(json.contains("\"clusterNetwork\""));
        assert!(json.contains("\"osdDevice\""));
        assert!(json.contains("\"nodeId\""));
        let back = spec_from_json(&json).expect("decode");
        assert_eq!(back.fsid, spec.fsid);
        assert_eq!(back.public_network, spec.public_network);
        assert_eq!(back.cluster_network, spec.cluster_network);
        assert_eq!(back.size, spec.size);
        assert_eq!(back.min_size, spec.min_size);
        assert_eq!(back.force_wipe, spec.force_wipe);
        assert_eq!(back.encrypt_osds, spec.encrypt_osds);
        assert_eq!(back.nodes.len(), 3);
        assert_eq!(back.nodes[0].node_id, "dell-1");
        assert_eq!(back.nodes[0].osd_device, "/dev/nvme0n1");
    }

    #[test]
    fn json_defaults_force_wipe_to_false_when_omitted() {
        let json = r#"{
            "fsid":"x","publicNetwork":"10.0.0.0/24","clusterNetwork":"10.1.0.0/24",
            "size":3,"minSize":2,
            "nodes":[{"nodeId":"n1","monAddr":"a","clusterAddr":"b",
              "publicIface":"e1","clusterIface":"e2","osdDevice":"/dev/sda"}]
        }"#;
        let spec = spec_from_json(json).expect("decode");
        assert!(!spec.force_wipe);
        // Pre-E1 JSON has no encryptOsds → legacy plain OSDs.
        assert!(!spec.encrypt_osds);
    }

    #[test]
    fn json_preserves_encrypt_osds() {
        let mut spec = valid_spec();
        spec.encrypt_osds = true;
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert!(back.encrypt_osds);
        spec.encrypt_osds = false;
        let back = spec_from_json(&spec_to_json(&spec).unwrap()).unwrap();
        assert!(!back.encrypt_osds);
    }

    fn without(spec: &CephClusterSpec, id: &str) -> CephClusterSpec {
        let mut s = spec.clone();
        s.nodes.retain(|n| n.node_id != id);
        s
    }

    fn ctx<'a>(health: Option<&'a str>, pending: &'a [String]) -> RemovalContext<'a> {
        RemovalContext {
            health_status: health,
            pending_removals: pending,
        }
    }

    #[test]
    fn removal_plan_is_empty_when_no_member_leaves() {
        let old = valid_spec();
        let plan = plan_member_removal(&old, &old, &ctx(None, &[]));
        assert_eq!(plan, RemovalPlan::default());
    }

    #[test]
    fn removal_needs_size_hosts_left_and_health_ok() {
        let old = valid_spec();
        let new = without(&old, "dell-3");
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &[]));
        assert_eq!(plan.removed, vec!["dell-3".to_string()]);
        assert_eq!(plan.issues.len(), 1, "{:?}", plan.issues);
        assert!(plan.issues[0].contains("leaves 2 host(s) for size 3"));

        let mut shrunk = new.clone();
        shrunk.size = 2;
        let plan = plan_member_removal(&old, &shrunk, &ctx(Some("HEALTH_OK"), &[]));
        assert!(plan.issues.is_empty(), "{:?}", plan.issues);
        assert!(plan.warnings.iter().any(|w| w.contains("2 monitors")));
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("dell-3 will be drained")));

        let plan = plan_member_removal(&old, &shrunk, &ctx(Some("HEALTH_WARN"), &[]));
        assert!(plan
            .issues
            .iter()
            .any(|i| i.contains("cluster is HEALTH_WARN")));
        let plan = plan_member_removal(&old, &shrunk, &ctx(None, &[]));
        assert!(plan
            .issues
            .iter()
            .any(|i| i.contains("refusing to remove a host blind")));
    }

    #[test]
    fn removal_is_one_member_at_a_time() {
        let old = valid_spec();
        let mut new = without(&without(&old, "dell-3"), "dell-2");
        new.size = 1;
        new.min_size = 1;
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &[]));
        assert!(plan
            .issues
            .iter()
            .any(|i| i.contains("remove one member per apply")));

        let mut new = without(&old, "dell-3");
        new.size = 2;
        let pending = vec!["dell-2".to_string()];
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &pending));
        assert!(plan
            .issues
            .iter()
            .any(|i| i.contains("removal of dell-2 is still in progress")));
        // Re-applying the same removal is not blocked by its own pending entry.
        let pending = vec!["dell-3".to_string()];
        let plan = plan_member_removal(&old, &new, &ctx(Some("HEALTH_OK"), &pending));
        assert!(plan.issues.is_empty(), "{:?}", plan.issues);
    }
}
