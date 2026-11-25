# Storage and VM movement: what is still missing

A prioritised list of operations an operator will reach for and not find, why
each matters, and the shape of the fix. Prune rows as they land.

**Ceph status lives in [`ceph-lifecycle.md`](./ceph-lifecycle.md).** That page
is the only list of what the Ceph integration does. The Ceph rows below are a
short pointer into it, kept here so this page still reads as one priority list.

Priority is by blast radius: P0 can lose data or strand a running guest, P1
blocks routine day-2 work, P2 is operational comfort.

## P0: data safety

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **A guest that ignores the ACPI power button has no fallback** | In the lab a guest with a second client attached to its serial socket ignored `vm.power-button`; the unit's `ExecStop` then had to SIGTERM the VMM after 25 s, which is a hard kill for the guest. The agent no longer writes into the serial socket, but the trigger is not pinned down. | Re-run the power-button test with one and two serial readers. Give `ExecStop` a middle step between the button and SIGTERM: `vm.pause` and a flush of the block devices, or the guest agent's `poweroff` when it is installed. |
| **Ceph: write-through is cluster-wide, not per VM** | `cachePolicy: writethrough` exists, but writeback stays the default and there is no per-VM choice, so a failover-eligible VM on a writeback cluster can still lose the client cache. | Default write-through for VMs that can fail over; keep writeback for pinned VMs. See [`ceph-lifecycle.md`](./ceph-lifecycle.md#vm-disks-and-data-safety). |
| **Filesystem disks and GPU guests on LVM/ZFS still cannot move** | `MigrateVm` copies LVM and ZFS (dm-mirror while the guest runs, `qemu-img` when it is stopped). `filesystem` disks stay on the node. A local VM that also has a GPU or PCI device is left in place on drain, because a cold GPU move would recreate an empty disk. There is still no replica for failover of a dead node. | Filesystem needs a file copy with the same seed-marker rule. GPU plus local disk needs the copy first, then the existing GPU retarget. |
| **VM backend conversion** | `kctl convert volume --to ceph` and `kctl convert vm --to ceph --yes` exist, but neither has been run on the garage cluster. | Exercise the convert path live; see [`ceph-lifecycle.md`](./ceph-lifecycle.md#vm-disks-and-data-safety). |

## P1: routine day-2 work

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Stranded receive sessions are cleared by hand** | A controller crash mid-migration leaves a receive VMM and an RBD map on the destination; the next attempt fails with `ALREADY_EXISTS`. The runbook in [`vm-migration.md`](./vm-migration.md#runbook-a-stranded-receive-session) is manual by design because liveness is hard to judge. | Record the session in the controller DB with the operation id; on controller start, reconcile sessions whose operation is not running and whose source VM is still on the source. Keep the manual path for the ambiguous case. |
| **Ceph: upgrade ordering and finalize** | A cluster update now waits for `ok-to-stop` and holds `noout`, but does not put mons first, wait for `active+clean` between nodes, or finalize `require-osd-release`. | See [`update-lifecycle.md`](./update-lifecycle.md#ceph-in-a-cluster-update). |
| **Ceph: copy between clusters is one-shot** | `kctl copy volume --to-cluster` streams a snapshot, but an interrupted copy starts again from zero and there is no continuous mode. | Resumable operation record; `rbd-mirror` for a planned cutover. See [`ceph-lifecycle.md`](./ceph-lifecycle.md#vm-disks-and-data-safety). |

## P2: visibility and comfort

| Gap | Why it matters | Shape of the fix |
|-----|----------------|------------------|
| **Each node's bridge has its own MAC** | After a live migration the guest keeps its MAC but the gateway side of `kbr-default` changes host; peers that cached the old bridge MAC see a short blackhole until ARP ages out. | Send a gratuitous ARP / unsolicited NA from the destination for every guest address when the handoff finishes (`arping -U` on the tap), or give the bridges a stable MAC per network. |
| **Live migration progress is opaque** | `kctl migrate vm` blocks until done; the only signal is the final message. Long memory dirtying loops look like a hang. | Poll Cloud Hypervisor's migration state from the source agent and report it on the operation (`kctl get operation`): bytes transferred, dirty rounds, downtime. |
| **No bandwidth cap for migration** | Migration shares the cluster NIC with guest storage I/O. The lab's 5G uplink is already the bottleneck. Ceph recovery has its own knobs in `CephCluster.spec.recovery`. | A migration rate limit when Cloud Hypervisor gains one. |
| **Image cache management** | Cached images are sha-checked at seed time, but `EnsureImage` keeps an existing file whose digest differs (the path may be a live `filesystem` VM disk), so a corrupt cache entry for a block-backed VM fails every seed on that node until an operator deletes the file. | Keep the no-delete rule for `filesystem` disks; for the shared image cache, delete on digest mismatch and refetch on the next push. |

## Done (for reference)

- Live migration from a slower-TSC host (CPU/TSC preflight, patched Cloud
  Hypervisor, stale socket cleanup).
- `drain` / `cordon` survive a node-agent restart and controller replication.
- A VM unit never starts a second VMM for a guest that is already running;
  cached images are sha-verified right before seeding a volume.
- Firewall tables live in their own atomically reloaded unit.
- `kctl attach volume` / `kctl detach volume` on a running Ceph VM (hotplug).
- The node agent only probes `<vm>.sock`, never the serial console socket next
  to it, and gives up on a VMM after 5 s instead of stalling the heartbeat.
- Volume export and import, changed-extent export (`--from-snapshot`), the
  `backup-operator` role, and snapshots on ZFS and LVM as well as Ceph
  ([`backup.md`](./backup.md)). Flatten and per-volume encryption are still
  Ceph-only.
- Ceph items that used to be listed here (placement gate after reboot,
  watch-expired fencing, exclusive-lock, guarded member removal, lost members,
  pool tuning, typed health, maintenance with `noout`, OSD drain before purge,
  backfill hold on a new host) are tracked in
  [`ceph-lifecycle.md`](./ceph-lifecycle.md).
