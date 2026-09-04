# kcore SAN (Ceph)

kcore SAN is the cluster’s distributed shared block storage fabric. It is powered by **Ceph** on NixOS; the controller fills the gaps that NixOS does not declare (FSID/keyrings, `ceph-volume` OSD prepare, pools, health, and later RBD lifecycle).

Public product docs: [kcore SAN (Ceph)](https://kcorehypervisor.com/docs/user/storage-vsan.html). Roadmap status: **Phases 1–2 shipped** (live migration still planned).

## Lab topology (3 Dell towers)

| Role | Disk | Notes |
|------|------|--------|
| OS + MON/MGR state | Disk 1 (SSD) | Existing `kctl node install --os-disk …` |
| One OSD | Disk 2 (~500 GB SSD) | Whole device for `ceph-volume`; **not** a local LVM/ZFS VM data disk |

- **Daemons:** each of the three nodes runs MON + MGR + 1 OSD.
- **Network:** dedicate the MikroTik 10Gb fiber fabric as Ceph **cluster/replication** (`clusterNetwork`). Use the existing server 10Gb (or another VLAN) as **public/client** (`publicNetwork`).
- **Capacity:** ~1.5 TiB raw; with `size=3` / `minSize=2`, usable capacity is about **~0.5 TiB**. Budget a small number of 40–80 GB VM disks for this lab.

## Phase 1 — Declarative `CephCluster` (fabric)

VMs still use `filesystem` / `lvm` / `zfs`. Phase 1 only brings Ceph to `HEALTH_OK`.

### Manifest

```yaml
kind: CephCluster
metadata:
  name: lab-san
spec:
  fsid: "2f3f204e-65ca-4b69-a86c-bf8408f5c792"
  publicNetwork: "10.10.0.0/24"
  clusterNetwork: "10.20.0.0/24"   # MikroTik 10Gb
  replication:
    size: 3
    minSize: 2
  # forceWipe: false   # set true only to erase an OSD disk with existing signatures
  nodes:
    - nodeId: dell-1
      monAddr: "10.10.0.11:6789"
      clusterAddr: "10.20.0.11"
      publicIface: eth1
      clusterIface: eth2
      osdDevice: /dev/nvme0n1
    - nodeId: dell-2
      monAddr: "10.10.0.12:6789"
      clusterAddr: "10.20.0.12"
      publicIface: eth1
      clusterIface: eth2
      osdDevice: /dev/nvme0n1
    - nodeId: dell-3
      monAddr: "10.10.0.13:6789"
      clusterAddr: "10.20.0.13"
      publicIface: eth1
      clusterIface: eth2
      osdDevice: /dev/nvme0n1
```

### Operator commands

```bash
kctl apply -f lab-san-ceph.yaml
kctl get ceph-cluster
kctl describe ceph-cluster lab-san
kctl delete ceph-cluster lab-san   # does not automatically wipe OSDs
```

Discover NICs and disks before writing the manifest:

```bash
kctl --node <addr>:9091 node nics
kctl --node <addr>:9091 node disks
```

### What the control plane does

1. Persist desired state (`ceph_clusters` + generation + bootstrap secrets).
2. Reconciler pushes per-node Nix (`kcore-ceph` / `services.ceph`, cluster name always `ceph`, full `mon_host` list) via node `ApplyCephConfig`.
3. First node generates mon/admin/bootstrap-osd keyrings with `ceph-authtool`; the controller redistributes that package to every member, then each node runs `ceph-mon --mkfs` / mgr auth.
4. Bootstraps OSD disks with `BootstrapCephOsd` (`ceph-volume lvm create`, activates systemd units), refusing non-empty devices unless `forceWipe: true`.
5. Ensures RBD pool `kcore-vms` (`EnsureCephPool`) and polls `GetCephHealth` until `HEALTH_OK` (degraded/failed stays queued).

NixOS module: [`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix). Runtime packages come from nixpkgs Ceph; KCore owns bootstrap RPCs in the node-agent.

## Phase 2 — RBD VM disks

Once the fabric is healthy:

- Create VMs with `--storage-backend ceph --storage-size-bytes …` (rejected until a healthy `CephCluster` includes the target node).
- Controller allocates a cluster-scoped RBD image in pool `kcore-vms` (volume row independent of node); size is passed to `rbd create` in **MiB**.
- Node-agent maps RBD and seeds the guest image once (`qemu-img convert`) before Cloud Hypervisor starts; any Ceph member node can run the VM.
- `kctl drain <node>` reassigns Ceph VMs to other members without copying disks (cold stop/start until live migration).
- Deleting a Ceph VM best-effort removes the RBD image.

```bash
kctl create vm app-1 \
  --storage-backend ceph \
  --storage-size-bytes 42949672960 \
  --image <https-url> \
  --image-sha256 <sha256>
```

Local backends remain available for latency-sensitive workloads on a single node.

## Later

- Cold migrate UX (`kctl migrate`) and drain policies tuned for Ceph.
- Cloud Hypervisor **live** migration over the 10Gb path with shared RBD.

## Safety

- Do not point `osdDevice` at the OS disk.
- Default `forceWipe: false` refuses OSD prepare when the device has partitions/filesystems/LVM signatures.
- Keep Ceph traffic on the dedicated fabric; do not rely on a congested management NIC for replication.
