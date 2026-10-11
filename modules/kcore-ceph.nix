{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.ceph;
  nonEmpty = value: value != "";
  # nixpkgs b40629ef (the revision nodes are installed with) cannot evaluate its
  # own `ceph` 19.2.3: the python3.11 environment pulls in sphinx 9.1.0, which
  # requires python >= 3.12. Take only the ceph package from a newer pinned
  # revision whose ceph (20.2.4) evaluates and is in cache.nixos.org, so nodes
  # download it instead of compiling. Drop this once the base pin is bumped to a
  # revision where `nix eval nixpkgs#ceph.drvPath` succeeds without an overlay.
  cephNixpkgs = import (builtins.fetchTarball {
    url = "https://github.com/NixOS/nixpkgs/archive/e7439b6b14ad3cc35d05608ebca9bce01a25f5f8.tar.gz";
    sha256 = "19y4py973w62z4qb7gzn0chjs67ihxy8rbjb7g71iyk8yz7z5k8z";
  }) { system = "x86_64-linux"; };
  clusterPrefixLength =
    let
      parts = lib.splitString "/" cfg.clusterNetwork;
    in
    if builtins.length parts == 2 then lib.toInt (builtins.elemAt parts 1) else 24;
  # Fallback when NodeNetwork is not managing interfaces: open Ceph ports and
  # optionally pin a static address on the cluster iface.
  cephPublicPorts = [
    3300
    6789
  ];
  cephDaemonRange = {
    from = 6800;
    to = 7568;
  };
in
{
  options.kcore.ceph = {
    enable = lib.mkEnableOption "kcore SAN Ceph services";
    clusterName = lib.mkOption {
      type = lib.types.str;
      default = "ceph";
    };
    fsid = lib.mkOption {
      type = lib.types.str;
      default = "";
    };
    publicNetwork = lib.mkOption {
      type = lib.types.str;
      default = "";
    };
    clusterNetwork = lib.mkOption {
      type = lib.types.str;
      default = "";
    };
    monAddress = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Comma-separated public mon IPs (mon host).";
    };
    monInitialMembers = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = ''
        This node's mon daemon id for ceph.conf mon_initial_members.
        Use only the local daemon — listing peers that are not yet in the
        monmap makes Ceph invent 0.0.0.0 placeholders and keeps the mon probing.
        Later members join via `ceph mon add`.
      '';
    };
    publicAddr = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "This node's public/client address. Written into ceph.conf so the mon binds the monmap IP.";
    };
    clusterAddr = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "This node's cluster/replication address.";
    };
    publicIface = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Public/client NIC name. Used for firewall rules when set.";
    };
    clusterIface = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Cluster/replication NIC name. Trusted for OSD traffic; may receive a static clusterAddr.";
    };
    daemonId = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
    };
    enableMon = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
    enableMgr = lib.mkOption {
      type = lib.types.bool;
      default = true;
    };
    enableOsd = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install Ceph tools and ship ceph-osd@ + ceph-volume-activate units for --no-systemd OSD prepare.";
    };
    enableMds = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run a CephFS metadata server (MDS) on this node.";
    };
    enableRgw = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Run a Ceph RADOS Gateway (RGW) instance on this node.";
    };
    rgwPort = lib.mkOption {
      type = lib.types.port;
      default = 7480;
      description = "Client port when enableRgw is true.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = cephNixpkgs.ceph;
      defaultText = lib.literalMD "`ceph` from a pinned newer nixpkgs revision";
      description = "Ceph package used for the daemons, CLI, and Ceph-backed VM disks (`pkgs.ceph` is overridden to this).";
    };
    poolSize = lib.mkOption {
      type = lib.types.ints.positive;
      default = 3;
    };
    poolMinSize = lib.mkOption {
      type = lib.types.ints.positive;
      default = 2;
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.fsid != "";
        message = "kcore.ceph.fsid is required";
      }
      {
        assertion = cfg.publicNetwork != "";
        message = "kcore.ceph.publicNetwork is required";
      }
      {
        assertion = cfg.clusterNetwork != "";
        message = "kcore.ceph.clusterNetwork is required";
      }
      {
        assertion = cfg.poolSize >= cfg.poolMinSize;
        message = "Ceph poolSize must be >= poolMinSize";
      }
      {
        assertion = cfg.enableMon -> cfg.daemonId != "";
        message = "kcore.ceph.daemonId is required when the mon is enabled";
      }
    ];
    nixpkgs.overlays = [ (_final: _prev: { ceph = cfg.package; }) ];
    # rbd-nbd maps images (kernel krbd cannot use Ceph 20 aes256k keys).
    boot.kernelModules = [
      "rbd"
      "ceph"
      "nbd"
    ];
    boot.extraModprobeConfig = "options nbd nbds_max=128 max_part=0";
    environment.systemPackages = [ cfg.package ];

    # Minimal networking fallback so Ceph works before / without NodeNetwork.
    # Public: mon/msgr2 + daemon range. Cluster iface: daemon range + trust.
    networking.firewall = {
      allowedTCPPorts = cephPublicPorts ++ lib.optionals cfg.enableRgw [ cfg.rgwPort ];
      allowedTCPPortRanges = [ cephDaemonRange ];
      interfaces = lib.mkMerge [
        (lib.mkIf (cfg.publicIface != "") {
          ${cfg.publicIface} = {
            allowedTCPPorts = cephPublicPorts ++ lib.optionals cfg.enableRgw [ cfg.rgwPort ];
            allowedTCPPortRanges = [ cephDaemonRange ];
          };
        })
        (lib.mkIf (cfg.clusterIface != "") {
          ${cfg.clusterIface} = {
            allowedTCPPortRanges = [ cephDaemonRange ];
          };
        })
      ];
      trustedInterfaces = lib.mkIf (cfg.clusterIface != "") [ cfg.clusterIface ];
    };

    networking.interfaces = lib.mkIf (cfg.clusterIface != "" && cfg.clusterAddr != "") {
      ${cfg.clusterIface}.ipv4.addresses = [
        {
          address = cfg.clusterAddr;
          prefixLength = clusterPrefixLength;
        }
      ];
    };

    # Option names match nixos/modules/services/network-filesystems/ceph.nix
    # (camelCase). The previous mapping used public_network / mon_host / cluster,
    # which are not options, so `nixos-rebuild` rejected the whole Ceph config
    # and a new cluster never formed a quorum.
    services.ceph = {
      enable = true;
      global = {
        inherit (cfg) fsid;
        inherit (cfg) clusterName;
        inherit (cfg) publicNetwork;
        inherit (cfg) clusterNetwork;
        monHost = cfg.monAddress;
        monInitialMembers = if cfg.monInitialMembers == "" then null else cfg.monInitialMembers;
      };
      extraConfig = lib.filterAttrs (_: nonEmpty) {
        "public addr" = cfg.publicAddr;
        "cluster addr" = cfg.clusterAddr;
        "osd pool default size" = toString cfg.poolSize;
        "osd pool default min size" = toString cfg.poolMinSize;
      };
      mon = {
        enable = cfg.enableMon;
        daemons = lib.optionals cfg.enableMon [ cfg.daemonId ];
        extraConfig = lib.filterAttrs (_: nonEmpty) {
          "public addr" = cfg.publicAddr;
        };
      };
      mgr = {
        enable = cfg.enableMgr;
        daemons = lib.optionals cfg.enableMgr [ cfg.daemonId ];
      };
      osd = {
        # NixOS refuses `osd.enable` with an empty daemon list, and its
        # hostname-scoped units do not match ceph-volume's ceph-osd@ID
        # instances. Leave this off; we ship a template + activate unit below.
        enable = false;
        daemons = [ ];
      };
      mds = {
        enable = cfg.enableMds;
        daemons = lib.optionals cfg.enableMds [ cfg.daemonId ];
      };
      rgw = {
        enable = cfg.enableRgw;
        daemons = lib.optionals cfg.enableRgw [ cfg.daemonId ];
      };
    };

    # ceph-volume cannot `systemctl enable` into NixOS's read-only
    # /etc/systemd/system. Prepare with --no-systemd and run OSDs via this
    # template; the activate oneshot brings LVs up at boot and starts instances.
    systemd.services."ceph-osd@" = lib.mkIf cfg.enableOsd {
      description = "Ceph object storage daemon %i";
      after = [
        "network-online.target"
        "time-sync.target"
        "local-fs.target"
        "ceph-volume-activate.service"
      ];
      wants = [
        "network-online.target"
        "time-sync.target"
        "ceph-volume-activate.service"
      ];
      unitConfig.ConditionPathExists = "/var/lib/ceph/osd/ceph-%i/fsid";
      startLimitBurst = 30;
      startLimitIntervalSec = 1800;
      serviceConfig = {
        Type = "simple";
        User = "ceph";
        Group = "disk";
        LimitNOFILE = 1048576;
        LimitNPROC = 1048576;
        Environment = "CLUSTER=${cfg.clusterName}";
        ExecStartPre = "${cfg.package.lib}/libexec/ceph/ceph-osd-prestart.sh --id %i --cluster ${cfg.clusterName}";
        ExecStart = "${cfg.package}/bin/ceph-osd -f --cluster ${cfg.clusterName} --id %i";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        RestartSec = "10s";
        PrivateDevices = false;
        PrivateTmp = true;
        ProtectHome = true;
        ProtectSystem = "full";
      };
    };

    systemd.services.ceph-volume-activate = lib.mkIf cfg.enableOsd {
      description = "Activate Ceph OSDs prepared by ceph-volume (no systemd integration)";
      wantedBy = [ "multi-user.target" ];
      after = [
        "local-fs.target"
        "network-online.target"
      ];
      wants = [ "network-online.target" ];
      # Boot-time only: re-running it on `switch` would re-prime live OSDs.
      restartIfChanged = false;
      path = [
        pkgs.util-linux
        pkgs.lvm2
        pkgs.coreutils
        pkgs.systemd
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "ceph-volume-activate-all" ''
          set -euo pipefail
          ${cfg.package}/bin/ceph-volume lvm activate --all --no-systemd
          shopt -s nullglob
          for dir in /var/lib/ceph/osd/ceph-*; do
            id="''${dir##*/ceph-}"
            if [[ -f "$dir/fsid" ]]; then
              systemctl start --no-block "ceph-osd@''${id}.service" || true
            fi
          done
        '';
      };
    };
  };
}
