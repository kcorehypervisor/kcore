# Slurm on kcoreOS: plan

Status: implemented on branch `slurm-cluster-plan`. The controller, node agent,
NixOS module, `kctl`, MCP, dashboard, and docs are in this change. The
one-cluster lab apply has **not** been run: it rebuilds NixOS on live
hypervisors and is blocked on the operator answers at the end of this document.
No node was cordoned, drained, or rebuilt while landing this.

## What this is

Slurm runs as host services on kcoreOS nodes, not inside guest VMs:

| Role | Services |
| --- | --- |
| `controller` | `slurmctld` |
| `dbd` | `slurmdbd` and MariaDB |
| `login` | Slurm client tools and user SSH |
| `compute` | `slurmd` |

One node belongs to at most one SlurmCluster (`slurm_cluster_nodes.node_id` is
the primary key). Several clusters share one kcore installation by using
disjoint nodes. A node may hold several roles inside one cluster.

Compute members are fenced from guests. Plan refuses a compute member that
still hosts VMs. On create, the reconciler cordons compute members, and the VM
scheduler already skips nodes whose status is not `ready`
(`crates/controller/src/scheduler.rs`). `UncordonNode` refuses while a node is
a compute member unless `force` is set. Controller, login, and dbd members may
share a node with guests.

The kcore controller host is refused as a compute member unless
`spec.allowControllerHost` is true. The check compares the target node id with
the replication controller id and the local hostname. If the controller has no
self node id, the hostname check is the fallback and is documented in
`crates/controller/src/slurm_cluster_spec.rs`.

## Resource

Canonical YAML is camelCase.

```yaml
kind: SlurmCluster
metadata:
  name: hpc-a
spec:
  clusterName: hpc-a          # immutable
  controller:
    nodeId: node-101
    address: 10.0.0.11
    iface: enp6s0
  backupController:           # optional
    nodeId: node-102
    address: 10.0.0.12
    iface: enp6s0
  login:
    nodeIds: [node-105]
  accounting:
    enabled: false
    # nodeId defaults to the controller node
  compute:
    - nodeId: node-108
      address: 10.0.0.18
      iface: enp6s0
      partitions: [batch]
      reserve: { cores: 2, memoryMb: 4096 }
      gres: none
  partitions:
    - name: batch
      default: true
      maxTime: INFINITE
      state: UP
  users:
    - name: alice
      uid: 2001
      sshKeys: [ssh-ed25519 AAAA...]
  storage:
    mode: nfs
    exports: [/home, /scratch]
  extraConfig: ""
  allowControllerHost: false
  gpuDriver: false
```

Immutable: `metadata.name`, `spec.clusterName`, `spec.controller.nodeId`.
Everything else is mutable. `plan` returns `create`, `update`, `unchanged`, or
`replace`; `replace` is required for an immutable change and `create` refuses.

`spec.gpuDriver` is the explicit opt-in for the host NVIDIA driver. It is not in
the original resource sketch because the module option defaults to false; it is
needed so `gres: auto` can fail clearly when the driver is off.

## Rendering

`crates/controller/src/slurm_render.rs` renders one `slurm.conf`, one
`cgroup.conf`, and one `gres.conf` per generation. Every member gets the same
files; only the per-node fragment differs by `roles`.

Scheduling defaults in the shared config:

```text
SelectType=select/cons_tres
SelectTypeParameters=CR_Core_Memory
ProctrackType=proctrack/cgroup
TaskPlugin=task/cgroup,task/affinity
SchedulerType=sched/backfill
SrunPortRange=60001-63000
```

`CoreSpecCount` and `MemSpecLimit` come from `reserve.cores` and
`reserve.memoryMb`. `NodeAddr` and `networking.hosts` are rendered from the
spec so resolution does not depend on site DNS. Every string in the Nix
fragment goes through `kcore_sanitize::nix_escape`.

The pinned nixpkgs (`flake.lock` rev
`b40629efe5d6ec48dd1efba650c797ddbd39ace0`) has real
`services.slurm.{server,client,dbdserver,enableStools,clusterName,controlMachine,controlAddr,nodeName,partitionName,extraConfig,extraCgroupConfig}`
and `services.munge` options. The module uses those names. Slurm options that
the upstream module does not expose (`BackupController`, `BackupAddr`,
`AccountingStorageType`, and the `NodeName`/`PartitionName` lines) go through
`extraConfig`. `gres.conf` is added through `extraConfigPaths`. No option-name
mapping was needed beyond that.

## Secrets

At create the controller generates 1024 random bytes for munge and stores them
in `slurm_clusters.munge_key` (plaintext, like the Ceph bootstrap keyring; no new
KMS). The node agent writes `/etc/munge/munge.key` mode `0400` owner
`munge:munge` before the rebuild, outside the Nix store. The key is never logged
or returned by Get.

When accounting is enabled, a slurmdbd password is generated, stored beside the
munge key, written by the agent to `/etc/slurm/dbd-password`, and used to seed
MariaDB through `/etc/slurm/dbd-init.sql`. It is never returned by Get.

Rotation pushes the new key to every member and restarts munge in order `dbd`,
`controller`, `compute`, `login`.

## Storage

- `nfs` (default): the controller exports `spec.storage.exports` (default
  `/home` and `/scratch`) with `services.nfs.server`; other members mount them.
  NFS is a single point of failure.
- `shared-filesystem`: plan refuses this mode. The existing
  `shared_filesystem_reconciler.rs` is still a scaffold and cannot issue a
  CephFS client key. Writing a mount that cannot boot would be worse than the
  refusal.
- `none`: no shared filesystem.

## Network and firewall

Slurm traffic uses each member's management address. Preflight requires the
address on `iface`, the interface not DHCP-managed, not `br0`, not `kbr-*`, and
not the Ceph cluster interface. The firewall accepts TCP 6817 (ctld), 6818
(slurmd), 6819 (dbd), and `SrunPortRange=60001-63000` only from
`memberAddresses` on `iface`; bridge and Ceph interfaces are never opened.

## Accounting, GPUs, HA

Accounting is off by default. When on, the `dbd` role runs MariaDB and
slurmdbd, and the shared config adds `AccountingStorageType=accounting_storage/slurmdbd`.
`accounting.nodeId` defaults to the controller.

`compute[].gres` is `none` or `auto`. `auto` renders `gres.conf` from the
controller's GPU inventory (`ListGpus` / `node_gpus`). `kcore.slurm.gpu.driver`
defaults to false. When `spec.gpuDriver` is true, the module installs the host
NVIDIA driver and keeps those PCI ids out of VFIO binding. If the driver cannot
be built on the node, `gres: auto` plan-fails with a clear message. The driver
is not enabled in the eval test. Licensing and build-on-node cost are the
operator's decision.

`backupController` renders `BackupController`, `BackupAddr`, and
`StateSaveLocation` on the shared storage path. Adding a backup is mutable; the
primary controller node id stays immutable.

## Upgrades

`reconcile_update` is still one-at-a-time and refuses `canary`, `batch`, and
`per-dc`. For nodes in `slurm_cluster_nodes`, members are ordered `dbd`,
`controller` (primary then backup), `compute`, `login`. A compute node is
drained through `SlurmControl` before its rebuild. Members of one Slurm cluster
must be in the same cluster-update run because Slurm requires
dbd >= ctld >= slurmd.

## API and operator surface

New RPCs: `PlanSlurmCluster`, `CreateSlurmCluster`, `GetSlurmCluster`,
`ListSlurmClusters`, `DeleteSlurmCluster`, `RotateSlurmMungeKey`,
`DrainSlurmNode`, `ResumeSlurmNode`. Node agent adds
`CheckSlurmPreflight`, `ApplySlurmConfig`, `GetSlurmStatus`, `SlurmControl`.

```bash
kctl slurm plan -f hpc-a.yaml
kctl apply -f hpc-a.yaml
kctl diff -f hpc-a.yaml
kctl get slurm-cluster hpc-a
kctl describe slurm-cluster hpc-a
kctl delete slurm-cluster hpc-a
kctl slurm drain-node hpc-a node-108 --reason maintenance
kctl slurm resume-node hpc-a node-108
kctl slurm rotate-munge-key hpc-a
```

MCP catalog id `slurm-cluster` (`yamlKind: SlurmCluster`) asks for the controller
node and address, interface, compute nodes and cordon permission, partitions,
first users, storage mode, accounting, and gres. Apply stays behind
`confirm: true`.

Dashboard has a generic list page with name, phase, controller node, compute
count, and idle/alloc/down counts. Terraform records `kcore_slurm_cluster`; the
provider repo is external and is cut at the same `VERSION`.

## Database

Schema version 68 adds:

```text
slurm_clusters(name PK, spec_json, generation, munge_key, dbd_password, status_json, created_at, updated_at)
slurm_cluster_nodes(node_id PK, cluster_name FK, roles, applied_generation, slurm_state, config_hash, updated_at)
```

The down migration drops `slurm_cluster_nodes` then `slurm_clusters`. Older
readers never select these tables.

## Not built

- No VM-guest Slurm image or multi-VM composition resource.
- No tenants, quotas, or LDAP.
- No binary cache, Cachix, Attic, or Harmonia.
- No shared-filesystem mount, because the CephFS client key path is still a
  scaffold.

## Lab questions (not answered here)

1. Which node is the first controller/login node, and which nodes become
   compute? The kcore controller host stays out unless the operator says
   otherwise.
2. Static management addresses and interface names for those nodes.
3. May those compute nodes be cordoned and emptied of guests before apply?
4. Which path or device on the controller node backs `/home` and `/scratch`?
5. First user accounts: name, uid, SSH public key.
