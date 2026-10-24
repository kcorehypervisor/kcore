# Eval the installed-node module with a fixture guest fragment. Does not build
# the Rust workspace: service packages are hello.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  eval = nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      ../modules/kcore-host.nix
      (
        { pkgs, ... }:
        {
          networking.hostName = "kcore-host-eval";
          system.stateVersion = "25.05";
          nixpkgs.overlays = [ (_final: prev: { ceph = prev.hello; }) ];
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "nodev";
            fsType = "tmpfs";
          };
          services.kcore.version = "0.4.2";
          services.kcore.nodeAgent = {
            enable = true;
            package = pkgs.hello;
          };
          services.kcore.controller = {
            enable = true;
            package = pkgs.hello;
          };
          services.kcore.dashboard = {
            enable = true;
            package = pkgs.hello;
          };
          services.kcore.console = {
            enable = true;
            package = pkgs.hello;
          };
          services.kcore.kctl.package = pkgs.hello;
          kcore.metrics = {
            enable = true;
            allowedScrapeSources = [ "127.0.0.1" ];
            server.enable = true;
          };
          ch-vm.vms = {
            enable = true;
            cloudHypervisorPackage = pkgs.cloud-hypervisor;
            gatewayInterface = "eth0";
            networks.default = {
              externalIP = "10.0.2.15";
              gatewayIP = "192.168.100.1";
            };
            virtualMachines.guest = {
              image = "/var/lib/kcore/images/guest.img";
              cores = 1;
              memorySize = 256;
              network = "default";
              autoStart = true;
            };
          };
        }
      )
    ];
  };
  cfg = eval.config;
in
pkgs.runCommand "kcore-host-eval"
  {
    preferLocalBuild = true;
  }
  ''
    test ${lib.escapeShellArg cfg.networking.hostName} = kcore-host-eval
    test ${toString cfg.system.stateVersion} = 25.05
    test ${cfg.services.dbus.implementation} = dbus
    test ${toString cfg.boot.loader.systemd-boot.configurationLimit} = 8
    test ${toString cfg.boot.loader.timeout} = 5
    test ${toString cfg.boot.kernel.sysctl."kernel.panic"} = 10
    test -n ${lib.escapeShellArg cfg.systemd.services.kcore-node-agent.serviceConfig.ExecStart}
    test ${lib.boolToString cfg.services.kcore-console.enable} = true
    test ${lib.escapeShellArg cfg.systemd.services.kcore-console.serviceConfig.ExecStart} != ""
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-console.serviceConfig.ExecStart} | grep -q hello
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-console.serviceConfig.ExecStart} | grep -qv /opt/kcore
    test -n ${lib.escapeShellArg cfg.systemd.services.kcore-controller.serviceConfig.ExecStart}
    test ${cfg.systemd.services.kcore-controller.environment.KCORE_WEB_ENABLED} = true
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-controller.environment.KCORE_WEB_UI_DIR} | grep -q /share/kcore-dashboard
    echo ${lib.escapeShellArg (toString cfg.networking.firewall.allowedTCPPorts)} | grep -qw 9443
    test ${lib.boolToString cfg.systemd.services.kcore-boot-confirm.enable} = true
    test -n ${lib.escapeShellArg cfg.systemd.services."kcore-vm-guest".serviceConfig.ExecStart}
    echo ${lib.escapeShellArg cfg.environment.etc."kcore/version".text} | grep -q 0.4.2
    touch $out
  ''
