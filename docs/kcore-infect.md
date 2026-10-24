# kcore-infect

On the Debian, Ubuntu, Fedora, or other systemd server, download the installer and run it:

```bash
curl -fsSL https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/kcore-infect | sudo bash -s -- --yes --reboot
```

That host becomes a controller and a node agent. The script installs Nix beside the current system, builds a NixOS generation that imports `kcore.nixosModules.kcore-host`, installs GRUB, and marks the root for lustrate. The next boot moves the old distro to `/old-root` and starts kcoreOS.

Flags go after `--`. The script arrives on stdin, so there is no prompt: `--yes` confirms the replacement, and `--reboot` boots into kcoreOS when the bootloader is installed.

Root stays on the filesystem that is already there (`ext2`, `ext3`, `ext4`, `xfs`, `btrfs`, or `f2fs`). Software RAID and an existing LUKS root are refused. The kcore ISO installer is the path that wipes a disk and enrolls LUKS.

The build compiles the kcore Rust packages on this machine. Nixpkgs comes from `cache.nixos.org`.

## Requirements

- Linux with systemd as pid 1
- root, or a user that can `sudo`
- at least 12 GiB free on `/`
- an IPv4 address on the default route
- a BIOS disk, or a mounted vfat EFI system partition

A machine with less than 6 GiB available memory gets a 4 GiB swap file for the compile. That file is removed before reboot.

## Preview

```bash
curl -fsSL https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/kcore-infect | sudo bash -s -- --dry-run
```

This writes `./kcore-infect-preview` and does not install Nix or change the bootloader. Pass the same flags you would pass for the real run.

## First machine

The command at the top of this page is the first machine. With no `--controller`, this host runs the controller, the dashboard, and the node agent.

On the next boot:

- the physical NIC is enslaved to `br0`, and `br0` keeps that NIC's MAC
- DHCP is used when the old system was using DHCP; otherwise the current address is pinned
- root can log in over SSH. The initial password is `kcore` unless you pass `--root-password` or set `KCORE_ROOT_PASSWORD`
- authorized keys from `/root/.ssh/authorized_keys` are installed for root
- the controller listens on `:9090`, the dashboard and its gRPC-web API on `:9443` (HTTPS, operator login with TOTP), and the node agent on `:9091`

The cluster CA is created with `kctl create cluster`. The CA private key stays in `/root/.kcore/kcore` and is not copied to `/etc/kcore/certs`. Copy `/root/.kcore` to the operator machine, then remove the CA key from the node if this host should not keep it.

A retry of a failed first-node run replaces that CA. Copy the key off the machine before retrying if you already handed it out.

## Another node

On the machine that holds the CA:

```bash
kctl write-node-pki \
  --certs-dir /root/.kcore/kcore \
  --host 10.0.0.6 \
  --out /tmp/node-certs
```

Copy `/tmp/node-certs` to the new host (`ca.crt`, `node.crt`, `node.key`) and run:

```bash
curl -fsSL https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/kcore-infect | sudo bash -s -- --yes --reboot \
  --controller 10.0.0.5:9090 \
  --cert-dir /tmp/node-certs
```

Adding another controller uses the same CA directory (`ca.crt`, `ca.key`, `sub-ca.crt`, `sub-ca.key`) plus `--run-controller` and `--controller` for each peer.

## Flake

The script on `main` builds `github:kcorehypervisor/kcore`. Override it with `--flake` or `KCORE_FLAKE`. A `path:/absolute/checkout` input is copied to `/etc/nixos/kcore-src` so a later rebuild still works after the old root moves to `/old-root`.

## After reboot

```bash
systemctl is-active kcore-node-agent
systemctl is-active kcore-controller
```

The old distro is in `/old-root`. SSH host keys are the same keys as before the switch. `/etc/kcore/disk-management-mode` is `installer-only`.

Build the generation again from the installed flake:

```bash
nixos-rebuild switch --flake /etc/nixos#kcore
```
