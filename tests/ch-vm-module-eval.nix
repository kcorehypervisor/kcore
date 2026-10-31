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
assert lib.hasInfix "localLiveMigrate" cfg.environment.etc."kcore/ch-vm-features".text;
assert lib.hasInfix "cloudInitGuestOpsKeys" cfg.environment.etc."kcore/ch-vm-features".text;
assert lib.hasInfix "ceph-guest ceph" cfg.environment.etc."kcore/vm-placement".text;
assert lib.hasInfix "lvm-guest lvm" cfg.environment.etc."kcore/vm-placement".text;
assert cfg.systemd.targets ? "kcore-placement-confirmed";
assert lib.elem "kcore-placement-confirmed.target" cephUnit.requires;
assert lib.elem "kcore-placement-confirmed.target" cephUnit.after;
assert lib.elem "kcore-placement-confirmed.target" lvmUnit.requires;
assert lib.elem {
  from = 18128;
  to = 18255;
} cfg.networking.firewall.interfaces.eth0.allowedTCPPortRanges;
# A second VMM for the same VM is refused with a status systemd will not retry.
assert cephUnit.serviceConfig.RestartPreventExitStatus == "75";
assert lvmUnit.serviceConfig.RestartPreventExitStatus == "75";
# Stop presses the power button and then waits for the VMM (an adopted one
# lives outside the cgroup, so systemd alone would not), within TimeoutStopSec.
assert lib.hasSuffix "-kcore-vm-ceph-guest-stop" cephUnit.serviceConfig.ExecStop;
assert lib.hasSuffix "-kcore-vm-lvm-guest-stop" lvmUnit.serviceConfig.ExecStop;
assert lib.hasSuffix "-kcore-vm-ceph-guest-start" cephUnit.serviceConfig.ExecStart;
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

let
  vgpuBase = {
    networking.hostName = "chvm-vgpu";
    system.stateVersion = "25.05";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
  };
  vgpuEval = nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../modules/ch-vm
      (
        { pkgs, ... }:
        {
          imports = [ vgpuBase ];
          ch-vm.vgpu = {
            enable = true;
            managerPackage = pkgs.hello;
            hostCards = [ "0000:AF:00.0" ];
          };
          ch-vm.vms = {
            enable = true;
            cloudHypervisorPackage = pkgs.cloud-hypervisor;
            gatewayInterface = "eth0";
            networks.default = {
              externalIP = "10.0.2.15";
              gatewayIP = "192.168.100.1";
            };
            virtualMachines.whole = {
              image = "/var/lib/kcore/images/guest.img";
              cores = 1;
              memorySize = 256;
              network = "default";
              pciDevices = [ { address = "0000:03:00.0"; } ];
              autoStart = false;
            };
            virtualMachines.slice = {
              image = "/var/lib/kcore/images/guest.img";
              cores = 1;
              memorySize = 256;
              network = "default";
              pciDevices = [
                {
                  address = "0000:af:00.4";
                  vgpuProfile = "A2-4C";
                }
              ];
              autoStart = false;
            };
          };
        }
      )
    ];
  };
  vgpuCfg = vgpuEval.config;
  wholeUnit = vgpuCfg.systemd.services."kcore-vm-whole";
  sliceUnit = vgpuCfg.systemd.services."kcore-vm-slice";
  sriov = vgpuCfg.systemd.services.kcore-vgpu-sriov;
  asList =
    value:
    if builtins.isList value then
      value
    else if value == null then
      [ ]
    else
      [ value ];
  mentions = name: value: lib.any (item: lib.hasInfix name (toString item)) (asList value);
  missingManager = nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../modules/ch-vm
      {
        networking.hostName = "chvm-vgpu-assert";
        system.stateVersion = "25.05";
        boot.loader.grub.enable = false;
        fileSystems."/" = {
          device = "nodev";
          fsType = "tmpfs";
        };
        ch-vm.vgpu.enable = true;
      }
    ];
  };
  managerAssertion = lib.findFirst (
    item: lib.hasInfix "managerPackage" item.message
  ) null missingManager.config.assertions;
in
assert managerAssertion != null;
assert managerAssertion.assertion == false;
assert lib.elem "iommu=pt" vgpuCfg.boot.kernelParams;
assert lib.elem "nouveau" vgpuCfg.boot.blacklistedKernelModules;
assert vgpuCfg.environment.etc."kcore/vgpu-hosts".text == "0000:af:00.0\n";
assert lib.hasInfix "vgpuProfile" vgpuCfg.environment.etc."kcore/ch-vm-features".text;
assert lib.hasInfix "vgpuManagerRun" vgpuCfg.environment.etc."kcore/ch-vm-features".text;
assert !(lib.elem "nvidia" vgpuCfg.boot.kernelModules);
assert lib.hasInfix "modprobe nvidia" vgpuCfg.systemd.services.nvidia-modprobe.script;
assert lib.hasInfix "nouveau is still loaded" vgpuCfg.systemd.services.nvidia-vgpu-mgr.script;
assert vgpuCfg.systemd.services ? nvidia-vgpud;
assert vgpuCfg.systemd.services ? nvidia-vgpu-mgr;
assert lib.elem "nvidia-vgpu-mgr.service" (asList sriov.requires);
let
  managerRunEval = nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../modules/ch-vm
      (
        { ... }:
        {
          imports = [ vgpuBase ];
          ch-vm.vgpu = {
            enable = true;
            hostCards = [ "0000:61:00.0" ];
            managerRun = {
              file = "NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run";
              sha256 = "2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d";
              version = "595.91.04";
            };
          };
        }
      )
    ];
  };
in
assert lib.any (
  pkg: lib.hasPrefix "nvidia-vgpu-manager" (pkg.name or "")
) managerRunEval.config.boot.extraModulePackages;
assert lib.hasInfix "sriov-manage" sriov.script;
assert lib.hasInfix "-e" sriov.script;
assert !(lib.hasInfix "sriov_numvfs" sriov.script);
assert mentions "vfio-bind-whole" wholeUnit.serviceConfig.ExecStartPre;
assert !(mentions "vgpu-bind-whole" wholeUnit.serviceConfig.ExecStartPre);
assert mentions "vgpu-bind-slice" sliceUnit.serviceConfig.ExecStartPre;
assert !(mentions "vfio-bind-slice" sliceUnit.serviceConfig.ExecStartPre);
assert lib.elem "kcore-vgpu-sriov.service" sliceUnit.requires;
assert !(lib.elem "kcore-vgpu-sriov.service" wholeUnit.requires);
assert mentions "vgpu-clear-slice" sliceUnit.serviceConfig.ExecStopPost;
pkgs.runCommand "ch-vm-module-eval-ok" { } ''
  echo ok > "$out"
''
