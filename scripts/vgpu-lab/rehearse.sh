#!/usr/bin/env bash
# Rehearse the vGPU inventory without an NVIDIA card. Runs the fake-sysfs
# tests only. Does not write to /sys and does not look for an A2.
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"

if [[ -z "${PROTOC:-}" ]]; then
  if command -v protoc >/dev/null 2>&1; then
    export PROTOC
    PROTOC=$(command -v protoc)
  else
    found=$(find /nix/store -maxdepth 1 -type d -name '*-protobuf-*' -print 2>/dev/null | head -n 1 || true)
    if [[ -n "$found" && -x "$found/bin/protoc" ]]; then
      export PROTOC="$found/bin/protoc"
    fi
  fi
fi

echo "vGPU lab rehearse: fake sysfs, no card, no licence"
exec cargo test -p kcore-gpu --offline
