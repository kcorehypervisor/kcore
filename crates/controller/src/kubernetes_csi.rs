//! Ceph RBD CSI manifests for Kubernetes clusters.
//!
//! Uses the rbd-nbd mounter because the kernel RBD client rejects aes256k
//! keys used by kcore's encrypted OSDs.

use serde::{Deserialize, Serialize};

/// Pinned ceph-csi image used by both the provisioner Deployment and the
/// nodeplugin DaemonSet. Bump deliberately; do not float to `:latest`.
pub const CEPH_CSI_IMAGE: &str = "quay.io/cephcsi/cephcsi:v3.11.0";
pub const CSI_SNAPSHOTTER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-snapshotter:v7.0.2";
pub const CSI_PROVISIONER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-provisioner:v4.0.1";
pub const CSI_ATTACHER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-attacher:v4.5.1";
pub const CSI_RESIZER_IMAGE: &str = "registry.k8s.io/sig-storage/csi-resizer:v1.10.1";
pub const CSI_NODE_DRIVER_IMAGE: &str =
    "registry.k8s.io/sig-storage/csi-node-driver-registrar:v2.10.1";
pub const SNAPSHOT_CONTROLLER_IMAGE: &str =
    "registry.k8s.io/sig-storage/snapshot-controller:v7.0.2";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CephStorage {
    pub enabled: bool,
    /// Ceph cluster resource name in kcore.
    pub cluster: String,
    pub pool: String,
    pub monitors: Vec<String>,
    /// Base64 user key for client.k8s-<cluster>.
    pub user_key: String,
    pub user_id: String,
    /// Make StorageClass the default.
    pub default_storage_class: bool,
}

impl CephStorage {
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.cluster.trim().is_empty() {
            return Err("storage.ceph.cluster is required".into());
        }
        if self.pool.trim().is_empty() {
            return Err("storage.ceph.pool is required".into());
        }
        if self.monitors.is_empty() {
            return Err("storage.ceph.monitors is required".into());
        }
        if self.user_key.trim().is_empty() {
            return Err("storage.ceph.userKey is required".into());
        }
        Ok(())
    }

    pub fn client_name(&self, k8s_cluster: &str) -> String {
        if self.user_id.is_empty() {
            format!("k8s-{k8s_cluster}")
        } else {
            self.user_id.clone()
        }
    }
}

/// Render ceph-csi rbd: namespace, secret, configmap, StorageClass, provisioner
/// Deployment, nodeplugin DaemonSet, VolumeSnapshot CRDs/controller/class.
pub fn manifests(storage: &CephStorage, cluster_name: &str) -> String {
    if !storage.enabled {
        return String::new();
    }
    let user = storage.client_name(cluster_name);
    let mons = storage
        .monitors
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(",");
    let default_ann = if storage.default_storage_class {
        "storageclass.kubernetes.io/is-default-class: \"true\""
    } else {
        ""
    };
    format!(
        r#"apiVersion: v1
kind: Namespace
metadata:
  name: ceph-csi
---
apiVersion: v1
kind: Secret
metadata:
  name: csi-rbd-secret
  namespace: ceph-csi
type: Opaque
stringData:
  userID: {user}
  userKey: {key}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: ceph-csi-config
  namespace: ceph-csi
data:
  config.json: |
    [{{"clusterID":"{ceph}","monitors":[{mons}]}}]
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata:
  name: kcore-ceph
  annotations:
    {default_ann}
provisioner: rbd.csi.ceph.com
parameters:
  clusterID: {ceph}
  pool: {pool}
  imageFeatures: layering,exclusive-lock
  csi.storage.k8s.io/provisioner-secret-name: csi-rbd-secret
  csi.storage.k8s.io/provisioner-secret-namespace: ceph-csi
  csi.storage.k8s.io/controller-expand-secret-name: csi-rbd-secret
  csi.storage.k8s.io/controller-expand-secret-namespace: ceph-csi
  csi.storage.k8s.io/node-stage-secret-name: csi-rbd-secret
  csi.storage.k8s.io/node-stage-secret-namespace: ceph-csi
  mounter: rbd-nbd
reclaimPolicy: Delete
allowVolumeExpansion: true
volumeBindingMode: Immediate
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: ceph-csi-rbd-provisioner
  namespace: ceph-csi
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: ceph-csi-rbd-nodeplugin
  namespace: ceph-csi
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: ceph-csi-rbd-provisioner
rules:
  - apiGroups: [""]
    resources: ["secrets", "persistentvolumes", "persistentvolumeclaims", "nodes", "configmaps"]
    verbs: ["get", "list", "watch", "create", "delete", "update", "patch"]
  - apiGroups: ["storage.k8s.io"]
    resources: ["storageclasses", "volumeattachments", "csinodes", "volumeattachments/status"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["snapshot.storage.k8s.io"]
    resources: ["volumesnapshotclasses", "volumesnapshots", "volumesnapshotcontents", "volumesnapshotcontents/status"]
    verbs: ["get", "list", "watch", "create", "update", "delete", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: ceph-csi-rbd-provisioner
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: ceph-csi-rbd-provisioner
subjects:
  - kind: ServiceAccount
    name: ceph-csi-rbd-provisioner
    namespace: ceph-csi
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: csi-rbdplugin-provisioner
  namespace: ceph-csi
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
      serviceAccountName: ceph-csi-rbd-provisioner
      containers:
        - name: csi-provisioner
          image: {provisioner}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--retry-interval-start=500ms"
            - "--leader-election=true"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-snapshotter
          image: {snapshotter}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election=true"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-attacher
          image: {attacher}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--leader-election=true"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-resizer
          image: {resizer}
          args:
            - "--csi-address=$(ADDRESS)"
            - "--v=1"
            - "--timeout=150s"
            - "--leader-election"
          env:
            - name: ADDRESS
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
        - name: csi-rbdplugin
          image: {cephcsi}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--type=rbd"
            - "--controllerserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=1"
            - "--drivername=rbd.csi.ceph.com"
            - "--pidlimit=-1"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi-provisioner.sock
          volumeMounts:
            - name: socket-dir
              mountPath: /csi
            - name: ceph-csi-config
              mountPath: /etc/ceph-csi-config/
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          emptyDir: {{}}
        - name: ceph-csi-config
          configMap:
            name: ceph-csi-config
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
---
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: csi-rbdplugin
  namespace: ceph-csi
spec:
  selector:
    matchLabels:
      app: csi-rbdplugin
  template:
    metadata:
      labels:
        app: csi-rbdplugin
    spec:
      serviceAccountName: ceph-csi-rbd-nodeplugin
      hostNetwork: true
      containers:
        - name: driver-registrar
          image: {registrar}
          args:
            - "--v=1"
            - "--csi-address=$(ADDRESS)"
            - "--kubelet-registration-path=$(DRIVER_REG_SOCK_PATH)"
          env:
            - name: ADDRESS
              value: unix:///csi/csi.sock
            - name: DRIVER_REG_SOCK_PATH
              value: /var/lib/kubelet/plugins/rbd.csi.ceph.com/csi.sock
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
          image: {cephcsi}
          args:
            - "--nodeid=$(NODE_ID)"
            - "--type=rbd"
            - "--nodeserver=true"
            - "--endpoint=$(CSI_ENDPOINT)"
            - "--v=1"
            - "--drivername=rbd.csi.ceph.com"
          env:
            - name: NODE_ID
              valueFrom:
                fieldRef:
                  fieldPath: spec.nodeName
            - name: CSI_ENDPOINT
              value: unix:///csi/csi.sock
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
            - name: keys-tmp-dir
              mountPath: /tmp/csi/keys
      volumes:
        - name: socket-dir
          hostPath:
            path: /var/lib/kubelet/plugins/rbd.csi.ceph.com
            type: DirectoryOrCreate
        - name: registration-dir
          hostPath:
            path: /var/lib/kubelet/plugins_registry
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
        - name: keys-tmp-dir
          emptyDir:
            medium: Memory
---
{crds}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: snapshot-controller
  namespace: ceph-csi
spec:
  replicas: 1
  selector:
    matchLabels:
      app: snapshot-controller
  template:
    metadata:
      labels:
        app: snapshot-controller
    spec:
      serviceAccountName: ceph-csi-rbd-provisioner
      containers:
        - name: snapshot-controller
          image: {snapctrl}
          args:
            - "--v=1"
            - "--leader-election=true"
---
apiVersion: snapshot.storage.k8s.io/v1
kind: VolumeSnapshotClass
metadata:
  name: kcore-ceph-snap
driver: rbd.csi.ceph.com
deletionPolicy: Delete
parameters:
  clusterID: {ceph}
  csi.storage.k8s.io/snapshotter-secret-name: csi-rbd-secret
  csi.storage.k8s.io/snapshotter-secret-namespace: ceph-csi
"#,
        user = user,
        key = storage.user_key,
        ceph = storage.cluster,
        mons = mons,
        pool = storage.pool,
        default_ann = default_ann,
        provisioner = CSI_PROVISIONER_IMAGE,
        snapshotter = CSI_SNAPSHOTTER_IMAGE,
        attacher = CSI_ATTACHER_IMAGE,
        resizer = CSI_RESIZER_IMAGE,
        cephcsi = CEPH_CSI_IMAGE,
        registrar = CSI_NODE_DRIVER_IMAGE,
        snapctrl = SNAPSHOT_CONTROLLER_IMAGE,
        crds = snapshot_crds(),
    )
}

/// Upstream external-snapshotter CRDs (v7.0.2 schemas), embedded at compile time.
fn snapshot_crds() -> String {
    [
        include_str!("csi_crds/volumesnapshotclasses.yaml"),
        include_str!("csi_crds/volumesnapshots.yaml"),
        include_str!("csi_crds/volumesnapshotcontents.yaml"),
    ]
    .join("\n")
}

/// metrics-server addon for kubectl top / HPA.
pub fn metrics_server_manifest() -> String {
    r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: metrics-server
  namespace: kube-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: system:metrics-server
rules:
  - apiGroups: [""]
    resources: ["nodes", "nodes/metrics", "pods", "namespaces"]
    verbs: ["get", "list", "watch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: system:metrics-server
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: system:metrics-server
subjects:
  - kind: ServiceAccount
    name: metrics-server
    namespace: kube-system
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: metrics-server-auth-reader
  namespace: kube-system
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: extension-apiserver-authentication-reader
subjects:
  - kind: ServiceAccount
    name: metrics-server
    namespace: kube-system
---
apiVersion: v1
kind: Service
metadata:
  name: metrics-server
  namespace: kube-system
  labels:
    k8s-app: metrics-server
spec:
  ports:
    - port: 443
      protocol: TCP
      targetPort: https
  selector:
    k8s-app: metrics-server
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: metrics-server
  namespace: kube-system
  labels:
    k8s-app: metrics-server
spec:
  replicas: 1
  selector:
    matchLabels:
      k8s-app: metrics-server
  template:
    metadata:
      labels:
        k8s-app: metrics-server
    spec:
      serviceAccountName: metrics-server
      containers:
        - name: metrics-server
          image: registry.k8s.io/metrics-server/metrics-server:v0.7.2
          args:
            - --cert-dir=/tmp
            - --secure-port=10250
            - --kubelet-preferred-address-types=InternalIP,ExternalIP,Hostname
            - --kubelet-use-node-status-port
            - --metric-resolution=15s
            - --kubelet-insecure-tls
          ports:
            - name: https
              containerPort: 10250
              protocol: TCP
          volumeMounts:
            - name: tmp
              mountPath: /tmp
      volumes:
        - name: tmp
          emptyDir: {}
---
apiVersion: apiregistration.k8s.io/v1
kind: APIService
metadata:
  name: v1beta1.metrics.k8s.io
spec:
  service:
    name: metrics-server
    namespace: kube-system
  group: metrics.k8s.io
  version: v1beta1
  insecureSkipTLSVerify: true
  groupPriorityMinimum: 100
  versionPriority: 100
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_include_daemonset_and_snapshot_class() {
        let storage = CephStorage {
            enabled: true,
            cluster: "kcore-san".into(),
            pool: "kcore-vms".into(),
            monitors: vec!["192.168.47.2:6789".into()],
            user_key: "abc=".into(),
            user_id: String::new(),
            default_storage_class: true,
        };
        let yaml = manifests(&storage, "lab");
        assert!(yaml.contains("kind: DaemonSet"));
        assert!(yaml.contains("kind: VolumeSnapshotClass"));
        assert!(yaml.contains("csi-rbdplugin-provisioner"));
        assert!(yaml.contains("mounter: rbd-nbd"));
        assert!(yaml.contains(CEPH_CSI_IMAGE));
        // Upstream external-snapshotter CRDs (not the old minimal stubs).
        assert!(yaml.contains("volumesnapshotclasses.snapshot.storage.k8s.io"));
        assert!(yaml.contains("volumesnapshots.snapshot.storage.k8s.io"));
        assert!(yaml.contains("volumesnapshotcontents.snapshot.storage.k8s.io"));
        assert!(yaml.contains("x-kubernetes-validations") || yaml.contains("openAPIV3Schema"));
        assert!(
            yaml.len() > 20_000,
            "embedded CRDs should be full upstream schemas"
        );
    }
}
