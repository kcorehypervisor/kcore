# kcore SAN (Ceph) — integration

kcore SAN is the cluster’s **shared block storage** fabric. Guests use **Ceph RBD** images so a VM’s disk is not tied to one node’s local filesystem/LVM/ZFS. That is what makes **cold drain** and **Cloud Hypervisor live migration** possible without copying disk bytes.

Operator-facing overview: [kcore SAN (Ceph)](https://kcorehypervisor.com/docs/user/storage-vsan.html) · migration runbook: [VM migration](https://kcorehypervisor.com/docs/user/vm-migration.html) · deep migration notes in-repo: [`vm-migration.md`](./vm-migration.md).

Status: **Phases 1–2 + live migration shipped**.

## Design split: NixOS vs KCore

| Concern | Owned by | Mechanism |
|---------|----------|-----------|
| Ceph packages / units | NixOS | `services.ceph` via [`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix) |
| FSID, keyrings, monmap | Controller + node-agent | Bootstrap package redistributed to members |
| OSD prepare / activate | Node-agent | `BootstrapCephOsd` → `ceph-volume lvm create` (+ systemd activate) |
| Pool / RBD init | Node-agent (reconciler-driven) | `EnsureCephPool` → pool `kcore-vms` |
| Health / phase | Controller reconciler | `GetCephHealth` until `HEALTH_OK` |
| VM RBD create/delete | Node storage adapter | `rbd create` / `rbd rm` (`layering` only) |
| Map + one-time seed | Node systemd `ExecStartPre` | `rbd map` + `qemu-img convert`; image-meta `kcore.seeded=1` |
| Guest VMM | Cloud Hypervisor | `ch-vm` unit; Ceph VMs use `memory …,shared=on` |

KCore does **not** use cephadm or Rook. The reconciler pushes declarative Nix and fills the imperative gaps NixOS cannot express.

## Control-plane data model

- **`ceph_clusters`** — desired `CephCluster` spec (FSID, networks, nodes, size/minSize) + bootstrap JSON (keys).
- **`ceph_cluster_status`** — reconciled phase (`bootstrapping` / `healthy` / `degraded` / …).
- **`volumes`** — cluster-scoped RBD identity (`pool`, `image`, `size_bytes`) keyed by `vm_id`. The volume does **not** move when the VM’s `node_id` changes.
- **`vms.storage_backend = ceph`** — schedule only onto nodes that are members of a healthy `CephCluster` (membership via `CephCluster.spec.nodes`, not only `nodes.storage_backend`).

Runtime Nix for a Ceph VM includes `storageBackend = "ceph"` and `rbdImage = "kcore-<vm-id>"` (see `crates/controller/src/nixgen.rs`).

## Phase 1 — fabric bootstrap

### Manifest

```yaml
kind: CephCluster
metadata:
  name: lab-san
spec:
  fsid: "2f3f204e-65ca-4b69-a86c-bf8408f5c792"
  publicNetwork: "10.10.0.0/24"
  clusterNetwork: "10.20.0.0/24"
  replication:
    size: 3
    minSize: 2
  nodes:
    - nodeId: dell-1
      monAddr: "10.10.0.11:6789"
      clusterAddr: "10.20.0.11"
      publicIface: eth1
      clusterIface: eth2
      osdDevice: /dev/nvme0n1
    # … additional members
```

```bash
kctl apply -f lab-san-ceph.yaml
kctl get ceph-cluster
kctl describe ceph-cluster lab-san
```

### Reconciler sequence

1. Persist spec + generation.
2. `ApplyCephConfig` — write `/etc/nixos/kcore-ceph.nix`, rebuild; cluster name is always `ceph`; `mon_host` lists every member.
3. First eligible node builds mon/admin/bootstrap-osd keyrings (`ceph-authtool`); controller redistributes the package; peers run `ceph-mon --mkfs` / mgr auth.
4. `BootstrapCephOsd` — `ceph-volume lvm create` on `osdDevice` (refuses non-empty disks unless `forceWipe: true`), activate units (no `--no-systemd`).
5. `EnsureCephPool` for `kcore-vms` (size/min_size, RBD application, `rbd pool init`).
6. Poll health until `HEALTH_OK`; otherwise requeue.

Discover hardware before writing the manifest:

```bash
kctl --node <addr>:9091 node nics
kctl --node <addr>:9091 node disks
```

## Phase 2 — RBD VM disks

```bash
kctl create vm app-1 \
  --storage-backend ceph \
  --storage-size-bytes 42949672960 \
  --image <https-url> \
  --image-sha256 <sha256>
```

Create path (controller → node):

1. Gate: target node must be in a **healthy** `CephCluster`.
2. Storage RPC creates `kcore-<vm-id>` in pool `kcore-vms` with `--image-feature layering` (no exclusive-lock — required for dual-map during live migrate).
3. Upsert `volumes` row; insert `vms` with `node_id`.
4. Push Nix; node unit `ExecStartPre` maps `/dev/rbd/kcore-vms/kcore-<vm-id>` and seeds once from the guest image (`qemu-img convert` + RBD image-meta `kcore.seeded=1`), then Cloud Hypervisor boots with that block device.

Delete best-effort `rbd rm`s the image.

Local backends (`filesystem` / `lvm` / `zfs`) remain for single-node latency-sensitive workloads.

## Lab topology (reference)

| Role | Disk | Notes |
|------|------|--------|
| OS + MON/MGR state | Disk 1 | `kctl node install --os-disk …` |
| One OSD | Disk 2 | Whole device for `ceph-volume`; **not** the local LVM/ZFS data disk |

Prefer a dedicated fabric for `clusterNetwork` (replication) and a separate `publicNetwork` for client/MON traffic.

## Safety

- Never point `osdDevice` at the OS disk.
- Default `forceWipe: false` refuses OSD prepare when signatures exist.
- Keep Ceph replication off a congested management NIC.
- Live migration needs ephemeral TCP between members (see [`vm-migration.md`](./vm-migration.md)).

## Source map

| Area | Paths |
|------|--------|
| Proto | `proto/controller.proto` (`CephCluster*`, `MigrateVm`), `proto/node.proto` (Ceph + live-migrate peer RPCs) |
| Reconciler / spec | `crates/controller/src/ceph_cluster_*.rs` |
| DB | `crates/controller/src/db.rs` (`ceph_clusters`, `volumes`, …) |
| Nixgen | `crates/controller/src/nixgen.rs` |
| Node Ceph / RBD | `crates/node-agent/src/ceph_bootstrap.rs`, `storage/mod.rs` (`CephAdapter`) |
| Live migrate | `crates/node-agent/src/live_migrate.rs`, `vmm/client.rs`, `grpc/admin.rs` |
| Units | `modules/kcore-ceph.nix`, `modules/ch-vm/vm-service.nix` |
