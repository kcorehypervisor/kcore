#!/usr/bin/env bash
# Manage read-only collaborators on the private enterprise release repo.
# The collaborator list is the access list for paying subscribers.
#
# Usage:
#   ./scripts/enterprise-access.sh add <github-user>
#   ./scripts/enterprise-access.sh remove <github-user>
#   ./scripts/enterprise-access.sh list
#
# Requires: gh authenticated for the kcorehypervisor org. Non-interactive.
set -euo pipefail

ROOT="$(cd "$(dirname "${0}")/.." && pwd)"
cd "${ROOT}"

REPO="${KCORE_ENTERPRISE_REPO:-kcorehypervisor/enterprise}"

die() {
	echo "enterprise-access.sh: $*" >&2
	exit 1
}

load_dotenv() {
	if [[ -f .env ]]; then
		set -a
		# shellcheck disable=SC1091
		source .env
		set +a
	fi
}

require_gh() {
	command -v nix >/dev/null 2>&1 || die "nix is required (run under nix develop)"
	load_dotenv
	if ! nix develop --command gh auth status >/dev/null 2>&1; then
		die "gh is not authenticated; set GH_TOKEN in .env or run: nix develop --command gh auth login"
	fi
}

cmd_add() {
	local user="${1:-}"
	[[ -n "${user}" ]] || die "usage: ${0} add <github-user>"
	require_gh
	echo "==> Inviting ${user} as read-only collaborator on ${REPO}..."
	nix develop --command gh api \
		-X PUT \
		"repos/${REPO}/collaborators/${user}" \
		-f permission=pull \
		--silent
	echo "==> ${user} can pull ${REPO} (pending their acceptance if new)."
}

cmd_remove() {
	local user="${1:-}"
	[[ -n "${user}" ]] || die "usage: ${0} remove <github-user>"
	require_gh
	echo "==> Removing ${user} from ${REPO}..."
	nix develop --command gh api \
		-X DELETE \
		"repos/${REPO}/collaborators/${user}" \
		--silent
	echo "==> ${user} no longer has access to ${REPO}."
}

cmd_list() {
	require_gh
	echo "==> Collaborators on ${REPO}:"
	nix develop --command gh api \
		"repos/${REPO}/collaborators" \
		--jq '.[] | "\(.login)\t\(.role_name // .permissions.pull | tostring)"'
}

usage() {
	echo "Usage: ${0} {add <user>|remove <user>|list}"
	exit 1
}

case "${1:-}" in
	add) shift; cmd_add "$@" ;;
	remove) shift; cmd_remove "$@" ;;
	list) cmd_list ;;
	-h | --help | "") usage ;;
	*) die "unknown command: ${1}" ;;
esac
