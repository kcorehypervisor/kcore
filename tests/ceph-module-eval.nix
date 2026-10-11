# Eval-only guard: kcore.ceph.enable must evaluate (incl. the pinned ceph
# overlay and firewall ports) without requiring a full system build.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  eval = nixosSystem {
    inherit (pkgs) system;
    modules = [
      ../modules/kcore-ceph.nix
      (
        { ... }:
        {
          networking.hostName = "ceph-eval";
          system.stateVersion = "25.11";
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "nodev";
            fsType = "tmpfs";
          };
          kcore.ceph = {
            enable = true;
            fsid = "00000000-0000-0000-0000-000000000001";
            publicNetwork = "192.168.40.0/24";
            clusterNetwork = "192.168.47.0/24";
            monAddress = "192.168.40.103";
            monInitialMembers = "ceph-eval";
            publicAddr = "192.168.40.103";
            clusterAddr = "192.168.47.2";
            publicIface = "enp6s0";
            clusterIface = "enp6s0d1";
            daemonId = "ceph-eval";
          };
        }
      )
    ];
  };
  cfg = eval.config;
  ports = cfg.networking.firewall.allowedTCPPorts;
  ranges = cfg.networking.firewall.allowedTCPPortRanges;
  hasPort = p: lib.elem p ports;
  hasRange = from: to: lib.any (r: r.from == from && r.to == to) ranges;
in
assert cfg.kcore.ceph.enable;
assert hasPort 3300;
assert hasPort 6789;
assert hasRange 6800 7568;
assert lib.elem "enp6s0d1" cfg.networking.firewall.trustedInterfaces;
assert (builtins.head cfg.networking.interfaces.enp6s0d1.ipv4.addresses).address == "192.168.47.2";
# Touch the overlaid package so evaluation fails if the pinned overlay breaks.
assert cfg.kcore.ceph.package.pname or cfg.kcore.ceph.package.name != "";
# OSD path uses --no-systemd + these units (not ceph-volume's systemctl enable).
assert cfg.systemd.services ? "ceph-osd@";
assert cfg.systemd.services ? "ceph-volume-activate";
assert cfg.systemd.services."ceph-osd@".serviceConfig.ExecStart != null;
assert cfg.systemd.services.ceph-volume-activate.serviceConfig.ExecStart != null;
pkgs.runCommand "ceph-module-eval-ok" { } ''
  echo ok > "$out"
''
