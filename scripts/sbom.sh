#!/usr/bin/env bash
# Generate the crate SBOM into dist/, where `release.sh dist` folds it into
# SHA256SUMS.
#
# Usage:
#   ./scripts/sbom.sh crates   # dist/kcore-$(VERSION)-crates.cdx.json
#
# This is the Rust dependency graph resolved from Cargo.lock, and only that.
# The ISO's Nix closure -- kernel, systemd, cloud-hypervisor, openssl, qemu --
# is a different universe that the Cargo graph knows nothing about.
set -euo pipefail

ROOT="$(cd "$(dirname "${0}")/.." && pwd)"
cd "${ROOT}"

VERSION="$(tr -d '\n' < VERSION)"
DIST="${ROOT}/dist"
CRATES_SBOM="${DIST}/kcore-${VERSION}-crates.cdx.json"

# A floor, not an exact count: the failure this catches is an invocation that
# resolves to an empty or near-empty graph, which is otherwise a perfectly
# valid document and passes every schema check. The real graph is in the
# hundreds, so anything this small means the target is wrong.
MIN_COMPONENTS=100

# CycloneDX 1.5 is the newest spec version cargo-cyclonedx 0.5 can emit. The
# two tools spell the same version differently.
CDX_SPEC="1.5"
CDX_SPEC_CLI="v1_5"

# Placeholder substituted for the maintainer's checkout path; see
# strip_checkout_path().
CDX_PATH_PLACEHOLDER="file:///kcore"

die() {
	echo "sbom.sh: $*" >&2
	exit 1
}

require_cmd() {
	command -v "${1}" >/dev/null 2>&1 ||
		die "missing required command: ${1} (run inside 'nix develop')"
}

# cyclonedx-cli writes an http_cache.sqlite into its working directory, which
# would leave the tree dirty after a release. Give it scratch space instead.
cyclonedx_cli() {
	local scratch status=0
	scratch="$(mktemp -d)"
	(cd "${scratch}" && cyclonedx "$@") || status=$?
	rm -rf "${scratch}"
	return "${status}"
}

# cargo-cyclonedx has neither a --package selector nor an output-directory
# flag: it always writes one <package>.cdx.json next to every workspace
# member's Cargo.toml, regardless of cwd. Those files are intermediates and
# never release assets, so they are collected, merged, and removed here.
member_sboms=()

collect_member_sboms() {
	shopt -s nullglob
	member_sboms=("${ROOT}"/crates/*/*.cdx.json)
	shopt -u nullglob
}

clean_member_sboms() {
	collect_member_sboms
	[[ "${#member_sboms[@]}" -eq 0 ]] || rm -f "${member_sboms[@]}"
}

# cargo-cyclonedx records Cargo path dependencies -- i.e. every workspace
# member -- as absolute file:// URLs, so an unedited document would publish
# the maintainer's checkout layout. Substituting the literal root string is
# reference-preserving because bom-ref and the `dependencies` graph spell the
# path the same way; the grep guards against a root containing characters sed
# treats specially, and validate_cdx() runs immediately afterwards.
strip_checkout_path() {
	local file="${1}"
	sed -i "s|file://${ROOT}|${CDX_PATH_PLACEHOLDER}|g" "${file}"
	if grep -qF "${ROOT}" "${file}"; then
		die "could not strip build path ${ROOT} from ${file}"
	fi
}

# Count occurrences of a per-component JSON key and insist on a floor. Uses
# grep rather than jq or python so the script depends only on the pinned dev
# shell; the generator emits one key per component, and over-counting would
# only make this check more permissive, never falsely fail.
assert_min_components() {
	local file="${1}" key="${2}" found
	found="$(grep -o "\"${key}\"" "${file}" | wc -l)"
	[[ "${found}" -ge "${MIN_COMPONENTS}" ]] ||
		die "$(basename "${file}") lists only ${found} ${key} entries (expected >= ${MIN_COMPONENTS}); the generator target is probably wrong"
	echo "==> $(basename "${file}"): ${found} ${key} entries"
}

# cyclonedx-cli defaults --input-version to the newest spec it knows, which
# rejects older documents. Read the version out of the document rather than
# guessing.
validate_cdx() {
	local file="${1}" spec cli_version
	spec="$(sed -n 's/.*"specVersion"[[:space:]]*:[[:space:]]*"\([0-9]\{1,\}\.[0-9]\{1,\}\)".*/\1/p' "${file}" | head -n 1)"
	[[ -n "${spec}" ]] || die "${file} has no specVersion field"
	cli_version="v${spec//./_}"
	echo "==> Validating $(basename "${file}") against CycloneDX ${spec}..."
	cyclonedx_cli validate \
		--input-file "${file}" \
		--input-format json \
		--input-version "${cli_version}" \
		--fail-on-errors
}

cmd_crates() {
	require_cmd cargo
	require_cmd cargo-cyclonedx
	require_cmd cyclonedx
	mkdir -p "${DIST}"

	# Clear stale intermediates first so a merge cannot silently pick up a
	# member SBOM left behind by an earlier, differently-versioned run.
	clean_member_sboms
	trap clean_member_sboms EXIT

	echo "==> Generating per-crate CycloneDX SBOMs..."
	# --all: the full transitive graph, not just direct dependencies.
	# --target all: match Cargo.lock, which is the union over all targets;
	#   the default resolves for the host target only.
	cargo cyclonedx \
		--format json \
		--spec-version "${CDX_SPEC}" \
		--all \
		--target all \
		--quiet

	collect_member_sboms
	[[ "${#member_sboms[@]}" -gt 0 ]] ||
		die "cargo-cyclonedx wrote no SBOMs under crates/*/"

	# Hierarchical rather than flat merge. kcore ships several binaries with
	# different dependency sets, and a flat union would assert one
	# dependency list that no shipped artifact actually has. Hierarchical
	# keeps each crate's subtree, so an auditor can see which binary pulls
	# in a given crate.
	echo "==> Merging ${#member_sboms[@]} crate SBOMs into $(basename "${CRATES_SBOM}")..."
	cyclonedx_cli merge \
		--hierarchical \
		--name kcore \
		--version "${VERSION}" \
		--input-format json \
		--output-format json \
		--output-version "${CDX_SPEC_CLI}" \
		--output-file "${CRATES_SBOM}" \
		--input-files "${member_sboms[@]}"

	strip_checkout_path "${CRATES_SBOM}"
	validate_cdx "${CRATES_SBOM}"
	assert_min_components "${CRATES_SBOM}" purl

	ls -lh "${CRATES_SBOM}"
}

usage() {
	echo "Usage: ${0} {crates}"
	exit 1
}

case "${1:-}" in
	crates) cmd_crates ;;
	*) usage ;;
esac
