#!/usr/bin/env bash
# Install kctl from GitHub Releases (latest or pinned version).
#
#   curl -fsSL https://raw.githubusercontent.com/kcorehypervisor/kcore/main/scripts/get-kctl.sh | bash
#
# Optional environment:
#   KCORE_GITHUB_REPO   owner/repo (default: kcorehypervisor/kcore)
#   KCTL_VERSION        Exact semver without leading v (e.g. 0.2.0); default: latest release
#   INSTALL_DIR         Directory for the kctl binary (default: /usr/local/bin if writable, else ~/.local/bin)
#   GITHUB_TOKEN / GH_TOKEN  Optional bearer token for higher GitHub API rate limits
#
# This script only supports platforms we publish: Linux x86_64, macOS x86_64, macOS aarch64 (Apple Silicon).
set -euo pipefail

DEFAULT_REPO="kcorehypervisor/kcore"
REPO="${KCORE_GITHUB_REPO:-${DEFAULT_REPO}}"

GET_KCTL_TMPDIR=""

cleanup_get_kctl_tmp() {
	if [[ -n "${GET_KCTL_TMPDIR:-}" ]]; then
		rm -rf "${GET_KCTL_TMPDIR}"
	fi
}
trap cleanup_get_kctl_tmp EXIT

die() {
	echo "get-kctl.sh: $*" >&2
	exit 1
}

have_cmd() {
	command -v "$1" >/dev/null 2>&1
}

verify_tarball_sha256() {
	local sums_file="$1" tarball="$2"
	local expected actual
	expected="$(awk '{print $1}' "${sums_file}")"
	[[ "${#expected}" -eq 64 ]] || die "invalid SHA256SUMS line for $(basename "${tarball}")"
	if have_cmd openssl; then
		actual="$(openssl dgst -sha256 "${tarball}" | awk '{print $NF}')"
	elif have_cmd sha256sum; then
		actual="$(sha256sum "${tarball}" | awk '{print $1}')"
	elif have_cmd shasum; then
		actual="$(shasum -a 256 "${tarball}" | awk '{print $1}')"
	else
		die "need openssl, sha256sum, or shasum to verify checksums"
	fi
	[[ "${expected}" == "${actual}" ]] || die "checksum mismatch for $(basename "${tarball}")"
}

curl_github_api() {
	local url="$1"
	local -a args=(-fsSL -H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28")
	local tok="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
	if [[ -n "${tok}" ]]; then
		args+=(-H "Authorization: Bearer ${tok}")
	fi
	curl "${args[@]}" "${url}"
}

detect_archive_suffix() {
	local os_raw arch_raw
	os_raw="$(uname -s)"
	arch_raw="$(uname -m)"

	case "${os_raw}" in
	Linux)
		case "${arch_raw}" in
		x86_64) echo "linux-x86_64" ;;
		aarch64 | arm64)
			die "Linux arm64/aarch64 kctl is not published yet; build from source or use Linux x86_64."
			;;
		*) die "unsupported Linux architecture: ${arch_raw}" ;;
		esac
		;;
	Darwin)
		case "${arch_raw}" in
		x86_64) echo "macos-x86_64" ;;
		aarch64 | arm64) echo "macos-aarch64" ;;
		*) die "unsupported macOS architecture: ${arch_raw}" ;;
		esac
		;;
	*) die "unsupported OS: ${os_raw} (need Linux or Darwin)" ;;
	esac
}

pick_release_tarball_name() {
	local tag="$1" version="$2" suffix="$3"
	local base="https://github.com/${REPO}/releases/download/${tag}"
	local cand code
	for cand in "kctl-${version}-${suffix}.tar.gz" "kcore-kctl-${version}-${suffix}.tar.gz"; do
		code="$(curl -sSIL -o /dev/null -w "%{http_code}" "${base}/${cand}")"
		if [[ "${code}" == "200" ]]; then
			echo "${cand}"
			return 0
		fi
	done
	die "no kctl tarball for ${tag} (${suffix}); looked for kctl-… and kcore-kctl-… on GitHub Releases"
}

resolve_release_json() {
	local url
	if [[ -n "${KCTL_VERSION:-}" ]]; then
		local ver="${KCTL_VERSION#v}"
		[[ "${ver}" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]] || die "KCTL_VERSION must look like 0.2.0 (got '${KCTL_VERSION}')"
		url="https://api.github.com/repos/${REPO}/releases/tags/v${ver}"
	else
		url="https://api.github.com/repos/${REPO}/releases/latest"
	fi
	curl_github_api "${url}"
}

parse_tag_name() {
	local json="$1"
	if have_cmd python3; then
		python3 -c 'import json,sys; print(json.load(sys.stdin)["tag_name"])' <<<"${json}"
		return
	fi
	# Fallback: first "tag_name" line (works for GitHub's compact JSON)
	local line
	line="$(grep -m1 '"tag_name"' <<<"${json}")" || die "could not find tag_name in GitHub API response"
	line="${line#*: \"}"
	line="${line%%\"*}"
	[[ -n "${line}" ]] || die "could not parse tag_name"
	echo "${line}"
}

default_install_dir() {
	if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
		echo "/usr/local/bin"
		return
	fi
	if [[ -d /usr/local/bin && -w /usr/local/bin ]]; then
		echo "/usr/local/bin"
	else
		echo "${HOME}/.local/bin"
	fi
}

main() {
	local suffix archive tag version sums_url tarball_url tmpdir json sums_file need_sudo=0

	suffix="$(detect_archive_suffix)"

	json="$(resolve_release_json)" || die "failed to fetch release metadata from GitHub"

	tag="$(parse_tag_name "${json}")"
	[[ "${tag}" =~ ^v ]] || die "unexpected tag_name from API: ${tag}"
	version="${tag#v}"

	archive="$(pick_release_tarball_name "${tag}" "${version}" "${suffix}")"
	tarball_url="https://github.com/${REPO}/releases/download/${tag}/${archive}"
	sums_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS"

	local install_dir="${INSTALL_DIR:-$(default_install_dir)}"
	if [[ ! -d "${install_dir}" ]]; then
		echo "==> Creating ${install_dir}"
		mkdir -p "${install_dir}"
	fi
	if [[ ! -w "${install_dir}" ]]; then
		if have_cmd sudo && [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
			need_sudo=1
		else
			die "cannot write to INSTALL_DIR=${install_dir}"
		fi
	fi

	GET_KCTL_TMPDIR="$(mktemp -d)"
	tmpdir="${GET_KCTL_TMPDIR}"

	echo "==> Downloading ${archive}"
	curl -fsSL -o "${tmpdir}/${archive}" "${tarball_url}"

	echo "==> Downloading SHA256SUMS"
	curl -fsSL -o "${tmpdir}/SHA256SUMS" "${sums_url}"

	sums_file="${tmpdir}/sums.chk"
	grep -F "${archive}" "${tmpdir}/SHA256SUMS" >"${sums_file}" || die "SHA256SUMS has no entry for ${archive}"
	verify_tarball_sha256 "${sums_file}" "${tmpdir}/${archive}"

	tar -xzf "${tmpdir}/${archive}" -C "${tmpdir}"
	local extracted=""
	if [[ -f "${tmpdir}/kctl" ]]; then
		extracted="${tmpdir}/kctl"
	elif [[ -f "${tmpdir}/kcore-kctl" ]]; then
		extracted="${tmpdir}/kcore-kctl"
	else
		die "archive did not contain kctl or kcore-kctl at top level"
	fi

	if [[ "${need_sudo}" -eq 1 ]]; then
		echo "==> Installing kctl to ${install_dir} (sudo)"
		sudo install -m 0755 "${extracted}" "${install_dir}/kctl"
	else
		echo "==> Installing kctl to ${install_dir}"
		install -m 0755 "${extracted}" "${install_dir}/kctl"
	fi

	echo "==> Done: $("${install_dir}/kctl" --version 2>/dev/null || true)"
	case ":${PATH}:" in
	*":${install_dir}:"*) ;;
	*)
		echo "Note: ${install_dir} may not be on your PATH; add it or open a new shell."
		;;
	esac
}

main "$@"
