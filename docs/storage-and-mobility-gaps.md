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
| **The node agent probes the serial console** | `list_vm_names` matches every `*.sock` under `/run/kcore`, so each heartbeat writes an HTTP `GET /api/v1/vm.info` into `<vm>.serial.sock`. The guest sees it as console input; with a second client attached it correlated with a guest that ignored the ACPI power button. | Skip `*.serial.sock` in the socket scan and put a request timeout on the API probe. Then re-run the power-button test with one and two serial readers to pin the trigger. |
| **Hard kill of an rbd-nbd writer leaves dirty writeback** | Volumes are mapped with `--rbd-cache=true --rbd-cache-policy=writeback`. A VMM or node that dies without `power-button` → unmap loses up to the cache size of acknowledged writes; the guest's journal then disagrees with the image. | Map with `--rbd-cache-policy=writethrough` for images whose VM is not pinned to a node, or flush (`rbd device flush` is not available for nbd; `blockdev --flushbufs` on the mapped device) at every `vm.pause` and before live-migration send. |
| **OSD replacement is a purge** | `ReconcileCephOsds` retires any OSD not in `osdDevices` by `out` → stop → `purge` → `zap` in one go. Swapping a disk on a `size: 2` cluster, or on a cluster that is already degraded, removes the last copy of some PGs before anything else holds it. | Reuse the member-removal drain: `osd out`, hold until every PG is `active+clean`, then purge. Refuse while the cluster is not `HEALTH_OK`, same as member removal. |
| **No `noout` around planned node work** | Rebooting a member for a kernel update (cluster-updates) or a drain lets Ceph mark its OSDs out after `mon_osd_down_out_interval` (10 min) and start a full rebalance that then has to be undone. | `ceph osd set noout` when a member is cordoned or a system update is activated on it; `unset` on uncordon / when it is back and `HEALTH_OK`. Expose it in `kctl get cluster-health`. |
| **Live migration is Ceph-only; local-disk VMs have no planned move** | `MigrateVm` rejects `filesystem` / `lvm` / `zfs` with `FAILED_PRECONDITION`. Draining a node that runs local VMs leaves them where they are, so a node cannot be emptied for hardware work without downtime and manual copying. | Storage migration: `qemu-img convert` (or `zfs send`) to the destination, then the existing cold path. Live with local disks needs Cloud Hypervisor block migration, which it does not offer; cold with copy is the realistic target. |
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

## P2: visibility and comfort

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Each node's bridge has its own MAC** | After a live migration the guest keeps its MAC but the gateway side of `kbr-default` changes host; peers that cached the old bridge MAC see a short blackhole until ARP ages out. | Send a gratuitous ARP / unsolicited NA from the destination for every guest address when the handoff finishes (`arping -U` on the tap), or give the bridges a stable MAC per network. |
| **Live migration progress is opaque** | `kctl migrate vm` blocks until done; the only signal is the final message. Long memory dirtying loops look like a hang. | Poll Cloud Hypervisor's migration state from the source agent and report it on the operation (`kctl get operation`): bytes transferred, dirty rounds, downtime. |
| **No bandwidth cap for migration or backfill** | Migration and Ceph recovery share the cluster NIC with guest storage I/O. The lab's 5G uplink is already the bottleneck. | `osd_recovery_max_active` / `osd_max_backfills` in the Ceph config; a migration rate limit when Cloud Hypervisor gains one. |
| **Cluster health is a single string** | `kctl get ceph-cluster` shows `HEALTH_WARN` and a message; the operator has to ssh for PG state, slow ops, nearfull. | Parse `ceph -s` into typed status (PGs by state, nearfull OSDs, slow ops, mon quorum) and expose it on the resource and in the dashboard. |
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
