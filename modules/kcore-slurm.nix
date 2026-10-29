# Slurm host services on kcoreOS.
#
# One node belongs to at most one SlurmCluster. The controller writes this
# fragment per node; only `roles` and the node's own address differ.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.slurm;
  inherit (lib)
    concatMapStringsSep
    elem
    listToAttrs
    mkEnableOption
    mkIf
    mkOption
    optional
    types
    ;

  inherit (cfg) memberAddresses;
  hasController = elem "controller" cfg.roles;
  hasDbd = elem "dbd" cfg.roles;
  hasCompute = elem "compute" cfg.roles;
  hasLogin = elem "login" cfg.roles;
  usesNfs = cfg.storage.mode == "nfs";
  usesCephFs = cfg.storage.mode == "shared-filesystem";

  nftRules = concatMapStringsSep "\n" (
    source:
    ''iifname "${cfg.iface}" ip saddr ${source} tcp dport { 6817, 6818, 6819, 60001-63000 } accept comment "kcore-slurm"''
  ) memberAddresses;

  iptablesRules = concatMapStringsSep "\n" (source: ''
    iptables -A nixos-fw -i ${lib.escapeShellArg cfg.iface} -s ${lib.escapeShellArg source} -p tcp -m multiport --dports 6817,6818,6819 -j nixos-fw-accept
    iptables -A nixos-fw -i ${lib.escapeShellArg cfg.iface} -s ${lib.escapeShellArg source} -p tcp --dport 60001:63000 -j nixos-fw-accept
  '') memberAddresses;

  nfsExports = concatMapStringsSep "\n" (
    path:
    let
      targets = concatMapStringsSep " " (
        address: "${address}(rw,sync,no_subtree_check,no_root_squash)"
      ) memberAddresses;
    in
    "${path} ${targets}"
  ) cfg.storage.exports;

  nfsMounts = listToAttrs (
    map (path: {
      name = path;
      value = {
        device = "${cfg.controlAddr}:${path}";
        fsType = "nfs";
        options = [
          "_netdev"
          "x-systemd.automount"
          "noatime"
        ];
      };
    }) cfg.storage.exports
  );

  cephMounts = listToAttrs (
    map (path: {
      name = path;
      value = {
        device = "${cfg.storage.filesystem}:/";
        fsType = "ceph";
        options = [
          "name=admin"
          "secretfile=/etc/ceph/ceph.client.admin.keyring"
          "_netdev"
          "noatime"
        ];
      };
    }) cfg.storage.exports
  );

  gresFile = pkgs.writeTextDir "gres.conf" cfg.gresConf;

  userIds = map (user: user.uid) cfg.users;
in
{
  options.kcore.slurm = {
    enable = mkEnableOption "kcore Slurm host services";

    nodeId = mkOption {
      type = types.str;
      default = "";
      description = "This node's kcore id.";
    };

    nodeAddress = mkOption {
      type = types.str;
      default = "";
      description = "This node's management address.";
    };

    clusterName = mkOption {
      type = types.str;
      default = "slurm";
      description = "Slurm cluster name.";
    };

    roles = mkOption {
      type = types.listOf (
        types.enum [
          "controller"
          "dbd"
          "login"
          "compute"
        ]
      );
      default = [ ];
      description = "Slurm roles on this node.";
    };

    controlMachine = mkOption {
      type = types.str;
      default = "";
      description = "Primary slurmctld node id.";
    };

    controlAddr = mkOption {
      type = types.str;
      default = "";
      description = "Primary slurmctld management address.";
    };

    backupControlMachine = mkOption {
      type = types.str;
      default = "";
      description = "Backup slurmctld node id; empty means none.";
    };

    backupControlAddr = mkOption {
      type = types.str;
      default = "";
      description = "Backup slurmctld management address.";
    };

    iface = mkOption {
      type = types.str;
      default = "";
      description = "Management interface that carries Slurm traffic.";
    };

    memberAddresses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Management addresses allowed to reach Slurm ports.";
    };

    memberHosts = mkOption {
      type = types.listOf (
        types.submodule {
          options = {
            name = mkOption { type = types.str; };
            address = mkOption { type = types.str; };
          };
        }
      );
      default = [ ];
      description = "Static /etc/hosts entries for every member.";
    };

    slurmConf = mkOption {
      type = types.lines;
      default = "";
      description = "Shared slurm.conf options (identical on every member).";
    };

    cgroupConf = mkOption {
      type = types.lines;
      default = "";
      description = "Shared cgroup.conf.";
    };

    gresConf = mkOption {
      type = types.lines;
      default = "";
      description = "Shared gres.conf; empty when gres is none.";
    };

    users = mkOption {
      type = types.listOf (
        types.submodule {
          options = {
            name = mkOption { type = types.str; };
            uid = mkOption { type = types.int; };
            sshKeys = mkOption {
              type = types.listOf types.str;
              default = [ ];
            };
          };
        }
      );
      default = [ ];
      description = "Cluster users with fixed uid/gid and SSH keys.";
    };

    storage = {
      mode = mkOption {
        type = types.enum [
          "nfs"
          "shared-filesystem"
          "none"
        ];
        default = "nfs";
        description = "Shared storage mode.";
      };
      exports = mkOption {
        type = types.listOf types.str;
        default = [
          "/home"
          "/scratch"
        ];
        description = "Paths exported by the controller or mounted from CephFS.";
      };
      filesystem = mkOption {
        type = types.str;
        default = "";
        description = "SharedFilesystem name when mode is shared-filesystem.";
      };
    };

    gpu = {
      driver = mkOption {
        type = types.bool;
        default = false;
        description = "Install the host NVIDIA driver on this compute member.";
      };
      pciIds = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "GPU PCI ids that must stay out of VFIO binding.";
      };
    };

    accounting = {
      enabled = mkOption {
        type = types.bool;
        default = false;
        description = "Run MariaDB and slurmdbd for accounting.";
      };
      nodeId = mkOption {
        type = types.str;
        default = "";
        description = "Accounting node id; empty means the controller node.";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.roles != [ ];
        message = "kcore.slurm.roles must not be empty when Slurm is enabled";
      }
      {
        assertion = !hasController || cfg.controlAddr != "";
        message = "kcore.slurm.controlAddr is required for the controller role";
      }
      {
        assertion = !hasCompute || lib.hasInfix "PartitionName=" cfg.slurmConf;
        message = "a compute role requires at least one rendered partition";
      }
      {
        assertion = lib.length (lib.unique userIds) == lib.length userIds;
        message = "kcore.slurm.users uids must be unique";
      }
      {
        assertion = !(usesNfs && cfg.storage.exports != [ ]) || hasController;
        message = "the NFS server runs only on the controller role";
      }
      {
        assertion = !usesCephFs || cfg.storage.filesystem != "";
        message = "kcore.slurm.storage.filesystem is required for shared-filesystem mode";
      }
      {
        assertion = !cfg.accounting.enabled || hasDbd;
        message = "accounting requires the dbd role";
      }
    ];

    services.munge = {
      enable = true;
      password = "/etc/munge/munge.key";
    };

    services.slurm = {
      enableStools = hasLogin || hasController || hasDbd;
      inherit (cfg) clusterName controlMachine controlAddr;
      server.enable = hasController;
      client.enable = hasCompute;
      dbdserver.enable = hasDbd;
      dbdserver.dbdHost = cfg.nodeAddress;
      dbdserver.storageUser = "slurm";
      dbdserver.storagePassFile = if hasDbd then "/etc/slurm/dbd-password" else null;
      extraConfig = cfg.slurmConf;
      extraCgroupConfig = cfg.cgroupConf;
      procTrackType = "proctrack/cgroup";
      stateSaveLocation =
        if cfg.storage.mode == "none" then "/var/spool/slurmctld" else "/scratch/slurm/state";
      extraConfigPaths = optional (cfg.gresConf != "") gresFile;
    };

    services.mysql = mkIf hasDbd {
      enable = true;
      package = pkgs.mariadb;
      ensureDatabases = [ "slurm_acct_db" ];
      initialScript = "/etc/slurm/dbd-init.sql";
    };

    users.users = listToAttrs (
      map (user: {
        inherit (user) name;
        value = {
          isNormalUser = true;
          inherit (user) uid;
          group = user.name;
          extraGroups = [ "slurm" ];
          openssh.authorizedKeys.keys = user.sshKeys;
        };
      }) cfg.users
    );

    users.groups = listToAttrs (
      map (user: {
        inherit (user) name;
        value = {
          gid = user.uid;
        };
      }) cfg.users
    );

    networking.hosts = listToAttrs (
      map (host: {
        inherit (host) name;
        value = [ host.address ];
      }) cfg.memberHosts
    );

    networking.firewall.extraInputRules = mkIf config.networking.nftables.enable nftRules;
    networking.firewall.extraCommands = mkIf (!config.networking.nftables.enable) iptablesRules;

    services.nfs.server.enable = hasController && usesNfs;
    services.nfs.server.exports = mkIf (hasController && usesNfs) nfsExports;
    systemd.tmpfiles.rules = mkIf (hasController && usesNfs) (
      map (path: "d ${path} 0755 root root -") cfg.storage.exports
    );

    fileSystems = lib.mkMerge [
      (mkIf (usesNfs && !hasController) nfsMounts)
      (mkIf usesCephFs cephMounts)
    ];

    hardware.nvidia = mkIf cfg.gpu.driver {
      modesetting.enable = true;
      package = config.boot.kernelPackages.nvidiaPackages.stable;
    };
    services.xserver.videoDrivers = mkIf cfg.gpu.driver [ "nvidia" ];
    boot.blacklistedKernelModules = mkIf cfg.gpu.driver [ "nouveau" ];

    systemd.services.prometheus-slurm-exporter = mkIf hasController {
      description = "Prometheus exporter for Slurm";
      wantedBy = [ "multi-user.target" ];
      after = [
        "munged.service"
        "slurmctld.service"
      ];
      serviceConfig = {
        ExecStart = "${pkgs.prometheus-slurm-exporter}/bin/prometheus-slurm-exporter";
        Restart = "on-failure";
        User = "root";
      };
    };
  };
}
