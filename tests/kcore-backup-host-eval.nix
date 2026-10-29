# Eval the backup appliance profile. It must boot as a normal host and must
# not pull in the hypervisor, Ceph, or the node agent.
{
  pkgs,
  lib,
  nixosSystem,
  consoleModule,
}:
let
  installScript = builtins.readFile ../modules/kcore-backup-install.sh;
  eval = nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      consoleModule
      ../modules/kcore-backup-host.nix
      (_: {
        networking.hostName = "kcore-backup-eval";
        system.stateVersion = "25.05";
        boot.loader.grub.enable = false;
        fileSystems."/" = {
          device = "nodev";
          fsType = "tmpfs";
        };
        services.kcoreBackup.version = "0.5.0";
        services.kcore-console.package = pkgs.hello;
      })
    ];
  };
  cfg = eval.config;
  ports = cfg.networking.firewall.allowedTCPPorts;
  flag = value: if value then "1" else "0";
in
pkgs.runCommand "kcore-backup-host-eval"
  {
    preferLocalBuild = true;
  }
  ''
    test ${lib.escapeShellArg cfg.networking.hostName} = kcore-backup-eval
    test ${cfg.services.dbus.implementation} = dbus
    test ${lib.boolToString cfg.services.openssh.enable} = true
    test ${lib.boolToString cfg.boot.loader.systemd-boot.enable} = true
    test ${toString cfg.boot.loader.systemd-boot.configurationLimit} = 8
    test ${flag (lib.elem 22 ports)} = 1
    test ${flag (lib.elem 9191 ports)} = 1
    test ${flag (lib.elem "kvm" cfg.boot.kernelModules)} = 0
    test ${flag (lib.elem "br_netfilter" cfg.boot.kernelModules)} = 0
    test ${flag (cfg.boot.kernel.sysctl ? "net.ipv4.ip_forward")} = 0
    test ${flag (eval.options ? ch-vm)} = 0
    test ${flag (eval.options.services ? kcore)} = 0
    test ${flag (cfg.systemd.services ? kcore-console)} = 1
    test ${flag (lib.hasInfix "--product vault" cfg.systemd.services.kcore-console.serviceConfig.ExecStart)} = 1
    test ${flag (cfg.systemd.services ? kcore-node-agent)} = 0
    test ${flag (cfg.systemd.services ? kcore-controller)} = 0
    echo ${lib.escapeShellArg cfg.environment.etc."kcore/version".text} | grep -q 0.5.0
    echo ${lib.escapeShellArg (lib.concatStringsSep " " cfg.systemd.tmpfiles.rules)} | grep -q /var/lib/kcore-vault
    test ${flag (lib.hasInfix "kcore-vault.url = \"github:kcorehypervisor/kcore-vault\"" installScript)} = 1
    test ${flag (lib.hasInfix "kcore-vault.nixosModules.kcore-vault" installScript)} = 1
    test ${flag (lib.hasInfix "services.kcore-vault.enable = true;" installScript)} = 1
    test ${flag (lib.hasInfix "443" installScript)} = 1
    test ${flag (lib.hasInfix "10809" installScript)} = 1
    test ${flag (lib.hasInfix "nix build --impure --no-link --print-out-paths" installScript)} = 1
    test ${flag (lib.hasInfix "nixos-install --system \"\$SYSTEM\" --root /mnt --no-root-passwd" installScript)} = 1
    test ${flag (lib.hasInfix "--offline" installScript)} = 0
    test ${flag (lib.hasInfix "nixos-install --flake" installScript)} = 0
    test ${flag (lib.hasInfix "--store /mnt" installScript)} = 0
    touch $out
  ''
