{
  config,
  lib,
  ...
}:

let
  cfg = config.kcore.disko;
  inherit (lib)
    mkOption
    mkEnableOption
    mkIf
    types
    listToAttrs
    imap0
    optionalAttrs
    ;

  dataDiskPartitions =
    idx: _device:
    if cfg.storageBackend == "lvm" then
      {
        lvm = {
          size = "100%";
          content = {
            type = "lvm_pv";
            vg = cfg.lvm.vgName;
          };
        };
      }
    else if cfg.storageBackend == "zfs" then
      {
        zfs = {
          size = "100%";
          content = {
            type = "zfs";
            pool = cfg.zfs.poolName;
          };
        };
      }
    else
      {
        data = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint =
              if idx == 0 then "/var/lib/kcore/volumes" else "/var/lib/kcore/volumes${toString idx}";
          };
        };
      };

  mkDataDisk = idx: device: {
    name = "data${toString idx}";
    value = {
      type = "disk";
      inherit device;
      content = {
        type = "gpt";
        partitions = dataDiskPartitions idx device;
      };
    };
  };
in
{
  options.kcore.disko = {
    enable = mkEnableOption "kcore declarative disk layout via disko";

    osDisk = mkOption {
      type = types.str;
      description = "Block device path for the OS disk.";
      example = "/dev/sda";
    };

    luksPasswordFile = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Path to file containing LUKS passphrase (format-time only).";
    };

    dataDisks = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Block device paths for data disks.";
      example = [ "/dev/nvme0n1" ];
    };

    storageBackend = mkOption {
      type = types.enum [
        "filesystem"
        "lvm"
        "zfs"
      ];
      default = "filesystem";
      description = "Storage backend for data disks.";
    };

    lvm = {
      vgName = mkOption {
        type = types.str;
        default = "vg_kcore";
        description = "LVM volume group name for data disks.";
      };
    };

    zfs = {
      poolName = mkOption {
        type = types.str;
        default = "tank0";
        description = "ZFS pool name for data disks.";
      };
    };
  };

  config = mkIf cfg.enable {
    disko.devices =
      {
        disk =
          {
            os = {
              type = "disk";
              device = cfg.osDisk;
              content = {
                type = "gpt";
                partitions = {
                  ESP = {
                    size = "512M";
                    type = "EF00";
                    content = {
                      type = "filesystem";
                      format = "vfat";
                      mountpoint = "/boot";
                      mountOptions = [ "umask=0077" ];
                    };
                  };
                  root = {
                    size = "100%";
                    content = {
                      type = "luks";
                      name = "cryptroot";
                      passwordFile = cfg.luksPasswordFile;
                      settings = {
                        allowDiscards = true;
                      };
                      content = {
                        type = "filesystem";
                        format = "ext4";
                        mountpoint = "/";
                      };
                    };
                  };
                };
              };
            };
          }
          // listToAttrs (imap0 mkDataDisk cfg.dataDisks);
      }
      // optionalAttrs (cfg.storageBackend == "lvm" && cfg.dataDisks != [ ]) {
        lvm_vg = {
          ${cfg.lvm.vgName} = {
            type = "lvm_vg";
            lvs = { };
          };
        };
      }
      // optionalAttrs (cfg.storageBackend == "zfs" && cfg.dataDisks != [ ]) {
        zpool = {
          ${cfg.zfs.poolName} = {
            type = "zpool";
            datasets = { };
          };
        };
      };
  };
}
