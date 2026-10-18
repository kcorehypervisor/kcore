# NVIDIA A2 vGPU lab

This directory is a lab procedure. It is not installed on a kcore node and it
is not a NixOS module. Nothing here downloads NVIDIA software or redeems the
90-day evaluation licence.

The A2 (16 GB, HPE ProLiant ML350 Gen10) is not in the lab yet. Steps 1 to 7
are the host procedure for the day it arrives. Step 8 is the QEMU reference.
Step 9 is the Cloud Hypervisor check of the same virtual function. Step 10 is
what to bring back into the tree.

Whole-device passthrough is unchanged. `--gpu nvidia0` still means the whole
card, bound to `vfio-pci`. A vGPU host card, and every virtual function of
that card, must stay off `vfio-pci`. The function's driver is
`nvidia-vgpu-vfio`. The physical function stays on `nvidia`.

## Before the card arrives

`scripts/vgpu-lab/rehearse.sh` runs the fake-sysfs tests. It does not look for
an A2 and it does not write to sysfs.

`scripts/capture-vgpu-sysfs.sh` is the capture to run once SR-IOV is up. It
only reads.

## When the card is installed

1. Confirm the card. `lspci -d 10de: -nn` should show an NVIDIA A2, device
   `10de:25b6`, at one address. Call that address `PF`.
2. Install the operator-supplied NVIDIA Virtual GPU Manager built for this
   kernel. kcore does not ship it. Leave the physical function on the
   `nvidia` driver. Blacklist `nouveau`. Boot with the IOMMU on
   (`intel_iommu=on` on this ML350).
3. Do not point `ch-vm.vfio` at this address, and do not list it as a
   whole-device `pciDevices` entry. `ch-vm.vgpu.hostCards` is the list that
   stays on the host driver.
4. Enable SR-IOV with `sriov-manage -e PF`, or write a chosen count to
   `sriov_numvfs` once you know how many functions you want. Confirm
   `virtfn0` appears.
5. Confirm each virtual function's driver is `nvidia-vgpu-vfio`. If a
   function is on `vfio-pci`, unbind it and stop. A function on `vfio-pci`
   is not a vGPU slice.
6. Read `nvidia/creatable_vgpu_types` on one function. Pick one profile name
   from that file. The names used in this tree, such as `A2-4C`, are the
   request syntax; the number beside the name is what the card accepts. One
   profile per card. Mixed sizes stay off.
7. Write that type id to the function's `nvidia/current_vgpu_type` and read
   it back. Then run `scripts/capture-vgpu-sysfs.sh PF`.
8. QEMU is the reference, and it comes before Cloud Hypervisor. Point a
   guest at the virtual function only, not at the physical function:

   ```bash
   qemu-system-x86_64 \
     -machine q35,accel=kvm \
     -cpu host -m 8G \
     -device vfio-pci,host=VF
   ```

   In the guest, `lspci -nn` must show the physical A2 id `10de:25b6`, not a
   zero id. That is the result Cloud Hypervisor has to match. Do not install
   the guest driver yet, and do not redeem the evaluation licence, until this
   guest can see the card and the only remaining step is that install.
9. Repeat the same function with Cloud Hypervisor:

   ```bash
   cloud-hypervisor \
     --cpus boot=4 \
     --memory size=8G \
     --device path=/sys/bus/pci/devices/VF,iommu=on
   ```

   The guest must show the same PCI id as the QEMU guest.
   `modules/ch-vm/patches/cloud-hypervisor-51.1-vfio-config-identity.patch`
   copies vendor, class, and subsystem from the VFIO config region into the
   VMM's shadow when the device is created, and guest reads of those three
   dwords ignore a config patch. It was drafted against Cloud Hypervisor 51.1
   without this card. A whole-device passthrough of a card that is not a
   vGPU host is a null check: those registers already come from the device,
   and the patch returns the same dwords.
10. Bring the capture tarball back and correct the fake sysfs fixtures from
    it. The profile id table comes from that capture, not from a guess.

## Rehearse without the card

From the repository root:

```bash
scripts/vgpu-lab/rehearse.sh
```
