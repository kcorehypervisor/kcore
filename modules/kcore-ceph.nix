{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.ceph;
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
    ];
    boot.kernelModules = [
      "rbd"
      "ceph"
    ];
    environment.systemPackages = [ pkgs.ceph ];
    services.ceph = {
      global = {
        inherit (cfg) fsid;
        cluster = cfg.clusterName;
        public_network = cfg.publicNetwork;
        cluster_network = cfg.clusterNetwork;
        mon_host = cfg.monAddress;
        osd_pool_default_size = cfg.poolSize;
        osd_pool_default_min_size = cfg.poolMinSize;
      };
      mon = {
        enable = cfg.enableMon;
        daemons = lib.optionals cfg.enableMon [ cfg.daemonId ];
      };
      mgr = {
        enable = cfg.enableMgr;
        daemons = lib.optionals cfg.enableMgr [ cfg.daemonId ];
      };
      osd = {
        enable = cfg.enableOsd;
        daemons = [ ];
      };
    };
  };
}
