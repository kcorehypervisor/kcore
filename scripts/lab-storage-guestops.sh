#!/usr/bin/env bash
# Lab integration checklist for GuestOps resize + snapshot/clone + E2 encrypt.
# Requires a healthy 3-node CephCluster and a running Ceph VM with SSH (kcore user).
set -euo pipefail

VM="${1:?usage: $0 <vm-name>}"
VOL="${2:-${VM}-pgdata}"

echo "== create encrypted data volume =="
kctl create volume "$VOL" --size-bytes $((20 * 1024 * 1024 * 1024)) --encrypt
kctl attach volume "$VOL" --vm "$VM"
kctl stop vm "$VM" || true
kctl start vm "$VM"
kctl get volume "$VOL"

echo "== write marker, snapshot, write again, clone =="
# Operator: SSH into guest and write a marker on the data disk, then:
#   kctl snapshot volume "$VOL" --name t0
#   write a second marker
#   kctl create volume "${VOL}-clone" --from-snapshot t0
#   attach clone to a stopped scratch VM and confirm only first marker is present

echo "== resize + GuestOps probe/grow =="
kctl resize volume "$VOL" --size-bytes $((40 * 1024 * 1024 * 1024)) --grow-filesystem
kctl get volume "$VOL"
# Expect guestVisibleBytes ≈ 40GiB when GuestOps SSH key is in cloud-init

echo "done (manual guest write/clone verification still required — see comments)"
