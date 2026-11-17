#!/usr/bin/env bash
# Build release artifacts (Nix ISO + Linux/macOS kctl), package dist/, publish
# the enterprise channel, then stage (and later publish) the community release.
# Usage:
#   ./scripts/release.sh build              # nix build ISO + Linux/macOS kctl
#   ./scripts/release.sh dist               # dist/*.tar.gz, ISO, SBOMs, SHA256SUMS
#   ./scripts/release.sh tag                # create local annotated tag (no push)
#   ./scripts/release.sh prebuilt           # enterprise prebuilt Nix file cache
#   ./scripts/release.sh publish-enterprise # release on kcorehypervisor/enterprise
#   ./scripts/release.sh stage-public       # draft community release (no tag push)
#   ./scripts/release.sh publish-public     # push tag + publish draft (≥14 days)
#   ./scripts/release.sh sign               # cosign sign-blob dist/SHA256SUMS
#   ./scripts/release.sh publish            # alias of stage-public (compat)
#   ./scripts/release.sh release            # tag + build + dist + prebuilt +
#                                           # publish-enterprise + stage-public
# Options:
#   --no-sign         Skip Sigstore signing. Explicit opt-out; unsigned release.
#   --break-embargo   Allow publish-public before the 14-day embargo ends.
# Environment:
#   RELEASE_NOTES              Optional notes file (defaults to RELEASE_NOTES.md)
#   GH_REPO                    owner/repo override for the public channel
#   KCORE_NIX_SIGNING_KEY_FILE Path to the kcore-enterprise-1 Nix secret key
#   KCORE_ENTERPRISE_REPO      Override enterprise repo (default kcorehypervisor/enterprise)
#   KCORE_EMBARGO_DAYS         Days before a draft may be published (default 14)
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
# included, so this is the only signature a community release carries.
SIGSTORE_BUNDLE="SHA256SUMS.sigstore.json"
# Must match docs/release-verification.md, scripts/verify-release.sh, and
# scripts/get-kctl.sh.
SIGNER_IDENTITY="82217+rtacconi@users.noreply.github.com"
SIGNER_ISSUER="https://github.com/login/oauth"
# cosign 3.0.0-3.0.4 cannot verify Rekor v2 bundles without
# --use-signed-timestamps; 3.0.5 handles it automatically and already
# defaults --new-bundle-format=true, which is what writes .sigstore.json.
COSIGN_MIN_VERSION="3.0.5"
TAG="v${VERSION}"
ENTERPRISE_REPO="${KCORE_ENTERPRISE_REPO:-kcorehypervisor/enterprise}"
PREBUILT_NAME="kcore-prebuilt-${VERSION}-x86_64-linux.tar"
EMBARGO_DAYS="${KCORE_EMBARGO_DAYS:-14}"
NO_SIGN=0
BREAK_EMBARGO=0

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
	# Day 0 creates the local tag only. The remote tag is pushed by
	# publish-public after the enterprise embargo ends.
	require_cmd git
	validate_version
	ensure_clean_tree

	head_commit="$(git rev-parse HEAD)"
	if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
		local_tag_commit="$(git rev-parse "${TAG}^{commit}")"
		[[ "${local_tag_commit}" == "${head_commit}" ]] || die "local tag ${TAG} points to ${local_tag_commit}, not HEAD ${head_commit}"
		echo "==> Local tag ${TAG} already points at HEAD."
	else
		echo "==> Creating annotated tag ${TAG} (not pushed; publish-public pushes it)..."
		git tag -a "${TAG}" -m "kcore ${VERSION}"
	fi

	remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}^{}" | awk '{print $1}')"
	if [[ -z "${remote_tag_commit}" ]]; then
		remote_tag_commit="$(git ls-remote origin "refs/tags/${TAG}" | awk '{print $1}')"
	fi
	if [[ -n "${remote_tag_commit}" ]]; then
		[[ "${remote_tag_commit}" == "${head_commit}" ]] || die "remote tag ${TAG} points to ${remote_tag_commit}, not HEAD ${head_commit}"
		echo "==> Remote tag ${TAG} already points at HEAD."
	fi
}

cmd_prebuilt() {
	load_dotenv
	export ROOT VERSION KCORE_NIX_SIGNING_KEY_FILE
	bash "${ROOT}/scripts/enterprise-prebuilt.sh"
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

	echo "==> Writing dist/kcore-${VERSION}-release.json..."
	nixpkgs_rev="$(nix eval --raw --file flake.lock --apply 'lock: lock.nodes.nixpkgs.locked.rev' 2>/dev/null || python3 -c 'import json; print(json.load(open("flake.lock"))["nodes"]["nixpkgs"]["locked"]["rev"])')"
	schema_version="$(sed -n 's/^pub const SCHEMA_VERSION: i32 = \([0-9]*\);/\1/p' crates/controller/src/db.rs | head -n 1)"
	min_reader="$(sed -n 's/^pub const MIN_COMPATIBLE_SCHEMA_READER: i32 = \([0-9]*\);/\1/p' crates/controller/src/db.rs | head -n 1)"
	ceph_rev="$(sed -n 's|.*nixpkgs/archive/\([0-9a-f]*\)\.tar.gz.*|\1|p' modules/kcore-ceph.nix | head -n 1)"
	ceph_sha="$(sed -n 's/.*sha256 = "\([^"]*\)".*/\1/p' modules/kcore-ceph.nix | head -n 1)"
	k8s_version="$(nix eval --raw .#nixosConfigurations.kcore-node.config.services.kcore.version >/dev/null 2>&1; nix eval --raw nixpkgs#kubernetes.version 2>/dev/null || true)"
	kcore_rev="$(git rev-parse HEAD)"
	# Community nodes fetch this git commit and compile it. A later private
	# cache will serve prebuilt closures, the provisioner, and backup.
	cat >"dist/kcore-${VERSION}-release.json" <<EOF
{
  "version": "${VERSION}",
  "channel": "community",
  "nodeBuild": "source",
  "kcoreRev": "${kcore_rev}",
  "schemaVersion": ${schema_version:-0},
  "previousSchemaVersion": ${min_reader:-0},
  "minCompatibleReader": ${min_reader:-0},
  "iso": "${ISO_NAME}",
  "kctl": [
    "${KCTL_LINUX_ARCHIVE}",
    "${KCTL_MACOS_X86_64_ARCHIVE}",
    "${KCTL_MACOS_AARCH64_ARCHIVE}"
  ],
  "docsUrl": "https://kcorehypervisor.com/docs/${VERSION}/",
  "terraformProvider": {
    "source": "kcorehypervisor/kcore",
    "version": "${VERSION}"
  },
  "testedOsPins": [
    {
      "nixpkgsRev": "${nixpkgs_rev}",
      "cephRev": "${ceph_rev}",
      "cephSha256": "${ceph_sha}",
      "kubernetesVersion": "${k8s_version}"
    }
  ]
}
EOF
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
		sha256sum "${ISO_NAME}" "${KCTL_ARCHIVES[@]}" "${SBOM_FILES[@]}" "kcore-${VERSION}-release.json" >SHA256SUMS
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
	echo "    A browser window opens for OIDC login. Choose GitHub and sign in"
	echo "    as ${SIGNER_IDENTITY}; the certificate and its identity go into"
	echo "    the public Rekor log."
	# --yes accepts that transparency-log entry non-interactively, which
	# would otherwise block the release on a prompt.
	nix develop --command cosign sign-blob \
		--yes \
		--bundle "dist/${SIGSTORE_BUNDLE}" \
		dist/SHA256SUMS
	[[ -s "dist/${SIGSTORE_BUNDLE}" ]] ||
		die "cosign wrote no dist/${SIGSTORE_BUNDLE}"
	# A login with another account still produces a valid bundle, which
	# every verifier would then reject. Catch it before anything is published.
	if ! nix develop --command cosign verify-blob \
		--bundle "dist/${SIGSTORE_BUNDLE}" \
		--certificate-identity "${SIGNER_IDENTITY}" \
		--certificate-oidc-issuer "${SIGNER_ISSUER}" \
		dist/SHA256SUMS; then
		rm -f "dist/${SIGSTORE_BUNDLE}"
		die "dist/SHA256SUMS was not signed as ${SIGNER_IDENTITY} (${SIGNER_ISSUER}); run '${0} sign' again with that account"
	fi
	echo "==> Wrote dist/${SIGSTORE_BUNDLE}"
}

public_assets() {
	local assets=("dist/${ISO_NAME}")
	local archive sbom
	for archive in "${KCTL_ARCHIVES[@]}"; do
		assets+=("dist/${archive}")
	done
	for sbom in "${SBOM_FILES[@]}"; do
		assets+=("dist/${sbom}")
	done
	assets+=("dist/kcore-${VERSION}-release.json")
	assets+=(dist/SHA256SUMS)
	if [[ -f "dist/${SIGSTORE_BUNDLE}" ]]; then
		assets+=("dist/${SIGSTORE_BUNDLE}")
	fi
	printf '%s\n' "${assets[@]}"
}

require_public_dist() {
	local archive sbom
	for archive in "${KCTL_ARCHIVES[@]}"; do
		[[ -f "dist/${archive}" ]] || die "run '${0} dist' first (missing dist/${archive})"
	done
	[[ -f "dist/${ISO_NAME}" ]] || die "run '${0} dist' first"
	for sbom in "${SBOM_FILES[@]}"; do
		[[ -f "dist/${sbom}" ]] || die "run '${0} dist' first (missing dist/${sbom})"
	done
	[[ -f dist/SHA256SUMS ]] || die "run '${0} dist' first"
	[[ -f "dist/kcore-${VERSION}-release.json" ]] || die "run '${0} dist' first (missing release.json)"
}

release_notes_body() {
	if [[ -n "${RELEASE_NOTES:-}" ]]; then
		[[ -f "${RELEASE_NOTES}" ]] || die "missing RELEASE_NOTES file: ${RELEASE_NOTES}"
		cat "${RELEASE_NOTES}"
	elif [[ -f RELEASE_NOTES.md ]]; then
		cat RELEASE_NOTES.md
	else
		echo "kcore ${VERSION}"
	fi
}

cmd_publish_enterprise() {
	require_cmd nix
	require_cmd git
	require_cmd sha256sum
	load_dotenv
	[[ -f "dist/enterprise/${PREBUILT_NAME}" ]] || die "run '${0} prebuilt' first (missing dist/enterprise/${PREBUILT_NAME})"

	mkdir -p dist/enterprise
	local kcore_rev schema_version min_reader
	kcore_rev="$(git rev-parse HEAD)"
	schema_version="$(sed -n 's/^pub const SCHEMA_VERSION: i32 = \([0-9]*\);/\1/p' crates/controller/src/db.rs | head -n 1)"
	min_reader="$(sed -n 's/^pub const MIN_COMPATIBLE_SCHEMA_READER: i32 = \([0-9]*\);/\1/p' crates/controller/src/db.rs | head -n 1)"

	echo "==> Writing dist/enterprise/kcore-${VERSION}-release.json..."
	cat >"dist/enterprise/kcore-${VERSION}-release.json" <<EOF
{
  "version": "${VERSION}",
  "channel": "enterprise",
  "nodeBuild": "prebuilt",
  "kcoreRev": "${kcore_rev}",
  "schemaVersion": ${schema_version:-0},
  "previousSchemaVersion": ${min_reader:-0},
  "minCompatibleReader": ${min_reader:-0},
  "prebuilt": "${PREBUILT_NAME}",
  "iso": "${ISO_NAME}",
  "kctl": [
    "${KCTL_LINUX_ARCHIVE}",
    "${KCTL_MACOS_X86_64_ARCHIVE}",
    "${KCTL_MACOS_AARCH64_ARCHIVE}"
  ],
  "docsUrl": "https://kcorehypervisor.com/docs/${VERSION}/"
}
EOF

	# Enterprise SHA256SUMS covers the prebuilt tar and release.json. Community
	# assets are published on the public draft and are not duplicated here.
	(
		cd dist/enterprise
		sha256sum "${PREBUILT_NAME}" "kcore-${VERSION}-release.json" >SHA256SUMS
	)

	# Sign the enterprise checksums with the same Sigstore identity without
	# clobbering a community SHA256SUMS / bundle already in dist/.
	local saved_sums saved_bundle
	saved_sums="$(mktemp)"
	saved_bundle="$(mktemp)"
	if [[ -f dist/SHA256SUMS ]]; then
		cp dist/SHA256SUMS "${saved_sums}"
	else
		rm -f "${saved_sums}"
		saved_sums=""
	fi
	if [[ -f "dist/${SIGSTORE_BUNDLE}" ]]; then
		cp "dist/${SIGSTORE_BUNDLE}" "${saved_bundle}"
	else
		rm -f "${saved_bundle}"
		saved_bundle=""
	fi
	cp dist/enterprise/SHA256SUMS dist/SHA256SUMS
	cmd_sign
	if [[ -f "dist/${SIGSTORE_BUNDLE}" ]]; then
		cp "dist/${SIGSTORE_BUNDLE}" "dist/enterprise/${SIGSTORE_BUNDLE}"
	fi
	if [[ -n "${saved_sums}" ]]; then
		mv "${saved_sums}" dist/SHA256SUMS
	else
		rm -f dist/SHA256SUMS
	fi
	if [[ -n "${saved_bundle}" ]]; then
		mv "${saved_bundle}" "dist/${SIGSTORE_BUNDLE}"
	else
		rm -f "dist/${SIGSTORE_BUNDLE}"
	fi

	local target_commit
	target_commit="$(git rev-parse HEAD)"
	local -a create_args=(
		api "repos/${ENTERPRISE_REPO}/releases"
		-X POST
		-f tag_name="${TAG}"
		-f target_commitish="main"
		-f name="kcore ${VERSION} (enterprise)"
		-F draft=false
		-F prerelease=false
		-f body="$(release_notes_body)

Enterprise channel. Prebuilt packages for paying subscribers. Source is public at github:kcorehypervisor/kcore@${kcore_rev}."
	)

	echo "==> Creating enterprise release ${TAG} on ${ENTERPRISE_REPO}..."
	# Create an empty commit-less tag on the enterprise repo via the Releases
	# API by targeting main; the repo holds artifacts only.
	if nix develop --command gh release view "${TAG}" --repo "${ENTERPRISE_REPO}" >/dev/null 2>&1; then
		echo "==> Enterprise release ${TAG} exists; uploading assets with --clobber..."
	else
		nix develop --command gh "${create_args[@]}" --jq .html_url
	fi

	local asset
	for asset in \
		"dist/enterprise/${PREBUILT_NAME}" \
		"dist/enterprise/kcore-${VERSION}-release.json" \
		dist/enterprise/SHA256SUMS; do
		echo "==> Uploading $(basename "${asset}") to ${ENTERPRISE_REPO}..."
		nix develop --command gh release upload "${TAG}" --repo "${ENTERPRISE_REPO}" --clobber "${asset}"
	done
	if [[ -f "dist/enterprise/${SIGSTORE_BUNDLE}" ]]; then
		nix develop --command gh release upload "${TAG}" --repo "${ENTERPRISE_REPO}" --clobber "dist/enterprise/${SIGSTORE_BUNDLE}"
	fi
	# Also attach the community ISO and kctl so subscribers can download them
	# during the embargo without waiting for the public draft to publish.
	require_public_dist
	for asset in "dist/${ISO_NAME}" "dist/${KCTL_LINUX_ARCHIVE}" "dist/${KCTL_MACOS_X86_64_ARCHIVE}" "dist/${KCTL_MACOS_AARCH64_ARCHIVE}"; do
		echo "==> Uploading $(basename "${asset}") to ${ENTERPRISE_REPO}..."
		nix develop --command gh release upload "${TAG}" --repo "${ENTERPRISE_REPO}" --clobber "${asset}"
	done
	echo "==> Enterprise release ready: nix develop --command gh release view ${TAG} --repo ${ENTERPRISE_REPO}"
	echo "    commit ${target_commit}"
}

cmd_stage_public() {
	# Upload community assets as a draft. Maintainers can see it; the public
	# cannot. The tag is not pushed yet.
	require_cmd nix
	require_cmd git
	load_dotenv
	require_public_dist
	cmd_sign

	local target_commit repo
	target_commit="$(git rev-parse "${TAG}^{commit}" 2>/dev/null)" || die "missing local tag ${TAG}; run '${0} tag' first"
	repo="$(repo_slug)"

	local -a create_args=(
		api "repos/${repo}/releases"
		-X POST
		-f tag_name="${TAG}"
		-f target_commitish="${target_commit}"
		-f name="kcore ${VERSION}"
		-F draft=true
		-F prerelease=false
		-f body="$(release_notes_body)"
	)

	local existing
	existing="$(nix develop --command gh api "repos/${repo}/releases/tags/${TAG}" --jq .id 2>/dev/null || true)"
	if [[ -n "${existing}" ]]; then
		echo "==> Public draft/release ${TAG} exists (id ${existing}); uploading assets with --clobber..."
	else
		echo "==> Creating draft GitHub release ${TAG} on ${repo}..."
		nix develop --command gh "${create_args[@]}" --jq .html_url
	fi

	local asset
	while IFS= read -r asset; do
		[[ -n "${asset}" ]] || continue
		echo "==> Uploading $(basename "${asset}")..."
		nix develop --command gh release upload "${TAG}" --repo "${repo}" --clobber "${asset}"
	done < <(public_assets)
	echo "==> Draft staged. After ${EMBARGO_DAYS} days run: make release-public"
}

cmd_publish_public() {
	require_cmd nix
	require_cmd git
	load_dotenv

	local repo target_commit draft_created created_epoch now_epoch age_days
	repo="$(repo_slug)"
	target_commit="$(git rev-parse "${TAG}^{commit}" 2>/dev/null)" || die "missing local tag ${TAG}; run '${0} tag' first"

	draft_created="$(nix develop --command gh api "repos/${repo}/releases/tags/${TAG}" --jq .created_at 2>/dev/null)" ||
		die "no GitHub release ${TAG} on ${repo}; run '${0} stage-public' first"
	created_epoch="$(date -u -d "${draft_created}" +%s 2>/dev/null || date -u -j -f '%Y-%m-%dT%H:%M:%SZ' "${draft_created}" +%s)"
	now_epoch="$(date -u +%s)"
	age_days=$(((now_epoch - created_epoch) / 86400))
	if ((age_days < EMBARGO_DAYS)) && [[ "${BREAK_EMBARGO}" -ne 1 ]]; then
		die "enterprise embargo: draft is ${age_days} day(s) old; need ${EMBARGO_DAYS} (pass --break-embargo to override)"
	fi
	if [[ "${BREAK_EMBARGO}" -eq 1 ]] && ((age_days < EMBARGO_DAYS)); then
		echo "==> --break-embargo: publishing after ${age_days} day(s) (embargo is ${EMBARGO_DAYS})"
	fi

	echo "==> Pushing tag ${TAG} to origin..."
	git push origin "${TAG}"

	local release_id
	release_id="$(nix develop --command gh api "repos/${repo}/releases/tags/${TAG}" --jq .id)"
	echo "==> Publishing draft release ${TAG} (id ${release_id})..."
	nix develop --command gh api "repos/${repo}/releases/${release_id}" -X PATCH -F draft=false --jq .html_url
	echo "==> Community release published."
}

# Compat alias used by older docs and make targets.
cmd_publish() {
	cmd_stage_public
}

cmd_release() {
	cmd_tag
	cmd_build
	cmd_dist
	cmd_prebuilt
	cmd_publish_enterprise
	cmd_stage_public
}

usage() {
	echo "Usage: ${0} [--no-sign] [--break-embargo] {build|dist|tag|prebuilt|publish-enterprise|stage-public|publish-public|sign|publish|release}"
	exit 1
}

subcommand=""
while [[ "$#" -gt 0 ]]; do
	case "${1}" in
		--no-sign) NO_SIGN=1 ;;
		--break-embargo) BREAK_EMBARGO=1 ;;
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
	prebuilt) cmd_prebuilt ;;
	publish-enterprise) cmd_publish_enterprise ;;
	stage-public) cmd_stage_public ;;
	publish-public) cmd_publish_public ;;
	sign) cmd_sign ;;
	publish) cmd_publish ;;
	release) cmd_release ;;
	*) usage ;;
esac
