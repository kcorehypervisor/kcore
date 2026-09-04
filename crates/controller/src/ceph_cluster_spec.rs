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
