# Client parity

Every feature on `main` should be reachable from each client that can express it. This page is the reconciliation list: what each client covers, what it deliberately leaves out, and what is still open.

The clients are `kctl` (commands and `apply -f` manifests), the Terraform provider, the MCP server, and the dashboard. `kcore_rpc` in the MCP server calls any implemented controller method, so the typed MCP kinds and operations are a convenience layer, not the limit of what an agent can do.

## Feature families

| Family | kctl | `apply -f` kind | Terraform | MCP | Dashboard | Example |
| --- | --- | --- | --- | --- | --- | --- |
| VM | yes | `VM` | `kcore_vm` | `vm` | yes | `examples/vm` |
| Container | yes | `Container` | `kcore_workload` | `container` | yes | `examples/container` |
| Network | yes | `Network` | `kcore_network` | `network` | yes | `examples/network` |
| SSH key | yes | `SshKey` | `kcore_ssh_key` | `ssh-key` | yes | `examples/ssh-key` |
| Security group | yes | `SecurityGroup` | `kcore_security_group`, `kcore_security_group_attachment` | `security-group` | yes | `examples/security-groups` |
| PostgreSQL | yes | `PostgreSQL` | `kcore_postgresql` | `postgresql` | yes | `examples/postgresql` |
| Volume, snapshot, snapshot policy | yes | `Volume`, `SnapshotPolicy` | `kcore_volume`, `kcore_volume_snapshot`, `kcore_snapshot_policy` | `volume`, `volume-snapshot`, `snapshot-policy`, volume operations | yes | `examples/storage` |
| Disk layout | yes | `DiskLayout` | `kcore_disk_layout` | `disk-layout` | yes | `examples/storage` |
| Ceph cluster | yes | `CephCluster` | `kcore_ceph_cluster` | `ceph-cluster` | yes | `examples/ceph` |
| Shared filesystem | yes | `SharedFilesystem` | `kcore_shared_filesystem` | `shared-filesystem` | yes | `examples/ceph` |
| Object store | yes | `ObjectStore` | `kcore_object_store` | `object-store` | yes | `examples/ceph` |
| Object user | yes | `ObjectUser` | `kcore_object_user` | delete only | yes | `examples/object-user` |
| Kubernetes cluster | yes | `KubernetesCluster` | `kcore_kubernetes_cluster` | `kubernetes-cluster` | yes | `examples/kubernetes` |
| Cluster update | yes | `ClusterUpdate` | `kcore_cluster_update` | `cluster-update` | yes | `examples/cluster-update` |
| Cluster version and canary | `update version`, `update cluster promote` | n/a | `kcore_cluster_version` | `set-version`, `promote-update` | Release page | n/a |
| Slurm cluster | yes | `SlurmCluster` | `kcore_slurm_cluster` | `slurm-cluster` | yes | `examples/slurm` |
| Ingress gateway, route, WAF policy | yes | `IngressGateway`, `IngressRoute`, `WafPolicy` | `kcore_ingress_gateway`, `kcore_ingress_route`, `kcore_waf_policy` | `ingress-gateway`, `ingress-route`, `waf-policy` | yes | `examples/ingress` |
| vGPU host (NVIDIA, Intel, AMD) | yes | `NodeVgpu` | `kcore_vgpu_host` | `node-vgpu` | node actions | `examples/vgpu` |
| Operator and roles | yes | `Operator` | `kcore_operator` | `operator`, role operations | yes | `examples/operator` |
| Node | yes | `NodeInstall` | `kcore_node` | `node` (read, delete), node operations | yes | `examples/node-install` |
| Cluster context | yes | `Cluster` | `kcore_cluster` | `cluster` | n/a | `examples/cluster` |

## What changed in this reconciliation

kctl:

- `kctl create object-user` and `kctl delete object-user`, and `kind: ObjectUser` in `apply -f`. The Terraform docs already pointed at a command that did not exist.
- `apply -f` takes several documents in one file, separated by `---`. All are checked before the first is applied.
- New `apply -f` kinds `Volume` (create if missing, immutable after), `SnapshotPolicy` (upsert) and `Operator` (create, then reconcile roles).
- `apply -f` routes `KubernetesCluster` and `ClusterUpdate`. Both were only reachable through their own subcommands.
- `apply -f` refuses an unknown `kind`. It used to send the file to the controller as a NixOS configuration, which turned a misspelled kind into a configuration push. A file with no `kind` is still treated as Nix.
- Examples with a parse test for `CephCluster`, `SharedFilesystem`, `ObjectStore`, `ObjectUser`, `KubernetesCluster`, `DiskLayout` and `ClusterUpdate`.
- The `ClusterUpdate` example in `docs/cluster-updates.md` used keys that `kctl` ignores (`drainPolicy`, `rollbackPolicy`, `strategy.type`). It now matches the real fields.

MCP:

- New kinds: `kubernetes-cluster` (plan, apply as create or update, read, delete), `operator` (apply creates it and grants roles), `node` (read, delete).
- New operations: `attach-volume`, `detach-volume`, `resize-volume`, `restore-volume`, `flatten-volume`, `format-volume`, `mount-volume`, `unmount-volume`, `encrypt-volume`, `rotate-node-certs`, `reboot-update-node`, `set-route-waf-mode`, `cancel-vm-operation`, `delete-object-user`, `grant-operator-role`, `revoke-operator-role`.
- `cluster-update` now carries the whole release manifest: selector, activation, approval policy, update kind, schema versions and tested OS pins. Before, an update created from MCP had no selector.
- `object-user` creation is refused with a pointer to kctl. The server redacts secrets, and the secret is returned once, so the user would be created with credentials nobody can read.

Terraform (separate repository, [#11](https://github.com/kcorehypervisor/terraform-provider-kcore/pull/11)):

- `kcore_postgresql` resource and data source.
- Read-only list data sources: `kcore_volumes`, `kcore_security_groups`, `kcore_ssh_keys`, `kcore_operators`, `kcore_ingress_gateways`, `kcore_ingress_routes`, `kcore_waf_policies`, `kcore_cluster_updates`.
- Bindings regenerated from this proto (adds `GetCrlResponse.issuer_pem`).

Dashboard:

- Slurm: create from a JSON spec, plan, drain node, resume node, delete with force.
- Ingress: plan for gateways, routes and WAF policies; set a gateway secret file.
- Nodes: reboot, enable and disable a vGPU host for any vendor, and the vGPU record in the node detail.
- The Terraform hints on the ingress, WAF, PostgreSQL, GPU and container views name the resources that exist.

## Controller methods operators do not call

These belong to node agents, the gateway agent, the backup vault, or a console stream. A client may still wrap one, such as `kctl` for the backup methods, but a gap here is not a missing feature.

| Methods | Caller |
| --- | --- |
| `RegisterNode`, `Heartbeat`, `SyncVmState`, `SyncWorkloadState`, `SignNodeCsr` | node agent |
| `RenewNodeCert` | node agent; operators use `rotate-node-certs` |
| `ReportWafEvents` | ingress gateway agent |
| `OpenVmConsole`, `SendVmConsoleInput` | console streams; `kctl` attaches with `AttachVmConsole` |
| `FreezeVmFilesystems`, `PrepareInstantRecovery`, `FinalizeInstantRecovery`, `ExposeVolumeSnapshot`, `UnexposeVolumeSnapshot`, `ListVolumeChangedExtents`, `ReadVolumeBlocks`, `WriteVolumeBlocks` | the backup vault |

## Still open

- **Terraform actions.** Migrate, drain, cordon, approve, reboot, backup, restore, certificate rotation and revocation are one-shot actions, not state, so they stay as `kctl` commands. Terraform has no data source for the vault or for block-level backup state.
- **Dashboard.** No view for block-level backup methods, which the vault uses.
- **Lab validation.** The Slurm and ingress paths have not been applied on the lab cluster. Intel and AMD vGPU have not run on real hardware.
