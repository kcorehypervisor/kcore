# Windows guests

kcore boots Windows Server on Cloud Hypervisor the same way it boots a Linux VM.
The guest image is prepared once, outside the cluster. The node does not install
Windows from an ISO.

## What the node already does

Every VM unit starts Cloud Hypervisor with:

- `kvm_hyperv=on` on the CPU line. Windows will not boot reliably without Hyper-V
  enlightenments. Linux guests tolerate the flag, so it is unconditional.
- Cloud Hypervisor UEFI firmware (`CLOUDHV.fd`), not a legacy BIOS.
- A virtio disk and a virtio NIC.

LVM, ZFS, and Ceph copy the uploaded image onto a raw volume before Cloud
Hypervisor opens it. `--storage-size-bytes` is that volume's size and must be
the image's virtual size (`qemu-img info`), not the size of the compact qcow2
file. The first start can take several minutes. The unit allows 15 minutes for
that copy, and it retries the copy if a previous start was interrupted. A later
start of the same VM does not copy the image again.

A cloud-init seed ISO is still attached. Windows ignores it. The login is the
Administrator account baked into the image, not the cloud-init user.

## Build the image

Use the public recipe at [kcorehypervisor/windows-guest](https://github.com/kcorehypervisor/windows-guest).
`./build.sh` installs Windows Server 2022. `./build-win10.sh` installs Windows 10
with the `w10` virtio drivers. Both pin virtio-win 0.1.240, enable OpenSSH, and
shut the guest down. Server also enables the serial console (SAC). Do not
publish the ISO, the disk, or the Administrator password.

Newer virtio-win builds break on Cloud Hypervisor (NetKVM on the virtio-net
control queue, and viostor when discard is advertised). Stay on the pin in
that recipe.

Microsoft evaluation images must reach the internet to activate within 10 days,
and they expire after 180 days.

## Windows 11

Windows 11 uses the same node configuration and the same `kctl create vm` or
`kcore_vm` resource. Change the image path. kcore does not grow a guest-type
field for it.

Windows 11 Setup stops when it cannot see a TPM 2.0 and Secure Boot. kcore
does not attach either device to the guest. A lab image can skip those two
checks in the answer file and then boot on the same Cloud Hypervisor command
line as Server 2022. That install is not a Microsoft-supported configuration:
some later feature updates refuse to run without a TPM. The
[windows-guest](https://github.com/kcorehypervisor/windows-guest) recipe
builds Server 2022 today. A Windows 11 disk is the same scripts with the
edition name and the `w11` virtio driver directory from the pinned virtio ISO.

Give the guest at least 2 CPUs and 4 GB of RAM. That is already the size used
for the Server example.

## Create the VM

Upload the qcow2 produced by the recipe, then create the VM. Example for a
40 GiB disk on LVM:

```bash
kctl --node <node-host>:9091 node upload-image \
  -f windows-server-2022.qcow2 \
  --name windows-server-2022.qcow2 \
  --format qcow2

kctl create vm win-01 \
  --cpu 2 \
  --memory 4G \
  --network default \
  --storage-backend lvm \
  --storage-size-bytes 42949672960 \
  --target-node <node-id> \
  --image-path /var/lib/kcore/images/windows-server-2022.qcow2 \
  --image-format qcow2
```

`--wait-for-ssh` only checks that TCP port 22 accepts a connection from the
node. It does not log in. Skip it if you only need the serial console.

## Reach the guest

`kctl console win-01` attaches to the serial console. SAC is available once
Windows starts (`ch` lists channels, `cmd` opens a command prompt).

On a NAT network the guest address is reachable from the node, not from the
operator's workstation. Use the node as a jump host:

```bash
ssh -J root@<node-host> Administrator@<guest-ip>
```

IIS, when installed inside the guest, is reached the same way. A local forward
publishes it on the workstation:

```bash
ssh -N -L 8080:<guest-ip>:80 root@<node-host>
```

Install the web server from the Administrator prompt:

```bat
powershell -Command "Install-WindowsFeature Web-Server -IncludeManagementTools"
```
