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
        if !Path::new(node.osd_device.trim()).is_absolute() {
            return Err(format!("osdDevice for {} must be absolute", node.node_id));
        }
    }
    Ok(())
}

pub fn spec_to_json(spec: &CephClusterSpec) -> Result<String, serde_json::Error> {
    serde_json::to_string(&StoredSpec {
        fsid: spec.fsid.clone(),
        public_network: spec.public_network.clone(),
        cluster_network: spec.cluster_network.clone(),
        size: spec.size,
        min_size: spec.min_size,
        force_wipe: spec.force_wipe,
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
    }
}
