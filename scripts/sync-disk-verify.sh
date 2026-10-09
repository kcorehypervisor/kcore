#!/usr/bin/env bash
# crates/kcore-disk-verify is a copy of kcore-provisioning/crates/kcore-disk-verify
# (src/ and tests/); the provisioning repository is the source of truth. The
# Cargo.toml is not synced: here it has explicit versions instead of workspace
# inheritance.
#
#   scripts/sync-disk-verify.sh PATH_TO_KCORE_PROVISIONING          copy
#   scripts/sync-disk-verify.sh --check PATH_TO_KCORE_PROVISIONING  fail if they differ
set -euo pipefail

mode=copy
if [ "${1:-}" = "--check" ]; then
	mode=check
	shift
fi
src="${1:?path to the kcore-provisioning checkout}/crates/kcore-disk-verify"
dest="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/crates/kcore-disk-verify"
[ -d "$src/src" ] || { echo "not a kcore-provisioning checkout: $1" >&2; exit 1; }

for d in src tests; do
	if [ "$mode" = check ]; then
		diff -ru "$src/$d" "$dest/$d" || { echo "kcore-disk-verify/$d differs from kcore-provisioning" >&2; exit 1; }
	else
		rm -rf "${dest:?}/$d"
		cp -r "$src/$d" "$dest/$d"
	fi
done
[ "$mode" = check ] && echo "kcore-disk-verify is in sync" || echo "copied kcore-disk-verify from $src"
