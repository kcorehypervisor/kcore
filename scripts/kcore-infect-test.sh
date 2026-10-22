#!/usr/bin/env bash
# Render checks for scripts/kcore-infect. Does not install Nix or replace a distro.
set -euo pipefail

INFECT=${1:-"$(cd "$(dirname "$0")" && pwd)/kcore-infect"}
[[ -f $INFECT ]] || {
  echo "kcore-infect not found: $INFECT" >&2
  exit 1
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

base=(
  --dry-run
  --yes
  --hostname node1
  --ip 10.1.2.3
  --iface eth0
  --mac aa:bb:cc:dd:ee:ff
  --dhcp
  --root-uuid 11111111-2222-3333-4444-555555555555
  --root-fstype ext4
  --boot bios
  --grub-device /dev/vda
  --system x86_64-linux
  --virt none
  --flake github:kcorehypervisor/kcore
)

url='https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/kcore-infect'

bash "$INFECT" --help >"$tmp/help"
grep -q -- '--dry-run' "$tmp/help"
grep -q -- '--cert-dir' "$tmp/help"
grep -q -- '--run-controller' "$tmp/help"
grep -F -q -- "$url" "$tmp/help"

# The server runs `curl … | bash -s --`. Stdin is the script, and $0 is bash.
bash -s -- --help <"$INFECT" >"$tmp/help-stdin"
grep -F -q -- "$url" "$tmp/help-stdin"
bash -s -- "${base[@]}" --out "$tmp/stdin" <"$INFECT" >"$tmp/stdin.out"
grep -q 'inputs.kcore.url = "github:kcorehypervisor/kcore"' "$tmp/stdin/flake.nix"

bash "$INFECT" "${base[@]}" --out "$tmp/controller" >"$tmp/controller.out"
grep -q 'inputs.kcore.url = "github:kcorehypervisor/kcore"' "$tmp/controller/flake.nix"
grep -q 'kcore.nixosModules.kcore-host' "$tmp/controller/flake.nix"
grep -q 'kcore.packages.${system}.kcore-node-agent' "$tmp/controller/flake.nix"
grep -q 'system = "x86_64-linux"' "$tmp/controller/flake.nix"
grep -q 'device = "/dev/vda"' "$tmp/controller/hardware-configuration.nix"
grep -q 'systemd-boot.enable = false' "$tmp/controller/hardware-configuration.nix"
if grep -q 'qemu-guest.nix' "$tmp/controller/hardware-configuration.nix"; then
  echo "qemu guest profile imported without a qemu guest" >&2
  exit 1
fi
grep -q 'networking.hostName = "node1"' "$tmp/controller/node.nix"
grep -F -q 'networking.bridges.br0.interfaces = [ "eth0" ]' "$tmp/controller/node.nix"
grep -q 'usePredictableInterfaceNames = false' "$tmp/controller/node.nix"
grep -q 'macAddress = "aa:bb:cc:dd:ee:ff"' "$tmp/controller/node.nix"
grep -q 'initialPassword = "kcore"' "$tmp/controller/node.nix"
grep -q 'services.kcore.controller.enable = true' "$tmp/controller/node.nix"
grep -q 'services.kcore.dashboard.enable = true' "$tmp/controller/node.nix"
grep -F -q 'allowedUDPPorts = [ 4789 ]' "$tmp/controller/node.nix"
grep -q 'server.enable = true' "$tmp/controller/node.nix"
grep -q '"10.1.2.0/24"' "$tmp/controller/node.nix"
grep -q 'nodeId: node1' "$tmp/controller/node-agent.yaml"
grep -q 'controllerAddr: "10.1.2.3:9090"' "$tmp/controller/node-agent.yaml"
grep -q 'dcId: "DC1"' "$tmp/controller/node-agent.yaml"
grep -q 'caFile: /etc/kcore/certs/ca.crt' "$tmp/controller/node-agent.yaml"
grep -q 'gatewayIp: 10.240.0.1' "$tmp/controller/controller.yaml"
grep -q 'externalIp: 10.1.2.3' "$tmp/controller/controller.yaml"
grep -F -q 'peers: []' "$tmp/controller/controller.yaml"
grep -q 'KCORE_CONTROLLER=10.1.2.3:9090' "$tmp/controller/dashboard.env"
grep -q 'Generate a new cluster CA' "$tmp/controller/pki-plan.txt"
grep -q 'Dry run wrote' "$tmp/controller.out"

keyfile=$tmp/authorized_keys
echo 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForKcoreInfect user@host' >"$keyfile"
bash "$INFECT" "${base[@]}" \
  --ssh-key "$keyfile" \
  --root-fstype btrfs \
  --root-opts 'subvol=@,compress=zstd' \
  --virt kvm \
  --out "$tmp/keys" >"$tmp/keys.out"
grep -F 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITestKeyForKcoreInfect user@host' "$tmp/keys/node.nix" >/dev/null
grep -F -q 'options = [ "subvol=@" ]' "$tmp/keys/hardware-configuration.nix"
grep -q 'qemu-guest.nix' "$tmp/keys/hardware-configuration.nix"
if grep -q 'compress=zstd' "$tmp/keys/hardware-configuration.nix"; then
  echo "unrelated mount options were copied into Nix" >&2
  exit 1
fi

bash "$INFECT" "${base[@]}" \
  --ip 10.240.1.5 \
  --out "$tmp/lan" >"$tmp/lan.out"
grep -q 'gatewayIp: 10.241.0.1' "$tmp/lan/controller.yaml"
if grep -q 'kvm-node-10-240-1-5' "$tmp/lan.out"; then
  echo "explicit hostname was replaced" >&2
  exit 1
fi
grep -q 'networking.hostName = "node1"' "$tmp/lan/node.nix"

bash "$INFECT" "${base[@]}" \
  --controller 10.9.8.7:9090 \
  --cert-dir /var/lib/kcore-certs \
  --static \
  --prefix 24 \
  --gateway 10.1.2.1 \
  --dns 1.1.1.1 \
  --iface ens3 \
  --boot efi \
  --esp-mount /boot/efi \
  --esp-uuid ABCD-1234 \
  --virt kvm \
  --disable-vxlan \
  --out "$tmp/agent" >"$tmp/agent.out"
grep -q 'services.kcore.controller.enable = false' "$tmp/agent/node.nix"
grep -q 'services.kcore.nodeAgent.enable = true' "$tmp/agent/node.nix"
grep -q 'usePredictableInterfaceNames = true' "$tmp/agent/node.nix"
grep -q 'address = "10.1.2.3"' "$tmp/agent/node.nix"
grep -q 'prefixLength = 24' "$tmp/agent/node.nix"
grep -q 'efiInstallAsRemovable = true' "$tmp/agent/hardware-configuration.nix"
grep -q 'efiSysMountPoint = "/boot/efi"' "$tmp/agent/hardware-configuration.nix"
grep -q 'device = "/dev/disk/by-uuid/ABCD-1234"' "$tmp/agent/hardware-configuration.nix"
grep -q 'qemu-guest.nix' "$tmp/agent/hardware-configuration.nix"
grep -q 'server.enable = false' "$tmp/agent/node.nix"
if grep -q '4789' "$tmp/agent/node.nix"; then
  echo "vxlan port opened despite --disable-vxlan" >&2
  exit 1
fi
if [[ -f $tmp/agent/controller.yaml || -f $tmp/agent/dashboard.env ]]; then
  echo "agent render wrote controller files" >&2
  exit 1
fi
grep -q 'controllerAddr: "10.9.8.7:9090"' "$tmp/agent/node-agent.yaml"
grep -q 'Copy ca.crt, node.crt, and node.key from /var/lib/kcore-certs' "$tmp/agent/pki-plan.txt"

expect_fail() {
  local err=$1
  shift
  if "$@" >"$tmp/fail.out" 2>"$tmp/fail.err"; then
    echo "expected failure: $err" >&2
    exit 1
  fi
  grep -q "$err" "$tmp/fail.err"
}

expect_fail 'flake url' bash "$INFECT" "${base[@]}" --flake 'not a url' --out "$tmp/bad-flake"
expect_fail 'root password' bash "$INFECT" "${base[@]}" --root-password 'bad"pass' --out "$tmp/bad-pass"
expect_fail 'cert-dir' bash "$INFECT" "${base[@]}" --controller 10.9.9.9 --out "$tmp/bad-join"
expect_fail 'existing cluster' bash "$INFECT" "${base[@]}" --run-controller --controller 10.9.9.9:9090 --out "$tmp/bad-peer"

# shellcheck disable=SC1090
source "$INFECT"
nix_quote 'ok-value' >/dev/null
if nix_quote 'bad"value'; then
  echo "nix_quote accepted a quote" >&2
  exit 1
fi

echo "kcore-infect tests passed"
