# CephFS and RGW on kcore

kcore builds shared file and object storage on the same Ceph fabric as RBD VM
disks. Phase F introduces two controller resources:

- **SharedFilesystem** — CephFS volumes for containers (host mount + bind) and,
  later, VM virtio-fs workloads.
- **ObjectStore** — RADOS Gateway (RGW) for S3-compatible buckets; **ObjectUser**
  credentials are issued once at create time.

This document describes the scaffold shipped in-tree. Full MDS/RGW lab automation
(probes, Nix push, firewall) evolves in later phases.

## Prerequisites

1. A healthy **CephCluster** (mons, mgrs, OSDs, `kcore-vms` RBD pool).
2. Optional Nix flags on members (`modules/kcore-ceph.nix`):
   - `kcore.ceph.enableMds = true` — run an MDS on the node.
   - `kcore.ceph.enableRgw = true` — run RGW on the node (`rgwPort` defaults to 7480).

## SharedFilesystem (CephFS)

Example manifest:

```yaml
kind: SharedFilesystem
metadata:
  name: team-data
spec:
  cephCluster: ceph
  quotaBytes: 1099511627776
  clients:
    - name: app-a
      paths: ["/"]
```

Apply or create:

```bash
kctl apply -f team-data-fs.yaml
# or
kctl create shared-filesystem -f team-data-fs.yaml
kctl get shared-filesystems
kctl describe shared-filesystem team-data
kctl delete shared-filesystem team-data
```

Status phases: `pending` → `bootstrapping` → `healthy` (when CephFS exists and
the reconciler can see it). Without a local `ceph` CLI, the resource is still
recorded and stays in `bootstrapping` with an explanatory message.

## ObjectStore (RGW)

Example manifest:

```yaml
kind: ObjectStore
metadata:
  name: backups
spec:
  cephCluster: ceph
  members:
    - node-a
    - node-b
  port: 7480
  tls: false
```

```bash
kctl apply -f backups-store.yaml
kctl create object-store -f backups-store.yaml
kctl get object-stores
kctl describe object-store backups
kctl delete object-store backups
```

Object users are created through the controller API (`CreateObjectUser`); the
secret is returned only in that response. Delete all users before deleting the
store.

## See also

- CephCluster and RBD volumes: operator docs for SAN setup.
- Private design notes: `.cursor/design/cephfs-rgw.md` (local, not in git).
