//! Host storage for PersistentVolumeClaims on a Kubernetes cluster.
//!
//! `shared` mode formats and mounts one LVM logical volume or ZFS zvol. The
//! rendered manifest is a Rancher local-path provisioner, so many claims
//! become directories on that one filesystem.
//!
//! `lvm` mode creates an LVM thin pool of the declared size and renders no
//! provisioner. kcore does not install a CSI driver for that pool.

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
    /// "filesystem" or "pool".
    pub role: String,
    pub volume_group: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProvisioner {
    pub enabled: bool,
    #[serde(default)]
    pub storage_class_name: String,
    #[serde(default)]
    pub volumes: Vec<LocalVolume>,
    /// "shared" or "lvm".
    #[serde(default)]
    pub mode: String,
}

impl LocalProvisioner {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            storage_class_name: String::new(),
            volumes: Vec::new(),
            mode: String::new(),
        }
    }

    pub fn is_lvm(&self) -> bool {
        self.mode == "lvm"
    }
}

pub struct VolumeDecl<'a> {
    pub name: &'a str,
    pub node_id: &'a str,
    pub storage_class: &'a str,
    pub size_bytes: i64,
    pub fs_type: &'a str,
    pub mount_path: &'a str,
    pub role: &'a str,
    pub volume_group: &'a str,
}

pub fn normalize(
    cluster_name: &str,
    members: &BTreeSet<String>,
    enabled: bool,
    storage_class_name: &str,
    mode: &str,
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
    let mode = match mode.trim() {
        "" | "shared" => "shared".to_string(),
        "lvm" => "lvm".to_string(),
        other => {
            return Err(format!(
                "local provisioner mode must be shared or lvm, got '{other}'"
            ));
        }
    };
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
        let role = match (mode.as_str(), volume.role.trim()) {
            ("lvm", "" | "pool") => "pool",
            ("shared", "" | "filesystem") => "filesystem",
            ("lvm", other) => {
                return Err(format!(
                    "volume '{name}' role must be pool in lvm mode, got '{other}'"
                ));
            }
            ("shared", other) => {
                return Err(format!(
                    "volume '{name}' role must be filesystem in shared mode, got '{other}'"
                ));
            }
            _ => unreachable!(),
        };
        if role == "pool" {
            if storage_class != "lvm" {
                return Err(format!(
                    "volume '{name}' pool must use storage_class lvm, got '{storage_class}'"
                ));
            }
            validate_volume_group(volume.volume_group.trim())
                .map_err(|err| format!("volume '{name}' volume_group: {err}"))?;
            if !volume.mount_path.trim().is_empty() {
                return Err(format!(
                    "volume '{name}' is a thin pool and is not mounted; leave mount_path empty"
                ));
            }
        }
        let mount_path = if role == "pool" {
            String::new()
        } else {
            let requested = volume.mount_path.trim();
            let mount_path = if requested.is_empty() {
                format!(
                    "/mnt/kcore/{}/{}",
                    path_segment(cluster_name),
                    path_segment(name)
                )
            } else {
                validate_mount_path(requested)?;
                requested.to_string()
            };
            if !seen_mounts.insert((node_id.to_string(), mount_path.clone())) {
                return Err(format!(
                    "mount path {mount_path} is used by more than one volume on {node_id}"
                ));
            }
            mount_path
        };
        normalized.push(LocalVolume {
            name: name.to_string(),
            node_id: node_id.to_string(),
            storage_class: storage_class.to_string(),
            size_bytes: volume.size_bytes,
            fs_type,
            mount_path,
            role: role.to_string(),
            volume_group: volume.volume_group.trim().to_string(),
        });
    }
    if mode == "lvm" {
        let first = &normalized[0];
        for volume in &normalized[1..] {
            if volume.name != first.name
                || volume.volume_group != first.volume_group
                || volume.size_bytes != first.size_bytes
                || volume.fs_type != first.fs_type
            {
                return Err(format!(
                    "lvm mode uses one thin pool on every node; {} on {} does not match {} on {}",
                    volume.name, volume.node_id, first.name, first.node_id
                ));
            }
        }
    }
    Ok(LocalProvisioner {
        enabled: true,
        storage_class_name,
        volumes: normalized,
        mode,
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
pub fn controller_volume_name(cluster: &str, node: &str, volume: &str) -> String {
    format!(
        "k8s-{}-{}-{}",
        path_segment(cluster),
        path_segment(node),
        path_segment(volume)
    )
}

pub fn manifests(provisioner: &LocalProvisioner) -> String {
    manifests_at(provisioner, "")
}

/// `api_server_endpoint` is `host:port` of this cluster's API. An extra
/// cluster has no kube-proxy: Cilium on the primary host already owns
/// service dispatch. The provisioner then uses the host network and that
/// endpoint instead of the ClusterIP, which nothing on the bridge answers.
pub fn manifests_at(provisioner: &LocalProvisioner, api_server_endpoint: &str) -> String {
    if !provisioner.enabled {
        return String::new();
    }
    if provisioner.is_lvm() {
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
    let (host_net, api_env) = provisioner_api_access(api_server_endpoint);
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
parameters:
  kcore.io/layout: shared-filesystem
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
{host_net}      serviceAccountName: local-path-provisioner-service-account
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
{api_env}            - name: POD_NAMESPACE
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
        class_name = provisioner.storage_class_name,
        host_net = host_net,
        api_env = api_env,
    )
}

fn provisioner_api_access(endpoint: &str) -> (String, String) {
    let Some((host, port)) = endpoint.trim().rsplit_once(':') else {
        return (String::new(), String::new());
    };
    if host.is_empty()
        || !port.chars().all(|c| c.is_ascii_digit())
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return (String::new(), String::new());
    }
    (
        "      hostNetwork: true\n      dnsPolicy: ClusterFirstWithHostNet\n".to_string(),
        format!(
            "            - name: KUBERNETES_SERVICE_HOST\n              value: \"{host}\"\n            - name: KUBERNETES_SERVICE_PORT\n              value: \"{port}\"\n"
        ),
    )
}

fn validate_volume_group(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
        && !name.contains("..");
    if ok {
        Ok(())
    } else {
        Err(format!(
            "'{name}' must be letters, digits, '_', '.', '+', or '-'"
        ))
    }
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
            role: "",
            volume_group: "",
        }
    }

    fn pool<'a>(name: &'a str, node: &'a str, size: i64) -> VolumeDecl<'a> {
        VolumeDecl {
            name,
            node_id: node,
            storage_class: "lvm",
            size_bytes: size,
            fs_type: "",
            mount_path: "",
            role: "pool",
            volume_group: "vg_kcore",
        }
    }

    #[test]
    fn defaults_ext4_and_a_mount_under_mnt_kcore() {
        let provisioner = normalize(
            "lab",
            &members(),
            true,
            "",
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
        assert!(!yaml.contains("hostNetwork"));
        let reached = manifests_at(&provisioner, "192.168.40.201:7443");
        assert!(reached.contains("hostNetwork: true"));
        assert!(reached.contains("value: \"192.168.40.201\""));
        assert!(reached.contains("value: \"7443\""));
        assert!(!manifests_at(&provisioner, "not a url").contains("hostNetwork"));
        assert!(yaml.contains("kind: StorageClass"));
        assert!(yaml.contains("name: local-path"));
        assert!(yaml.contains("rancher.io/local-path"));
        assert!(yaml.contains("/mnt/kcore/lab/kcore-kdata"));
        assert!(yaml.contains("node-105"));
        assert_eq!(
            controller_volume_name("lab", "node-105", "kcore-kdata"),
            "k8s-lab-node-105-kcore-kdata"
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
            "",
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
            "",
            &[volume("kdata", "node-103", "lvm", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("not a member"));
        let err = normalize(
            "lab",
            &members(),
            true,
            "local-path",
            "",
            &[volume("kdata", "node-107", "ceph", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("lvm or zfs"));
        let err = normalize(
            "lab",
            &members(),
            false,
            "",
            "",
            &[volume("kdata", "node-107", "lvm", 1024)],
        )
        .unwrap_err();
        assert!(err.contains("enabled"));
        assert!(
            !normalize("lab", &members(), false, "", "", &[])
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn lvm_mode_creates_one_thin_pool_and_installs_no_provisioner() {
        let size = 50 * 1024 * 1024 * 1024;
        let provisioner = normalize(
            "lab",
            &members(),
            true,
            "kcore-lvm",
            "lvm",
            &[
                pool("kcore-kdata", "node-105", size),
                pool("kcore-kdata", "node-107", size),
                pool("kcore-kdata", "node-119", size),
            ],
        )
        .unwrap();
        assert_eq!(provisioner.mode, "lvm");
        assert!(provisioner
            .volumes
            .iter()
            .all(|volume| volume.role == "pool"));
        assert!(provisioner
            .volumes
            .iter()
            .all(|volume| volume.mount_path.is_empty()));
        assert!(provisioner
            .volumes
            .iter()
            .all(|volume| volume.fs_type == "ext4"));
        let yaml = manifests(&provisioner);
        assert!(yaml.is_empty());
        let stored = decode(&encode(&provisioner)).unwrap();
        assert_eq!(stored, provisioner);

        let mismatch = normalize(
            "lab",
            &members(),
            true,
            "kcore-lvm",
            "lvm",
            &[
                pool("kcore-kdata", "node-105", size),
                pool("other", "node-107", size),
            ],
        )
        .unwrap_err();
        assert!(mismatch.contains("one thin pool"));
        let mut mounted = pool("kcore-kdata", "node-105", size);
        mounted.mount_path = "/mnt/kcore/lab/kcore-kdata";
        let err = normalize("lab", &members(), true, "kcore-lvm", "lvm", &[mounted]).unwrap_err();
        assert!(err.contains("not mounted"));
    }
}
