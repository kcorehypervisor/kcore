#!/usr/bin/env bash
# Copy the controller, node, and vault-install protos from this repo into the
# MCP server. `--check` only reports drift and exits 1 when there is any.
# provisioning.proto and vault.proto come from their own repositories.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
src="$here/../../proto"
dst="$here/../packages/server/proto"
protos=(controller.proto node.proto vault_install.proto)

check=false
case "${1:-}" in
  --check) check=true ;;
  "") ;;
  *)
    echo "usage: $0 [--check]" >&2
    exit 2
    ;;
esac

drift=0
for proto in "${protos[@]}"; do
  if [[ ! -f "$src/$proto" ]]; then
    echo "missing $src/$proto" >&2
    exit 1
  fi
  if cmp -s "$src/$proto" "$dst/$proto"; then
    continue
  fi
  if $check; then
    echo "out of date: packages/server/proto/$proto" >&2
    drift=1
  else
    cp "$src/$proto" "$dst/$proto"
    echo "updated packages/server/proto/$proto"
  fi
done

if [[ $drift -ne 0 ]]; then
  echo "run kcore-mcp/scripts/sync-protos.sh, then npm run build" >&2
  exit 1
fi
