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
    /// When set, install Ceph CSI (RBD, and CephFS when `ceph_filesystem` is set).
    #[serde(default)]
    pub ceph_cluster: String,
    /// Resolved from the CephCluster when the spec is applied, so rendering
    /// needs no database.
    #[serde(default)]
    pub ceph_monitors: Vec<String>,
    #[serde(default)]
    pub ceph_pool: String,
    /// CephFS filesystem name for CSI (`fsName`). Empty skips CephFS manifests.
    /// Resolved from a SharedFilesystem on the same CephCluster when applied.
    #[serde(default)]
    pub ceph_filesystem: String,
}

impl LocalProvisioner {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            storage_class_name: String::new(),
            volumes: Vec::new(),
            mode: String::new(),
            ceph_cluster: String::new(),
            ceph_monitors: Vec::new(),
            ceph_pool: String::new(),
            ceph_filesystem: String::new(),
        }
    }

    /// A provisioner that is off but still names a CephCluster: only Ceph CSI
    /// manifests are rendered (RBD, and CephFS when a filesystem name is set).
    pub fn ceph_only(ceph_cluster: &str) -> Self {
        Self {
            ceph_cluster: ceph_cluster.trim().to_string(),
            ..Self::disabled()
        }
    }

    pub fn has_ceph(&self) -> bool {
        !self.ceph_cluster.is_empty()
    }

    pub fn has_cephfs(&self) -> bool {
        self.has_ceph() && !self.ceph_filesystem.trim().is_empty()
    }

    /// True when `manifests_at` renders anything.
    pub fn renders_manifests(&self) -> bool {
        (self.enabled && !self.is_lvm()) || self.has_ceph()
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
        // Enabled with no host volumes is only valid when a Ceph CSI class
        // will be attached afterwards (ceph_cluster set by the caller).
        return Ok(LocalProvisioner {
            enabled: true,
            storage_class_name: {
                let name = storage_class_name.trim();
                if name.is_empty() {
                    "local-path".to_string()
                } else {
                    dns_label(name)?;
                    name.to_string()
                }
            },
            volumes: Vec::new(),
            mode: String::new(),
            ceph_cluster: String::new(),
            ceph_monitors: Vec::new(),
            ceph_pool: String::new(),
            ceph_filesystem: String::new(),
        });
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
        ceph_cluster: String::new(),
        ceph_monitors: Vec::new(),
        ceph_pool: String::new(),
        ceph_filesystem: String::new(),
    })
}

/// Monitor addresses and RBD pool a Kubernetes cluster's CSI StorageClass
/// uses for this CephCluster.
pub fn ceph_endpoints(spec: &crate::controller_proto::CephClusterSpec) -> (Vec<String>, String) {
    if let Some(ext) = crate::ceph_cluster_spec::active_external(spec) {
        let monitors = ext
            .mon_hosts
            .split(',')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect();
        return (monitors, ext.pool.trim().to_string());
    }
    let monitors = spec
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                crate::ceph_cluster_spec::normalize_role(&n.role).as_str(),
                "full" | "mon"
            )
        })
        .filter_map(|n| {
            let ip = n.mon_addr.split(':').next().unwrap_or("").trim();
            (!ip.is_empty()).then(|| format!("{ip}:6789"))
        })
        .collect();
    (monitors, "kcore-vms".to_string())
}

pub fn encode(provisioner: &LocalProvisioner) -> String {
    if !provisioner.enabled && !provisioner.has_ceph() {
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
    let mut out = local_path_manifests(provisioner, api_server_endpoint);
    if let Some(rbd) = rbd_manifests(provisioner) {
        if !out.is_empty() {
            out.push_str("---\n");
        }
        out.push_str(&rbd);
    }
    if let Some(cephfs) = cephfs_manifests(provisioner, &provisioner.ceph_filesystem) {
        if !out.is_empty() {
            out.push_str("---\n");
        }
        out.push_str(&cephfs);
    }
    out
}

/// Name of the StorageClass that provisions RBD images.
pub const RBD_STORAGE_CLASS: &str = "kcore-rbd";
/// Name of the StorageClass that provisions CephFS subvolumes.
pub const CEPHFS_STORAGE_CLASS: &str = "kcore-cephfs";
pub const RBD_NAMESPACE: &str = "ceph-csi";
pub const CEPH_CSI_NAMESPACE: &str = RBD_NAMESPACE;

/// Pinned Ceph CSI plugin image (RBD and CephFS).
pub const CEPH_CSI_IMAGE: &str = "quay.io/cephcsi/cephcsi:v3.13.1";

/// JSON body for `csi-rbd-secret` / `csi-cephfs-secret` (applied via kube API, never Nix).
pub fn csi_user_secret_json(name: &str, namespace: &str, user_id: &str, user_key: &str) -> String {
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": name, "namespace": namespace },
        "type": "Opaque",
        "stringData": {
            "userID": user_id,
            "userKey": user_key,
        }
    })
    .to_string()
}

/// Extract the `key = …` value from a cephx keyring body.
pub fn cephx_key_from_keyring(keyring: &str) -> Option<String> {
    for line in keyring.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("key") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let key = rest.trim();
                if !key.is_empty() {
                    return Some(key.to_string());
                }
            }
        }
    }
    None
}
const CSI_PROVISIONER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-provisioner:v5.0.1";
const CSI_ATTACHER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-attacher:v4.6.1";
const CSI_RESIZER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-resizer:v1.11.1";
const CSI_REGISTRAR_IMAGE: &str = "registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.11.1";

/// Ceph CSI Secrets (`csi-rbd-secret` / `csi-cephfs-secret`) are **not** rendered
/// into these manifests: a cephx key must never land in the world-readable Nix
/// store. The controller applies those Secrets via the kube API separately.
///
/// RBD CSI: Namespace, ConfigMaps, CSIDriver, StorageClass, RBAC, provisioner
/// Deployment (`csi-rbdplugin-provisioner`), and node DaemonSet (`csi-rbdplugin`).
pub fn rbd_manifests(provisioner: &LocalProvisioner) -> Option<String> {
    if !provisioner.has_ceph() || provisioner.ceph_monitors.is_empty() {
        return None;
    }
    let cluster = &provisioner.ceph_cluster;
    let monitors = provisioner
        .ceph_monitors
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let pool = if provisioner.ceph_pool.is_empty() {
        "kcore-vms"
    } else {
        provisioner.ceph_pool.as_str()
    };
    let ns = CEPH_CSI_NAMESPACE;
    Some(format!(
        r#"apiVersion: v1
kind: Namespace
metadata:
  name: {ns}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: ceph-csi-config
  namespace: {ns}
data:
  config.json: |-
    [{{"clusterID": "{cluster}", "monitors": [{monitors}]}}]
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: ceph-config
  namespace: {ns}
data:
  ceph.conf: |
    [global]
    auth_cluster_required = cephx
    auth_service_required = cephx
    auth_client_required = cephx
  keyring: |
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: rbd-csi-provisioner
  namespace: {ns}
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: rbd-csi-nodeplugin
  namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rbd-external-provisioner-runner
rules:
  - apiGroups: [""]
    resources: ["nodes", "secrets", "configmaps", "serviceaccounts"]
    verbs: ["get", "list", "watch"]
  - apiGroups: [""]
    resources: ["serviceaccounts/token"]
    verbs: ["create"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["list", "watch", "create", "update", "patch"]
  - apiGroups: [""]
    resources: ["persistentvolumes"]
    verbs: ["get", "list", "watch", "create", "update", "delete", "patch"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims"]
    verbs: ["get", "list", "watch", "update"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims/status"]
    verbs: ["update", "patch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["storageclasses", "csinodes"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["volumeattachments"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["volumeattachments/status"]
    verbs: ["patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: rbd-csi-provisioner-role
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: rbd-external-provisioner-runner
subjects:
  - kind: ServiceAccount
    name: rbd-csi-provisioner
    namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: rbd-external-provisioner-cfg
  namespace: {ns}
rules:
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["get", "list", "watch", "create", "update", "delete"]
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["get", "watch", "list", "delete", "update", "create"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: rbd-csi-provisioner-role-cfg
  namespace: {ns}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: rbd-external-provisioner-cfg
subjects:
  - kind: ServiceAccount
    name: rbd-csi-provisioner
    namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: rbd-csi-nodeplugin
rules:
  - apiGroups: [""]
    resources: ["nodes", "secrets", "configmaps", "serviceaccounts", "persistentvolumes"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["serviceaccounts/token"]
    verbs: ["create"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["volumeattachments"]
    verbs: ["get", "list"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: rbd-csi-nodeplugin
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: rbd-csi-nodeplugin
subjects:
  - kind: ServiceAccount
    name: rbd-csi-nodeplugin
    namespace: {ns}
---
apiVersion: storage.k8s.io/v1
kind: CSIDriver
metadata:
  name: rbd.csi.ceph.com
spec:
  attachRequired: true
  podInfoOnMount: false
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: {class}
provisioner: rbd.csi.ceph.com
parameters:
  clusterID: {cluster}
  pool: {pool}
  imageFeatures: layering
  csi.storage.k8s.io/provisioner-secret-name: csi-rbd-secret
  csi.storage.k8s.io/provisioner-secret-namespace: {ns}
  csi.storage.k8s.io/controller-expand-secret-name: csi-rbd-secret
  csi.storage.k8s.io/controller-expand-secret-namespace: {ns}
  csi.storage.k8s.io/node-stage-secret-name: csi-rbd-secret
  csi.storage.k8s.io/node-stage-secret-namespace: {ns}
reclaimPolicy: Delete
allowVolumeExpansion: true
volumeBindingMode: Immediate
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: csi-rbdplugin-provisioner
  namespace: {ns}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: csi-rbdplugin-provisioner
  template:
    metadata:
      labels:
        app: csi-rbdplugin-provisioner
    spec:
      serviceAccountName: rbd-csi-provisioner
      containers:
        - name: csi-provisioner
          image: {provisioner_image}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election=true"
            - "--default-fstype=ext4"
            - "--extra-create-metadata=true"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-attacher
          image: {attacher_image}
          args:
            - "--v=1"
            - "--csi-address=$(ADDRESS)"
            - "--leader-election=true"
            - "--default-fstype=ext4"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-resizer
          image: {resizer_image}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election"
            - "--handle-volume-inuse-error=false"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-rbdplugin
          image: {csi_image}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--type=rbd"
            - "--controllerserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=5"
            - "--drivername=rbd.csi.ceph.com"
            - "--pidlimit=-1"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi-provisioner.sock
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: ceph-csi-config
              mountPath: /etc/ceph-csi-config/
            - name: ceph-config
              mountPath: /etc/ceph/
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          emptyDir:
            medium: Memory
        - name: ceph-csi-config
          configMap:
            name: ceph-csi-config
        - name: ceph-config
          configMap:
            name: ceph-config
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: csi-rbdplugin
  namespace: {ns}
spec:
  selector:
    matchLabels:
      app: csi-rbdplugin
  template:
    metadata:
      labels:
        app: csi-rbdplugin
    spec:
      serviceAccountName: rbd-csi-nodeplugin
      hostNetwork: true
      hostPID: true
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: driver-registrar
          image: {registrar_image}
          args:
            - "--v=1"
            - "--csi-address=/csi/csi.sock"
            - "--kubelet-registration-path=/var/lib/kubelet/plugins/rbd.csi.ceph.com/csi.sock"
          env:
            - name: KUBE_NODE_NAME
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: registration-dir
              mountPath: /registration
        - name: csi-rbdplugin
          securityContext:
            privileged: true
            capabilities:
              add: ["SYS_ADMIN"]
            allowPrivilegeEscalation: true
          image: {csi_image}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--pluginpath=/var/lib/kubelet/plugins"
            - "--stagingpath=/var/lib/kubelet/plugins/kubernetes.io/csi/"
            - "--type=rbd"
            - "--nodeserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=5"
            - "--drivername=rbd.csi.ceph.com"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi.sock
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: host-dev
              mountPath: /dev
            - name: host-sys
              mountPath: /sys
            - name: lib-modules
              mountPath: /lib/modules
              readOnly: true
            - name: ceph-csi-config
              mountPath: /etc/ceph-csi-config/
            - name: ceph-config
              mountPath: /etc/ceph/
            - name: plugin-dir
              mountPath: /var/lib/kubelet/plugins
              mountPropagation: Bidirectional
            - name: mountpoint-dir
              mountPath: /var/lib/kubelet/pods
              mountPropagation: Bidirectional
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          hostPath:
            path: /var/lib/kubelet/plugins/rbd.csi.ceph.com
            type: DirectoryOrCreate
        - name: registration-dir
          hostPath:
            path: /var/lib/kubelet/plugins_registry/
            type: Directory
        - name: plugin-dir
          hostPath:
            path: /var/lib/kubelet/plugins
            type: Directory
        - name: mountpoint-dir
          hostPath:
            path: /var/lib/kubelet/pods
            type: DirectoryOrCreate
        - name: host-dev
          hostPath:
            path: /dev
        - name: host-sys
          hostPath:
            path: /sys
        - name: lib-modules
          hostPath:
            path: /lib/modules
        - name: ceph-csi-config
          configMap:
            name: ceph-csi-config
        - name: ceph-config
          configMap:
            name: ceph-config
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
"#,
        class = RBD_STORAGE_CLASS,
        csi_image = CEPH_CSI_IMAGE,
        provisioner_image = CSI_PROVISIONER_IMAGE,
        attacher_image = CSI_ATTACHER_IMAGE,
        resizer_image = CSI_RESIZER_IMAGE,
        registrar_image = CSI_REGISTRAR_IMAGE,
    ))
}

/// CephFS CSI manifests for `cephfs.csi.ceph.com` / StorageClass `kcore-cephfs`.
///
/// Like RBD, this never embeds cephx key material. The controller applies
/// `csi-cephfs-secret` in namespace `ceph-csi` via the kube API separately.
pub fn cephfs_manifests(provisioner: &LocalProvisioner, fs_name: &str) -> Option<String> {
    let fs_name = fs_name.trim();
    if !provisioner.has_ceph() || provisioner.ceph_monitors.is_empty() || fs_name.is_empty() {
        return None;
    }
    let cluster = &provisioner.ceph_cluster;
    let ns = CEPH_CSI_NAMESPACE;
    Some(format!(
        r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: cephfs-csi-provisioner
  namespace: {ns}
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: cephfs-csi-nodeplugin
  namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: cephfs-external-provisioner-runner
rules:
  - apiGroups: [""]
    resources: ["nodes", "secrets", "configmaps", "serviceaccounts"]
    verbs: ["get", "list", "watch"]
  - apiGroups: [""]
    resources: ["serviceaccounts/token"]
    verbs: ["create"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["list", "watch", "create", "update", "patch"]
  - apiGroups: [""]
    resources: ["persistentvolumes"]
    verbs: ["get", "list", "watch", "create", "update", "delete", "patch"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims"]
    verbs: ["get", "list", "watch", "update"]
  - apiGroups: [""]
    resources: ["persistentvolumeclaims/status"]
    verbs: ["update", "patch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["storageclasses", "csinodes"]
    verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: cephfs-csi-provisioner-role
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: cephfs-external-provisioner-runner
subjects:
  - kind: ServiceAccount
    name: cephfs-csi-provisioner
    namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: cephfs-external-provisioner-cfg
  namespace: {ns}
rules:
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["get", "list", "watch"]
  - apiGroups: ["coordination.k8s.io"]
    resources: ["leases"]
    verbs: ["get", "watch", "list", "delete", "update", "create"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: cephfs-csi-provisioner-role-cfg
  namespace: {ns}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: cephfs-external-provisioner-cfg
subjects:
  - kind: ServiceAccount
    name: cephfs-csi-provisioner
    namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: cephfs-csi-nodeplugin
rules:
  - apiGroups: [""]
    resources: ["nodes", "secrets", "configmaps", "serviceaccounts"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["serviceaccounts/token"]
    verbs: ["create"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: cephfs-csi-nodeplugin
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: cephfs-csi-nodeplugin
subjects:
  - kind: ServiceAccount
    name: cephfs-csi-nodeplugin
    namespace: {ns}
---
apiVersion: storage.k8s.io/v1
kind: CSIDriver
metadata:
  name: cephfs.csi.ceph.com
spec:
  attachRequired: false
  podInfoOnMount: false
  fsGroupPolicy: File
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: {class}
provisioner: cephfs.csi.ceph.com
parameters:
  clusterID: {cluster}
  fsName: {fs_name}
  csi.storage.k8s.io/provisioner-secret-name: csi-cephfs-secret
  csi.storage.k8s.io/provisioner-secret-namespace: {ns}
  csi.storage.k8s.io/controller-expand-secret-name: csi-cephfs-secret
  csi.storage.k8s.io/controller-expand-secret-namespace: {ns}
  csi.storage.k8s.io/node-stage-secret-name: csi-cephfs-secret
  csi.storage.k8s.io/node-stage-secret-namespace: {ns}
reclaimPolicy: Delete
allowVolumeExpansion: true
volumeBindingMode: Immediate
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: csi-cephfsplugin-provisioner
  namespace: {ns}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: csi-cephfsplugin-provisioner
  template:
    metadata:
      labels:
        app: csi-cephfsplugin-provisioner
    spec:
      serviceAccountName: cephfs-csi-provisioner
      containers:
        - name: csi-provisioner
          image: {provisioner_image}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election=true"
            - "--extra-create-metadata=true"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-resizer
          image: {resizer_image}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election"
            - "--handle-volume-inuse-error=false"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-cephfsplugin
          image: {csi_image}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--type=cephfs"
            - "--controllerserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=5"
            - "--drivername=cephfs.csi.ceph.com"
            - "--pidlimit=-1"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi-provisioner.sock
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: ceph-csi-config
              mountPath: /etc/ceph-csi-config/
            - name: ceph-config
              mountPath: /etc/ceph/
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          emptyDir:
            medium: Memory
        - name: ceph-csi-config
          configMap:
            name: ceph-csi-config
        - name: ceph-config
          configMap:
            name: ceph-config
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: csi-cephfsplugin
  namespace: {ns}
spec:
  selector:
    matchLabels:
      app: csi-cephfsplugin
  template:
    metadata:
      labels:
        app: csi-cephfsplugin
    spec:
      serviceAccountName: cephfs-csi-nodeplugin
      hostNetwork: true
      hostPID: true
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: driver-registrar
          image: {registrar_image}
          args:
            - "--v=1"
            - "--csi-address=/csi/csi.sock"
            - "--kubelet-registration-path=/var/lib/kubelet/plugins/cephfs.csi.ceph.com/csi.sock"
          env:
            - name: KUBE_NODE_NAME
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: registration-dir
              mountPath: /registration
        - name: csi-cephfsplugin
          securityContext:
            privileged: true
            capabilities:
              add: ["SYS_ADMIN"]
            allowPrivilegeEscalation: true
          image: {csi_image}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--type=cephfs"
            - "--nodeserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=5"
            - "--drivername=cephfs.csi.ceph.com"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi.sock
            - name: POD_NAMESPACE
              valueFrom:
                fieldRef:
                  fieldPath: metadata.namespace
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: mountpoint-dir
              mountPath: /var/lib/kubelet/pods
              mountPropagation: Bidirectional
            - name: plugin-dir
              mountPath: /var/lib/kubelet/plugins
              mountPropagation: Bidirectional
            - name: host-sys
              mountPath: /sys
            - name: lib-modules
              mountPath: /lib/modules
              readOnly: true
            - name: host-dev
              mountPath: /dev
            - name: ceph-csi-config
              mountPath: /etc/ceph-csi-config/
            - name: ceph-config
              mountPath: /etc/ceph/
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          hostPath:
            path: /var/lib/kubelet/plugins/cephfs.csi.ceph.com/
            type: DirectoryOrCreate
        - name: registration-dir
          hostPath:
            path: /var/lib/kubelet/plugins_registry/
            type: Directory
        - name: mountpoint-dir
          hostPath:
            path: /var/lib/kubelet/pods
            type: DirectoryOrCreate
        - name: plugin-dir
          hostPath:
            path: /var/lib/kubelet/plugins
            type: Directory
        - name: host-sys
          hostPath:
            path: /sys
        - name: lib-modules
          hostPath:
            path: /lib/modules
        - name: host-dev
          hostPath:
            path: /dev
        - name: ceph-csi-config
          configMap:
            name: ceph-csi-config
        - name: ceph-config
          configMap:
            name: ceph-config
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
"#,
        class = CEPHFS_STORAGE_CLASS,
        csi_image = CEPH_CSI_IMAGE,
        provisioner_image = CSI_PROVISIONER_IMAGE,
        resizer_image = CSI_RESIZER_IMAGE,
        registrar_image = CSI_REGISTRAR_IMAGE,
    ))
}

fn local_path_manifests(provisioner: &LocalProvisioner, api_server_endpoint: &str) -> String {
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

    #[test]
    fn a_ceph_cluster_adds_an_rbd_storage_class_and_keeps_local_path() {
        let mut provisioner = normalize(
            "lab",
            &members(),
            true,
            "",
            "",
            &[volume("kcore-kdata", "node-105", "lvm", 1 << 30)],
        )
        .unwrap();
        assert!(rbd_manifests(&provisioner).is_none());
        provisioner.ceph_cluster = "kcore-san".into();
        provisioner.ceph_monitors = vec!["192.168.47.2:6789".into(), "192.168.47.3:6789".into()];
        let yaml = manifests_at(&provisioner, "");
        assert!(yaml.contains("rancher.io/local-path"));
        assert!(yaml.contains("provisioner: rbd.csi.ceph.com"));
        assert!(yaml.contains("rbd.csi.ceph.com"));
        assert!(yaml.contains("kind: Deployment"));
        assert!(yaml.contains("kind: DaemonSet"));
        assert!(yaml.contains("name: csi-rbdplugin-provisioner"));
        assert!(yaml.contains("name: csi-rbdplugin"));
        assert!(yaml.contains(CEPH_CSI_IMAGE));
        assert!(yaml.contains("\"monitors\": [\"192.168.47.2:6789\", \"192.168.47.3:6789\"]"));
        assert!(yaml.contains("clusterID: kcore-san"));
        assert!(yaml.contains("csi-rbd-secret"));
        assert!(!yaml.contains("userKey"));
        assert!(!yaml.contains("userID"));
        assert!(cephfs_manifests(&provisioner, "").is_none());
        assert!(!yaml.contains("cephfs.csi.ceph.com"));
    }

    #[test]
    fn a_ceph_only_provisioner_renders_just_rbd_and_survives_encoding() {
        let mut provisioner = LocalProvisioner::ceph_only("kcore-san");
        provisioner.ceph_monitors = vec!["10.0.0.1:6789".into()];
        provisioner.ceph_pool = "kcore-vms".into();
        assert!(provisioner.renders_manifests());
        let yaml = manifests_at(&provisioner, "");
        assert!(!yaml.contains("local-path"));
        assert!(yaml.contains("name: kcore-rbd"));
        assert!(yaml.contains("kind: Deployment"));
        assert!(yaml.contains("kind: DaemonSet"));
        let back = decode(&encode(&provisioner)).unwrap();
        assert_eq!(back, provisioner);
        assert_eq!(encode(&LocalProvisioner::disabled()), "");
    }

    #[test]
    fn cephfs_renders_when_filesystem_name_is_set() {
        let mut provisioner = LocalProvisioner::ceph_only("kcore-san");
        provisioner.ceph_monitors = vec!["10.0.0.1:6789".into()];
        provisioner.ceph_filesystem = "lab-fs".into();
        let cephfs = cephfs_manifests(&provisioner, "lab-fs").unwrap();
        assert!(cephfs.contains("cephfs.csi.ceph.com"));
        assert!(cephfs.contains("name: kcore-cephfs"));
        assert!(cephfs.contains("fsName: lab-fs"));
        assert!(cephfs.contains("kind: Deployment"));
        assert!(cephfs.contains("kind: DaemonSet"));
        assert!(cephfs.contains("name: csi-cephfsplugin-provisioner"));
        assert!(cephfs.contains("name: csi-cephfsplugin"));
        assert!(cephfs.contains("csi-cephfs-secret"));
        assert!(cephfs.contains(CEPH_CSI_IMAGE));
        assert!(!cephfs.contains("userKey"));
        assert!(!cephfs.contains("userID"));

        let yaml = manifests_at(&provisioner, "");
        assert!(yaml.contains("rbd.csi.ceph.com"));
        assert!(yaml.contains("cephfs.csi.ceph.com"));
        assert!(yaml.contains("name: kcore-cephfs"));
        assert!(yaml.contains("kind: Deployment"));
        assert!(yaml.contains("kind: DaemonSet"));
    }
}
