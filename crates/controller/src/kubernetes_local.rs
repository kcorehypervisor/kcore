//! Local-path provisioner declared on a Kubernetes cluster.
//!
//! Each volume is an LVM logical volume or a ZFS zvol on one cluster node.
//! The rendered manifest is a Rancher local-path provisioner whose node paths
//! are those mount points, so a PersistentVolumeClaim becomes a directory on
//! the disk that holds the volume.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalVolume {
    pub name: String,
    pub node_id: String,
    pub storage_class: String,
    pub size_bytes: i64,
    pub fs_type: String,
    pub mount_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProvisioner {
    pub enabled: bool,
    #[serde(default)]
    pub storage_class_name: String,
    #[serde(default)]
    pub volumes: Vec<LocalVolume>,
}

impl LocalProvisioner {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            storage_class_name: String::new(),
            volumes: Vec::new(),
        }
    }
}

pub struct VolumeDecl<'a> {
    pub name: &'a str,
    pub node_id: &'a str,
    pub storage_class: &'a str,
    pub size_bytes: i64,
    pub fs_type: &'a str,
    pub mount_path: &'a str,
}

pub fn normalize(
    cluster_name: &str,
    members: &BTreeSet<String>,
    enabled: bool,
    storage_class_name: &str,
    volumes: &[VolumeDecl<'_>],
) -> Result<LocalProvisioner, String> {
    if !enabled {
        if !volumes.is_empty() {
            return Err("local provisioner volumes require local_provisioner.enabled".to_string());
        }
        return Ok(LocalProvisioner::disabled());
    }
    if volumes.is_empty() {
        return Err("local provisioner requires at least one lvm or zfs volume".to_string());
    }
    let storage_class_name = {
        let name = storage_class_name.trim();
        if name.is_empty() {
            "local-path".to_string()
        } else {
            dns_label(name)?;
            name.to_string()
        }
    };
    let mut seen_names = BTreeSet::new();
    let mut seen_mounts = BTreeSet::new();
    let mut normalized = Vec::with_capacity(volumes.len());
    for volume in volumes {
        let name = volume.name.trim();
        let node_id = volume.node_id.trim();
        if !component_ok(name) {
            return Err(format!(
                "volume name '{name}' must be a single path segment without '.' or '..'"
            ));
        }
        if !seen_names.insert((node_id.to_string(), name.to_string())) {
            return Err(format!("volume '{name}' is declared twice on {node_id}"));
        }
        if !members.contains(node_id) {
            return Err(format!(
                "volume '{name}' is on {node_id}, which is not a member of this cluster"
            ));
        }
        let storage_class = volume.storage_class.trim();
        if storage_class != "lvm" && storage_class != "zfs" {
            return Err(format!(
                "volume '{name}' storage_class must be lvm or zfs, got '{storage_class}'"
            ));
        }
        if volume.size_bytes <= 0 {
            return Err(format!("volume '{name}' size_bytes must be positive"));
        }
        let fs_type = {
            let fs = volume.fs_type.trim();
            if fs.is_empty() {
                "ext4".to_string()
            } else if fs == "ext4" || fs == "xfs" {
                fs.to_string()
            } else {
                return Err(format!(
                    "volume '{name}' fs_type must be ext4 or xfs, got '{fs}'"
                ));
            }
        };
        let mount_path = {
            let requested = volume.mount_path.trim();
            if requested.is_empty() {
                format!(
                    "/mnt/kcore/{}/{}",
                    path_segment(cluster_name),
                    path_segment(name)
                )
            } else {
                validate_mount_path(requested)?;
                requested.to_string()
            }
        };
        if !seen_mounts.insert(mount_path.clone()) {
            return Err(format!(
                "mount path {mount_path} is used by more than one volume"
            ));
        }
        normalized.push(LocalVolume {
            name: name.to_string(),
            node_id: node_id.to_string(),
            storage_class: storage_class.to_string(),
            size_bytes: volume.size_bytes,
            fs_type,
            mount_path,
        });
    }
    Ok(LocalProvisioner {
        enabled: true,
        storage_class_name,
        volumes: normalized,
    })
}

pub fn encode(provisioner: &LocalProvisioner) -> String {
    if !provisioner.enabled {
        return String::new();
    }
    serde_json::to_string(provisioner).unwrap_or_default()
}

pub fn decode(json: &str) -> Result<LocalProvisioner, String> {
    let json = json.trim();
    if json.is_empty() {
        return Ok(LocalProvisioner::disabled());
    }
    serde_json::from_str(json).map_err(|e| format!("local provisioner config: {e}"))
}

/// Controller volume name for a cluster-declared host volume.
pub fn controller_volume_name(cluster: &str, volume: &str) -> String {
    format!("k8s-{}-{}", path_segment(cluster), path_segment(volume))
}

pub fn manifests(provisioner: &LocalProvisioner) -> String {
    if !provisioner.enabled {
        return String::new();
    }
    let mut by_node: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for volume in &provisioner.volumes {
        by_node
            .entry(volume.node_id.as_str())
            .or_default()
            .push(volume.mount_path.as_str());
    }
    let mut node_paths = String::new();
    for (index, (node, paths)) in by_node.iter().enumerate() {
        if index > 0 {
            node_paths.push_str(",\n");
        }
        let joined = paths
            .iter()
            .map(|path| format!("\"{path}\""))
            .collect::<Vec<_>>()
            .join(", ");
        node_paths.push_str(&format!(
            "        {{\"node\":\"{node}\",\"paths\":[{joined}]}}"
        ));
    }
    format!(
        r#"apiVersion: v1
kind: Namespace
metadata:
  name: local-path-storage
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: local-path-provisioner-service-account
  namespace: local-path-storage
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: local-path-provisioner-role
rules:
  - apiGroups: [""]
    resources: ["nodes", "persistentvolumeclaims", "configmaps", "pods", "pods/log"]
    verbs: ["get", "list", "watch"]
  - apiGroups: [""]
    resources: ["persistentvolumes"]
    verbs: ["get", "list", "watch", "create", "patch", "update", "delete"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["create", "patch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["storageclasses"]
    verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: local-path-provisioner-bind
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: local-path-provisioner-role
subjects:
  - kind: ServiceAccount
    name: local-path-provisioner-service-account
    namespace: local-path-storage
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: local-path-config
  namespace: local-path-storage
data:
  config.json: |
    {{
      "nodePathMap": [
{node_paths}
      ]
    }}
  setup: |
    #!/bin/sh
    set -eu
    mkdir -m 0777 -p "$VOL_DIR"
  teardown: |
    #!/bin/sh
    set -eu
    rm -rf "$VOL_DIR"
  helperPod.yaml: |
    apiVersion: v1
    kind: Pod
    metadata:
      name: helper-pod
    spec:
      containers:
        - name: helper-pod
          image: busybox:1.36
          command: ["/bin/sh"]
          args: ["-c", "sleep 3600"]
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: {class_name}
provisioner: rancher.io/local-path
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Delete
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: local-path-provisioner
  namespace: local-path-storage
spec:
  replicas: 1
  selector:
    matchLabels:
      app: local-path-provisioner
  template:
    metadata:
      labels:
        app: local-path-provisioner
    spec:
      serviceAccountName: local-path-provisioner-service-account
      containers:
        - name: local-path-provisioner
          image: rancher/local-path-provisioner:v0.0.30
          imagePullPolicy: IfNotPresent
          command:
            - local-path-provisioner
            - start
            - --config
            - /etc/config/config.json
          env:
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
            - name: CONFIG_MOUNT_PATH
              value: /etc/config
          volumeMounts:
            - name: config-volume
              mountPath: /etc/config/
      volumes:
        - name: config-volume
          configMap:
            name: local-path-config
"#,
        class_name = provisioner.storage_class_name
    )
}

fn dns_label(name: &str) -> Result<(), String> {
    let ok = name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !name.ends_with('-')
        && !name.contains("--");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "storage class name '{name}' must be a lowercase DNS label"
        ))
    }
}

fn component_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && value != "."
        && value != ".."
}

fn path_segment(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "cluster".to_string()
    } else {
        cleaned
    }
}

fn validate_mount_path(path: &str) -> Result<(), String> {
    let under_kcore = path.starts_with("/mnt/kcore/") || path.starts_with("/var/lib/kcore/");
    if !under_kcore || path.contains('\0') || path.split('/').any(|part| part == "..") {
        return Err(format!(
            "mount path {path} must be a directory under /mnt/kcore or /var/lib/kcore"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members() -> BTreeSet<String> {
        ["node-105", "node-107", "node-119"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn volume<'a>(name: &'a str, node: &'a str, class: &'a str, size: i64) -> VolumeDecl<'a> {
        VolumeDecl {
            name,
            node_id: node,
            storage_class: class,
            size_bytes: size,
            fs_type: "",
            mount_path: "",
        }
    }

    #[test]
    fn defaults_ext4_and_a_mount_under_mnt_kcore() {
        let provisioner = normalize(
            "lab",
            &members(),
            true,
            "",
            &[volume(
                "kcore-kdata",
                "node-105",
                "lvm",
                50 * 1024 * 1024 * 1024,
            )],
        )
        .unwrap();
        assert_eq!(provisioner.storage_class_name, "local-path");
        assert_eq!(provisioner.volumes[0].fs_type, "ext4");
        assert_eq!(
            provisioner.volumes[0].mount_path,
            "/mnt/kcore/lab/kcore-kdata"
        );
        let yaml = manifests(&provisioner);
        assert!(yaml.contains("kind: StorageClass"));
        assert!(yaml.contains("name: local-path"));
        assert!(yaml.contains("rancher.io/local-path"));
        assert!(yaml.contains("/mnt/kcore/lab/kcore-kdata"));
        assert!(yaml.contains("node-105"));
        assert_eq!(
            controller_volume_name("lab", "kcore-kdata"),
            "k8s-lab-kcore-kdata"
        );
    }

    #[test]
    fn groups_paths_for_one_node_and_keeps_zfs() {
        let mut zfs = volume("data", "node-119", "zfs", 1024);
        zfs.fs_type = "xfs";
        zfs.mount_path = "/var/lib/kcore/lab-data";
        let provisioner = normalize(
            "lab",
            &members(),
            true,
            "local-path",
            &[
                volume("kcore-kdata", "node-105", "lvm", 1024),
                volume("extra", "node-105", "lvm", 2048),
                zfs,
            ],
        )
        .unwrap();
        let yaml = manifests(&provisioner);
        assert!(yaml.contains("\"/mnt/kcore/lab/kcore-kdata\", \"/mnt/kcore/lab/extra\""));
        assert!(yaml.contains("/var/lib/kcore/lab-data"));
        let stored = decode(&encode(&provisioner)).unwrap();
        assert_eq!(stored, provisioner);
    }

    #[test]
    fn rejects_a_volume_outside_the_cluster_or_a_bad_class() {
        let err = normalize(
            "lab",
            &members(),
            true,
            "local-path",
            &[volume("kdata", "node-103", "lvm", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("not a member"));
        let err = normalize(
            "lab",
            &members(),
            true,
            "local-path",
            &[volume("kdata", "node-107", "ceph", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("lvm or zfs"));
        let err = normalize(
            "lab",
            &members(),
            false,
            "",
            &[volume("kdata", "node-107", "lvm", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("enabled"));
        assert!(
            !normalize("lab", &members(), false, "", &[])
                .unwrap()
                .enabled
        );
    }
}
