# Backup, snapshots and volume export

This page describes what kcore Community provides for protecting data today,
the open hooks that are planned so any backup tool can drive kcore volumes, and
how that relates to the kcore Vault backup service shipped on the private
channel (see [`release.md`](./release.md)).

## What exists today

| Capability | Command | Scope |
|------------|---------|-------|
| Controller database copy | `kctl backup -o snapshot.sqlite`, `kctl restore controller -f snapshot.sqlite --confirm` | Controller state only (VMs, volumes, networks, nodes). **Not** guest data. See [`vm-migration.md`](./vm-migration.md). |
| Volume snapshot | `kctl snapshot volume <name> [--consistency quiesced]` | Ceph RBD volumes. `quiesced` runs `fsfreeze` in the guest over SSH first. |
| Scheduled snapshots | `SnapshotPolicy` resource (`kctl apply -f policy.yaml`) | Ceph RBD, crash-consistent, pruned to `keep`. |
| Clone and rollback | `kctl create volume --from-snapshot`, `kctl restore volume`, `kctl flatten volume` | Ceph RBD. |

Snapshots stay inside the cluster. They protect against operator mistakes and
bad upgrades, not against the loss of the Ceph pool, the node that holds an
LVM or ZFS volume, or an attacker with cluster-admin. Copying guest data out of
the cluster needs the export hooks below.

## Open hooks (protobuf / gRPC)

These are defined in the canonical protos and implemented in open-core kcore.
They are usable from `kctl` (and any mTLS gRPC client) without a paid component.

| Layer | Proto | Service |
|-------|-------|---------|
| Node | [`proto/node.proto`](../proto/node.proto) | `NodeStorage`: `SnapshotVolume`, `ExposeSnapshot`, `UnexposeSnapshot`, `ListChangedExtents`, `ReadVolumeExtents` (stream), `WriteVolumeExtents` (stream) |
| Controller | [`proto/controller.proto`](../proto/controller.proto) | `Controller`: `CreateVolumeSnapshot`, `ExposeVolumeSnapshot`, `UnexposeVolumeSnapshot`, `ListVolumeChangedExtents`, `ReadVolumeBlocks` (stream), `WriteVolumeBlocks` (stream), `FreezeVmFilesystems` |

Annotated sketches (same messages, with notes): [`proto-snippets-node.proto`](./proto-snippets-node.proto),
[`proto-snippets-controller.proto`](./proto-snippets-controller.proto).
Codegen is via `tonic-build` in each crate's `build.rs` (`buf generate` is not required).

### Snapshots for every backend

`SnapshotVolume`, `DeleteVolumeSnapshot`, `CloneVolume` and `RollbackVolume`
on `NodeStorage` take an optional `storage_class` (`ceph` \| `zfs` \| `lvm`)
and dispatch accordingly:

| Backend | Snapshot | Clone | Rollback |
|---------|----------|-------|----------|
| `ceph` | `rbd snap create` (exists) | `rbd clone` | `rbd snap rollback` |
| `zfs` | `zfs snapshot pool/vol@s` + `zfs hold` | `zfs clone` | `zfs rollback` |
| `lvm` thin | `lvcreate -s -n vol_s vg/vol` | `lvcreate -s` of the snapshot | `lvconvert --merge` |
| `lvm` linear | `lvcreate -s -L <cow-size>` | not supported | `lvconvert --merge` |
| `filesystem` | not supported (stop the VM and copy the file) | | |

The controller drops its `require_ceph` check on these paths, and
`SnapshotPolicy` can select host volumes (`storageClass: zfs`).

### Expose a snapshot as a read-only device

`NodeStorage.ExposeSnapshot(volume, snapshot)` returns a read-only block device
path on the node for the life of the lease; `UnexposeSnapshot` releases it.

- `ceph`: `rbd-nbd map --read-only pool/image@snap`
- `zfs`: `zfs set snapdev=visible`, `/dev/zvol/pool/vol@snap`
- `lvm`: activate the snapshot LV (`lvchange -ay -K`)

Leases expire after an idle timeout, so a crashed client never leaves a
device mapped.

### Changed extents between two snapshots

`NodeStorage.ListChangedExtents(volume, from_snapshot, to_snapshot)` returns
`[ {offset, length} ]` of the regions that differ, so an incremental copy reads
only what changed.

- `ceph`: `rbd diff --from-snap A --whole-object --format json image@B`
- `zfs`: WRITE records from `zfs send -i @A pool/vol@B | zstream dump -v`
- `lvm` thin: `thin_delta` over a reserved metadata snapshot
- `lvm` linear, `filesystem`: not available; the response says so and the
  caller reads the whole volume

### Stream extents in and out

`NodeStorage.ReadVolumeExtents` (server stream) reads the requested extents of
an exposed device in fixed 4 MiB aligned blocks, eliding all-zero blocks.
`NodeStorage.WriteVolumeExtents` (client stream) writes blocks into a fresh,
unattached volume for restore. tonic needs its HTTP/2 windows raised for these
to run at line rate (see the snippet notes).

### `kctl export` and `kctl import`

```bash
kctl snapshot volume db-root --consistency quiesced --name pre-upgrade
kctl export volume db-root --snapshot pre-upgrade -o db-root.raw
kctl import volume -f db-root.raw --name db-root-copy --storage-class zfs --node node-119
```

`export` writes a sparse raw image (zero blocks become holes). `--from-snapshot
<older>` writes only the changed extents as a sparse delta that `import --base`
can apply. This also closes the "no `rbd export`" gap listed in
[`storage-and-mobility-gaps.md`](./storage-and-mobility-gaps.md).

### Operator role `backup-operator`

A new operator role between `read-only` and `vm-admin`: everything `read-only`
grants, plus snapshot create and delete, expose, changed extents, read and
write extents, `CreateVolume`, `CreateVm`, `SetVmDesiredState`,
`GuestFsFreeze`, and `DeleteVm` / `DeleteVolume` only for resources labelled
`kcore-vault/restore`. It cannot delete arbitrary VMs or volumes and has no
access to `ControllerAdmin`. Issue it with `kctl create operator backup --role
backup-operator`.

### Webhook events

`vm.created`, `vm.deleted` and `vm.migrated` join `vm.state.changed` in
[`webhooks.md`](./webhooks.md), so an external backup tool learns about new
or moved VMs without polling.

### ObjectStore hardening

The RGW `ObjectStore` resource gains `objectLock: true` (bucket created with
Object Lock, default `COMPLIANCE` retention) and `ObjectUser` policy templates
`ingest` (put only), `reader`, `janitor`. See [`cephfs-rgw.md`](./cephfs-rgw.md).

## kcore Vault (paid)

kcore Vault is the backup product built on these hooks and distributed on the
private channel. In outline:

- Whole-VM backups (spec plus every volume) on a schedule, crash-, filesystem-
  or application-consistent, incremental after the first full.
- A pull model: cluster nodes hold no credential that can write to or delete
  from a repository.
- Content-addressed, encrypted, deduplicated repositories on a ZFS vault box
  and on S3 with Object Lock, replicated vault to vault or to a second site.
- Every target reports how immutable it really is (`storage-enforced`,
  `appliance-enforced`, `none`), and policies refuse mutable targets by default.
- Restore to a new VM, into another kcore cluster, or into an empty cluster
  from the repository alone; scheduled restore tests.

Community users can build their own flow with `kctl export` on a schedule and
any object-storage or deduplicating tool of their choice; the hooks above are
all that is needed.
