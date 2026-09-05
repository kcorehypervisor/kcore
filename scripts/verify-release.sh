#!/usr/bin/env bash
# Verify a published kcore release: the Sigstore signature over SHA256SUMS,
# then the checksums of the downloaded assets.
#
# This is the executable form of docs/release-verification.md. Keeping both in
# one place is deliberate: a documented procedure nobody runs drifts.
#
#   ./scripts/verify-release.sh v0.3.0
#   ./scripts/verify-release.sh v0.3.0 'kctl-*-linux-x86_64.tar.gz'
#
# What this proves: a kcore maintainer, holding the identity below, stood
# behind these exact bytes. It does NOT prove which commit or machine built
# them -- see docs/release-verification.md.
#
# Environment:
#   KCORE_SIGNER_IDENTITY         Expected certificate identity (exact match).
#   KCORE_SIGNER_IDENTITY_REGEXP  Alternative: Go regexp over identities,
#                                 useful when several maintainers may sign.
#   KCORE_SIGNER_ISSUER           Expected OIDC issuer.
#   KCORE_VERIFY_DIR              Download directory. Kept on exit when set;
#                                 otherwise a temp dir is used and removed.
#   KCORE_VERIFY_ISO=1            Also download and check the ~1.3 GB ISO.
#   KCORE_GITHUB_REPO             owner/repo (default: kcorehypervisor/kcore).
set -euo pipefail

DEFAULT_REPO="kcorehypervisor/kcore"
REPO="${KCORE_GITHUB_REPO:-${DEFAULT_REPO}}"

# Sigstore's public Fulcio issues certificates through its Dex instance for
# interactive (non-CI) signing, so this is the issuer recorded for a
# maintainer signing on their own machine -- not github.com. The identity is
# the email address of the account chosen at the OIDC prompt.
SIGNER_ISSUER="${KCORE_SIGNER_ISSUER:-https://oauth2.sigstore.dev/auth}"
SIGNER_IDENTITY="${KCORE_SIGNER_IDENTITY:-}"
SIGNER_IDENTITY_REGEXP="${KCORE_SIGNER_IDENTITY_REGEXP:-}"

# cosign 3.0.0-3.0.4 fails to verify Rekor v2 bundles unless called with
# --use-signed-timestamps; 3.0.5 handles it automatically. Keep this in step
# with COSIGN_MIN_VERSION in scripts/release.sh.
COSIGN_MIN_VERSION="3.0.5"

SUMS_FILE="SHA256SUMS"
BUNDLE_FILE="SHA256SUMS.sigstore.json"

VERIFY_TMPDIR=""

cleanup() {
	if [[ -n "${VERIFY_TMPDIR}" ]]; then
		rm -rf "${VERIFY_TMPDIR}"
	fi
}
trap cleanup EXIT

die() {
	echo "verify-release.sh: $*" >&2
	exit 1
}

require_cmd() {
	command -v "${1}" >/dev/null 2>&1 || die "missing required command: ${1}"
}

# True when $1 is at least $2, both dotted versions. Deliberately awk rather
# than `sort -V`: auditors run this on macOS too, and BSD sort has no -V.
version_ge() {
	awk -v have="${1}" -v need="${2}" 'BEGIN {
		n = split(have, h, ".")
		m = split(need, r, ".")
		top = (n > m) ? n : m
		for (i = 1; i <= top; i++) {
			hv = (i <= n) ? h[i] + 0 : 0
			rv = (i <= m) ? r[i] + 0 : 0
			if (hv > rv) exit 0
			if (hv < rv) exit 1
		}
		exit 0
	}'
}

check_cosign_version() {
	local have
	have="$(cosign version 2>/dev/null | awk '/^GitVersion:/ {print $2}')"
	have="${have#v}"
	[[ -n "${have}" ]] || die "could not parse the output of 'cosign version'"
	version_ge "${have}" "${COSIGN_MIN_VERSION}" || die "\
cosign ${have} is too old to verify this release; need >= ${COSIGN_MIN_VERSION}.
  cosign 3.0.0-3.0.4 rejects Rekor v2 bundles unless you also pass
  --use-signed-timestamps. Upgrade cosign, or from this repo run
  'nix develop --command cosign ...' to get the pinned version."
	echo "==> cosign ${have} (>= ${COSIGN_MIN_VERSION})"
}

main() {
	TAG="${1:-}"
	[[ -n "${TAG}" ]] || die "usage: ${0} vX.Y.Z [asset-pattern ...]"
	[[ "${TAG}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]] ||
		die "expected a tag like v0.3.0 (got '${TAG}')"
	shift

	require_cmd gh
	require_cmd cosign
	require_cmd sha256sum
	check_cosign_version

	# A Sigstore signature is only meaningful against an identity you
	# expect: verifying that "somebody" signed the bytes proves nothing.
	local -a identity_args
	if [[ -n "${SIGNER_IDENTITY_REGEXP}" ]]; then
		identity_args=(--certificate-identity-regexp "${SIGNER_IDENTITY_REGEXP}")
	elif [[ -n "${SIGNER_IDENTITY}" ]]; then
		identity_args=(--certificate-identity "${SIGNER_IDENTITY}")
	else
		die "\
no expected signing identity configured.

  Set the identity published for this project, for example:

    KCORE_SIGNER_IDENTITY='maintainer@example.org' ${0} ${TAG}

  docs/release-verification.md carries the published value; once it is
  agreed, set SIGNER_IDENTITY at the top of this script so the default
  works with no environment at all."
	fi

	local workdir
	if [[ -n "${KCORE_VERIFY_DIR:-}" ]]; then
		workdir="${KCORE_VERIFY_DIR}"
		mkdir -p "${workdir}"
	else
		VERIFY_TMPDIR="$(mktemp -d)"
		workdir="${VERIFY_TMPDIR}"
	fi

	# The ISO is excluded by default: it is ~1.3 GB and the point of
	# signing SHA256SUMS is that you can verify the manifest first and
	# fetch only the assets you actually want.
	local -a patterns=("${SUMS_FILE}" "${BUNDLE_FILE}")
	if [[ "$#" -gt 0 ]]; then
		patterns+=("$@")
	else
		patterns+=('*.tar.gz' '*.cdx.json' '*.spdx.json')
		if [[ "${KCORE_VERIFY_ISO:-0}" == "1" ]]; then
			patterns+=('*.iso')
		fi
	fi

	echo "==> Downloading ${TAG} assets from ${REPO} into ${workdir}..."
	local -a download_args=(release download "${TAG}" --repo "${REPO}" --dir "${workdir}" --clobber)
	local pattern
	for pattern in "${patterns[@]}"; do
		download_args+=(--pattern "${pattern}")
	done
	gh "${download_args[@]}"

	[[ -f "${workdir}/${SUMS_FILE}" ]] || die "${TAG} has no ${SUMS_FILE} asset"
	[[ -f "${workdir}/${BUNDLE_FILE}" ]] || die "\
${TAG} has no ${BUNDLE_FILE} asset, so it is unsigned.

  Releases cut with 'release.sh --no-sign' carry no signature. Checksums
  alone do not detect tampering, because SHA256SUMS is served from the
  same place as the artifacts."

	# Requires network access to Rekor: the bundle's inclusion proof is
	# checked against the public transparency log.
	echo "==> Verifying ${SUMS_FILE} signature (needs network access to Rekor)..."
	cosign verify-blob \
		--bundle "${workdir}/${BUNDLE_FILE}" \
		"${identity_args[@]}" \
		--certificate-oidc-issuer "${SIGNER_ISSUER}" \
		"${workdir}/${SUMS_FILE}"

	# --ignore-missing so a partial download (no ISO, say) still checks
	# everything present instead of failing on absent lines.
	echo "==> Checking asset checksums against the signed ${SUMS_FILE}..."
	(
		cd "${workdir}"
		sha256sum --check --ignore-missing "${SUMS_FILE}"
	)

	echo "==> ${TAG} verified: signature by ${SIGNER_IDENTITY:-${SIGNER_IDENTITY_REGEXP}} and matching checksums."
	if [[ -n "${KCORE_VERIFY_DIR:-}" ]]; then
		echo "    Assets kept in ${workdir}"
	fi
}

main "$@"
