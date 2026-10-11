# Eval-only guard for modules/ch-vm: unit properties that protect a running
# guest and the firewall, checked without building anything. Script contents
# (sha check, second-VMM refusal) are exercised in tests/vm-module.nix.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  eval = nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../modules/ch-vm
      (
        { pkgs, ... }:
        {
          networking.hostName = "chvm-eval";
          system.stateVersion = "25.05";
          # The provision scripts interpolate `${pkgs.ceph}`; the stock nixpkgs
          # ceph does not evaluate on this revision (nodes get it from the
          # kcore-ceph overlay). Nothing here runs it, so any package will do.
          nixpkgs.overlays = [ (_final: prev: { ceph = prev.hello; }) ];
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "nodev";
            fsType = "tmpfs";
          };
          ch-vm.vms = {
            enable = true;
            cloudHypervisorPackage = pkgs.cloud-hypervisor;
            gatewayInterface = "eth0";
            networks.default = {
              externalIP = "10.0.2.15";
              gatewayIP = "192.168.100.1";
            };
            virtualMachines.ceph-guest = {
              image = "/var/lib/kcore/images/guest.img";
              imageFormat = "qcow2";
              imageSha256 = "2f119f50bf29fe40e73b69ae2a736e8210d8146796f036e0324315b6b3a777df";
              cores = 1;
              memorySize = 256;
              network = "default";
              storageBackend = "ceph";
              storageSizeBytes = 10737418240;
              autoStart = true;
            };
            virtualMachines.lvm-guest = {
              image = "/var/lib/kcore/images/guest.img";
              cores = 1;
              memorySize = 256;
              network = "default";
              storageBackend = "lvm";
              storageSizeBytes = 10737418240;
              autoStart = false;
            };
          };
        }
      )
    ];
  };
  cfg = eval.config;
  cephUnit = cfg.systemd.services."kcore-vm-ceph-guest";
  lvmUnit = cfg.systemd.services."kcore-vm-lvm-guest";
  fwUnit = cfg.systemd.services."kcore-firewall-default";
  chBin = lib.removeSuffix "\n" cfg.environment.etc."kcore/cloud-hypervisor".text;
  unpatched = "${pkgs.cloud-hypervisor}/bin/cloud-hypervisor";
in
# The node agent spawns the receiving VMM from this file, so it must name the
# patched build (not the stock package) and the units must exec the same one.
assert lib.hasSuffix "/bin/cloud-hypervisor" chBin;
assert chBin != unpatched;
assert lib.hasInfix "cloud-hypervisor-51.1" chBin;
assert cfg.environment.etc."kcore/vm-socket-dir".text == "/run/kcore\n";
# A second VMM for the same VM is refused with a status systemd will not retry.
assert cephUnit.serviceConfig.RestartPreventExitStatus == "75";
assert lvmUnit.serviceConfig.RestartPreventExitStatus == "75";
# Restart must not depend on incomingMigration (see vm-service.nix).
assert cephUnit.serviceConfig.Restart == "always";
assert lvmUnit.serviceConfig.Restart == "no";
assert cephUnit.serviceConfig.TimeoutStopSec == 30;
# The raw `ip kcore-<net>` / `bridge kcoreew-<net>` tables live outside the
# NixOS nftables ruleset; a flush on reload would drop VM NAT and isolation.
assert cfg.networking.nftables.enable;
assert !cfg.networking.nftables.flushRuleset;
assert cfg.networking.nftables.ruleset == "";
assert cfg.networking.nftables.rulesetFile == null;
# Those tables belong to a unit of their own: applied as one `nft -f`
# transaction, reloaded (not stopped) on rule changes, deleted only when the
# network goes away. The bridge unit never restarts, so it must not own them.
assert fwUnit.reloadIfChanged;
assert fwUnit.serviceConfig.RemainAfterExit;
assert lib.hasSuffix "-kcore-firewall-default.nft" fwUnit.serviceConfig.ExecStart;
assert fwUnit.serviceConfig.ExecReload == fwUnit.serviceConfig.ExecStart;
assert lib.elem "kcore-bridge-default.service" fwUnit.requires;
assert lib.hasInfix "nft delete table ip kcore-default" fwUnit.preStop;
assert !(lib.hasInfix "nft " cfg.systemd.services."kcore-bridge-default".script);
assert !(lib.hasInfix "nft " cfg.systemd.services."kcore-bridge-default".preStop);
assert !cfg.systemd.services."kcore-bridge-default".restartIfChanged;
# Setting cloudHypervisorPatches = [] must hand back the stock package.
assert
  (nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../modules/ch-vm
      (
        { pkgs, ... }:
        {
          networking.hostName = "chvm-eval-stock";
          system.stateVersion = "25.05";
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "nodev";
            fsType = "tmpfs";
          };
          ch-vm.vms = {
            enable = true;
            cloudHypervisorPackage = pkgs.cloud-hypervisor;
            cloudHypervisorPatches = [ ];
            gatewayInterface = "eth0";
          };
        }
      )
    ];
  }).config.environment.etc."kcore/cloud-hypervisor".text == unpatched + "\n";
pkgs.runCommand "ch-vm-module-eval-ok" { } ''
  echo ok > "$out"
''
