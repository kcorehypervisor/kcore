#!/usr/bin/env bash
# Build release artifacts (Nix ISO + Linux/macOS kctl), package dist/, publish GitHub Release.
# Usage:
#   ./scripts/release.sh build    # nix build ISO + Linux kctl, cargo-zigbuild macOS kctl
#   ./scripts/release.sh dist     # dist/*.tar.gz, ISO copy, SBOMs, dist/SHA256SUMS
#   ./scripts/release.sh tag      # create/push v$(VERSION)
#   ./scripts/release.sh sign     # cosign sign-blob dist/SHA256SUMS (Sigstore keyless)
#   ./scripts/release.sh publish  # sign + gh release create/upload (needs tag on remote)
#   ./scripts/release.sh release  # tag + build + dist + publish
# Options:
#   --no-sign       Skip Sigstore signing. Explicit opt-out for a maintainer
#                   without cosign configured; the release is then UNSIGNED.
# Environment:
#   RELEASE_NOTES   Optional path to release notes file (defaults to RELEASE_NOTES.md if present;
#                   otherwise GitHub auto-generated release notes are used)
#   GH_REPO         owner/repo override for gh (optional; defaults to git remote)
set -euo pipefail

ROOT="$(cd "$(dirname "${0}")/.." && pwd)"
cd "${ROOT}"

VERSION="$(tr -d '\n' < VERSION)"
ISO_NAME="kcoreos-${VERSION}-x86_64-linux.iso"
KCTL_LINUX_ARCHIVE="kctl-${VERSION}-linux-x86_64.tar.gz"
KCTL_MACOS_X86_64_ARCHIVE="kctl-${VERSION}-macos-x86_64.tar.gz"
KCTL_MACOS_AARCH64_ARCHIVE="kctl-${VERSION}-macos-aarch64.tar.gz"
KCTL_ARCHIVES=(
	"${KCTL_LINUX_ARCHIVE}"
	"${KCTL_MACOS_X86_64_ARCHIVE}"
	"${KCTL_MACOS_AARCH64_ARCHIVE}"
)
# Two SBOMs describing different universes; see scripts/sbom.sh.
SBOM_FILES=(
	"kcore-${VERSION}-crates.cdx.json"
	"kcore-${VERSION}-iso-closure.cdx.json"
	"kcore-${VERSION}-iso-closure.spdx.json"
)
# Signing dist/SHA256SUMS transitively covers every other asset, SBOMs
# included, so this is the only signature a release carries.
SIGSTORE_BUNDLE="SHA256SUMS.sigstore.json"
# cosign 3.0.0-3.0.4 cannot verify Rekor v2 bundles without
# --use-signed-timestamps; 3.0.5 handles it automatically and already
# defaults --new-bundle-format=true, which is what writes .sigstore.json.
COSIGN_MIN_VERSION="3.0.5"
TAG="v${VERSION}"
NO_SIGN=0

die() {
	echo "release.sh: $*" >&2
	exit 1
}

require_cmd() {
	command -v "${1}" >/dev/null 2>&1 || die "missing required command: ${1}"
}

load_dotenv() {
	if [[ -f .env ]]; then
		set -a
		# shellcheck disable=SC1091
		source .env
		set +a
	fi
}

repo_slug() {
	if [[ -n "${GH_REPO:-}" ]]; then
		echo "${GH_REPO}"
		return
	fi

	remote_url="$(git remote get-url origin)"
	case "${remote_url}" in
		git@github.com:*)
			slug="${remote_url#git@github.com:}"
			;;
		https://github.com/*)
			slug="${remote_url#https://github.com/}"
			;;
		*)
			die "cannot infer GitHub repository from origin URL: ${remote_url}; set GH_REPO=owner/repo"
			;;
	esac
	echo "${slug%.git}"
}

validate_version() {
	[[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]] || die "VERSION must be semver-like X.Y.Z (got '${VERSION}')"
}

ensure_clean_tree() {
	[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash changes before release"
}

cmd_tag() {
	require_cmd git
	validate_version
	ensure_clean_tree

	head_commit="$(git rev-parse HEAD)"
	if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
		local_tag_commit="$(git rev-parse "${TAG}^{commit}")"
		[[ "${local_tag_commit}" == "${head_commit}" ]] || die "local tag ${TAG} points to ${local_tag_commit}, not HEAD ${head_commit}"
	else
		echo "==> Creating annotated tag ${TAG}..."
		git tag -a "${TAG}" -m "kcore ${VERSION}"
	fi

	remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}^{}" | awk '{print $1}')"
	if [[ -z "${remote_tag_commit}" ]]; then
		remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}" | awk '{print $1}')"
	fi
	if [[ -n "${remote_tag_commit}" ]]; then
		[[ "${remote_tag_commit}" == "${head_commit}" ]] || die "remote tag ${TAG} points to ${remote_tag_commit}, not HEAD ${head_commit}"
		echo "==> Remote tag ${TAG} already points at HEAD."
	else
		echo "==> Pushing tag ${TAG} to origin..."
		git push origin "${TAG}"
	fi
}

cmd_build() {
	require_cmd nix
	echo "==> Building ISO (${ISO_NAME})..."
	nix build ".#nixosConfigurations.kcore-iso.config.system.build.isoImage" -o result-iso
	echo "==> Building kctl for linux-x86_64..."
	nix build ".#kctl" -o result-kctl
	echo "==> Cross-building kctl for macos-x86_64..."
	nix develop --command cargo zigbuild -p kctl --release --target x86_64-apple-darwin
	echo "==> Cross-building kctl for macos-aarch64..."
	nix develop --command cargo zigbuild -p kctl --release --target aarch64-apple-darwin
	echo "==> Build outputs:"
	ls -lh result-iso/iso/*.iso
	ls -lh result-kctl/bin/kctl
	ls -lh target/x86_64-apple-darwin/release/kctl
	ls -lh target/aarch64-apple-darwin/release/kctl
}

cmd_dist() {
	require_cmd tar
	require_cmd sha256sum
	require_cmd nix
	[[ -f result-kctl/bin/kctl ]] || die "run '${0} build' first (missing result-kctl/bin/kctl)"
	[[ -f target/x86_64-apple-darwin/release/kctl ]] || die "run '${0} build' first (missing target/x86_64-apple-darwin/release/kctl)"
	[[ -f target/aarch64-apple-darwin/release/kctl ]] || die "run '${0} build' first (missing target/aarch64-apple-darwin/release/kctl)"
	shopt -s nullglob
	iso_candidates=(result-iso/iso/*.iso)
	shopt -u nullglob
	[[ "${#iso_candidates[@]}" -eq 1 ]] || die "expected exactly one ISO under result-iso/iso/; run '${0} build' first"
	ISO_SRC="${iso_candidates[0]}"

	mkdir -p dist
	echo "==> Packaging ${KCTL_LINUX_ARCHIVE}..."
	tar -C result-kctl/bin -czf "dist/${KCTL_LINUX_ARCHIVE}" kctl
	echo "==> Packaging ${KCTL_MACOS_X86_64_ARCHIVE}..."
	tar -C target/x86_64-apple-darwin/release -czf "dist/${KCTL_MACOS_X86_64_ARCHIVE}" kctl
	echo "==> Packaging ${KCTL_MACOS_AARCH64_ARCHIVE}..."
	tar -C target/aarch64-apple-darwin/release -czf "dist/${KCTL_MACOS_AARCH64_ARCHIVE}" kctl
	echo "==> Copying $(basename "${ISO_SRC}") to dist/${ISO_NAME}..."
	cp -f "${ISO_SRC}" "dist/${ISO_NAME}"

	# Generated here, before SHA256SUMS, so the SBOMs are covered by the
	# checksums file and therefore by the single signature over it.
	echo "==> Generating SBOMs..."
	nix develop --command bash ./scripts/sbom.sh all
	for sbom in "${SBOM_FILES[@]}"; do
		[[ -s "dist/${sbom}" ]] || die "scripts/sbom.sh produced no dist/${sbom}"
	done

	echo "==> Writing dist/SHA256SUMS..."
	(
		cd dist
		sha256sum "${ISO_NAME}" "${KCTL_ARCHIVES[@]}" "${SBOM_FILES[@]}" >SHA256SUMS
	)
	echo "==> dist layout:"
	ls -lh dist/
	cat dist/SHA256SUMS
}

# True when $1 is at least $2, both dotted versions. `sort -V` is fine here
# because releases are cut on Linux x86_64 by policy; verify-release.sh and
# get-kctl.sh also run on macOS, where BSD sort has no -V, and use awk.
version_ge() {
	[[ "$(printf '%s\n%s\n' "${2}" "${1}" | sort -V | head -n 1)" == "${2}" ]]
}

cmd_sign() {
	[[ -f dist/SHA256SUMS ]] || die "run '${0} dist' first (missing dist/SHA256SUMS)"

	if [[ "${NO_SIGN}" -eq 1 ]]; then
		echo "==> --no-sign given: skipping Sigstore signing."
		echo "    This release will be UNSIGNED; scripts/verify-release.sh"
		echo "    will fail for it. Drop --no-sign to sign."
		# A stale bundle from an earlier run would be worse than none:
		# it would verify against different bytes.
		rm -f "dist/${SIGSTORE_BUNDLE}"
		return 0
	fi

	require_cmd nix
	local cosign_version
	cosign_version="$(nix develop --command cosign version 2>/dev/null | awk '/^GitVersion:/ {print $2}')"
	cosign_version="${cosign_version#v}"
	[[ -n "${cosign_version}" ]] ||
		die "could not determine cosign version from 'nix develop --command cosign version'"
	version_ge "${cosign_version}" "${COSIGN_MIN_VERSION}" ||
		die "cosign ${cosign_version} is too old; need >= ${COSIGN_MIN_VERSION} for Rekor v2 (see docs/release-verification.md)"

	echo "==> Signing dist/SHA256SUMS with Sigstore (cosign ${cosign_version}, keyless)..."
	echo "    A browser window opens for OIDC login. Sign in with the"
	echo "    identity published in docs/release-verification.md; the"
	echo "    certificate and its identity go into the public Rekor log."
	# --yes accepts that transparency-log entry non-interactively, which
	# would otherwise block the release on a prompt.
	nix develop --command cosign sign-blob \
		--yes \
		--bundle "dist/${SIGSTORE_BUNDLE}" \
		dist/SHA256SUMS
	[[ -s "dist/${SIGSTORE_BUNDLE}" ]] ||
		die "cosign wrote no dist/${SIGSTORE_BUNDLE}"
	echo "==> Wrote dist/${SIGSTORE_BUNDLE}"
}

cmd_publish() {
	require_cmd nix
	require_cmd git
	load_dotenv
	for archive in "${KCTL_ARCHIVES[@]}"; do
		[[ -f "dist/${archive}" ]] || die "run '${0} dist' first (missing dist/${archive})"
	done
	[[ -f "dist/${ISO_NAME}" ]] || die "run '${0} dist' first"
	for sbom in "${SBOM_FILES[@]}"; do
		[[ -f "dist/${sbom}" ]] || die "run '${0} dist' first (missing dist/${sbom})"
	done
	[[ -f dist/SHA256SUMS ]] || die "run '${0} dist' first"

	cmd_sign

	target_commit="$(git rev-parse "${TAG}^{commit}" 2>/dev/null)" || die "missing local tag ${TAG}; run '${0} tag' first"
	remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}^{}" | awk '{print $1}')"
	if [[ -z "${remote_tag_commit}" ]]; then
		remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}" | awk '{print $1}')"
	fi
	[[ -n "${remote_tag_commit}" ]] || die "missing remote tag ${TAG}; run '${0} tag' first"
	[[ "${remote_tag_commit}" == "${target_commit}" ]] || die "remote tag ${TAG} points to ${remote_tag_commit}, not local tag target ${target_commit}"

	repo="$(repo_slug)"
	assets=("dist/${ISO_NAME}")
	for archive in "${KCTL_ARCHIVES[@]}"; do
		assets+=("dist/${archive}")
	done
	for sbom in "${SBOM_FILES[@]}"; do
		assets+=("dist/${sbom}")
	done
	assets+=(dist/SHA256SUMS)
	# Absent only under --no-sign, which cmd_sign already warned about.
	if [[ -f "dist/${SIGSTORE_BUNDLE}" ]]; then
		assets+=("dist/${SIGSTORE_BUNDLE}")
	fi
	create_args=(
		api "repos/${repo}/releases"
		-X POST
		-f tag_name="${TAG}"
		-f target_commitish="${target_commit}"
		-f name="kcore ${VERSION}"
		-F draft=false
		-F prerelease=false
	)
	if [[ -n "${RELEASE_NOTES:-}" ]]; then
		[[ -f "${RELEASE_NOTES}" ]] || die "missing RELEASE_NOTES file: ${RELEASE_NOTES}"
		create_args+=(-f body="$(<"${RELEASE_NOTES}")")
	elif [[ -f RELEASE_NOTES.md ]]; then
		create_args+=(-f body="$(<RELEASE_NOTES.md)")
	else
		create_args+=(-F generate_release_notes=true)
	fi

	if nix develop --command gh release view "${TAG}" >/dev/null 2>&1; then
		echo "==> GitHub release ${TAG} exists; uploading assets with --clobber..."
	else
		echo "==> Creating GitHub release ${TAG}..."
		nix develop --command gh "${create_args[@]}" --jq .html_url
	fi

	for asset in "${assets[@]}"; do
		echo "==> Uploading $(basename "${asset}")..."
		nix develop --command gh release upload "${TAG}" --clobber "${asset}"
	done
	echo "==> Done: nix develop --command gh release view ${TAG}"
}

cmd_release() {
	cmd_tag
	cmd_build
	cmd_dist
	cmd_publish
}

usage() {
	echo "Usage: ${0} [--no-sign] {build|dist|tag|sign|publish|release}"
	exit 1
}

subcommand=""
while [[ "$#" -gt 0 ]]; do
	case "${1}" in
		--no-sign) NO_SIGN=1 ;;
		-h | --help) usage ;;
		-*) die "unknown option: ${1}" ;;
		*)
			[[ -z "${subcommand}" ]] || die "unexpected extra argument: ${1}"
			subcommand="${1}"
			;;
	esac
	shift
done

case "${subcommand}" in
	build) cmd_build ;;
	dist) cmd_dist ;;
	tag) cmd_tag ;;
	sign) cmd_sign ;;
	publish) cmd_publish ;;
	release) cmd_release ;;
	*) usage ;;
esac
