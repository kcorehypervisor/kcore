# Bare-metal Kubernetes on kcoreOS

Status: **plan**. No module, API, or lab apply yet.

Source: the [roadmap](https://kcorehypervisor.com/roadmap.html) section **Bare-Metal** (Q4 2026). That card says PostgreSQL, MySQL, and Kubernetes are installed on kcoreOS itself. Each one is a declarative NixOS service on the node, applied the same way as the rest of the host. They do not run inside a VM.

This document is the implementation plan for the Kubernetes line of that card. PostgreSQL and MySQL stay on the same host-service pattern and are sequenced after this slice.

The working tree for this work is `/mnt/data/kcore-rust-k8s`, cloned from `/mnt/data/kcore-rust` at `37b56f8` on `cursor/ceph-bringup-fixes-64a3`.

## What “done” means

An operator declares a `KubernetesCluster`. The controller renders a Nix fragment. The node agent writes it, runs `nixos-rebuild`, and the NixOS `services.kubernetes` units come up on the selected kcoreOS nodes. `kubectl` against that apiserver can schedule a pod on the host. Guest VMs keep using `ch-vm` and cloud-hypervisor. kcore’s own API stays the single-operator gRPC API described in [security-k8s-vs-kcore.md](./security-k8s-vs-kcore.md).

## Pattern to copy

Ceph is the host-service precedent. Follow it, with a smaller first slice (one control-plane node that also runs workloads).

| Ceph today | Kubernetes |
|---|---|
| [`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix) wraps the NixOS package | `modules/kcore-kubernetes.nix` wraps `services.kubernetes` |
| Installer seeds an empty `/etc/nixos/kcore-ceph.nix` and copies the module (`flake.nix`) | Same for `kcore-kubernetes.nix` |
| `ApplyCephConfig` refreshes the module from the agent binary and rebuilds | `ApplyKubernetesConfig` does the same |
| `generate_ceph_nix` in `crates/controller/src/ceph_cluster_reconciler.rs`, strings passed through `nix_escape` | `generate_kubernetes_nix` |
| `PlanCephCluster` dry-builds on each member before create | `PlanKubernetesCluster` |
| `kctl diff` / `kctl apply` / get / delete | Same verbs for the new kind |
| MCP kind `ceph-cluster` (advise, plan, apply) | MCP kind `kubernetes-cluster` |

NixOS owns packages and units. The controller owns membership, CIDRs, and certificates that must be the same on every member. The node agent owns the rebuild and the preflight. That split is the one documented in [ceph.md](./ceph.md).

## First slice

One member. Its role is control plane and worker (`services.kubernetes.roles` includes master and node). `easyCerts` is on, so the NixOS module generates the local PKI. etcd data stays on the OS disk at the module’s default state directory. The CNI is the module’s flannel, pinned to an interface the operator names.

Workers, extra control-plane members, CSI, and ingress are later phases. The container track in [support-for-containerd.md](./support-for-containerd.md) stays separate: that design runs OCI workloads through kcore’s own API. This design runs upstream Kubernetes on the host.

### Defaults the module will ship

These are the values `PlanKubernetesCluster` accepts unless the spec overrides them. Plan still prints them.

| Field | Default | Why |
|---|---|---|
| `clusterCidr` | `10.244.0.0/16` | Flannel’s usual pod range. Outside `192.168.40.0/24` (lab management) and `192.168.47.0/24` (lab Ceph). |
| `serviceCidr` | `10.96.0.0/12` | NixOS kube-apiserver default service range. |
| `clusterDns` | `10.96.0.10` | CoreDNS inside `serviceCidr`. |
| Flannel | enabled | Ships with `services.kubernetes`. |
| `easyCerts` | true while there is one control-plane node | NixOS generates the PKI locally. |
| etcd | on the OS disk | The lab OS disk is a separate device from the Ceph OSD. |

Preflight rejects a spec whose pod CIDR, service CIDR, or advertise address overlaps `192.168.40.0/24`, `192.168.47.0/24`, or any `NodeNetwork` range on that node. Preflight also rejects a flannel interface that is `br0`, a `kbr-*` bridge, or the node’s Ceph cluster interface.

### Example manifest

```yaml
kind: KubernetesCluster
metadata:
  name: lab
spec:
  clusterCidr: 10.244.0.0/16
  serviceCidr: 10.96.0.0/12
  nodes:
    - nodeId: node-105
      role: control-plane
      advertiseAddress: 192.168.40.105
      iface: enp6s0
```

`role: control-plane` on a one-node cluster also schedules pods (the rendered Nix sets both Kubernetes roles). A later worker uses `role: worker` and does not run etcd or the apiserver.

`advertiseAddress` must be a static address or a DHCP reservation. Preflight fails when the named interface is DHCP-managed, the same rule Ceph uses for mon addresses.

The example node is a placeholder. The garage lab’s kcore controller is `node-107`. Leave that host out of `spec.nodes` until the operator explicitly puts it there. A kubelet on the controller host is root on the same machine that stores cluster state.

## API

Add to `proto/controller.proto`, next to `CephCluster`:

```protobuf
message KubernetesNodeSpec {
  string node_id = 1;
  string role = 2;              // "control-plane" or "worker"
  string advertise_address = 3; // static IP, no port
  string iface = 4;             // flannel / apiserver bind iface
}

message KubernetesClusterSpec {
  string cluster_cidr = 1;
  string service_cidr = 2;
  string cluster_dns = 3;
  repeated KubernetesNodeSpec nodes = 4;
}

message KubernetesCluster {
  string name = 1;
  int64 generation = 2;
  KubernetesClusterSpec spec = 3;
  KubernetesClusterStatus status = 4;
}
```

RPCs, matching Ceph: `PlanKubernetesCluster`, `CreateKubernetesCluster`, `GetKubernetesCluster`, `ListKubernetesClusters`, `DeleteKubernetesCluster`.

Immutable after create: `name`, `clusterCidr`, `serviceCidr`. Changing those returns `replace` from plan and requires a new cluster. Mutable: node list, per-node `iface`, per-node `advertiseAddress` only when the node is not yet joined.

SQLite: `kubernetes_clusters` (spec JSON, generation) and `kubernetes_cluster_status` (phase, message). Phases: `pending`, `applying`, `ready`, `degraded`, `deleting`, `failed`.

## Nix module

`modules/kcore-kubernetes.nix` options:

- `enable`
- `role` (`control-plane` or `worker`)
- `masterAddress`
- `clusterCidr`, `serviceCidr`, `clusterDns`
- `iface`
- `easyCerts`

When `enable` is false the module sets `services.kubernetes` off and leaves guest VM config alone. Assertions:

- `clusterCidr` and `serviceCidr` are disjoint
- `clusterDns` sits inside `serviceCidr`
- `masterAddress` is set when `enable` is true
- `iface` is non-empty when flannel is on
- a worker’s `masterAddress` is the control-plane advertise address

Rendered fragment, written to `/etc/nixos/kcore-kubernetes.nix`:

```nix
{ ... }: {
  kcore.kubernetes = {
    enable = true;
    role = "control-plane";
    masterAddress = "192.168.40.105";
    clusterCidr = "10.244.0.0/16";
    serviceCidr = "10.96.0.0/12";
    clusterDns = "10.96.0.10";
    iface = "enp6s0";
    easyCerts = true;
  };
}
```

Every interpolated string goes through `nix_escape`. The installer and `ApplyKubernetesConfig` copy `modules/kcore-kubernetes.nix` to `/etc/nixos/modules/kcore-kubernetes.nix` and import both files from the host `configuration.nix`, the same way `flake.nix` seeds Ceph.

Firewall: open TCP 6443 and 10250 on `iface` only. Flannel’s UDP 8472 stays on that interface. Do not open those ports on the Ceph VLAN interface.

If the pinned nixpkgs cannot evaluate `services.kubernetes`, use the Ceph overlay approach: take the package from a pinned revision that is in `cache.nixos.org`, and record the pin in the module. Check that with `nix build .#checks.x86_64-linux.kubernetes-module-eval` before writing a reconciler.

## Node agent

`ApplyKubernetesConfig` in `crates/node-agent/src/grpc/admin.rs`:

1. Refresh `/etc/nixos/modules/kcore-kubernetes.nix` from `include_str!` of the module.
2. Write `/etc/nixos/kcore-kubernetes.nix`.
3. `nixos-rebuild` test, then switch, under the existing apply lock so it cannot overlap a VM or Ceph rebuild.

`PlanKubernetesCluster` calls a preflight on each member:

- nixpkgs pin resolves
- dry-build of the fragment succeeds
- `iface` exists
- `iface` is not `br0`, not `kbr-*`, and not the Ceph cluster interface
- advertise address is present on `iface` and the interface is not DHCP-managed
- TCP 6443, 10250, and 2379 are free on a new control plane
- pod and service CIDRs do not overlap host or Ceph networks

Plan prints one line per node plus the CIDRs it will render. Create refuses to run when plan is not ok.

## Reconciler

`crates/controller/src/kubernetes_cluster_reconciler.rs`.

Single control-plane member:

1. Plan (preflight). Refuse on any blocking issue.
2. Persist spec and bump generation.
3. `ApplyKubernetesConfig` on that node.
4. Poll `GetNixApplyStatus` until the rebuild finishes.
5. Readiness: `kube-apiserver` and `kubelet` are active. Set phase `ready`. A failed rebuild sets `failed` and leaves the previous generation’s Nix on disk only if the switch did not activate; otherwise status records the active generation.

Delete:

1. Render `enable = false` and apply it.
2. Wait until the Kubernetes units are inactive.
3. Delete the controller row.

Delete stops when the member cannot be reached. A `force` flag drops the row anyway, matching `DeleteCephCluster`. Force does not uninstall units on an unreachable node; the next successful apply of an empty fragment does.

## Operator surface

- `kctl diff -f lab-k8s.yaml` runs plan and prints the Nix action (`create`, `update`, `unchanged`, `replace`).
- `kctl apply -f lab-k8s.yaml` persists only after plan is ok.
- `kctl get kubernetes-cluster` and `kctl delete kubernetes-cluster lab`.
- MCP catalog entry `kubernetes-cluster` with advise questions: member node ids, advertise addresses, interface names, pod CIDR, service CIDR. Apply stays behind `confirm: true`.
- Dashboard can wait until get/list returns a phase. The first UI is a single status card, the same weight as the Ceph health card.

## Phases

### 1. Module and eval test

- Add `modules/kcore-kubernetes.nix`.
- Add `tests/kubernetes-module-eval.nix` and a flake check `kubernetes-module-eval`, copied from `tests/ceph-module-eval.nix`.
- Seed the empty fragment and the module import in the installer path inside `flake.nix`.
- Unit-test nothing in Rust yet.

Exit: `nix build .#checks.x86_64-linux.kubernetes-module-eval` succeeds, including the `enable = false` fragment.

### 2. Render and proto

- Messages and RPCs above. `buf`/proto build stays green.
- `generate_kubernetes_nix` plus tests: escaping of quotes and `${`, CIDR overlap helper, role mapping (`control-plane` → master+node, `worker` → node).
- SQLite migration for the two tables.

Exit: `cargo test -p kcore-controller -- kubernetes` passes. No node is contacted.

### 3. Apply and plan RPC

- `ApplyKubernetesConfig` and preflight on the node agent.
- Controller `PlanKubernetesCluster` dials each member.
- Create/Get/List/Delete persist state. Create on a one-node spec calls apply.

Exit: controller tests with a fake node agent cover plan failure, create, and delete. A real node is still untouched.

### 4. kctl and MCP

- CLI verbs and the MCP kind, advise questions, and plan path.
- MCP tests follow `kcore-mcp/packages/server/test/plan.test.ts`.

Exit: `kctl diff` against a fake controller prints the CIDRs and the target node. Apply is refused without a successful plan.

### 5. One-node lab apply

Blocked until the operator answers the questions below and agrees to the printed plan. This phase rebuilds NixOS on a live hypervisor.

Exit: `kubectl get nodes` shows the member Ready, and a pause pod reaches Running. `kctl get kubernetes-cluster lab` shows `ready`.

### 6. Workers

A worker node gets `role: worker`, the control-plane advertise address as `masterAddress`, and the PKI the control plane already generated. The controller copies that material the way the Ceph reconciler redistributes keyrings, then pushes the worker fragment.

One control-plane node remains the only etcd. A second control plane is a later design (stacked etcd or an external etcd) and is not part of phase 6.

### 7. After the roadmap line is met

- PostgreSQL and MySQL as sibling host services (`services.postgresql`, the NixOS MySQL service), each with its own module, empty fragment, and resource. They do not share the Kubernetes reconciler.
- Local persistent volumes on a directory the operator names on the OS disk.
- Ceph CSI only after the SAN cluster is healthy and the operator asks for it. Rook is not the integration path; kcore already reconciles Ceph itself.
- Ingress and a default StorageClass.

## Tests

- Nix eval check from phase 1.
- Rust tests for Nix escaping, CIDR checks, and role mapping. Property tests if the string builder grows past a handful of fields. No new Kani harness: this is string rendering, which the repo covers with proptest.
- Controller tests with a fake node client for plan / create / delete.
- MCP plan tests for missing answers and for a complete spec.

A live `nixos-rebuild` on garage hardware is phase 5 and waits for an explicit yes.

## Risks

- `nixos-rebuild switch` on a hypervisor restarts host units. The apply lock serializes it with VM and Ceph applies. Plan names the node so the operator can drain guests first.
- Pods share the host kernel. A privileged pod is root on kcoreOS. Membership is an operator choice; the plan default keeps the kcore controller host out of the cluster.
- Flannel’s VXLAN and kcore’s VM VXLAN both use overlay UDP. Binding flannel to the management NIC, and rejecting `kbr-*`, keeps the two overlays off the same bridge. MTU on that NIC has to leave room for the overlay header; preflight warns when the NIC MTU is under 1500.
- The Kubernetes NixOS module is tied to the flake’s nixpkgs pin. Phase 1 discovers whether that pin evaluates. An overlay pin is the fallback, as with Ceph 20.2.4.
- etcd and container images land on the OS disk. The lab OS disk is a 300 GB Intel SSD. Image pulls need a garbage-collection limit in the kubelet config so a full disk cannot block `nixos-rebuild`.
- TCP 6443 on the management network is a new listener. The module opens it on `iface` only.

## Blocking questions before phase 5

Code phases 1–4 do not need these. A lab apply does.

1. Which node is the first member? The placeholder is `node-105`. `node-107` is the kcore controller and stays out unless you say otherwise.
2. Advertise address and interface for that node. The lab management NIC on the Dell is `enp6s0` at `192.168.40.103`. Lenovo names differ. Confirm the static address.
3. Keep the default pod CIDR `10.244.0.0/16` and service CIDR `10.96.0.0/12`?
4. Is flannel on the management NIC acceptable for the first cluster?
5. May phase 5 run `nixos-rebuild switch` on that node while it may still be hosting guests?
