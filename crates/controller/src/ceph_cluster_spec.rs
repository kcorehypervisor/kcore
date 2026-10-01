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
    }
    Ok(())
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
}
