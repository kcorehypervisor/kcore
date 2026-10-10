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
      description = "Comma-separated mon daemon ids that form the initial quorum.";
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
      description = "Install Ceph so ceph-volume can create OSD units. The NixOS osd daemon list stays empty: those units are not hostname-scoped.";
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
    boot.kernelModules = [
      "rbd"
      "ceph"
    ];
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
        fsid = cfg.fsid;
        clusterName = cfg.clusterName;
        publicNetwork = cfg.publicNetwork;
        clusterNetwork = cfg.clusterNetwork;
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
        # NixOS refuses `osd.enable` with an empty daemon list, and its OSD
        # units are keyed by OSD id, which does not exist until ceph-volume
        # creates them. Leave the module OSD list off; ceph-volume installs
        # `ceph-osd@<id>` itself.
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
  };
}
