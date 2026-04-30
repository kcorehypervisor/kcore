#!/usr/bin/env bash
# Build release artifacts (Nix ISO + Linux/macOS kctl), package dist/, publish GitHub Release.
# Usage:
#   ./scripts/release.sh build    # nix build ISO + Linux kctl, cargo-zigbuild macOS kctl
#   ./scripts/release.sh dist     # dist/*.tar.gz, ISO copy, dist/SHA256SUMS
#   ./scripts/release.sh tag      # create/push v$(VERSION)
#   ./scripts/release.sh publish  # gh release create/upload (needs tag on remote)
#   ./scripts/release.sh release  # tag + build + dist + publish
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
TAG="v${VERSION}"

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
	echo "==> Writing dist/SHA256SUMS..."
	(
		cd dist
		sha256sum "${ISO_NAME}" "${KCTL_ARCHIVES[@]}" >SHA256SUMS
	)
	echo "==> dist layout:"
	ls -lh dist/
	cat dist/SHA256SUMS
}

cmd_publish() {
	require_cmd nix
	require_cmd git
	load_dotenv
	for archive in "${KCTL_ARCHIVES[@]}"; do
		[[ -f "dist/${archive}" ]] || die "run '${0} dist' first (missing dist/${archive})"
	done
	[[ -f "dist/${ISO_NAME}" ]] || die "run '${0} dist' first"
	[[ -f dist/SHA256SUMS ]] || die "run '${0} dist' first"

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
	assets+=(dist/SHA256SUMS)
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
	echo "Usage: ${0} {build|dist|tag|publish|release}"
	exit 1
}

case "${1:-}" in
	build) cmd_build ;;
	dist) cmd_dist ;;
	tag) cmd_tag ;;
	publish) cmd_publish ;;
	release) cmd_release ;;
	*) usage ;;
esac
