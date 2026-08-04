#!/usr/bin/env bash
# Copy freshly built kcore packages to a NixOS host via nix copy and
# activate them without a full OS reinstall.
#
# Deploys: kcore-dashboard, kcore-controller, kcore-node-agent
#
# Activation strategy (in order):
#   1. If units ExecStart /opt/kcore/bin/*, replace those binaries (kcoreOS
#      ships this way; /etc/systemd/system is often a read-only nix symlink).
#   2. Else write systemd drop-ins under /etc/systemd/system when writable.
#
# Usage:
#   ./scripts/deploy-kcore-to-host.sh root@192.168.40.105
#   FLAKE=/path/to/kcore ./scripts/deploy-kcore-to-host.sh user@host
#
# Requires: passwordless sudo on the remote for systemctl, or run as root@host.
#
# Non-interactive password auth: install `sshpass`, then e.g.
#   SSHPASS=kcore ./scripts/deploy-kcore-to-host.sh root@192.168.40.105
# Also sets NIX_SSHOPTS so ssh does not try every local key first (avoids "Too many authentication failures").
set -euo pipefail

TARGET="${1:?usage: $0 [user@]host}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FLAKE="${FLAKE:-$ROOT}"

run_ssh() {
  if [[ -n "${SSHPASS:-}" ]] && command -v sshpass >/dev/null 2>&1; then
    sshpass -e ssh "$@"
  else
    ssh "$@"
  fi
}

echo "==> Building kcore-dashboard, kcore-controller, kcore-node-agent from $FLAKE"
DASH="$(nix build "$FLAKE#kcore-dashboard" --no-link --print-out-paths)"
CTRL="$(nix build "$FLAKE#kcore-controller" --no-link --print-out-paths)"
AGENT="$(nix build "$FLAKE#kcore-node-agent" --no-link --print-out-paths)"
echo "    dashboard:  $DASH"
echo "    controller: $CTRL"
echo "    node-agent: $AGENT"

if [[ -n "${SSHPASS:-}" ]]; then
  export NIX_SSHOPTS="${NIX_SSHOPTS:+$NIX_SSHOPTS }-o StrictHostKeyChecking=accept-new -o PreferredAuthentications=password -o PubkeyAuthentication=no"
fi

echo "==> nix copy (closures) -> ssh://$TARGET"
if [[ -n "${SSHPASS:-}" ]] && command -v sshpass >/dev/null 2>&1; then
  sshpass -e nix copy --to "ssh://$TARGET" "$DASH" "$CTRL" "$AGENT"
else
  nix copy --to "ssh://$TARGET" "$DASH" "$CTRL" "$AGENT"
fi

echo "==> Remote: activate binaries + restart"
run_ssh "$TARGET" bash -s -- "$DASH" "$CTRL" "$AGENT" <<'REMOTE'
set -euo pipefail
DASH="$1"
CTRL="$2"
AGENT="$3"
SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
  SUDO="sudo"
fi

install_opt_bin() {
  local name="$1" src="$2"
  local dest="/opt/kcore/bin/$name"
  $SUDO mkdir -p /opt/kcore/bin
  $SUDO rm -f "$dest"
  $SUDO ln -s "$src" "$dest"
  echo "    $dest -> $src"
}

unit_uses_opt_bin() {
  local name="$1"
  systemctl cat "$name" 2>/dev/null | grep -q 'ExecStart=/opt/kcore/bin/'
}

etc_systemd_writable() {
  # NixOS often has /etc/systemd/system -> /etc/static/... (not mkdir-able).
  [[ -d /etc/systemd/system && ! -L /etc/systemd/system ]]
}

write_dropin() {
  local name="$1" exe="$2"
  $SUDO mkdir -p "/etc/systemd/system/${name}.service.d"
  $SUDO tee "/etc/systemd/system/${name}.service.d/z-nix-store-override.conf" >/dev/null <<EOF
[Service]
ExecStart=
ExecStart=$exe
EOF
  echo "    override $name -> $exe"
}

# Prefer /opt/kcore/bin when that is what the units already run — works on
# installed kcoreOS without touching read-only systemd unit trees.
if unit_uses_opt_bin kcore-dashboard || unit_uses_opt_bin kcore-controller || unit_uses_opt_bin kcore-node-agent; then
  echo "    using /opt/kcore/bin symlinks"
  install_opt_bin kcore-dashboard "$DASH/bin/kcore-dashboard"
  install_opt_bin kcore-controller "$CTRL/bin/kcore-controller"
  install_opt_bin kcore-node-agent "$AGENT/bin/kcore-node-agent"
elif etc_systemd_writable; then
  echo "    using systemd drop-ins"
  if systemctl cat kcore-dashboard &>/dev/null; then
    write_dropin kcore-dashboard "$DASH/bin/kcore-dashboard"
  fi
  if systemctl cat kcore-controller &>/dev/null; then
    write_dropin kcore-controller "$CTRL/bin/kcore-controller --config /etc/kcore/controller.yaml"
  fi
  if systemctl cat kcore-node-agent &>/dev/null; then
    write_dropin kcore-node-agent "$AGENT/bin/kcore-node-agent --config /etc/kcore/node-agent.yaml"
  fi
  $SUDO systemctl daemon-reload
else
  echo "error: units do not use /opt/kcore/bin and /etc/systemd/system is not writable" >&2
  echo "hint: copy binaries manually or reconfigure the host via NixOS" >&2
  exit 1
fi

for u in kcore-dashboard kcore-controller kcore-node-agent; do
  if systemctl cat "$u" &>/dev/null; then
    $SUDO systemctl restart "$u"
    echo "    restarted $u"
  fi
done

echo "==> status (first lines)"
for u in kcore-dashboard kcore-controller kcore-node-agent; do
  if systemctl cat "$u" &>/dev/null; then
    $SUDO systemctl --no-pager -l status "$u" | head -12 || true
    echo ""
  fi
done

echo "==> binary versions (mtime / path)"
ls -l /opt/kcore/bin/kcore-dashboard /opt/kcore/bin/kcore-controller /opt/kcore/bin/kcore-node-agent 2>/dev/null || true
REMOTE

HOST_ONLY="${TARGET#*@}"
echo "Done. Open http://${HOST_ONLY}:8080/vms (hard-refresh). Serial console needs a running VM."
