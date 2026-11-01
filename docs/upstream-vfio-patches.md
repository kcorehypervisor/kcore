# Upstreaming the VFIO and vGPU patches

kcore carries eight small patches so Cloud Hypervisor can run NVIDIA vGPU
slices and SR-IOV functions, and so a live migration works between CPUs with
different TSC rates. Carrying patches costs a rebuild of Cloud Hypervisor on
every release, so each one should go upstream or be dropped. This page has the
text for each pull request. Nothing here has been submitted.

Before opening any of them:

- Read each project's `CONTRIBUTING.md`, including any policy on tool-assisted
  code. The patches were drafted with an AI assistant. The submitter has to be
  comfortable with that policy and has to understand every line.
- Add a `Signed-off-by:` trailer (DCO) as yourself.
- Rebase on the project's `main`. The patches apply to Cloud Hypervisor 51.1
  and `vfio-ioctls` 0.5.1; line numbers and surrounding code will have moved.
- Run the project's own tests (`cargo test`, `cargo clippy`, `cargo fmt`).
  kcore builds the patched package without its test suite (see
  `modules/ch-vm/`), so none of the patches has been through upstream CI.

## Status against upstream `main` (1 November 2026)

Checked with `git apply --check` on `cloud-hypervisor/cloud-hypervisor` at
`901b91f` and `rust-vmm/vfio` at `93bcf25`.

| Patch | Applies to upstream `main` |
|-------|----------------------------|
| CH `tolerate-tsc-khz-einval` | yes |
| CH `vfio-sparse-mmap-msix` | yes |
| CH `vfio-bar-writethrough` | no, `pci/src/vfio.rs` has moved |
| CH `vfio-config-identity` | no, `pci/src/vfio.rs` has moved |
| CH `vgpu-dma-before-open` | no, `vmm/src/device_manager.rs` has moved |
| vfio-ioctls `ensure-iommu-group` | yes, with `git apply --directory=vfio-ioctls` |
| vfio-ioctls `region-flags-from-caps` | yes, with `git apply --directory=vfio-ioctls` |

"Applies" is textual only. Upstream `vfio-ioctls` is now 0.10.0 and Cloud
Hypervisor `main` depends on it; the kcore patches target 0.5.1. 0.10 adds
iommufd support (`iommufd_ioctls`, `IommufdIoas`) next to `VfioContainer`, so
`ensure_iommu_group` and `vgpu-dma-before-open` need a design check against
the iommufd path, not only a rebase. The two that apply cleanly still need the
upstream test suite and a read of the surrounding code.

## Who submits

Cloud Hypervisor's `CONTRIBUTING.md` says every contribution must be submitted
by a human, that automated or bot-driven pull requests are not accepted, and
that the submitter must understand the design and every line. It asks for the
use of an LLM to be disclosed in the description. A new feature or enhancement
also needs an issue and the maintainers' agreement before the pull request.
These patches were drafted with an assistant, so an agent must not open the
Cloud Hypervisor pull requests or issues; the person who signs the
`Signed-off-by` line submits them. Start with an issue per patch, using the
text below as the description. `rust-vmm/vfio` states no such policy, but the
same rule is the safe one.

## What was tested, and what was not

| Patch | Verified on | Not verified |
|-------|-------------|--------------|
| vfio-ioctls `ensure-iommu-group` | NVIDIA A2 vGPU slices in the lab | any other device |
| vfio-ioctls `region-flags-from-caps` | NVIDIA A2 vGPU slices | other vendor drivers that report flags late |
| CH `vgpu-dma-before-open` | NVIDIA A2 | non-vGPU devices beyond a basic passthrough |
| CH `vfio-bar-writethrough` | NVIDIA A2 | Intel `xe` and AMD GIM virtual functions |
| CH `vfio-sparse-mmap-msix` | NVIDIA A2 | Intel and AMD virtual functions |
| CH `vfio-config-identity` | NVIDIA A2 | Intel and AMD virtual functions |
| CH `tolerate-tsc-khz-einval` | live migration between lab nodes with different TSC rates, per the patch description | hosts with hardware TSC scaling |
| CH `vgpu-qemu-uuid` | NVIDIA A2 | not suitable for upstream, see below |

The only vGPU hardware in the lab is one NVIDIA A2. Say that in every pull
request. Reviewers will ask whether the change is a vGPU special case or
general VFIO behaviour, and the honest answer is "QEMU does this too" for each
patch except the UUID one.

## Order

1. `rust-vmm/vfio`: `ensure-iommu-group`, then `region-flags-from-caps`.
2. Wait for a `vfio-ioctls` release that contains them, and bump the
   dependency in a Cloud Hypervisor pull request.
3. Cloud Hypervisor: `vfio-bar-writethrough`, `vfio-sparse-mmap-msix`,
   `vfio-config-identity`, `tolerate-tsc-khz-einval` (independent of each
   other), then `vgpu-dma-before-open` once the `vfio-ioctls` release is in.

Cloud Hypervisor will not take `vgpu-dma-before-open` before `vfio-ioctls`
has `ensure_iommu_group`, because it calls that function.

## rust-vmm/vfio (`vfio-ioctls`)

### 1. `ensure_iommu_group`

**Title:** `vfio-ioctls: add VfioContainer::ensure_iommu_group`

**Body:**

> A caller sometimes has to install DMA mappings before the device fd is
> opened. NVIDIA's vGPU manager snapshots guest RAM when
> `VFIO_GROUP_GET_DEVICE_FD` runs. Mappings added afterwards are invisible to
> it (`guest_max_gpfn` stays 0), and the guest driver cannot read the slice.
>
> Attaching the group and selecting the type1 IOMMU currently happens inside
> `VfioDevice::new`, which also opens the device. This adds a public
> `ensure_iommu_group(group_id)` that does only the first half, so a caller can
> map memory and then open the device.
>
> The existing path is unchanged. No new behaviour unless the function is
> called.
>
> Tested with an NVIDIA A2 virtual function on Cloud Hypervisor. Not tested
> with other devices.

Source: `modules/ch-vm/patches/vfio-ioctls-0.5.1-ensure-iommu-group.patch`.
Check the crate for a way to unit-test this without a real VFIO device; if there is none, say so in the pull request. Confirm in the current source that attaching the group still happens in `VfioDevice::new`, since the wording above assumes it.

### 2. Region flags from the capability-sized answer

**Title:** `vfio-ioctls: take region flags from the full-size GET_REGION_INFO`

**Body:**

> When it enumerates regions, `vfio-ioctls` calls
> `VFIO_DEVICE_GET_REGION_INFO` twice: once with a bare `vfio_region_info` to
> learn the size, then again with room for the capability chain. It keeps
> `flags` and `size` from the first call.
>
> A vendor driver is allowed to complete the flags only when the buffer can
> hold its capabilities. NVIDIA's vGPU driver reports BAR0 as
> `READ|WRITE|CAPS` in the first answer and adds `MMAP` in the second. With
> the first answer, callers never see `MMAP`, never map the sparse areas and
> trap every BAR0 access, which the driver rejects with `EFAULT`.
>
> QEMU reads the flags from the final, full-size answer. This does the same.
>
> Devices that answer identically both times are unaffected.

Source: `modules/ch-vm/patches/vfio-ioctls-0.5.1-region-flags-from-caps.patch`.
It currently says "applies after the ensure-iommu-group patch"; drop that line
when the two are separate pull requests.

## Cloud Hypervisor

### 3. Write BAR addresses through to the VFIO device

**Title:** `pci: write BAR addresses through to the VFIO device`

**Body:**

> Cloud Hypervisor sizes each BAR by writing all ones to the device's config
> space, then keeps the guest's BAR programming in a local shadow only.
> `vfio-pci` virtualises BAR writes, so ordinary passthrough does not notice.
>
> A vendor driver that emulates the config space of a virtual function itself
> (NVIDIA vGPU) takes the BAR register as the guest's BAR base. It was left at
> `0xfffffff0`, so every BAR0 access reached the driver at that base plus the
> offset and failed, and the guest driver reported the GPU unsupported.
>
> After sizing, write the allocated address to the device, and forward later
> guest BAR writes. QEMU forwards all config writes.
>
> Whole-device passthrough is unchanged in effect: the written value is the
> address the guest would see anyway.

Source: `cloud-hypervisor-51.1-vfio-bar-writethrough.patch`. Expect a question
about 64-bit and I/O BARs and about the ROM BAR; the patch handles the flag
bits for each, so point at that.

### 4. Map sparse mmap areas when a BAR holds the MSI-X tables

**Title:** `pci: map VFIO sparse mmap areas even if MSIX_MAPPABLE is absent`

**Body:**

> Cloud Hypervisor skips mmap for any BAR that contains the MSI-X table or PBA
> unless the kernel advertises `MSIX_MAPPABLE`, and traps every access. NVIDIA's
> vGPU driver puts the tables in BAR0, does not advertise `MSIX_MAPPABLE`, and
> publishes a sparse mmap list that already leaves out the vector table page.
> It then rejects trapped accesses to the listed bytes with `EFAULT`.
>
> Treat a sparse mmap list as sufficient, as QEMU does. The areas are mapped
> exactly as published, because the driver returns `EINVAL` for a mapping that
> is not one of its areas.
>
> Devices without a sparse mmap list keep the current behaviour.

Source: `cloud-hypervisor-51.1-vfio-sparse-mmap-msix.patch`. Needs
`region-flags-from-caps` in `vfio-ioctls` to have any effect on NVIDIA, so
mention the dependency.

### 5. Seed the VFIO config shadow from the device

**Title:** `pci: seed the VFIO config shadow and return live identity registers`

**Body:**

> A virtual function of an NVIDIA vGPU keeps the physical GPU's PCI ids. Cloud
> Hypervisor serves non-BAR config from the VFIO region, but its shadow starts
> as vendor 0 and a `ConfigPatch` can mask the identity registers. Those bits
> are read-only, so `write_reg` cannot seed them.
>
> On a fresh device, read dwords 0x00, 0x08 and 0x2c from VFIO into the shadow
> (a restored snapshot keeps its saved shadow). `read_config_register` returns
> those three dwords from VFIO and does not apply `ConfigPatch` to them.
>
> Whole-device passthrough is unchanged, since the VFIO region already matches
> the physical card.

Source: `cloud-hypervisor-51.1-vfio-config-identity.patch` (312 lines, the
largest). The patch description says it was drafted against 51.1 without an
A2 and that the guest-visible check waits for the card; the A2 has since run
guests with it, but a passthrough of a card that is not a vGPU host is a null
test of this patch. This is the one most likely to get review comments about
size and about overlap with `ConfigPatch`. Offer to split it.

### 6. Map guest RAM before opening the VFIO device

**Title:** `vmm: install VFIO DMA mappings before the device fd is opened`

**Body:**

> Depends on rust-vmm/vfio adding `VfioContainer::ensure_iommu_group`.
>
> NVIDIA's vGPU manager snapshots guest RAM at `VFIO_GROUP_GET_DEVICE_FD`.
> `add_vfio_device` opens the device first and maps guest RAM afterwards, so the
> snapshot is empty and the guest driver cannot read the slice.
>
> Attach the IOMMU group, install the DMA mappings, then open the device. The
> group id comes from the `iommu_group` link in sysfs. The order of the
> remaining steps is unchanged.
>
> This only changes the order of two existing steps; it adds no new mapping.

Source: `cloud-hypervisor-51.1-vgpu-dma-before-open.patch`. Explain why mapping
before opening is also valid for ordinary devices (the container already exists
and the group is attached either way). Reviewers may worry about the failure
path when `VfioDevice::new` fails after mappings exist; check that the existing
cleanup covers it before sending.

### 7. Tolerate `KVM_SET_TSC_KHZ` failing with `EINVAL`

**Title:** `hypervisor: tolerate KVM_SET_TSC_KHZ returning EINVAL on restore`

**Body:**

> Without hardware TSC scaling (Intel client and Xeon E parts, and anything
> before Skylake-SP), KVM accepts a guest TSC rate above the host rate
> (catch-up mode) but rejects one below it with `EINVAL`, logging "user
> requested TSC rate below hardware speed". Cloud Hypervisor turns that into a
> hard restore failure, so a live migration from a 3.3 GHz node to a 3.4 GHz
> node aborts at the State step while the reverse direction works.
>
> QEMU warns and continues. Do the same: log a warning and keep the host rate.
> Guests on kvm-clock or the Hyper-V reference TSC page keep correct time,
> because the host recomputes the pvclock scale from the real TSC rate.
>
> `EIO` was already tolerated here.

Source: `cloud-hypervisor-51.1-tolerate-tsc-khz-einval.patch`. This one has
nothing to do with GPUs and is the easiest to get accepted. Be ready to say
what happens to a guest that uses the raw TSC without kvm-clock: its rate
changes after the migration. That is the same trade-off QEMU makes.

## Not for upstream: the QEMU UUID shim

`cloud-hypervisor-51.1-vgpu-qemu-uuid.patch` keeps `-uuid <uuid>` on the
process command line, because NVIDIA's vGPU manager reads
`/proc/<vmm-pid>/cmdline` for that pair and refuses to virtualise the slice
without it. It strips the pair from the arguments clap sees.

That is a workaround for a vendor manager that assumes QEMU. Cloud Hypervisor
maintainers are unlikely to want a `-uuid` shim in `main`. Options:

- Keep carrying it in kcore, which is what happens today.
- Ask NVIDIA to read the UUID from the mdev or VFIO device path instead of the
  QEMU command line.
- Offer a generic, documented way to pass an opaque argument that the VMM
  ignores, if the maintainers prefer that to a QEMU-specific flag.

Do not open a pull request for it as it stands.

## When a patch merges

Delete its file from `modules/ch-vm/patches/`, remove it from the lists in
`modules/ch-vm/options.nix` (both `default` and `defaultText`) and from
`modules/ch-vm/vm-service.nix` for the `vfio-ioctls` ones, and move the
Cloud Hypervisor package to a release that contains it. Keep `default` and `defaultText` equal by hand;
nothing checks that they match. The eval test in
`tests/ch-vm-module-eval.nix` only checks that an empty list gives the stock
package.
