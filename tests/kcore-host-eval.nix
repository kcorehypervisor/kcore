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
      ../modules/kcore-slurm.nix
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
          services.kcore.dashboard.package = pkgs.hello;
          services.kcore.console = {
            enable = true;
            package = pkgs.hello;
          };
          services.kcore.kctl.package = pkgs.hello;
          kcore.slurm.enable = false;
          kcore.metrics = {
            enable = true;
            allowedScrapeSources = [ "127.0.0.1" ];
            server.enable = true;
          };
          kcore.ingress = {
            enable = true;
            package = pkgs.hello;
            listenAddresses = [ "203.0.113.10" ];
            tcpListeners = [
              {
                address = "203.0.113.10";
                port = 5432;
              }
            ];
            vip = {
              address = "203.0.113.10";
              interface = "eth0";
              sourceAddress = "192.0.2.5";
              peers = [ "192.0.2.6" ];
            };
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
  # A host flake from before the dashboard default sets no dashboard package.
  noDashboardPackage = nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      ../modules/kcore-host.nix
      {
        services.kcore.controller = {
          enable = true;
          package = pkgs.hello;
        };
      }
    ];
  };
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
    echo ${lib.escapeShellArg (toString cfg.systemd.services.kcore-console.serviceConfig.ExecStartPre)} | grep -q 'dmesg -n 1'
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-console.serviceConfig.ExecStart} | grep -qv /opt/kcore
    test -n ${lib.escapeShellArg cfg.systemd.services.kcore-controller.serviceConfig.ExecStart}
    test ${lib.boolToString cfg.services.kcore.dashboard.enable} = true
    test ${lib.boolToString noDashboardPackage.config.services.kcore.dashboard.enable} = false
    test ${cfg.systemd.services.kcore-controller.environment.KCORE_WEB_ENABLED} = true
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-controller.environment.KCORE_WEB_UI_DIR} | grep -q /share/kcore-dashboard
    echo ${lib.escapeShellArg (toString cfg.networking.firewall.allowedTCPPorts)} | grep -qw 9443
    test ${lib.boolToString cfg.systemd.services.kcore-boot-confirm.enable} = true
    test -n ${lib.escapeShellArg cfg.systemd.services."kcore-vm-guest".serviceConfig.ExecStart}
    echo ${lib.escapeShellArg cfg.environment.etc."kcore/version".text} | grep -q 0.4.2
    test ${lib.boolToString cfg.systemd.services.kcore-ingress.serviceConfig.DynamicUser} = true
    test ${lib.boolToString cfg.systemd.services.kcore-ingress.serviceConfig.NoNewPrivileges} = true
    echo ${lib.escapeShellArg cfg.systemd.services.kcore-ingress.serviceConfig.ExecStart} | grep -q hello
    echo ${lib.escapeShellArg cfg.networking.firewall.extraCommands} | grep -q 'dport 443'
    echo ${lib.escapeShellArg cfg.networking.firewall.extraCommands} | grep -q 'udp --dport 443'
    test -n ${lib.escapeShellArg cfg.systemd.services.keepalived-ingress.serviceConfig.ExecStart}
    echo ${lib.escapeShellArg cfg.networking.firewall.extraCommands} | grep -q -- '-s 127.0.0.1 -p tcp --dport 2020'
    echo ${lib.escapeShellArg cfg.networking.firewall.extraCommands} | grep -q -- '-d 203.0.113.10 -p tcp --dport 5432'
    grep -F -q 'nix build --impure --no-link --print-out-paths' ${../flake.nix}
    grep -F -q 'nixos-install --system "$SYSTEM" --root /mnt --no-root-passwd' ${../flake.nix}
    if grep -F -q -- '--offline' ${../flake.nix}; then
      echo "install-to-disk must leave substituters enabled"
      exit 1
    fi
    if grep -F -q 'nixos-install --flake' ${../flake.nix}; then
      echo "install-to-disk must build on the installer store, then nixos-install --system"
      exit 1
    fi
    if grep -F -q -- '--store /mnt' ${../flake.nix}; then
      echo "install-to-disk must not build into the destination store"
      exit 1
    fi
    touch $out
  ''
