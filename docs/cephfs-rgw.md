# CephFS and RGW on kcore

kcore puts shared file and object storage on the same Ceph cluster as the RBD
VM disks, through two controller resources:

- **SharedFilesystem**: a CephFS filesystem with per-client keys.
- **ObjectStore**: a RADOS Gateway (RGW) for S3 buckets. **ObjectUser** records
  carry the S3 keys; the secret is shown once, at create time.

Status of each piece is in [`ceph-lifecycle.md`](./ceph-lifecycle.md#cephfs-rgw-kubernetes).
In short: the controller creates pools, filesystems, client keys, and RGW
users, and turns on MDS and RGW daemons on the members through the
`CephCluster` Nix it already pushes. None of it has been run on the garage
cluster.

## Requirements

1. A healthy `CephCluster`.
2. A `ceph` CLI on the controller host that reaches the cluster (and
   `radosgw-admin` for object users). Without it the resource is recorded and
   stays in `bootstrapping` with a message saying the CLI is missing.
3. Members that can host the daemons. The `CephCluster` reconciler sets
   `kcore.ceph.enableMds` on every `full` or `mon` member while a
   SharedFilesystem references the cluster, and `kcore.ceph.enableRgw` on the
   members an ObjectStore lists (all `full` or `mon` members when the list is
   empty). `osd`-only members get a flag only when an ObjectStore names them.
   RGW listens on the module default port 7480; `spec.port` is not pushed.
   Creating, changing, or deleting either resource requeues the `CephCluster`.
   The resource status says `queued for <nodes>` while that pass runs and
   `enabled on <nodes>` once it has finished.

## SharedFilesystem

```yaml
kind: SharedFilesystem
metadata:
  name: team-data
spec:
  cephCluster: lab-san
  fsName: team-data            # optional; defaults to the resource name
  quotaBytes: 1099511627776    # 0 = no quota
  clients:
    - name: app-a
      paths: ["/"]
```

```bash
kctl apply -f team-data-fs.yaml
kctl get shared-filesystems
kctl describe shared-filesystem team-data
kctl delete shared-filesystem team-data
```

The reconciler:

1. Creates `<fsName>-metadata` and `<fsName>-data` and tags them `cephfs`.
2. Runs `ceph fs new` if the filesystem does not exist.
3. Sets `max_bytes` from `quotaBytes`.
4. Runs `ceph fs authorize <fsName> client.<name> <path> rw` for each client.

Phases go `pending` → `bootstrapping` → `healthy` once `ceph fs ls` lists the
filesystem. The status message also says where MDS is enabled
(`MDS enabled on …` or `MDS queued for …`).

Slurm: `storage.mode: shared-filesystem` on a `SlurmCluster` mounts the
filesystem on every member (`modules/kcore-slurm.nix`). The mount uses the
`client.admin` keyring, not one of the per-client keys. VMs have no virtio-fs
path.

Terraform: `kcore_shared_filesystem`. MCP: `shared-filesystem`.

## ObjectStore

```yaml
kind: ObjectStore
metadata:
  name: backups
spec:
  cephCluster: lab-san
  members:
    - node-1
    - node-2
  port: 7480
  tls: false
```

```bash
kctl apply -f backups-store.yaml
kctl get object-stores
kctl describe object-store backups
kctl create object-user backup-bot --store backups   # prints the secret once
kctl delete object-store backups
```

The reconciler creates the default RGW pools (`.rgw.root`,
`default.rgw.control`, `default.rgw.meta`, `default.rgw.log`,
`default.rgw.buckets.index`, `default.rgw.buckets.data`) and, for each
ObjectUser, runs `radosgw-admin user create` with the stored access key and
secret so the keys work against RGW. The status message names the nodes that
get `enableRgw` and whether the `CephCluster` pass that applies it has
finished. RGW listens on 7480; `port` is not pushed to the module.

Delete every object user before deleting the store. Multi-site RGW is out of
scope.

Terraform: `kcore_object_store`, `kcore_object_user`. MCP: `object-store`.

## Kubernetes

There is no RBD or CephFS CSI driver yet. `KubernetesCluster` uses
`local-path` by default; see [`kubernetes-volumes.md`](./kubernetes-volumes.md).
