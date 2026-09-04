# kcore SAN (Ceph)

kcore SAN is the cluster’s distributed shared block storage fabric. It is powered by **Ceph** on NixOS; the controller fills the gaps that NixOS does not declare (FSID/keyrings, `ceph-volume` OSD prepare, pools, health, and later RBD lifecycle).

Public product docs: [kcore SAN (Ceph)](https://kcorehypervisor.com/docs/user/storage-vsan.html). Roadmap status: **In Progress**.

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

1. Persist desired state (`ceph_clusters` + generation).
2. Reconciler pushes per-node Nix (`kcore-ceph` / `services.ceph`) via node `ApplyCephConfig`.
3. Bootstraps OSD disks with `BootstrapCephOsd` (`ceph-volume`), refusing non-empty devices unless `forceWipe: true`.
4. Polls `GetCephHealth` until status is healthy (or records degraded/failed).

NixOS module: [`modules/kcore-ceph.nix`](../modules/kcore-ceph.nix). Runtime packages come from nixpkgs Ceph; KCore owns bootstrap RPCs in the node-agent.

## Phase 2 — RBD VM disks

Once the fabric is healthy:

- Create VMs with `--storage-backend ceph --storage-size-bytes …`.
- Controller allocates a cluster-scoped RBD image in pool `kcore-vms` (volume row independent of node).
- Node-agent maps RBD before Cloud Hypervisor starts; any Ceph member node can run the VM.
- Drain / reassignment of `ceph` VMs does **not** require copying local disks (still cold stop/start until live migration).

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
