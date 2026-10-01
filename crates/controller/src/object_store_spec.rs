use std::collections::HashSet;

use crate::controller_proto::ObjectStoreSpec;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSpec {
    ceph_cluster: String,
    members: Vec<String>,
    port: i32,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    rgw_id: String,
}

pub fn validate_spec(spec: &ObjectStoreSpec) -> Result<(), String> {
    if spec.ceph_cluster.trim().is_empty() {
        return Err("cephCluster is required".into());
    }
    if spec.members.is_empty() {
        return Err("members must contain at least one node id".into());
    }
    let mut ids = HashSet::new();
    for member in &spec.members {
        let id = member.trim();
        if id.is_empty() || !ids.insert(id.to_string()) {
            return Err(format!("members must be non-empty and unique: {member}"));
        }
    }
    if spec.port != 0 && !(1..=65535).contains(&spec.port) {
        return Err("port must be 0 (default) or between 1 and 65535".into());
    }
    Ok(())
}

pub fn spec_to_json(spec: &ObjectStoreSpec) -> Result<String, serde_json::Error> {
    let stored = StoredSpec {
        ceph_cluster: spec.ceph_cluster.trim().to_string(),
        members: spec.members.iter().map(|m| m.trim().to_string()).collect(),
        port: if spec.port == 0 { 7480 } else { spec.port },
        tls: spec.tls,
        rgw_id: spec.rgw_id.trim().to_string(),
    };
    serde_json::to_string(&stored)
}

pub fn spec_from_json(json: &str) -> Result<ObjectStoreSpec, serde_json::Error> {
    let stored: StoredSpec = serde_json::from_str(json)?;
    Ok(ObjectStoreSpec {
        ceph_cluster: stored.ceph_cluster,
        members: stored.members,
        port: stored.port,
        tls: stored.tls,
        rgw_id: stored.rgw_id,
    })
}

pub fn effective_rgw_id(resource_name: &str, spec: &ObjectStoreSpec) -> String {
    let custom = spec.rgw_id.trim();
    if custom.is_empty() {
        resource_name.trim().to_string()
    } else {
        custom.to_string()
    }
}
