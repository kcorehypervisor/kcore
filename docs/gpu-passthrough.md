# GPU and PCI passthrough

A VM can take exclusive ownership of host PCI devices. The node binds each
address to `vfio-pci` and cloud-hypervisor opens it with `--device`.

This is whole-device passthrough. The host driver releases the device when
the guest starts. Live migration does not move these devices. A cold migrate
of a named device attaches a free device of the same family on the destination.

GPUs are for AI compute. HDMI audio, and a USB controller on the same slot,
are passed only because they share the GPU's IOMMU group. They are not
separate GPUs.

## What you can attach

`kctl pci list` shows every device the nodes have reported, with its kind,
class, IOMMU group, and whether a VM can take it. `kctl pci list --local`
reads the machine you are on. `--type gpu`, `nic`, `nvme`, or `raw` filters
the table. `kctl gpu list` is the GPU rows only.

| Kind | Command | Name |
| --- | --- | --- |
| AI GPU | `--gpu radeon0` | `radeon`, `nvidia`, `intel`, `accel` plus a number. A family (`radeon`) takes the first free card. |
| NIC | `--nic nic0` | `nic0`, or family `nic` |
| NVMe | `--nvme nvme0` | `nvme0`, or family `nvme` |
| Raw | `--pci 0000:03:00.0` | Repeat the flag for every function in the IOMMU group |

A device is free to assign when its IOMMU group contains only that device
and functions on the same slot (the audio companion, for example). A group
that also contains SATA, USB, or a bridge on another slot is `blocked`.
The integrated GPU on this development host (`0000:0d:00.0`) is blocked
for that reason. Do not attach it.

```bash
kctl create vm ai-guest \
  --target-node node-a \
  --gpu radeon0 \
  --cpu 8 --memory 32G \
  --image https://example.com/debian.raw \
  --image-sha256 <sha256> \
  --storage-backend filesystem \
  --storage-size-bytes 21474836480
```

`--target-node` is required. Names are per node: `radeon0` is the lowest
address of that family on that machine. One device belongs to one VM there.

The same assignment in a manifest:

```yaml
apiVersion: kcore/v1
kind: VM
metadata:
  name: ai-guest
spec:
  cpu: 8
  memoryBytes: 34359738368
  targetNode: node-a
  gpus:
    - radeon0
  storageBackend: filesystem
  storageSizeBytes: 21474836480
```

`spec.devices`, `spec.nvmes`, and `spec.pciNics` are extra names (`nic0`,
`nvme0`). `spec.pciDevices` is the raw BDF list. Do not set both names and
raw addresses. Omitting them on a later apply leaves the stored list
unchanged. Changing it is rejected; delete the VM and create it again.

Short addresses (`03:00.0`) are stored as `0000:03:00.0`.

## Migration

```bash
kctl migrate vm ai-guest --target-node node-b
```

The guest stops on node A and starts on node B with a free GPU of the same
family. `radeon0` on node B may be a different PCI address. Pick one
explicitly with `--gpu radeon1`, `--nic nic0`, or `--nvme nvme0`.

A VM that only has raw `--pci` addresses has no family to match. Migrate
that VM by passing a destination name, or recreate it. Drain uses the same
rule: if no other node has a free device of that family, the VM stays and
the node is not marked drained.

## What the node does

When any VM lists PCI devices, the pushed Nix sets `ch-vm.vfio.enable`.
That adds:

- `iommu=pt`, `amd_iommu=on`, and `intel_iommu=on`
- kernel modules `vfio`, `vfio_pci`, and `vfio_iommu_type1`

Those parameters apply on the next boot. Until the device has an IOMMU
group, the VM unit fails in `ExecStartPre` instead of starting the guest.

On start, before cloud-hypervisor runs, the unit:

1. Checks the address exists in sysfs.
2. Refuses the start if any other device in that IOMMU group was not listed.
3. Binds each address to `vfio-pci` with `driver_override` when it is not
   already there.
4. Adds `--device path=/sys/bus/pci/devices/<address>,iommu=on` for each one.

The device stays on `vfio-pci` after the guest stops. Bind it back to the
host driver by hand when the host should use it again.
