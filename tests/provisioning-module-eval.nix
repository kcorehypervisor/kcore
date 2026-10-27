# Eval the provisioner module with two networks and stand-in packages.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  eval = nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      ../modules/kcore-provisioning.nix
      (
        { pkgs, ... }:
        {
          networking.hostName = "provisioning-eval";
          system.stateVersion = "25.11";
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "nodev";
            fsType = "tmpfs";
          };
          services.kcore.provisioning = {
            enable = true;
            package = pkgs.hello;
            upstreamDns = [
              "1.1.1.1"
              "8.8.8.8"
            ];
            listenAddresses = [
              "127.0.0.1:9443"
              "192.168.40.10:9443"
            ];
            networks = {
              lan = {
                interface = "lan10g";
                cidr = "192.168.40.0/24";
                gateway = "192.168.40.1";
                domain = "home.arpa";
                dnsServers = [ "192.168.40.10" ];
                pools = [
                  {
                    start = "192.168.40.100";
                    end = "192.168.40.200";
                  }
                ];
              };
              garage = {
                interface = "vlan50";
                cidr = "192.168.50.0/24";
                gateway = "192.168.50.1";
                domain = "garage.home.arpa";
                dnsServers = [ "192.168.40.10" ];
                pools = [
                  {
                    start = "192.168.50.100";
                    end = "192.168.50.200";
                  }
                ];
              };
            };
          };
        }
      )
    ];
  };
  cfg = eval.config;
  seed = cfg.environment.etc."kcore-provisioning/seed.json".text;
in
pkgs.runCommand "provisioning-module-eval"
  {
    preferLocalBuild = true;
  }
  ''
    test ${lib.escapeShellArg cfg.networking.hostName} = provisioning-eval
    echo ${lib.escapeShellArg seed} | grep -q '192.168.40.0/24'
    echo ${lib.escapeShellArg seed} | grep -q '192.168.50.0/24'
    echo ${lib.escapeShellArg seed} | grep -q '1.1.1.1'
    echo ${lib.escapeShellArg seed} | grep -q '8.8.8.8'
    test ${lib.boolToString cfg.systemd.services.kcore-kea-dhcp4.enable} = true
    test ${lib.boolToString cfg.systemd.services.kcore-pdns.enable} = true
    test ${lib.boolToString cfg.systemd.services.kcore-pdns-recursor.enable} = true
    test ${lib.boolToString cfg.systemd.services.kcore-provisioning.enable} = true
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-provisioning.serviceConfig.ExecStart} | grep -q '192.168.40.10:9443'
    touch $out
  ''
