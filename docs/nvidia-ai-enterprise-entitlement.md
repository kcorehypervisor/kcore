# NVIDIA AI Enterprise and the Virtual GPU Manager

kcore assigns vGPU slices. It does not ship NVIDIA's Virtual GPU Manager.
That package is closed software. An operator downloads it from NGC after
NVIDIA entitles the account, then points `ch-vm.vgpu.managerPackage` at a
build for the node's kernel.

This page is the operator procedure for getting that entitlement and the
host installer. It is the source for the product-site guide. Do not put API
keys, PAK IDs, or passwords in this file or in git.

## What the entitlement is

NVIDIA AI Enterprise includes **vGPU for Compute**. That is the C-series
product used for shared GPU workloads (`A2-4C`, `A2-8C`, `A2-16C` on an A2).
The 90-day evaluation is requested separately from the older vGPU marketing
trial.

Two portals do different jobs:

| Portal | Address | What it is for |
| --- | --- | --- |
| Application Hub | https://nvid.nvidia.com/dashboard | Sign-in front door after the entitlement email |
| NGC | https://catalog.ngc.nvidia.com | The Virtual GPU Manager and the guest driver |
| Licensing Portal | same hub, **NVIDIA Licensing Portal** | Cloud License Service for guests. Not the `.run` |

The NGC catalog home (PyTorch, NIM, Triton) is not the driver. Open the
resource directly.

## 1. Request the evaluation

Use a company email address. NVIDIA rejects personal mailboxes.

https://enterpriseproductregistration.nvidia.com/?LicType=EVAL&ProductFamily=NVAIEnterprise

On the NVIDIA Cloud Account subscriptions, tick **NVIDIA AI Enterprise**.
Private Registry and Omniverse are part of the same trial and can stay
selected. Leave **NVIDIA Run:ai self-hosted** off. NVIDIA's trial page says
the 90-day license includes Omniverse and does not include Run:ai.

Name the GPU you actually have. An A2 is accepted on this form.

The confirmation says NVIDIA will email next steps within 24–48 business
hours. The mail can arrive sooner. Wait for it. There is nothing else to
click until then.

## 2. Read the two emails

1. `noreply@nvidia.com` attaches the entitlement certificate (PDF). It
   states the product, the quantity, and the start and end dates. The PAK ID
   on that certificate is for filing a support case. It is not the download
   login.
2. A second mail links to https://nvid.nvidia.com/dashboard. Sign in with
   the same company address.

## 3. Open the Virtual GPU Manager, not the catalog home

While signed in:

https://catalog.ngc.nvidia.com/orgs/nvidia/vgpu/resources/virtual-gpu-manager-8

Take the latest signed version. On 31 October 2026 that version is **8.2**.
The generic Linux KVM installer inside it is:

```text
NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
```

kcore and any other generic Linux KVM host use that `.run`. The same
version also contains an Ubuntu `.deb`, Red Hat `.rpm` packages, and
VMware zip files (`NVD-AIE-800`, `NVD-AIE-900`). Leave those unless the
hypervisor is Ubuntu, RHEL, or vSphere.

If the resource asks you to subscribe, the entitlement is not attached to
the NGC org you are browsing. Create the org in the next section, then
return to this URL.

The guest driver is a separate NGC resource in the same `nvidia/vgpu` org,
named **NVIDIA vGPU for Compute Guest Driver**. Install the guest build
whose version matches the manager. Infra 8.2 ships manager and guest files
at **595.91.04**.

## 4. Create an NGC personal API key

The key is not created on the Virtual GPU Manager page. The catalog has no
"create API key" control.

https://org.ngc.nvidia.com/setup/api-keys

1. Sign in as the same user.
2. Click **Generate Personal Key**.
3. Name the key, set an expiration, and under **Services Included** tick
   **NGC Catalog**. Tick **NVIDIA AI Enterprise** when it is listed.
4. Choose your NGC organization. Generate the key and copy it. NGC shows
   the secret once.

If **Generate Personal Key** is inactive, the account has no NGC
organization yet:

1. Click **Create NVIDIA NGC Org**.
2. Create the NVIDIA Cloud Account.
3. Click **Continue**. The generate button then becomes active.

Store the key in a file the CLI can read, mode `600`. Do not commit it and
do not paste it into chat or into this doc.

```bash
install -m 600 /dev/null ~/.ngc/api-key
# write the key into that file, then:
chmod 600 ~/.ngc/api-key
```

## 5. Install the NGC CLI and download 8.2

The CLI is public. Version 4.36.6 is the build used for this procedure.

```bash
wget --content-disposition \
  https://api.ngc.nvidia.com/v2/resources/nvidia/ngc-apps/ngc_cli/versions/4.36.6/files/ngccli_linux.zip \
  -O ngccli_linux.zip
unzip ngccli_linux.zip
chmod u+x ngc-cli/ngc
```

Keep the whole `ngc-cli` directory. The `ngc` binary needs the `_internal`
directory beside it.

Configure the CLI in a terminal. It prompts; there is no flag for the key.

```bash
ngc config set --auth-option api-key
```

Answer the prompts in this order:

1. API key. Paste the personal key.
2. CLI output format. `ascii`.
3. Org. Pick the numeric org id NGC prints under `Choices`.
4. Team. Leave the default `no-team` unless you created one.

Without a key, this command reports the version as not found. That is the
missing credential, not a wrong version string.

```bash
ngc registry resource download-version \
  nvidia/vgpu/virtual-gpu-manager-8:8.2 \
  --dest ./nvidia-vgpu
```

The Linux KVM file lands at:

```text
nvidia-vgpu/virtual-gpu-manager-8_v8.2/NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
```

Do not commit that directory. It is NVIDIA's software under the
[NVIDIA AI Product Agreement](https://www.nvidia.com/en-us/data-center/products/nvidia-ai-enterprise/eula/).

## 6. What to do with the `.run`

The usual path is `kctl node vgpu enable NODE --host-card ADDRESS --url URL
--sha256 SUM --file NAME`: serve the `.run` from a host the node can reach and
the node downloads it, checks the SHA256 and adds it to the Nix store. See
[nvidia-ai-enterprise-vgpu.md](nvidia-ai-enterprise-vgpu.md).

To build the package yourself instead, build it against the kcore kernel and
set `ch-vm.vgpu.managerPackage`.
`ch-vm.vgpu.enable` stays off until that package is set. Set
`ch-vm.vgpu.hostCards` to the physical function (the A2's address). The
module blacklists `nouveau`, turns the IOMMU on, and creates the virtual
functions with `sriov-manage`.

An A2 reports `sriov_totalvfs` of 16 and PCI id `10de:25b6`. BIOS needs
SR-IOV and VT-d enabled before those functions appear. A fresh kcore node
has no IOMMU groups until it boots with `intel_iommu=on`.

Whole-GPU passthrough (`--gpu nvidia0`) is a different path. It binds the
card to `vfio-pci`. A vGPU host card and its virtual functions stay off
`vfio-pci`. See [gpu-passthrough.md](gpu-passthrough.md).

## 7. Guest licenses

The host `.run` does not keep a guest at full speed. NVIDIA's client
licensing guide says an unlicensed vGPU starts at full capability, drops to
15 frames per second after 20 minutes, and drops to 3 frames per second
after 24 hours, with CUDA stopped. Create a Cloud License Service in the
NVIDIA Licensing Portal and point the guest at it when you need full speed.

The lab notes in `scripts/vgpu-lab/README.md` cover slice setup after the
manager is installed. They do not redeem the entitlement.
