#!/usr/bin/env bash
# Capture the sysfs of an NVIDIA vGPU host card and its virtual functions.
# Run it on the node the day the card is installed, after SR-IOV is enabled.
# This script only reads. It does not write sriov_numvfs and it does not
# redeem a licence.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: capture-vgpu-sysfs.sh 0000:af:00.0 [output.tar]" >&2
  exit 2
fi

addr=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
out=${2:-"vgpu-sysfs-${addr//:/-}.tar"}
sys=/sys/bus/pci/devices
dev="$sys/$addr"

if [[ ! -e "$dev" ]]; then
  echo "PCI device $addr does not exist under $sys" >&2
  exit 1
fi

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT
dest="$workdir/capture"
mkdir -p "$dest"

# Allowlist of small text files. resource*, config, reset, and remove stay
# out: those are the BAR mappings and the files that change device state.
allow=(
  vendor
  device
  class
  revision
  subsystem_vendor
  subsystem_device
  modalias
  enable
  sriov_numvfs
  sriov_totalvfs
  sriov_offset
  sriov_stride
  sriov_vf_device
  sriov_vf_total_msix
  ari_enabled
  numa_node
)

copy_file() {
  local src=$1 rel=$2
  if [[ -f "$src" && -r "$src" ]]; then
    mkdir -p "$dest/$(dirname "$rel")"
    cp -a "$src" "$dest/$rel"
  fi
}

capture_one() {
  local bdf=$1
  local base="$sys/$bdf"
  local name vf
  if [[ ! -e "$base" ]]; then
    echo "skipping $bdf; not present" >&2
    return
  fi
  mkdir -p "$dest/$bdf"
  for name in "${allow[@]}"; do
    copy_file "$base/$name" "$bdf/$name"
  done
  if [[ -L "$base/driver" ]]; then
    basename "$(readlink "$base/driver")" >"$dest/$bdf/driver"
  else
    echo unbound >"$dest/$bdf/driver"
  fi
  if [[ -L "$base/physfn" ]]; then
    basename "$(readlink "$base/physfn")" >"$dest/$bdf/physfn"
  fi
  if [[ -L "$base/iommu_group" ]]; then
    basename "$(readlink "$base/iommu_group")" >"$dest/$bdf/iommu_group"
  fi
  : >"$dest/$bdf/virtfns"
  for vf in "$base"/virtfn*; do
    [[ -L "$vf" ]] || continue
    printf '%s %s\n' "$(basename "$vf")" "$(basename "$(readlink "$vf")")" >>"$dest/$bdf/virtfns"
  done
  if [[ -d "$base/nvidia" ]]; then
    mkdir -p "$dest/$bdf/nvidia"
    copy_file "$base/nvidia/creatable_vgpu_types" "$bdf/nvidia/creatable_vgpu_types"
    copy_file "$base/nvidia/current_vgpu_type" "$bdf/nvidia/current_vgpu_type"
  fi
  if command -v lspci >/dev/null 2>&1; then
    lspci -vvv -xxxx -s "$bdf" >"$dest/$bdf/lspci.txt" || true
  else
    echo "lspci is not installed; config space was not captured" >"$dest/$bdf/lspci.txt"
  fi
}

capture_one "$addr"
if [[ -s "$dest/$addr/virtfns" ]]; then
  while read -r _name target; do
    [[ -n "$target" ]] || continue
    capture_one "$target"
  done <"$dest/$addr/virtfns"
fi

tar -C "$workdir" -cf "$out" capture
echo "wrote $out"
