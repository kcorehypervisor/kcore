use std::collections::HashSet;

use crate::controller_proto::{SharedFilesystemClientSpec, SharedFilesystemSpec};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSpec {
    ceph_cluster: String,
    quota_bytes: i64,
    #[serde(default)]
    clients: Vec<StoredClient>,
    #[serde(default)]
    fs_name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredClient {
    name: String,
    #[serde(default)]
    paths: Vec<String>,
}

pub fn validate_spec(spec: &SharedFilesystemSpec) -> Result<(), String> {
    if spec.ceph_cluster.trim().is_empty() {
        return Err("cephCluster is required".into());
    }
    if spec.quota_bytes < 0 {
        return Err("quotaBytes must be non-negative".into());
    }
    let mut names = HashSet::new();
    for client in &spec.clients {
        let name = client.name.trim();
        if name.is_empty() || !names.insert(name.to_string()) {
            return Err(format!(
                "client name must be non-empty and unique: {}",
                client.name
            ));
        }
    }
    Ok(())
}

pub fn spec_to_json(spec: &SharedFilesystemSpec) -> Result<String, serde_json::Error> {
    let stored = StoredSpec {
        ceph_cluster: spec.ceph_cluster.trim().to_string(),
        quota_bytes: spec.quota_bytes,
        clients: spec
            .clients
            .iter()
            .map(|c| StoredClient {
                name: c.name.trim().to_string(),
                paths: c
                    .paths
                    .iter()
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty())
                    .collect(),
            })
            .collect(),
        fs_name: spec.fs_name.trim().to_string(),
    };
    serde_json::to_string(&stored)
}

pub fn spec_from_json(json: &str) -> Result<SharedFilesystemSpec, serde_json::Error> {
    let stored: StoredSpec = serde_json::from_str(json)?;
    Ok(SharedFilesystemSpec {
        ceph_cluster: stored.ceph_cluster,
        quota_bytes: stored.quota_bytes,
        clients: stored
            .clients
            .into_iter()
            .map(|c| SharedFilesystemClientSpec {
                name: c.name,
                paths: c.paths,
            })
            .collect(),
        fs_name: stored.fs_name,
    })
}

pub fn effective_fs_name(resource_name: &str, spec: &SharedFilesystemSpec) -> String {
    let custom = spec.fs_name.trim();
    if custom.is_empty() {
        resource_name.trim().to_string()
    } else {
        custom.to_string()
    }
}
