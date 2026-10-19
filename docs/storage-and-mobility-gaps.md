# Ceph operations and VM movement: what is still missing

A prioritised list of the operations an operator of a kcore SAN will reach for
and not find, with the reason each matters and the shape of the fix. It is
written against the code as of the cluster-ops hardening work (CPU/TSC
preflight, guarded member removal, `kcore-firewall-<net>`, seed-time sha
check) and should be pruned as items land.

Priority is by blast radius: P0 can lose data or strand a running guest, P1
blocks routine day-2 work, P2 is operational comfort.

## P0: data safety

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **A rebooted node restarts guests that were failed over** | Failover now blocklists the lost node's rbd-nbd clients before the move, but the blocklist names client instances. A node that was down because it rebooted comes back with its old NixOS generation, in which the moved VM is still an `autoStart` unit, and starts it with a fresh client nonce. The controller removes the unit on the first heartbeat's config push, which is after the guest has already replayed the ext4 journal on an image another node is writing. | The VM start script must not trust the generation it booted from: before mapping, ask the local agent (or the controller) whether this VM is still assigned here, and refuse with a clear exit status otherwise. Alternatively hold every `kcore-vm-*` unit behind a `kcore-placement-confirmed.target` the agent reaches only after its first successful heartbeat. |
| **Fencing has nothing to blocklist when the watch has already expired** | `FenceVolume` blocklists the clients `rbd status` lists. A node cut off from the Ceph public network as well as the controller loses its watch after 30 s and shows no client; the image is reported fenced, yet if the network heals the old rbd-nbd reconnects and writes. | Enable `exclusive-lock` on new images and have the new mapping break the lock (`rbd_blocklist_on_break_lock`), so a returning client is rejected by the lock rather than by an address list; or blocklist the node's whole address with `ceph osd blocklist range add` (verify first that it does not also blocklist the node's OSD front-side sessions). |
| **A guest that ignores the ACPI power button has no fallback** | In the lab a guest with a second client attached to its serial socket ignored `vm.power-button`; the unit's `ExecStop` then had to SIGTERM the VMM after 25 s, which is a hard kill for the guest (see the writeback item). The agent no longer writes into the serial socket, but the trigger is not pinned down. | Re-run the power-button test with one and two serial readers. Give `ExecStop` a middle step between the button and SIGTERM: `vm.pause` + `vm.snapshot`-free flush of the block devices, or the guest agent's `poweroff` when it is installed. |
| **Hard kill of an rbd-nbd writer leaves dirty writeback** | Volumes are mapped with `--rbd-cache=true --rbd-cache-policy=writeback`. A VMM or node that dies without `power-button` → unmap loses up to the cache size of acknowledged writes; the guest's journal then disagrees with the image. | Map with `--rbd-cache-policy=writethrough` for images whose VM is not pinned to a node, or flush (`rbd device flush` is not available for nbd; `blockdev --flushbufs` on the mapped device) at every `vm.pause` and before live-migration send. |
| **OSD replacement is a purge** | `ReconcileCephOsds` retires any OSD not in `osdDevices` by `out` → stop → `purge` → `zap` in one go. Swapping a disk on a `size: 2` cluster, or on a cluster that is already degraded, removes the last copy of some PGs before anything else holds it. | Reuse the member-removal drain: `osd out`, hold until every PG is `active+clean`, then purge. Refuse while the cluster is not `HEALTH_OK`, same as member removal. |
| **No `noout` around planned node work** | Rebooting a member for a kernel update (cluster-updates) or a drain lets Ceph mark its OSDs out after `mon_osd_down_out_interval` (10 min) and start a full rebalance that then has to be undone. | `ceph osd set noout` when a member is cordoned or a system update is activated on it; `unset` on uncordon / when it is back and `HEALTH_OK`. Expose it in `kctl get cluster-health`. |
| **Filesystem disks and GPU guests on LVM/ZFS still cannot move** | `MigrateVm` copies LVM and ZFS (dm-mirror while the guest runs, `qemu-img` when it is stopped). `filesystem` disks stay on the node. A local VM that also has a GPU or PCI device is left in place on drain, because a cold GPU move would recreate an empty disk. There is still no replica for failover of a dead node. | Filesystem needs a file copy with the same seed-marker rule. GPU plus local disk needs the copy first, then the existing GPU retarget. |
| **Backend conversion** | A VM created on `lvm` cannot become `ceph` (or the reverse). The only way onto the SAN is a new VM and a reinstall. | `kctl volume convert --to ceph`: stop guest, `rbd import` the LV, flip `storage_backend`, push Nix, start. Same drain-then-copy plumbing as the item above. |

## P1: routine day-2 work

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Stranded receive sessions are cleared by hand** | A controller crash mid-migration leaves a receive VMM and an RBD map on the destination; the next attempt fails with `ALREADY_EXISTS`. The runbook in [`vm-migration.md`](./vm-migration.md#runbook-a-stranded-receive-session) is manual by design because liveness is hard to judge. | Record the session in the controller DB with the operation id; on controller start, reconcile sessions whose operation is not running and whose source VM is still on the source. Keep the manual path for the ambiguous case. |
| **Maintenance mode is two commands and a judgement call** | Cordon + drain + wait + do the work + uncordon, with no single state the dashboard shows and nothing that sets `noout` or checks Ceph quorum before the node goes down. | `kctl node maintenance <id> [--off]`: cordon, set `noout`, drain Ceph VMs live (cold fallback), refuse if the node is the only mon or if taking it down breaks `min_size`, then the reverse. |
| **Removing a member while it is unreachable** | The drain runs through a survivor, but `TeardownCephNode` and the `ceph mon rm` handshake need the leaving node to answer. A dead node stays "retired, pending" forever. | A `--lost` path: after the drain, survivors run `ceph osd purge` for the dead host's ids and `ceph mon rm` directly, and the controller forgets the node. Needs an explicit flag; this is destructive on purpose. |
| **Adding a member reuses the full create path** | Growing from 3 to 4 nodes works, but the new node's OSDs join with default weight immediately and the backfill is uncontrolled. | `osd crush reweight` ramp (or `norebalance` until all OSDs are in, then unset) driven by the reconciler; surface backfill progress in cluster status. |
| **Pool tuning is fixed** | `kcore-vms` is created with `size`/`min_size` only; PG count is whatever `pg_autoscaler` picks, and there is no way to set a CRUSH rule (e.g. SSD-only), compression, or a second pool. | Pool spec in the manifest: `pools: [{name, size, minSize, crushRule, pgNum, compression}]`; `EnsureCephPool` becomes idempotent per pool. |
| **Volume data movement between clusters** | One `CephCluster` per controller is the tested shape. A second SAN (or a rebuild) has no path to move images. | `rbd export | rbd import` through the controller with a resumable operation, or `rbd-mirror` configuration for a planned cutover. |

## P1: backup and export

Design and command sketches: [`backup.md`](./backup.md).

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Guest data never leaves the cluster** | `kctl backup` copies the controller database only. Snapshots live in the same pool or on the same node as the volume, so a lost pool, a dead LVM/ZFS host, or an attacker with cluster-admin takes the copies with the data. | `NodeStorage.ExposeSnapshot` + `ReadVolumeExtents` behind a controller passthrough, surfaced as `kctl export volume --snapshot <s> -o file.raw` (sparse). The same hooks are what the kcore Vault service pulls from. |
| **Snapshots are Ceph-only** | `require_ceph` rejects `zfs` and `lvm` volumes, so `SnapshotPolicy` and `kctl snapshot volume` do nothing for host-backed VMs, which are the common case on single-node and lab installs. | `zfs snapshot` + `zfs hold`, `lvcreate -s` (thin) in `storage/mod.rs` behind the existing `SnapshotVolume` RPC; drop the Ceph check in the controller. |
| **No changed-block tracking** | Any export is a full read of the volume, which on a 1 TiB data disk is hours per run and most of it unchanged. | `ListChangedExtents` from `rbd diff`, `zstream dump` WRITE records, `thin_delta`; `--from-snapshot` on `kctl export` writes a sparse delta. |
| **No way to write a volume from outside** | Restoring or moving a disk image in means `qemu-img convert` by hand on the node, with the VM's `storage_backend` and volume rows edited afterwards. | `WriteVolumeExtents` into a fresh unattached volume, surfaced as `kctl import volume -f file.raw`. Also the plumbing for the backend-conversion item above. |
| **Backup tools need vm-admin** | The only roles are `read-only`, `vm-admin`, `cluster-admin`; a backup identity that can snapshot and read must also be able to delete every VM. | `backup-operator` role: read-only plus snapshot/expose/read/write and create-for-restore; deletes only for resources it labelled. |

## P2: visibility and comfort

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Each node's bridge has its own MAC** | After a live migration the guest keeps its MAC but the gateway side of `kbr-default` changes host; peers that cached the old bridge MAC see a short blackhole until ARP ages out. | Send a gratuitous ARP / unsolicited NA from the destination for every guest address when the handoff finishes (`arping -U` on the tap), or give the bridges a stable MAC per network. |
| **Live migration progress is opaque** | `kctl migrate vm` blocks until done; the only signal is the final message. Long memory dirtying loops look like a hang. | Poll Cloud Hypervisor's migration state from the source agent and report it on the operation (`kctl get operation`): bytes transferred, dirty rounds, downtime. |
| **No bandwidth cap for migration or backfill** | Migration and Ceph recovery share the cluster NIC with guest storage I/O. The lab's 5G uplink is already the bottleneck. | `osd_recovery_max_active` / `osd_max_backfills` in the Ceph config; a migration rate limit when Cloud Hypervisor gains one. |
| **Cluster health is a single string** | `kctl get ceph-cluster` shows `HEALTH_WARN` and a message; the operator has to ssh for PG state, slow ops, nearfull. | Parse `ceph -s` into typed status (PGs by state, nearfull OSDs, slow ops, mon quorum) and expose it on the resource and in the dashboard (`/ceph/:name` detail with capacity pies + device-class max sizes via `GetCephClusterCapacity` / `ceph df`). |
| **Image cache management** | Cached images are sha-checked at seed time now, but `EnsureImage` deliberately keeps an existing file whose digest differs (the path may be a live `filesystem` VM disk), so a corrupt cache entry for a block-backed VM fails every seed on that node until an operator deletes the file. | Keep the no-delete rule for `filesystem` disks; for the shared image cache, delete on digest mismatch and refetch on the next push. |

## Done in the hardening pass (for reference)

- Live migration from a slower-TSC host (CPU/TSC preflight, patched Cloud
  Hypervisor, stale socket cleanup).
- `drain` / `cordon` survive a node-agent restart and controller replication.
- A VM unit never starts a second VMM for a guest that is already running;
  cached images are sha-verified right before seeding a volume.
- Ceph member removal is refused unless it is safe, and drains the host first.
- Firewall tables live in their own atomically-reloaded unit.
- Automatic failover blocklists the lost node's Ceph clients
  (`NodeStorage.FenceVolume`) before a Ceph VM is reassigned, and leaves the
  VM alone when the image is open from anywhere else.
- `kctl attach volume` / `kctl detach volume` on a running Ceph VM
  (hotplug) work: the map carries the volume serial, and hot-remove resolves
  that serial to Cloud Hypervisor's device id.
- The node agent only probes `<vm>.sock` for `vm.info`, never the serial
  console socket next to it, and gives up on a VMM after 5 s instead of
  stalling the heartbeat.
