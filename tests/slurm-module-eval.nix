# Eval-only guard: kcore.slurm evaluates for a controller/compute member and
# for enable = false without a full system build.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  base = {
    networking.hostName = "slurm-eval";
    networking.nftables.enable = true;
    system.stateVersion = "25.11";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
  };

  controller = nixosSystem {
    inherit (pkgs) system;
    modules = [
      ../modules/kcore-slurm.nix
      (
        _:
        base
        // {
          kcore.slurm = {
            enable = true;
            nodeId = "node-1";
            nodeAddress = "10.0.0.1";
            clusterName = "hpc-a";
            roles = [
              "controller"
              "dbd"
              "login"
            ];
            accounting = {
              enabled = true;
              nodeId = "node-1";
            };
            controlMachine = "node-1";
            controlAddr = "10.0.0.1";
            iface = "enp6s0";
            memberAddresses = [
              "10.0.0.1"
              "10.0.0.2"
            ];
            memberHosts = [
              {
                name = "node-1";
                address = "10.0.0.1";
              }
              {
                name = "node-2";
                address = "10.0.0.2";
              }
            ];
            slurmConf = ''
              SelectType=select/cons_tres
              NodeName=node-2 NodeAddr=10.0.0.2 CoreSpecCount=2 MemSpecLimit=4096 State=UNKNOWN
              PartitionName=batch Nodes=node-2 Default=YES MaxTime=INFINITE State=UP
            '';
            cgroupConf = ''
              ConstrainCores=yes
              ConstrainRAMSpace=yes
              ConstrainDevices=yes
            '';
            users = [
              {
                name = "alice";
                uid = 2001;
                sshKeys = [ "ssh-ed25519 AAAA" ];
              }
            ];
            storage = {
              mode = "nfs";
              exports = [
                "/home"
                "/scratch"
              ];
            };
          };
        }
      )
    ];
  };

  disabled = nixosSystem {
    inherit (pkgs) system;
    modules = [
      ../modules/kcore-slurm.nix
      (
        _:
        base
        // {
          kcore.slurm.enable = false;
        }
      )
    ];
  };

  cfg = controller.config;
  disabledCfg = disabled.config;
  rules = cfg.networking.firewall.extraInputRules;
in
assert cfg.kcore.slurm.enable;
assert lib.hasInfix "6817, 6818, 6819" rules;
assert lib.hasInfix "60001-63000" rules;
assert lib.hasInfix "10.0.0.2" rules;
assert lib.hasInfix "iifname \"enp6s0\"" rules;
assert cfg.networking.hosts ? "node-2";
assert cfg.services.slurm.server.enable;
assert cfg.services.slurm.client.enable == false;
assert cfg.services.munge.enable;
assert cfg.services.nfs.server.enable;
assert cfg.services.mysql.enable;
assert cfg.services.slurm.dbdserver.enable;
assert lib.hasInfix "10.0.0.2(rw" cfg.services.nfs.server.exports;
assert cfg.users.users.alice.uid == 2001;
assert cfg.users.groups.alice.gid == 2001;
assert cfg.systemd.services ? "prometheus-slurm-exporter";
assert disabledCfg.kcore.slurm.enable == false;
assert disabledCfg.services.munge.enable == false;
assert disabledCfg.services.slurm.server.enable == false;
assert disabledCfg.networking.firewall.extraInputRules == "";
pkgs.runCommand "slurm-module-eval-ok" { } ''
  echo ok > "$out"
''
