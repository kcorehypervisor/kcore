//! Canonical camelCase SlurmCluster spec plus validation and plan actions.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmClusterSpec {
    pub cluster_name: String,
    pub controller: SlurmControllerSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_controller: Option<SlurmControllerSpec>,
    #[serde(default)]
    pub login: SlurmLoginSpec,
    #[serde(default)]
    pub accounting: SlurmAccountingSpec,
    #[serde(default)]
    pub compute: Vec<SlurmComputeSpec>,
    #[serde(default)]
    pub partitions: Vec<SlurmPartitionSpec>,
    #[serde(default)]
    pub users: Vec<SlurmUserSpec>,
    #[serde(default)]
    pub storage: SlurmStorageSpec,
    #[serde(default)]
    pub extra_config: String,
    #[serde(default)]
    pub allow_controller_host: bool,
    #[serde(default)]
    pub gpu_driver: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmControllerSpec {
    pub node_id: String,
    pub address: String,
    pub iface: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmLoginSpec {
    #[serde(default)]
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmAccountingSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmComputeSpec {
    pub node_id: String,
    pub address: String,
    pub iface: String,
    #[serde(default)]
    pub partitions: Vec<String>,
    #[serde(default)]
    pub reserve: SlurmReserveSpec,
    #[serde(default = "default_gres")]
    pub gres: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmReserveSpec {
    #[serde(default)]
    pub cores: i32,
    #[serde(default)]
    pub memory_mb: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmPartitionSpec {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub max_time: String,
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmUserSpec {
    pub name: String,
    pub uid: u32,
    #[serde(default)]
    pub ssh_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlurmStorageSpec {
    #[serde(default = "default_storage_mode")]
    pub mode: String,
    #[serde(default = "default_exports")]
    pub exports: Vec<String>,
    #[serde(default)]
    pub filesystem: String,
}

impl Default for SlurmStorageSpec {
    fn default() -> Self {
        Self {
            mode: default_storage_mode(),
            exports: default_exports(),
            filesystem: String::new(),
        }
    }
}

fn default_gres() -> String {
    "none".to_string()
}

fn default_storage_mode() -> String {
    "nfs".to_string()
}

fn default_exports() -> Vec<String> {
    vec!["/home".to_string(), "/scratch".to_string()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub node_id: String,
    pub address: String,
    pub iface: String,
    pub roles: Vec<String>,
    pub compute: Option<SlurmComputeSpec>,
}

impl Member {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// Immutable fields checked by [`plan_action`].
pub fn immutable_change(existing: &SlurmClusterSpec, desired: &SlurmClusterSpec) -> Option<String> {
    if existing.cluster_name != desired.cluster_name {
        return Some("clusterName".into());
    }
    if existing.controller.node_id != desired.controller.node_id {
        return Some("controller.nodeId".into());
    }
    None
}

/// `create`, `update`, `unchanged`, or `replace`, plus changed top-level fields.
pub fn plan_action(
    existing: Option<&SlurmClusterSpec>,
    desired: &SlurmClusterSpec,
) -> (&'static str, Vec<String>) {
    let Some(existing) = existing else {
        return ("create", Vec::new());
    };
    if immutable_change(existing, desired).is_some() {
        return ("replace", vec!["spec".into()]);
    }
    if existing == desired {
        return ("unchanged", Vec::new());
    }
    let left = serde_json::to_value(existing).ok();
    let right = serde_json::to_value(desired).ok();
    let mut changed = Vec::new();
    if let (Some(serde_json::Value::Object(left)), Some(serde_json::Value::Object(right))) =
        (left, right)
    {
        for (key, value) in &right {
            if left.get(key) != Some(value) {
                changed.push(key.clone());
            }
        }
    }
    if changed.is_empty() {
        changed.push("spec".into());
    }
    ("update", changed)
}

pub fn validate_spec(spec: &SlurmClusterSpec) -> Result<(), String> {
    if spec.cluster_name.trim().is_empty() {
        return Err("clusterName is required".into());
    }
    validate_token("clusterName", &spec.cluster_name)?;
    validate_controller("controller", &spec.controller)?;
    if let Some(backup) = &spec.backup_controller {
        validate_controller("backupController", backup)?;
        if backup.node_id == spec.controller.node_id {
            return Err("backupController.nodeId must differ from controller.nodeId".into());
        }
    }
    if spec.compute.is_empty() {
        return Err("at least one compute node is required".into());
    }

    let mut partitions = HashSet::new();
    for partition in &spec.partitions {
        validate_token("partition name", &partition.name)?;
        if !partitions.insert(partition.name.clone()) {
            return Err(format!("duplicate partition '{}'", partition.name));
        }
        if !partition.max_time.trim().is_empty() {
            validate_token("partition maxTime", &partition.max_time)?;
        }
        if !partition.state.trim().is_empty()
            && partition.state != "UP"
            && partition.state != "DOWN"
            && partition.state != "DRAIN"
        {
            return Err(format!(
                "partition '{}' state must be UP, DOWN, or DRAIN",
                partition.name
            ));
        }
    }
    if partitions.is_empty() {
        return Err("at least one partition is required".into());
    }

    let mut node_ids = HashSet::new();
    for compute in &spec.compute {
        validate_token("compute nodeId", &compute.node_id)?;
        validate_token("compute address", &compute.address)?;
        validate_token("compute iface", &compute.iface)?;
        if !node_ids.insert(compute.node_id.clone()) {
            return Err(format!("duplicate node '{}'", compute.node_id));
        }
        if compute.partitions.is_empty() {
            return Err(format!(
                "compute node '{}' needs at least one partition",
                compute.node_id
            ));
        }
        for partition in &compute.partitions {
            if !partitions.contains(partition) {
                return Err(format!(
                    "compute node '{}' references unknown partition '{}'",
                    compute.node_id, partition
                ));
            }
        }
        if compute.reserve.cores < 0 || compute.reserve.memory_mb < 0 {
            return Err(format!(
                "compute node '{}' reserve values cannot be negative",
                compute.node_id
            ));
        }
        if compute.gres != "none" && compute.gres != "auto" {
            return Err(format!(
                "compute node '{}' gres must be none or auto",
                compute.node_id
            ));
        }
    }

    let mut users = HashSet::new();
    let mut uids = HashSet::new();
    for user in &spec.users {
        validate_token("user name", &user.name)?;
        if !users.insert(user.name.clone()) {
            return Err(format!("duplicate user '{}'", user.name));
        }
        if user.uid == 0 || !uids.insert(user.uid) {
            return Err(format!(
                "user '{}' has a duplicate or reserved uid",
                user.name
            ));
        }
    }

    match spec.storage.mode.as_str() {
        "nfs" => {
            if spec.storage.exports.is_empty() {
                return Err("storage.exports cannot be empty in nfs mode".into());
            }
            for path in &spec.storage.exports {
                if !path.starts_with('/') {
                    return Err(format!("storage export '{path}' must be absolute"));
                }
            }
        }
        "shared-filesystem" => {
            if spec.storage.filesystem.trim().is_empty() {
                return Err("storage.filesystem is required in shared-filesystem mode".into());
            }
            if spec.storage.exports.is_empty() {
                return Err("storage.exports cannot be empty".into());
            }
        }
        "none" => {}
        other => {
            return Err(format!(
                "storage.mode must be nfs, shared-filesystem, or none (got {other})"
            ))
        }
    }

    if spec.accounting.enabled && !spec.accounting.node_id.trim().is_empty() {
        validate_token("accounting.nodeId", &spec.accounting.node_id)?;
    }
    Ok(())
}

fn validate_controller(field: &str, controller: &SlurmControllerSpec) -> Result<(), String> {
    validate_token(&format!("{field}.nodeId"), &controller.node_id)?;
    validate_token(&format!("{field}.address"), &controller.address)?;
    validate_token(&format!("{field}.iface"), &controller.iface)
}

fn validate_token(field: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} is required"));
    }
    if value.len() > 253
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/'))
    {
        return Err(format!("{field} contains unsupported characters: {value}"));
    }
    Ok(())
}

/// Resolve every node that carries at least one Slurm role. Login nodes that
/// are not also controller/compute members get their address from `lookup`.
pub fn members(
    spec: &SlurmClusterSpec,
    lookup: impl Fn(&str) -> Option<(String, String)>,
) -> BTreeMap<String, Member> {
    let mut out = BTreeMap::new();
    let controller = &spec.controller;
    out.insert(
        controller.node_id.clone(),
        Member {
            node_id: controller.node_id.clone(),
            address: controller.address.clone(),
            iface: controller.iface.clone(),
            roles: vec!["controller".into()],
            compute: None,
        },
    );
    if let Some(backup) = &spec.backup_controller {
        out.insert(
            backup.node_id.clone(),
            Member {
                node_id: backup.node_id.clone(),
                address: backup.address.clone(),
                iface: backup.iface.clone(),
                roles: vec!["controller".into()],
                compute: None,
            },
        );
    }
    for node_id in &spec.login.node_ids {
        let entry = out.entry(node_id.clone()).or_insert_with(|| {
            let (address, iface) =
                lookup(node_id).unwrap_or_else(|| (String::new(), controller.iface.clone()));
            Member {
                node_id: node_id.clone(),
                address,
                iface,
                roles: Vec::new(),
                compute: None,
            }
        });
        if !entry.has_role("login") {
            entry.roles.push("login".into());
        }
    }
    for compute in &spec.compute {
        let entry = out
            .entry(compute.node_id.clone())
            .or_insert_with(|| Member {
                node_id: compute.node_id.clone(),
                address: compute.address.clone(),
                iface: compute.iface.clone(),
                roles: Vec::new(),
                compute: None,
            });
        entry.address = compute.address.clone();
        entry.iface = compute.iface.clone();
        entry.compute = Some(compute.clone());
        if !entry.has_role("compute") {
            entry.roles.push("compute".into());
        }
    }
    if spec.accounting.enabled {
        let dbd_id = if spec.accounting.node_id.trim().is_empty() {
            controller.node_id.clone()
        } else {
            spec.accounting.node_id.clone()
        };
        let entry = out.entry(dbd_id.clone()).or_insert_with(|| Member {
            node_id: dbd_id.clone(),
            address: controller.address.clone(),
            iface: controller.iface.clone(),
            roles: Vec::new(),
            compute: None,
        });
        if !entry.has_role("dbd") {
            entry.roles.push("dbd".into());
        }
    }
    out
}

/// A controller host may not also be a compute member unless the operator
/// explicitly opts in.
pub fn controller_host_conflict(
    spec: &SlurmClusterSpec,
    self_node_id: Option<&str>,
    self_hostname: Option<&str>,
    self_addresses: &[String],
) -> Option<String> {
    if spec.allow_controller_host {
        return None;
    }
    for compute in &spec.compute {
        let id_match = self_node_id.is_some_and(|id| id == compute.node_id);
        let host_match = self_hostname.is_some_and(|host| {
            !host.is_empty() && (host == compute.node_id || host == compute.address)
        });
        let address_match = self_addresses
            .iter()
            .any(|addr| !addr.is_empty() && (addr == &compute.address || addr == &compute.node_id));
        if id_match || host_match || address_match {
            return Some(format!(
                "compute node '{}' is the kcore controller host; set allowControllerHost: true to override",
                compute.node_id
            ));
        }
    }
    None
}

pub fn spec_to_json(spec: &SlurmClusterSpec) -> Result<String, serde_json::Error> {
    serde_json::to_string(spec)
}

pub fn spec_from_json(raw: &str) -> Result<SlurmClusterSpec, serde_json::Error> {
    serde_json::from_str(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SlurmClusterSpec {
        SlurmClusterSpec {
            cluster_name: "hpc-a".into(),
            controller: SlurmControllerSpec {
                node_id: "node-1".into(),
                address: "10.0.0.1".into(),
                iface: "enp6s0".into(),
            },
            backup_controller: None,
            login: SlurmLoginSpec {
                node_ids: vec!["node-1".into()],
            },
            accounting: SlurmAccountingSpec::default(),
            compute: vec![SlurmComputeSpec {
                node_id: "node-2".into(),
                address: "10.0.0.2".into(),
                iface: "enp6s0".into(),
                partitions: vec!["batch".into()],
                reserve: SlurmReserveSpec {
                    cores: 2,
                    memory_mb: 4096,
                },
                gres: "none".into(),
            }],
            partitions: vec![SlurmPartitionSpec {
                name: "batch".into(),
                default: true,
                max_time: "INFINITE".into(),
                state: "UP".into(),
            }],
            users: vec![SlurmUserSpec {
                name: "alice".into(),
                uid: 2001,
                ssh_keys: vec!["ssh-ed25519 AAAA".into()],
            }],
            storage: SlurmStorageSpec::default(),
            extra_config: String::new(),
            allow_controller_host: false,
            gpu_driver: false,
        }
    }

    #[test]
    fn validates_and_round_trips() {
        let spec = spec();
        validate_spec(&spec).unwrap();
        let json = spec_to_json(&spec).unwrap();
        assert!(json.contains("\"clusterName\":\"hpc-a\""));
        assert_eq!(spec_from_json(&json).unwrap(), spec);
    }

    #[test]
    fn immutable_fields_replace() {
        let mut next = spec();
        next.cluster_name = "hpc-b".into();
        assert_eq!(plan_action(Some(&spec()), &next).0, "replace");
        let mut next = spec();
        next.controller.node_id = "node-9".into();
        assert_eq!(plan_action(Some(&spec()), &next).0, "replace");
        let mut next = spec();
        next.compute.push(SlurmComputeSpec {
            node_id: "node-3".into(),
            address: "10.0.0.3".into(),
            iface: "enp6s0".into(),
            partitions: vec!["batch".into()],
            reserve: SlurmReserveSpec::default(),
            gres: "none".into(),
        });
        assert_eq!(plan_action(Some(&spec()), &next).0, "update");
        assert_eq!(plan_action(Some(&spec()), &spec()).0, "unchanged");
        assert_eq!(plan_action(None, &spec()).0, "create");
    }

    #[test]
    fn members_merge_roles() {
        let mut spec = spec();
        spec.login.node_ids = vec!["node-1".into(), "node-2".into()];
        spec.accounting = SlurmAccountingSpec {
            enabled: true,
            node_id: String::new(),
        };
        let members = members(&spec, |_| None);
        assert!(members["node-1"].has_role("controller"));
        assert!(members["node-1"].has_role("dbd"));
        assert!(members["node-1"].has_role("login"));
        assert!(members["node-2"].has_role("compute"));
        assert!(members["node-2"].has_role("login"));
    }
}
