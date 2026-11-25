# kcore SAN (Ceph): operator guide

kcore SAN is the cluster's shared block storage. Guests use Ceph RBD images, so
a VM's disk is not tied to one node. That is what makes cold drain, Cloud
Hypervisor live migration, and automatic failover possible without copying disk
bytes.

What is shipped, implemented but not yet run on real hardware, or missing is
listed in one place: [`ceph-lifecycle.md`](./ceph-lifecycle.md). This guide
says how to use what is there and points at that table where a feature is not
finished.

Site version: [kcore SAN (Ceph)](https://kcorehypervisor.com/docs/user/storage-vsan.html).
Migration: [`vm-migration.md`](./vm-migration.md).

## How it is built

| Concern | Owned by | Mechanism |
|---|---|---|
| Ceph packages and units | NixOS | `services.ceph` via [`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix) |
| FSID, keyrings, monmap | Controller and node agent | Bootstrap package redistributed to members |
| OSD prepare and activate | Node agent and NixOS | `ceph-volume lvm create --no-systemd`, then `ceph-osd@ID`; boot via `ceph-volume-activate` |
| Pools | Node agent, driven by the reconciler | `EnsureCephPool` for `kcore-vms` and each entry in `spec.pools` |
| Health and phase | Controller reconciler | `GetCephHealth`, parsed into typed health |
| VM images | Node storage adapter | `rbd create --image-feature layering,exclusive-lock` (as `client.kcore` when its keyring is present), `rbd rm` |
| Map and one-time seed | VM unit `ExecStartPre` | `rbd-nbd map` + `qemu-img convert`; image-meta `kcore.seeded=1` |
| Guest | Cloud Hypervisor | `ch-vm` unit; Ceph VMs use `memory …,shared=on` |

kcore does not use cephadm or Rook. The reconciler pushes declarative Nix and
fills the gaps NixOS cannot express.

## Create a cluster

Discover hardware first:

```bash
kctl --node <addr>:9091 node nics
kctl --node <addr>:9091 node disks
```

Then write a manifest. The full example with every optional field is
[`examples/ceph/cluster.yaml`](../examples/ceph/cluster.yaml).

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
  encryptOsds: true        # default; fixed once any OSD exists
  nodes:
    - nodeId: dell-1
      monAddr: "10.10.0.11:6789"
      clusterAddr: "10.20.0.11"
      publicIface: eth1
      clusterIface: eth2
      # by-id: /dev/sdX names move when a USB disk is plugged in.
      osdDevice: /dev/disk/by-id/ata-Micron_1300_SATA_512GB_2144329E98F2
```

```bash
kctl diff -f lab-san.yaml     # preflight on every member, prints the encryptOsds notice
kctl apply -f lab-san.yaml    # runs the same plan and refuses on any failure
kctl get ceph-clusters
kctl describe ceph-cluster lab-san
```

The same spec is the `kcore_ceph_cluster` Terraform resource and the
`ceph-cluster` kind in the kcore MCP server.

Rules preflight enforces:

- `monAddr` must be static or a DHCP reservation. A lease change breaks quorum.
- No hardware RAID virtual disks (PERC, MegaRAID) as OSDs.
- An OSD disk with signatures is refused unless `forceWipe: true`.
- Never point an OSD at the OS disk.

### What the reconciler does

1. Preflight on every member: `nixos-rebuild dry-build`, the OSD disk exists
   and is not mounted or RAID, addresses are present, the nixpkgs pin resolves,
   the mon is not on DHCP.
2. Persist the spec and generation.
3. `ApplyCephConfig`: refresh `/etc/nixos/modules/kcore-ceph.nix` from the
   agent build, write `/etc/nixos/kcore-ceph.nix`, rebuild. The cluster name is
   always `ceph`.
4. The first eligible node builds the mon, admin, and bootstrap-osd keyrings.
   The controller redistributes them. Peers run `ceph-mon --mkfs` from the
   running monmap after `ceph mon add`. The first mon uses a self-only monmap:
   listing future members up front left 0.0.0.0 placeholders and the mon never
   formed quorum.
5. `BootstrapCephOsd`: `ceph-volume lvm create --no-systemd` (with `--dmcrypt`
   when `encryptOsds` is true), then `systemctl start ceph-osd@ID`.
6. `EnsureCephPool` for `kcore-vms` and each pool in `spec.pools`, including
   quotas and the `recovery` throttles.
7. `EnsureCephClientKey` on every member. Each node writes
   `/etc/ceph/ceph.client.kcore.keyring` (caps `profile rbd` on `kcore-vms`).
8. Poll health until `HEALTH_OK`, otherwise requeue. If this pass created OSDs
   the controller set `norebalance` and clears it here once every OSD is up
   and in; `health_message` says `backfill ramp:` while it is held.

Step 3 also carries `kcore.ceph.enableMds` and `enableRgw`: when a
`SharedFilesystem` or `ObjectStore` references the cluster, the generated Nix
turns the daemon on for the right members (see
[`cephfs-rgw.md`](./cephfs-rgw.md)). Creating or deleting either resource
requeues the cluster.

Every `ceph` CLI call has a wall-clock timeout.

### Networks and firewall

The module opens TCP 3300, 6789, and 6800–7568, trusts `clusterIface`, and can
put a static `clusterAddr` on that interface from `clusterNetwork`'s prefix.
Prefer a `NodeNetwork` claim where you have one. Keep replication off a
congested management NIC.

### Ceph package

Nodes pin nixpkgs `b40629ef`, where in-tree Ceph does not evaluate.
[`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix) overlays Ceph 20.2.4
from a newer pinned revision. CI check:
`nix build .#checks.x86_64-linux.ceph-module-eval`. How that pin moves is in
[`update-lifecycle.md`](./update-lifecycle.md#ceph-in-a-cluster-update).

## Optional cluster settings

All of these are optional. A manifest without them behaves as before. Check
[`ceph-lifecycle.md`](./ceph-lifecycle.md) for which ones this build acts on;
a field the reconciler does not read yet is accepted and validated, and has no
effect.

| Field | Meaning |
|---|---|
| `pools[]` | `name`, `application` (`rbd`, `cephfs`, `rgw`), `size`, `minSize`, `pgNum` (0 = autoscaler), `crushRule`, `compressionMode`, `quotaBytes`, `poolType` (`replicated` or `erasure`), `erasureProfile`. `kcore-vms` stays the default RBD pool. |
| `recovery` | `maxBackfills`, `recoveryMaxActive`, `scrubBeginHour` / `scrubEndHour` (0–23, -1 leaves the default), `nearfullRatioBp` (8500 = 85%). |
| `msMode` | `secure` or `prefer-crc`, applied to `ms_cluster_mode` and `ms_service_mode`. |
| `imageFeatures` | Features for new RBD images. New images already get `layering,exclusive-lock`. |
| `cachePolicy` | `writeback` (default) or `writethrough` for new maps, cluster-wide. |
| `lostMembers[]` | Hosts that will never answer. See [A dead member](#a-dead-member). |
| `external` | `monHosts`, `pool`, `clientName`, `clientKeyring`: use a Ceph cluster kcore does not run. Cannot be combined with `nodes`. |
| `nodes[].role` | `full` (default: mon, mgr, OSD), `mon` (mon and mgr), or `osd`. |
| `nodes[].weight`, `nodes[].deviceClass` | CRUSH weight and device class overrides. |

## Health

`kctl describe ceph-cluster <name>` and the dashboard Ceph page show the
parsed `ceph -s`: health, mon quorum, OSDs up and in, PG counts by state,
flags such as `noout` and `norebalance`, health check summaries, and
capacity. The raw JSON is kept next to it. Terraform exposes the same data as
the computed `health` block.

## VM disks

```bash
kctl create vm app-1 \
  --storage-backend ceph \
  --storage-size-bytes 42949672960 \
  --image <https-url> \
  --image-sha256 <sha256>
```

1. The target node must be a member of a healthy `CephCluster`.
2. The node creates `kcore-<vm-id>` in `kcore-vms` with
   `layering,exclusive-lock`. Maps set `rbd_blocklist_on_break_lock`. Live
   migration still maps the image on both nodes; the destination takes the
   lock on its first write after switchover. Images created before this keep
   their features until they are recreated.
3. The controller pushes Nix and waits for the node's rebuild. The VM unit maps
   `/dev/rbd/kcore-vms/kcore-<vm-id>`, seeds it once from the guest image, and
   boots Cloud Hypervisor on it.

Maps use rbd-nbd with writeback cache by default. A node that dies without
unmapping loses what was in that cache. `cachePolicy: writethrough` on the
cluster switches every new map to write-through; it is cluster-wide, not per
VM. `rbd create`, map and unmap run as `client.kcore` once its keyring is on the
node, and fall back to `client.admin` if the file is missing or the call fails.
Snapshots, fencing, and export still use `client.admin`.

Delete resolves the VM by id or name, stops the unit and unmaps on the owning
node, then removes the image. `DeleteCephCluster` is refused while a Ceph VM
still lives on a member, and names the VMs.

### Volumes and snapshots

```bash
kctl create volume pgdata --size-bytes 107374182400 --vm db-1
kctl snapshot volume pgdata
kctl create volume pgdata-clone --from-snapshot <snap>
kctl restore volume pgdata --snapshot <snap>   # VM stopped or volume detached
kctl flatten volume pgdata-clone
kctl resize volume pgdata --size-bytes 214748364800
kctl create snapshot-policy nightly --vm db-1 --schedule @daily --keep 7
kctl export volume pgdata --snapshot <snap> -o pgdata.raw
kctl import volume -f pgdata.raw --name pgdata-copy --storage-class ceph
```

Copy a volume (optionally onto another `CephCluster`) or move a detached LVM
or ZFS volume onto Ceph:

```bash
kctl copy volume pgdata pgdata-dr --to-cluster dr-san
kctl convert volume scratch --to ceph
```

Resize only grows. On a running VM, `--grow-filesystem` grows the guest
filesystem over the guest agent. `kctl create volume … --encrypt` puts LUKS2 on
the host side of the mapped image. Export and import, incremental export with
`--from-snapshot`, and the `backup-operator` role are described in
[`backup.md`](./backup.md).

### Failover safety

- VM units wait for `kcore-placement-confirmed.target`, which the agent reaches
  after its first heartbeat. A node that reboots into an old generation does not
  restart a guest that has moved.
- Before a Ceph VM is restarted elsewhere, a healthy member blocklists the lost
  node's clients and its address range (`NodeStorage.FenceVolume`). Inspect
  with `ceph osd blocklist ls`.
- A VM unit never starts a second Cloud Hypervisor on an image another one is
  serving.

## Day-2 operations

### Add a member

Add the node to `spec.nodes` and apply. While its new OSDs come up the
reconciler holds `norebalance`, applies the pool and `recovery` settings, then
clears the flag on a tick where every OSD is up and in, so backfill starts in
one go under the `recovery` throttles. This has not been run on the garage
cluster. Set conservative
`recovery` values first if the cluster network is shared. A per-OSD weight
ramp is not in this tree.

### Remove a reachable member

Drop the node from `spec.nodes` and apply. The plan refuses unless:

- the survivors still number at least `size` (lower `size` and `minSize`
  first, in the same or an earlier apply, on a 3-node `size: 3` cluster);
- the cluster is `HEALTH_OK` as a survivor reports it;
- no other removal is in progress, and only one member leaves per apply.

The plan warns when only two mons remain. A survivor marks the leaving host's
OSDs out and the removal holds, with `health_message` counting PGs that are
not `active+clean`, until the data has moved. Then the leaving node stops its
daemons, its OSDs are purged and zapped, `kcore.ceph` is dropped from its
configuration, and a survivor runs `ceph mon rm`. Putting the node back in the
spec before teardown cancels the removal.

This path has not been run on the garage cluster. See
[`ceph-lifecycle.md`](./ceph-lifecycle.md#cluster-fabric).

### A dead member

When a host will not come back, remove it from `spec.nodes` and add its id to
`lostMembers` in the same apply. A survivor drains its PGs, purges its OSDs,
and runs `ceph mon rm`. kcore does not try to reach the dead host. The same
size and health checks as a normal removal apply.

`kctl delete ceph-cluster --force` is a different thing: it deletes the
controller record when a member cannot be reached and does not touch Ceph.

### Replace a disk

Change `osdDevice` / `osdDevices` and apply. An OSD that is no longer listed
is marked out, and the reconciler holds, reporting how many PGs are not yet
`active+clean`. Only when every PG is clean does the node purge and zap the
OSD. New disks in the list are prepared in the same pass. The gate is PG state;
check `HEALTH_OK` yourself on a `size: 2` cluster before swapping. Turning
`encryptOsds` on for an existing cluster means replacing every OSD this way.

### Maintenance

```bash
kctl node maintenance <id> on    # cordon, set noout through a survivor, drain VMs
# ... work, reboot ...
kctl node maintenance <id> off   # unset noout, uncordon
kctl get cluster-health          # shows Ceph flags such as noout
```

`on` is refused when the node is the only mon of its cluster, or when its OSDs
going away would leave fewer OSD hosts than `minSize`. Stopping a node agent
for more than 90 seconds triggers failover of its guests, so keep the agent
running until the drain has finished.

### Upgrade Ceph

Ceph moves with the kcore release that changes the pin in
`modules/kcore-ceph.nix`. A cluster update rolls nodes one at a time and, on
the canary, compares Ceph OSD commit latency with the peers. Before a Ceph
member starts it waits for `ceph osd ok-to-stop`, and it holds `noout` while
any member is mid-update. It does not yet order mons before OSDs, wait for
`active+clean` between nodes, or finalize `require-osd-release`, so check
`HEALTH_OK` between nodes yourself. Details:
[`update-lifecycle.md`](./update-lifecycle.md#ceph-in-a-cluster-update).

### External Ceph

`spec.external` with `monHosts`, `pool`, and a client key is refused when
combined with `nodes`. kcore creates no mons, OSDs, or pools. The reconciler
writes `/etc/ceph/ceph.conf` and the keyring on every approved, ready node
that is not an internal Ceph member, reads health through one of them, and
marks the cluster `healthy` only when every such node has the config and
Ceph reports `HEALTH_OK`. Every approved node then counts as a Ceph client for
placement. The node agent maps pool `kcore-vms` as `client.admin`, so
`pool` must be `kcore-vms` and `clientName` must be empty or `client.admin`;
anything else fails with a message that says so. Not exercised on a live
cluster. See [`examples/ceph/external.yaml`](../examples/ceph/external.yaml).

## Shared files and objects

`SharedFilesystem` (CephFS) and `ObjectStore` (RGW) are in
[`cephfs-rgw.md`](./cephfs-rgw.md).

## Safety checklist

- Keep `forceWipe: false` unless you mean it.
- Check `HEALTH_OK` before any removal or disk change.
- Live migration needs TCP 18000–18127 between members.
- A migration that fails with `ALREADY_EXISTS` usually means a stranded receive
  session on the destination. Inspect with `kctl get migrate-session <vm>` and
  clear with `kctl migrate reset-session <vm> --node <id> --force`; see
  [`vm-migration.md`](./vm-migration.md#runbook-a-stranded-receive-session).

## Source map

| Area | Paths |
|---|---|
| Proto | `proto/controller.proto` (`CephCluster*`), `proto/node.proto` (Ceph and live-migrate peer RPCs) |
| Spec, plan, reconciler | `crates/controller/src/ceph_cluster_spec.rs`, `crates/controller/src/ceph_cluster_reconciler.rs` |
| Node Ceph and RBD | `crates/node-agent/src/ceph_bootstrap.rs`, `ceph_preflight.rs`, `rbd_nbd.rs`, `storage/mod.rs`, `grpc/admin.rs` |
| Live migration | `crates/node-agent/src/live_migrate.rs` |
| Units | `modules/kcore-ceph.nix`, `modules/ch-vm/vm-service.nix` |
| CLI | `crates/kctl/src/commands/ceph_cluster.rs` |
| Dashboard | `dashboard/src/CephDetail.tsx`, `dashboard/src/ceph.ts` |
