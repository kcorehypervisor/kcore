# Kubernetes volumes

kcore gives a cluster two ways to store PersistentVolumeClaims. The default is a local-path StorageClass on a volume kcore creates. An operator who wants one block device per claim installs their own CSI and leaves those disks out of the kcore API.

## Local-path, the default

Set `localProvisioner.mode` to `shared`, or leave it empty. For each declared volume, kcore creates one LVM logical volume or one ZFS zvol, formats it (`ext4` or `xfs`), and mounts it on that node. The cluster then runs the Rancher local-path provisioner. The StorageClass name defaults to `local-path`.

A claim is a directory on that mounted filesystem. Several claims on the same volume stay separated as directories and share the volume's size. The size written on a claim is not a quota. This StorageClass does not expand one claim on its own. To give every directory more room, resize the kcore volume.

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

`storageClass: zfs` creates a zvol instead of a logical volume. The provisioner is the same: directories on the mounted filesystem.

Declaring more than one volume adds mount paths to that same StorageClass. It does not create a separate StorageClass, and it does not give each claim its own logical volume.

## A CSI the operator installs

kcore does not install a CSI driver, and it does not ship a StorageClass for one. An operator who wants a claim to be its own logical volume, with its own size, can install that driver into the cluster. TopoLVM is one such driver. kcore does not configure it.

Install that driver on the primary cluster, where the pod network can reach ClusterIP services. An extra cluster on the same machines uses a bridge network and has no kube-proxy, so a stock CSI chart cannot rely on ClusterIP there. Run one copy of the driver for a volume group. A second copy on another cluster on the same hosts would manage the same disks.

Do not create the driver's pool, or the logical volumes inside it, with the kcore API. kcore deletes only the volumes it created. Volumes the driver creates are left in place. Point the driver at a new thin pool in free space on the volume group. Leave the local-path volume as a normal filesystem volume. Virtual-machine disks can still be created with the kcore API, under other names, in the space that pool is not using.

## `mode: lvm`

`localProvisioner.mode: lvm` creates one LVM thin pool of the declared size on each node. kcore does not install a provisioner and does not turn claims into logical volumes in that pool. Use `shared` for local-path. An operator who installs their own CSI creates that driver's pool with the driver, not with this mode.
