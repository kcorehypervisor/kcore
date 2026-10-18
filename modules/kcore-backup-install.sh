# Install the kcore backup appliance onto a disk. The live ISO is not a
# hypervisor: the installed system imports kcore-backup-host and the
# kcore-vault module.
set -euo pipefail

DISK=""
DATA_DISK=""
HOSTNAME_VALUE="kcore-backup"
AUTO_YES="false"
REBOOT_AFTER_INSTALL="false"

usage() {
  cat <<'EOF'
Usage: install-backup --disk /dev/disk/by-id/DISK [options]

  --disk PATH         OS disk. The installer erases it.
  --data-disk PATH    Optional repository disk, formatted as ZFS pool "vault"
                      and mounted at /var/lib/kcore-vault.
  --hostname NAME     Installed hostname (default: kcore-backup).
  --yes               Erase the disks without prompting.
  --reboot            Reboot after nixos-install succeeds.

The installed host is a backup appliance. It does not run the hypervisor,
Ceph, Kubernetes, or the node agent. The install imports the kcore-vault
module, which runs the coordinator and opens the console on 443 and NBD on 10809.
EOF
}

die() {
  echo "install-backup: $*" >&2
  exit 1
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --disk)
      DISK="${2:-}"
      shift 2
      ;;
    --data-disk)
      DATA_DISK="${2:-}"
      shift 2
      ;;
    --hostname)
      HOSTNAME_VALUE="${2:-}"
      shift 2
      ;;
    --yes)
      AUTO_YES="true"
      shift
      ;;
    --reboot)
      REBOOT_AFTER_INSTALL="true"
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ "$(id -u)" -eq 0 ]] || die "run as root"
[[ -n "$DISK" ]] || die "--disk is required"
[[ -b "$DISK" ]] || die "not a block device: $DISK"
[[ "$HOSTNAME_VALUE" =~ ^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$ ]] || die "hostname must be letters, digits, and hyphens"

if [[ -n "$DATA_DISK" ]]; then
  [[ -b "$DATA_DISK" ]] || die "not a block device: $DATA_DISK"
  DISK_REAL="$(readlink -f "$DISK")"
  DATA_REAL="$(readlink -f "$DATA_DISK")"
  [[ "$DISK_REAL" != "$DATA_REAL" ]] || die "OS disk and data disk are the same device"
fi

echo "kcore backup appliance @KCORE_VERSION@"
echo "  OS disk:   $DISK"
if [[ -n "$DATA_DISK" ]]; then
  echo "  Data disk: $DATA_DISK (ZFS pool vault -> /var/lib/kcore-vault)"
else
  echo "  Data disk: none (repository directory on the OS disk)"
fi
echo "  Hostname:  $HOSTNAME_VALUE"
echo
echo "This erases the selected disk(s)."

if [[ "$AUTO_YES" != "true" ]]; then
  read -r -p "Type yes to continue: " answer
  [[ "$answer" == "yes" ]] || die "aborted"
fi

install -d -m 0700 /tmp/luks
umask 077
openssl rand -hex 32 | tr -d '\n' > /tmp/luks/password

DATA_DISK_NIX=""
EXTRA_DEVICES_NIX=""
if [[ -n "$DATA_DISK" ]]; then
  DATA_DISK_NIX=$(
    cat <<EOF
          data = {
            type = "disk";
            device = "$DATA_DISK";
            content = {
              type = "gpt";
              partitions = {
                zfs = {
                  size = "100%";
                  content = {
                    type = "zfs";
                    pool = "vault";
                  };
                };
              };
            };
          };
EOF
  )
  EXTRA_DEVICES_NIX=$(
    cat <<'EOF'
        zpool = {
          vault = {
            type = "zpool";
            options.ashift = "12";
            rootFsOptions = {
              compression = "zstd";
              atime = "off";
              mountpoint = "none";
            };
            datasets = {
              repo = {
                type = "zfs_fs";
                mountpoint = "/var/lib/kcore-vault";
                options.compression = "zstd";
              };
            };
          };
        };
EOF
  )
fi

cat > /tmp/disko-config.nix <<EOF
{
  disko.devices = {
    disk = {
      os = {
        type = "disk";
        device = "$DISK";
        content = {
          type = "gpt";
          partitions = {
            ESP = {
              size = "512M";
              type = "EF00";
              content = {
                type = "filesystem";
                format = "vfat";
                mountpoint = "/boot";
                mountOptions = [ "umask=0077" ];
              };
            };
            root = {
              size = "100%";
              content = {
                type = "luks";
                name = "cryptroot";
                passwordFile = "/tmp/luks/password";
                settings.allowDiscards = true;
                content = {
                  type = "filesystem";
                  format = "ext4";
                  mountpoint = "/";
                };
              };
            };
          };
        };
      };
${DATA_DISK_NIX}
    };
${EXTRA_DEVICES_NIX}
  };
}
EOF

echo "Partitioning and mounting..."
disko --mode format,mount --yes-wipe-all-disks --root-mountpoint /mnt /tmp/disko-config.nix

ROOT_PART=""
if [[ -e /dev/mapper/cryptroot ]]; then
  ROOT_PART="$(cryptsetup status cryptroot 2>/dev/null | awk '/device:/ {print $2; exit}')"
fi
[[ -n "$ROOT_PART" && -b "$ROOT_PART" ]] || die "could not find the LUKS device behind /dev/mapper/cryptroot"

ROOT_PART_UUID="$(blkid -s UUID -o value "$ROOT_PART")"
[[ -n "$ROOT_PART_UUID" ]] || die "$ROOT_PART has no UUID"

# The initrd is built on the live system, so the key has to exist at this
# path here and on the installed system.
install -d -m 0700 /etc/kcore/recovery /mnt/etc/kcore/recovery /mnt/root
install -m 0400 /tmp/luks/password /etc/kcore/recovery/crypto_keyfile.bin
install -m 0400 /tmp/luks/password /mnt/etc/kcore/recovery/crypto_keyfile.bin
install -m 0400 /tmp/luks/password /mnt/root/luks-recovery-key.txt

echo "Generating hardware configuration..."
nixos-generate-config --root /mnt

ZFS_BOOT=""
if [[ -n "$DATA_DISK" ]]; then
  HOST_ID="$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
  ZFS_BOOT=$(
    cat <<EOF
  boot.supportedFilesystems = [ "zfs" ];
  boot.zfs.extraPools = [ "vault" ];
  # The pool was created by the live ISO, which has a different hostId.
  boot.zfs.forceImportAll = true;
  networking.hostId = lib.mkForce "$HOST_ID";
EOF
  )
fi

SSH_KEYS=""
if [[ -f /root/.ssh/authorized_keys ]]; then
  while IFS= read -r key; do
    [[ -z "$key" || "$key" == \#* ]] && continue
    case "$key" in
      *\"* | *\\*) die "authorized_keys entry contains a quote or backslash" ;;
    esac
    SSH_KEYS="${SSH_KEYS}      \"${key}\"
"
  done < /root/.ssh/authorized_keys
fi

cat > /mnt/etc/nixos/backup.nix <<EOF
{ lib, ... }:
{
  networking.hostName = "$HOSTNAME_VALUE";
  networking.useDHCP = true;

  users.users.root = {
    initialPassword = "kcore";
    openssh.authorizedKeys.keys = [
${SSH_KEYS}    ];
  };
  users.mutableUsers = true;

  boot.initrd.luks.devices.cryptroot = {
    device = "/dev/disk/by-uuid/$ROOT_PART_UUID";
    preLVM = true;
    keyFile = "/crypto_keyfile.bin";
  };
  boot.initrd.secrets."/crypto_keyfile.bin" = "/etc/kcore/recovery/crypto_keyfile.bin";

${ZFS_BOOT}
  system.stateVersion = "25.05";
}
EOF

cat > /mnt/etc/nixos/flake.nix <<'EOF'
{
  description = "kcore backup server";

  inputs = {
    nixpkgs.url = "path:@NIXPKGS@";
    kcore.url = "path:@KCORE@";
    kcore-vault.url = "github:kcorehypervisor/kcore-vault";
  };

  outputs =
    { nixpkgs, kcore, kcore-vault, ... }:
    {
      nixosConfigurations.kcore-backup = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          kcore.nixosModules.kcore-backup-host
          kcore-vault.nixosModules.kcore-vault
          (
            { ... }:
            {
              services.kcoreBackup.version = "@KCORE_VERSION@";
              services.kcore-vault.enable = true;
              services.kcore-vault.package = kcore-vault.packages.x86_64-linux.kcore-vault;
              # Console and instant-recovery export. 10809 also comes from the vault module.
              networking.firewall.allowedTCPPorts = [ 443 10809 ];
            }
          )
          ./hardware-configuration.nix
          ./backup.nix
        ];
      };
    };
}
EOF

install -d -m 0755 /mnt/etc/kcore
sed 's|passwordFile = "/tmp/luks/password"|passwordFile = "/etc/kcore/recovery/crypto_keyfile.bin"|' /tmp/disko-config.nix > /mnt/etc/kcore/disko-config.nix
cat > /mnt/etc/kcore/vault.nix.example <<'EOF'
# The installed flake already imports kcore-vault and sets
# services.kcore-vault.enable = true. The console listens on 443 and the
# instant-recovery export listens on 10809. Point controllerEndpoint at the
# cluster controller when the repository is ready.
EOF

echo "Installing the backup appliance (this builds the system from the ISO store)..."
export NIX_CONFIG="experimental-features = nix-command flakes"
nixos-install --flake /mnt/etc/nixos#kcore-backup --no-root-passwd

echo
echo "Installed. LUKS recovery key: /root/luks-recovery-key.txt on the new system."
echo "Root SSH password is the kcore installer password. Change it after login."
echo "Vault coordinator and console are enabled. See /etc/kcore/vault.nix.example."
if [[ "$REBOOT_AFTER_INSTALL" == "true" ]]; then
  sync
  reboot
else
  echo "Remove the USB drive and reboot."
fi
