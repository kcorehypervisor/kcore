# Kubernetes volumes

Pods get disk in one of two ways. kcore installs the Rancher local-path provisioner, so the filesystem behind those claims is a kcore volume declared in the cluster manifest. Otherwise install TopoLVM yourself and manage that data volume with Kubernetes only.

## kcore volumes

`localProvisioner.enabled` is required when volumes are listed. An enabled provisioner with an empty volume list is rejected. Set `mode` to `shared`, or leave it empty. That is the default.

For each entry, kcore creates one LVM logical volume or one ZFS zvol, formats it, and mounts it. `storageClass` is `lvm` or `zfs`, and the node's storage backend must already be that class. `sizeBytes` must be positive. An empty `fsType` becomes ext4. The other accepted value is xfs. An empty role becomes filesystem. The default mount path is `/mnt/kcore/<cluster>/<name>`. A custom mount path is checked. The volume name is one path segment, and it cannot be `.` or `..`.

The node must already be a member of the cluster. The same volume name may be listed on different nodes. The same name twice on one node is rejected. Two volumes on one node cannot share a mount path. After the volume is formatted, a later change of `fsType` is rejected. Shrinking is rejected. To give the directories more room, raise `sizeBytes` and apply again.

The controller volume name is `k8s-<cluster>-<node>-<volume>`. The LVM device is `/dev/<vg>/<lv>`. The ZFS device is `/dev/zvol/<pool>/<dataset>`.

Do not create this filesystem with kubectl. Do not point local-path at a directory you mounted yourself. The disk is the volume in the manifest.

```yaml
localProvisioner:
  enabled: true
  storageClassName: local-path
  mode: shared
  volumes:
    - name: kcore-kdata
      nodeId: node-105
      storageClass: lvm
      sizeBytes: 53687091200
      fsType: ext4
      role: filesystem
```

`storageClass: zfs` creates a zvol. The provisioner is the same.

The control plane applies `rancher/local-path-provisioner:v0.0.30` from `/etc/kubernetes/provisioner.yaml`. The oneshot unit is `kcore-kubernetes-provisioner`. The StorageClass name defaults to `local-path`. If you set a name, it must be a DNS label. The provisioner is `rancher.io/local-path`. `volumeBindingMode` is `WaitForFirstConsumer`. `reclaimPolicy` is `Delete`. The helper creates the claim directory with mode 0777 and removes that directory when the claim is deleted. The kcore volume stays mounted.

A claim is a directory on that filesystem. Several claims share the volume's size. The `storage` request is recorded and is not a quota. Another volume adds a mount path to the same StorageClass. It does not create a second StorageClass, and it does not give each claim its own logical volume.

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: app-data
spec:
  accessModes:
    - ReadWriteOnce
  storageClassName: local-path
  resources:
    requests:
      storage: 10Gi
```

`10Gi` is recorded. It is not a quota.

## TopoLVM

Leave that data out of `localProvisioner.volumes`. Install TopoLVM yourself and manage the data volume with Kubernetes only. kcore does not install TopoLVM and does not create its StorageClass or its pool.

Make the pool with TopoLVM, or with LVM on the node, outside this API. kcore deletes only volumes it created. A logical volume TopoLVM creates is left in place.

Install TopoLVM on the primary cluster, the one whose API port is 6443, where pods can reach ClusterIP. An extra cluster uses a bridge and has no kube-proxy, so a chart that calls ClusterIP will not work there. One TopoLVM owns a volume group. A second copy on another cluster on the same hosts would manage the same disks.

Virtual-machine disks can still be created with the kcore volume API, under other names, in space that pool is not using.

## `mode: lvm`

`localProvisioner.mode: lvm` creates one LVM thin pool of the declared size on each listed node. Nothing in Kubernetes consumes that pool. This mode does not install local-path. The provisioner manifest is empty. It is not TopoLVM. Do not use this pool as TopoLVM's pool.

Every listed node must use the same name, volume group, size, and filesystem. The role is pool. `storageClass` must be `lvm`. `volumeGroup` is required. Leave `mountPath` empty.

Use `shared` when you want the provisioner kcore installs.
