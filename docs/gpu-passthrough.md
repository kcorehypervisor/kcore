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

## vGPU slices

`--gpu nvidia0` is still the whole card, and the node binds that card to
`vfio-pci`. A slice of one NVIDIA card is a different request: `--vgpu A2-4C`,
`--vgpu nvidia0=A2-4C`, or `--vgpu nvidia0/0=A2-4C`. The physical function
stays on the NVIDIA host driver. The node writes the profile onto one virtual
function and never binds that card, or the function, to `vfio-pci`.
Virtual GPU Manager 595.91 binds each function to `nvidia` and publishes
`nvidia/creatable_vgpu_types`. An older manager binds the function to
`nvidia-vgpu-vfio`. Both are a slice when that file lists profiles.

Turn the node into a vGPU host before creating guests. The controller stages
the NVIDIA Virtual GPU Manager, renders `ch-vm.vgpu.managerRun`, and says when
the node still needs a reboot (no IOMMU group, or the card is still on
`nouveau`):

```bash
kctl node vgpu enable node-193 \
  --host-card 0000:61:00.0 \
  --url http://192.168.40.10:18083/virtual-gpu-manager-8_v8.2/NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run \
  --sha256 2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d \
  --file NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
kctl node vgpu show node-193
```

The same record is `kind: NodeVgpu` for `kctl apply -f`. `kctl node reboot`
reboots through the controller and refuses while a VM is set to run unless
`--force` is set. Change a stopped VM with `kctl update vm web-01 --vgpu A2-4C`.

```yaml
kind: NodeVgpu
metadata:
  name: node-193
spec:
  hostCards: ["0000:61:00.0"]
  manager:
    url: http://192.168.40.10:18083/virtual-gpu-manager-8_v8.2/NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
    sha256: 2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d
    file: NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
    version: "595.91.04"
```

`kctl apply -f` takes one document per file. Apply the VM from its own file:
`examples/vgpu/nvidia-host.yaml` and `examples/vgpu/vm-nvidia.yaml` are the two
halves of this example, and `examples/vgpu/` has the Intel and AMD pairs.

```yaml
kind: VM
metadata:
  name: vgpu-01
spec:
  cpu: 4
  memoryBytes: 8G
  storageBackend: filesystem
  storageSizeBytes: 40G
  targetNode: node-193
  vgpus: ["A2-4C"]
  nics:
    - network: default
  disks:
    - image: https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img
      sha256: 04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94
      format: qcow2
```

Terraform uses `kcore_vgpu_host` for the node and `vgpus = ["A2-4C"]` on
`kcore_vm`. `kcore_gpus` reads the slice inventory. A requested `A2-4C` matches
a stored `nvidia0/0=A2-4C`.

The node must already be running a kcore generation that reports
`vgpuManagerRun`. Older nodes refuse `ConfigureNodeVgpu` until
`kctl update cluster` rolls that generation out.

### Intel and AMD SR-IOV slices

Intel and AMD cards split with plain SR-IOV. There is no licence and no
mediated device: each virtual function is a slice, and the VM gets it through
`vfio-pci` like any other passthrough function. `--vendor` picks the host
driver, and `--num-vfs` splits every card evenly.

| Vendor | Host driver | Example card | Virtual functions |
|--------|-------------|--------------|-------------------|
| `intel` | upstream `xe` | Arc Pro B50 / B70 | 2 on a B50 (8 GB each), 4 on a B70 |
| `amd` | GIM (MxGPU), built from source | Radeon Pro V710 | 1 to 12 |

```bash
kctl node vgpu enable node-7 --vendor intel --host-card 0000:03:00.0 --num-vfs 2
kctl create vm web-01 --target-node node-7 --vgpu intel-2vf ...
```

The slice profile names the split: `intel-2vf` is half of an Intel card and
`radeon-4vf` is a quarter of an AMD card. `--vgpu intel0=intel-2vf` and
`--vgpu intel0/1=intel-2vf` pin a card or a function, as with NVIDIA.

```yaml
kind: NodeVgpu
metadata:
  name: node-7
spec:
  vendor: amd
  hostCards: ["0000:c3:00.0=4", "0000:c4:00.0=6"]
  numVfs: 4
```

The controller renders `ch-vm.vgpu.sriovCards`. On Intel the
`kcore-gpu-sriov` unit turns off `sriov_drivers_autoprobe`, writes
`sriov_numvfs` on the `xe` physical function, and binds each virtual function
to `vfio-pci`. On AMD the node blacklists `amdgpu` and loads `gim` with
`vf_num`, which creates the functions at load time; the unit then binds them
to `vfio-pci`. Each card can have its own count: `--host-card 0000:c3:00.0=4
--host-card 0000:c4:00.0=6`, or `numVfs` per entry in `sriovCards`. A card
without `=N` takes `--num-vfs`. GIM reads `vf_num` by the number it gives
each AMD GPU in PCI address order, so kcore writes the list in address order.
Every AMD GPU on the node needs an entry: one that is missing shifts the
counts, and `kcore-gpu-sriov` then fails with "GIM created N virtual
functions" instead of leaving slices of the wrong size.
`ch-vm.vgpu.amd.schedulePolicy` sets GIM's `sch_policy`.

A VM with a slice of any vendor moves cold: `kctl migrate vm` stops it,
attaches a free slice of the same profile on the target, and starts it. Cloud
Hypervisor live migration does not carry `vfio-pci` devices for NVIDIA, Intel
or AMD, so `amd-vfio-pci` is not needed.

The node must report the `vgpuVendor` feature. The Intel and AMD paths are
tested against fake sysfs trees and Nix evaluations, not on a B50 or a V710
yet; treat them as preview until a real card has run a guest.

A slice guest runs on the Cloud Hypervisor that kcore builds from
`ch-vm.vms.cloudHypervisorPatches`. Those patches give the NVIDIA vendor
driver what it gets from QEMU: guest RAM mapped before the device opens,
`-uuid` on the command line, BAR addresses written through to the device,
and the published sparse mmap areas of BAR0 mapped instead of trapped. The
guest sees the slice behind no virtual IOMMU and its RAM is `shared=on`.
A filesystem guest boots `/var/lib/kcore/disks/<name>.raw`, seeded once from
the image; deleting the VM removes that file on the next config apply.
