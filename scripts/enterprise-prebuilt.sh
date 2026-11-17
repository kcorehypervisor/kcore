#!/usr/bin/env bash
# Build the enterprise prebuilt Nix file cache for a release.
# Invoked by scripts/release.sh prebuilt. Not meant to be run alone in CI.
#
# Inputs (env):
#   VERSION                     product version (X.Y.Z)
#   KCORE_NIX_SIGNING_KEY_FILE  path to the kcore-enterprise-1 secret key
#   ROOT                        repository root (set by release.sh)
# Outputs:
#   dist/enterprise/kcore-prebuilt-${VERSION}-x86_64-linux.tar
set -euo pipefail

ROOT="${ROOT:-$(cd "$(dirname "${0}")/.." && pwd)}"
cd "${ROOT}"

VERSION="${VERSION:-$(tr -d '\n' < VERSION)}"
PREBUILT_NAME="kcore-prebuilt-${VERSION}-x86_64-linux.tar"
WORK="${ROOT}/dist/enterprise-work"
CACHE="${WORK}/cache"
STORE="${WORK}/store"
REHEARSAL_STORE="${WORK}/rehearsal-store"
MAX_ASSET_BYTES=$((2 * 1024 * 1024 * 1024)) # GitHub release asset limit

die() {
	echo "enterprise-prebuilt.sh: $*" >&2
	exit 1
}

require_cmd() {
	command -v "${1}" >/dev/null 2>&1 || die "missing required command: ${1}"
}

[[ -n "${KCORE_NIX_SIGNING_KEY_FILE:-}" ]] ||
	die "set KCORE_NIX_SIGNING_KEY_FILE to the kcore-enterprise-1 secret key"
[[ -f "${KCORE_NIX_SIGNING_KEY_FILE}" ]] ||
	die "missing signing key file: ${KCORE_NIX_SIGNING_KEY_FILE}"
require_cmd nix
require_cmd tar
require_cmd curl

PUBLIC_KEY="$(nix key convert-secret-to-public <"${KCORE_NIX_SIGNING_KEY_FILE}")"
[[ "${PUBLIC_KEY}" == kcore-enterprise-1:* ]] ||
	die "signing key is not named kcore-enterprise-1 (got ${PUBLIC_KEY%%:*})"

rm -rf "${WORK}"
mkdir -p "${CACHE}/nar" "${STORE}" "dist/enterprise"
printf 'StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n' >"${CACHE}/nix-cache-info"

# Real host flakes pull these packages from the kcore input. The
# nixosConfigurations.kcore-node fixtures use hello placeholders so their
# closures are useless for a binary cache.
packages=(kctl kcore-node-agent kcore-controller kcore-console kcore-dashboard)
echo "==> Building kcore packages for prebuilt cache..."
for attr in "${packages[@]}"; do
	nix build ".#${attr}" -o "${WORK}/${attr}"
done

# Paths that cache.nixos.org already serves stay out of the enterprise tar.
# Everything else (kcore packages and their runtime closure) is signed and
# exported into the file cache.
echo "==> Collecting closure paths not on cache.nixos.org..."
: >"${WORK}/all-paths.txt"
: >"${WORK}/paths.txt"
for attr in "${packages[@]}"; do
	# Do not pass --store /nix/store: a bare path is a chroot store root, so
	# Nix would look under /nix/store/nix/store and fail on the read-only FS.
	nix path-info -r "$(readlink -f "${WORK}/${attr}")" >>"${WORK}/all-paths.txt"
done
sort -u "${WORK}/all-paths.txt" -o "${WORK}/all-paths.txt"

while IFS= read -r path; do
	[[ -n "${path}" ]] || continue
	hash_part="${path##*/}"
	hash_part="${hash_part%%-*}"
	if curl -fsI "https://cache.nixos.org/${hash_part}.narinfo" >/dev/null 2>&1; then
		continue
	fi
	echo "${path}" >>"${WORK}/paths.txt"
done <"${WORK}/all-paths.txt"

[[ -s "${WORK}/paths.txt" ]] || die "no local paths to export; nothing to put in the prebuilt cache"

echo "==> Signing $(wc -l <"${WORK}/paths.txt") paths with ${PUBLIC_KEY%%:*}..."
# shellcheck disable=SC2046
nix store sign --key-file "${KCORE_NIX_SIGNING_KEY_FILE}" $(tr '\n' ' ' <"${WORK}/paths.txt")

echo "==> Exporting file binary cache..."
# shellcheck disable=SC2046
nix copy --to "file://${CACHE}?compression=zstd" $(tr '\n' ' ' <"${WORK}/paths.txt")

echo "==> Writing ${PREBUILT_NAME}..."
tar -C "${CACHE}" -cf "dist/enterprise/${PREBUILT_NAME}" .
asset_size="$(wc -c <"dist/enterprise/${PREBUILT_NAME}")"
if ((asset_size > MAX_ASSET_BYTES)); then
	die "${PREBUILT_NAME} is ${asset_size} bytes; GitHub limits assets to 2 GiB"
fi

echo "==> Rehearsing: prebuilt + cache.nixos.org must build kcore packages with --max-jobs 0..."
rm -rf "${REHEARSAL_STORE}"
mkdir -p "${REHEARSAL_STORE}"
# Rehearse against this tree. Day-0 enterprise publish pins the same commit
# on main; customers resolve github:kcorehypervisor/kcore/<rev> to matching
# store paths.
flake_ref="path:${ROOT}"
for attr in "${packages[@]}"; do
	echo "    nix build ${flake_ref}#${attr} --max-jobs 0"
	nix build \
		--store "${REHEARSAL_STORE}" \
		--extra-substituters "file://${CACHE}?trusted=1 https://cache.nixos.org" \
		--extra-trusted-public-keys "${PUBLIC_KEY} cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=" \
		--max-jobs 0 \
		"${flake_ref}#${attr}" \
		-o "${WORK}/rehearsal-${attr}" ||
		die "rehearsal failed for ${attr}: a path would have to compile"
done

echo "==> Prebuilt ready: dist/enterprise/${PREBUILT_NAME} ($(numfmt --to=iec "${asset_size}" 2>/dev/null || echo "${asset_size} bytes"))"
