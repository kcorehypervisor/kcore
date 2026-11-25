# Ceph lifecycle: what is in this tree

This is the only list of what kcore's Ceph support does today. Every other
page ([`ceph.md`](./ceph.md), [`cephfs-rgw.md`](./cephfs-rgw.md),
[`storage-and-mobility-gaps.md`](./storage-and-mobility-gaps.md), the site's
SAN page) links here instead of keeping its own status. Change a row in the
same commit as the code that changes it.

The Croit briefing PDF is a snapshot from before this table existed. It is not
regenerated, and where it disagrees with this page, this page wins.

Each row has one status:

- **shipped**: in this tree, covered by tests, and run on the garage cluster
  (`kcore-san`, three Dells, Ceph 20.2.4) or in a release.
- **implemented, not exercised live**: the code path exists and has unit
  tests, but nobody has run it against the garage cluster yet.
- **not in this tree**: missing. Where a building block exists (a proto field,
  a node RPC) the note says so, but the operator workflow is not there.

A spec field that the controller accepts and validates but no reconciler acts
on counts as **not in this tree**.

## Cluster fabric

| Feature | Status | Where | Notes |
|---|---|---|---|
| Ceph from NixOS units, no cephadm or Rook | shipped | `modules/kcore-ceph.nix` | Ceph 20.2.4 from a second nixpkgs pin; see [`update-lifecycle.md`](./update-lifecycle.md). |
| Bootstrap mons, mgrs, OSDs from a `CephCluster` | shipped | `crates/controller/src/ceph_cluster_reconciler.rs`, `crates/node-agent/src/ceph_bootstrap.rs` | Self-only first monmap; later mons join with `ceph mon add`. |
| OSD dm-crypt (`encryptOsds`, default `true` in YAML) | shipped | `ceph-volume --dmcrypt` in `ceph_bootstrap.rs` | Immutable once an OSD exists. The Terraform provider sends `false` when the argument is omitted. |
| Preflight: RAID virtual disks, DHCP mon address, occupied disk, nixpkgs pin | shipped | `crates/node-agent/src/ceph_preflight.rs` | `kctl diff` / `kctl apply` print per-node results. |
| Add a member by adding it to `spec.nodes` | shipped | reconciler | Preflight, bootstrap and OSD creation have run on the garage cluster. The `norebalance` hold on the new OSDs is a separate row below and has not. |
| Remove a reachable member (drain, wait for active+clean, purge, `mon rm`) | implemented, not exercised live | `plan_member_removal` in `ceph_cluster_spec.rs`, `drain_member` in `ceph_cluster_reconciler.rs` | One host per apply, needs `HEALTH_OK` and survivors ≥ `size`. Do not run it on `kcore-san` without an explicit operator yes. |
| Remove a lost member (`lostMembers`) | implemented, not exercised live | `lost_members` in `ceph_cluster_spec.rs`, `PurgeLostCephHost` in `crates/node-agent/src/grpc/admin.rs` | Survivor purges the dead host's OSDs and runs `ceph mon rm`; `TeardownCephNode` is skipped. |
| `kctl delete ceph-cluster` stops daemons first; `--force` deletes only the record | shipped | `DeleteCephCluster` in `crates/controller/src/grpc/controller.rs` | Refused while Ceph VMs live on members. `--force` is not a lost-host purge. |
| Node roles `full`, `mon`, `osd` | implemented, not exercised live | `generate_ceph_nix` in `ceph_cluster_reconciler.rs` | `mon` runs mon and mgr; `osd` runs OSDs only. Unit tests cover the generated Nix for both. |
| Pools in `spec.pools` (size, min_size, application, pg_num, CRUSH rule, compression, quota) | implemented, not exercised live | `CephPoolSpec`, `EnsureCephPool` in `admin.rs` | `kcore-vms` stays the default RBD pool. |
| Erasure-coded pools | implemented, not exercised live | `pool_type: erasure` + `erasureProfile` | Validation refuses an erasure pool without a profile. |
| Recovery knobs: max backfills, recovery max active, scrub window, nearfull ratio | implemented, not exercised live | `CephRecoverySpec` → `EnsureCephPool` | Scrub hours of `-1` leave Ceph's default. |
| Wire encryption (`msMode: secure` / `prefer-crc`) | implemented, not exercised live | `ms_cluster_mode` / `ms_service_mode` in `admin.rs` | `enable-msgr2` alone is not encryption. |
| `client.kcore` key capped to the VM pool | implemented, not exercised live | `EnsureCephClientKey`, `rbd_nbd::client_args` | The reconciler ensures the key on every member; each node writes `/etc/ceph/ceph.client.kcore.keyring`. Caps are `profile rbd` on `kcore-vms` (needed for `rbd_blocklist_on_break_lock`). rbd map, unmap and create pass `-n client.kcore` when the file exists and retry as `client.admin` if the call fails. Snapshots, fencing, export and `rbd device list` still run as `client.admin`. |
| Per-node CRUSH `weight` and `deviceClass` | implemented, not exercised live | `apply_osd_crush_overrides` in `ceph_cluster_reconciler.rs`, `ReweightCephOsd` / `SetCephOsdDeviceClass` | Applied to OSDs returned as `created` by `ReconcileCephOsds`. |
| Controlled backfill when a host joins (`norebalance`) | implemented, not exercised live | `hold_backfill` / `settle_backfill_ramp` in `ceph_cluster_reconciler.rs` | New OSDs set `norebalance`; cleared when OSDs are up/in and `backfill_pgs == 0`. |
| Disk replacement: drain the OSD, wait for active+clean, then purge | implemented, not exercised live | `reconcile_ceph_osds` in `crates/node-agent/src/grpc/admin.rs`, `ReconcileCephOsdsResponse.pending_retire` | An unwanted OSD is marked out; purge and zap run only once all PGs are active+clean. Until then the reconciler tick reports `ceph not healthy yet` and retries. |
| Maintenance mode with `noout` (`kctl node maintenance <node> on\|off`) | implemented, not exercised live | `set_node_maintenance` in `crates/controller/src/grpc/controller.rs` | Cordons, sets `noout` through a surviving member, then drains VMs. Refused when the node is the only mon or survivors would drop below `minSize`. `off` unsets `noout` and uncordons. |
| Typed health (quorum, OSDs up/in, PG states, flags, checks, capacity) | implemented, not exercised live | `typed_health_from_status_json` in `controller.rs`, `GetCephHealth` in `admin.rs` | Raw `ceph -s` JSON is kept next to it. |
| Dashboard Ceph cluster page | implemented, not exercised live | `dashboard/src/CephDetail.tsx`, `dashboard/src/ceph.ts` | Read-only; mutations stay plan-then-apply. |
| External Ceph (`spec.external`) | implemented, not exercised live | `crates/controller/src/ceph_external.rs`, `ApplyCephClientConfig` | Writes `/etc/ceph` on approved ready nodes that are not internal members; VMs map `kcore-vms` as `client.admin`. |
| Ceph-aware rolling upgrade (`ok-to-stop`, `noout` per node, mons first, active+clean) | implemented, not exercised live | `crates/controller/src/ceph_update_gate.rs` | Wired into the cluster update reconciler before prepare. Finalize `require-osd-release` is `kctl ceph finalize-osd-release --yes`; see [`update-lifecycle.md`](./update-lifecycle.md#ceph-in-a-cluster-update). |
| CRUSH failure domain above host (rack, room, stretch) | implemented, not exercised live | `EnsureCephCrushTopology`, `ceph_crush.rs`, `spec.failureDomain` / `stretch` | Host stays the default; rack/room create buckets and `kcore-<domain>` rules. |
| Rotation of `client.kcore` | implemented, not exercised live | `RotateCephClientKey` in `admin.rs` | `auth del` then `get-or-create` with the same caps. |
| Rotation of mon key / OSD dm-crypt | implemented, not exercised live | `RotateCephMonKey`, `RotateCephOsdDmcrypt`, `kctl ceph rotate` | Mon key rotates on a member; dm-crypt drains/zaps/recreates one OSD per apply. |
| Mgr Prometheus module | implemented, not exercised live | `EnableCephMgrModule` after pool ensure | |
| VictoriaMetrics scrape of mgr `:9283` | implemented, not exercised live | `metrics_targets::render_ceph_targets` → `ceph.json`; firewall 9283 in `kcore-ceph.nix` | |
| `ceph.health` webhook when status leaves `HEALTH_OK` | implemented, not exercised live | `EVENT_CEPH_HEALTH` in `webhooks.rs`, `maybe_emit_ceph_health_webhook` | |
| Mgr dashboard module | implemented, not exercised live | `EnableCephMgrModule dashboard`, `ConfigureCephDashboard` | Stored credentials drive ac-user create; kcore CephDetail stays the primary UI. |
| krbd when the kernel accepts the cluster's key type | implemented, not exercised live | `rbd_nbd::krbd_usable` | Probe only. rbd-nbd remains the only map path. No krbd vs rbd-nbd benchmark has been run. |

## VM disks and data safety

| Feature | Status | Where | Notes |
|---|---|---|---|
| RBD root disks: create, map, seed once (`kcore.seeded=1`) | shipped | `CephAdapter` in `crates/node-agent/src/storage/mod.rs`, `modules/ch-vm/vm-service.nix` | Image names are `kcore-<vm-id>`. |
| Live migration with both sides mapped | shipped | `crates/node-agent/src/live_migrate.rs` | |
| Volumes: attach, detach, hotplug, resize, snapshot, clone, restore, flatten | shipped | volume RPCs in `controller.rs` | |
| Per-volume LUKS2 (`--encrypt`) | shipped | `storage/mod.rs` | |
| Snapshot policies | shipped | `SnapshotPolicy` | |
| Changed-block export and import (`kctl export volume --from-snapshot`, `kctl import volume`) | shipped | `crates/node-agent/src/storage/export.rs`, `crates/kctl/src/commands/volume.rs` | See [`backup.md`](./backup.md). |
| `backup-operator` role | shipped | `crates/controller/src/auth.rs`, `crates/controller/src/grpc/rbac_matrix.rs` | |
| Placement gate: VM units wait for `kcore-placement-confirmed.target` | shipped | `modules/ch-vm/vm-service.nix`, `crates/node-agent/src/ha.rs` | A rebooted node does not restart a guest that failed over. |
| Failover fencing: blocklist watchers and the lost host's `/32` before reassigning | shipped | `FenceVolume` in `storage/mod.rs`, caller in `controller.rs` | Verified on the garage cluster 2026-10-11 (node-103 → node-118). |
| New images get `layering,exclusive-lock`; maps set `rbd_blocklist_on_break_lock` | shipped | `create_volume` in `storage/mod.rs`, `crates/node-agent/src/rbd_nbd.rs` | Ran on the Dells from a lab build since 2026-10-11; `main` had `layering` only until this branch. Old images keep their features until recreated. Dual-map live migration still works: the destination takes the lock on its first write. |
| `imageFeatures` override per cluster | implemented, not exercised live | pushed via `KCORE_RBD_IMAGE_FEATURES` in `generate_ceph_nix`, `CephConfig.image_features` | `create_volume` uses the node config value when set, else `layering,exclusive-lock`. |
| Write-through cache (`cachePolicy: writethrough`) | implemented, not exercised live | pushed via `KCORE_RBD_CACHE_POLICY`, `rbd_nbd::map_with_policy`, `modules/ch-vm/vm-service.nix` | Node config or the env var selects the policy for maps; default stays writeback. |
| Volume copy between CephClusters (`kctl volume copy --to-cluster`) | implemented, not exercised live | `CopyVolumeToCluster` in `volume_move_rpc.rs`, `kctl volume copy` | Snapshots the source, creates the destination RBD, streams extents through the controller. One-shot; no resumable operation record. |
| Convert an LVM or ZFS volume to Ceph | implemented, not exercised live | `ConvertVolume`, `kctl convert volume --to ceph` | Detached, unmounted volume with no snapshots, not attached to a running VM. |
| Convert a VM root disk to Ceph | implemented, not exercised live | `ConvertVmStorage`, `kctl convert vm --to ceph --yes` | Stops the guest when `--yes`, streams the local disk, flips `storage_backend`, leaves the local disk in place. |
| `rbd-mirror` | implemented, not exercised live | `spec.mirroring`, `EnsureRbdMirror`, bootstrap/import peer, `enableRbdMirror` in `kcore-ceph.nix` | Continuous pool mirror between two CephClusters. |
| RBD namespaces, per-volume QoS, trash instead of `rbd rm` | implemented, not exercised live | `CreateVolume` parameters / `rbd trash move`, `List/PurgeVolumeTrash`, `kctl volume trash` | Pool quota remains `quotaBytes`. |

## CephFS, RGW, Kubernetes

| Feature | Status | Where | Notes |
|---|---|---|---|
| `SharedFilesystem` and `ObjectStore` resources (API, YAML, Terraform, MCP) | shipped | `shared_filesystem_spec.rs`, `object_store_spec.rs` | The records exist; see the rows below for what is created. |
| CephFS pools, `ceph fs new`, `ceph fs authorize` per client | implemented, not exercised live | `crates/controller/src/shared_filesystem_reconciler.rs` | Runs only when the controller host has a `ceph` CLI that reaches the cluster. |
| MDS daemons on members (`kcore.ceph.enableMds`) | implemented, not exercised live | `CephServiceFlags` / `CephNixExtras` in `ceph_cluster_reconciler.rs` | Set on every `full`/`mon` member when any `SharedFilesystem` references the cluster. |
| Container / Slurm `shared-filesystem` storage mode | implemented, not exercised live | plan path in `controller.rs` | Warns when the filesystem is not healthy yet; no longer refused for missing keys. |
| virtio-fs for VMs | implemented, not exercised live | volume role `filesystem`, `ch-vm` `filesystems`, virtiofsd | Cold migrate only; live migrate refuses virtio-fs VMs. |
| RGW pools and `radosgw-admin user create` for object users | implemented, not exercised live | `crates/controller/src/object_store_reconciler.rs` | Needs `ceph` and `radosgw-admin` on the controller host. |
| RGW daemons on members (`kcore.ceph.enableRgw`) | implemented, not exercised live | `CephServiceFlags` / `CephNixExtras` | Set on `ObjectStore.members`, or every `full`/`mon` member when the list is empty. |
| RBD CSI for a `KubernetesCluster` | implemented, not exercised live | `kubernetes_local::rbd_manifests` (Deploy/DS/RBAC/SC), `upsert_ceph_csi_secret` | Image pin `quay.io/cephcsi/cephcsi:v3.13.1`. Secret applied via kube API from `client.kcore`. |
| CephFS CSI | implemented, not exercised live | `cephfs_manifests`, SC `kcore-cephfs` | When a SharedFilesystem exists on the same CephCluster. |
| NFS-Ganesha, iSCSI, NVMe-oF, multi-site RGW | out of scope | | Not planned. |

## Where the old documents disagreed

| Topic | What used to be written | What is true |
|---|---|---|
| Exclusive lock | `ceph.md`: "no exclusive-lock, required for dual-map". Briefing: "in testing". | New images carry it. Dual-map live migration works with it. |
| Placement gate | Briefing: "in testing". Gaps doc: open P0. | Shipped. |
| Watch-expired fencing | Gaps doc: open P0. | Shipped (host range blocklist). |
| Changed-block export | Briefing: planned. Gaps doc: open. | Shipped. |
| `backup-operator` | Gaps doc: open. | Shipped. |
| Member removal | Briefing: "in testing". | Implemented, not exercised live. |
| CephFS / RGW | Site: "in progress". Briefing: planned. | Pools, filesystems, auth, RGW users, and MDS/RGW daemon enable flags are implemented, not exercised live. |

## Clients

Every `CephCluster` field reaches the controller the same way from:

- YAML: `crates/kctl/src/commands/ceph_cluster.rs`, examples in [`examples/ceph/`](../examples/ceph/)
- Terraform: `kcore_ceph_cluster` in the `terraform-provider-kcore` repository (`feat/ceph-lifecycle` worktree)
- MCP: `cephMessage` in `kcore-mcp/packages/server/src/requests.ts`
- Dashboard: `dashboard/src/catalog.ts` and the Ceph cluster page

A controller older than a field ignores it, because every field is additive.
