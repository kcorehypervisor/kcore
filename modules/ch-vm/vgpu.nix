# NVIDIA vGPU host. The Virtual GPU Manager stays bound to the physical
# card and creates SR-IOV virtual functions. kcore does not ship the manager;
# the operator supplies a build for this kernel, or a staged .run file.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.ch-vm.vgpu;
  runPackage =
    if cfg.managerRun == null then
      null
    else
      pkgs.callPackage ../../pkgs/nvidia-vgpu-manager {
        kernel = config.boot.kernelPackages.kernel;
        src = pkgs.requireFile {
          name = cfg.managerRun.file;
          inherit (cfg.managerRun) sha256;
          message = ''
            Stage ${cfg.managerRun.file} into the Nix store on this node
            (kctl node vgpu enable does this) before rebuilding.
          '';
        };
      };
  manage = if cfg.managerPackage != null then cfg.managerPackage else runPackage;
in
{
  options.ch-vm.vgpu = {
    enable = lib.mkEnableOption ''
      NVIDIA vGPU. Loads the operator-supplied Virtual GPU Manager and creates
      SR-IOV virtual functions on hostCards. The physical card is never bound
      to vfio-pci.
    '';

    managerPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = ''
        NVIDIA Virtual GPU Manager for Linux KVM, built for this kernel.
        kcore does not distribute it. Leave unset when managerRun names a
        .run file already added to the Nix store.
      '';
    };

    managerRun = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.submodule {
          options = {
            file = lib.mkOption {
              type = lib.types.str;
              example = "NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run";
              description = "File name passed to nix-store --add-fixed.";
            };
            sha256 = lib.mkOption {
              type = lib.types.str;
              example = "2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d";
              description = "sha256 hex of the .run file.";
            };
            version = lib.mkOption {
              type = lib.types.str;
              default = "595.91.04";
              description = "Manager version. Recorded for the operator; the package is 595.91.04.";
            };
          };
        }
      );
      default = null;
      description = ''
        Virtual GPU Manager .run already in the Nix store
        (`nix-store --add-fixed sha256`). Used when managerPackage is null.
      '';
    };

    hostCards = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "0000:af:00.0" ];
      description = "Physical functions that stay on the NVIDIA host driver.";
    };

    numVfs = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = null;
      description = "Virtual functions to create on each host card. Null asks sriov-manage for every function the card supports.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = manage != null;
        message = "ch-vm.vgpu.enable requires ch-vm.vgpu.managerPackage, or ch-vm.vgpu.managerRun for a .run file already in the Nix store";
      }
    ];

    nixpkgs.config.allowUnfreePredicate =
      pkg: lib.getName pkg == "nvidia-vgpu-manager" || lib.getName pkg == "nvidia-vgpu-manager-595.91.04";

    boot.kernelParams = [
      "iommu=pt"
      "amd_iommu=on"
      "intel_iommu=on"
    ];
    boot.blacklistedKernelModules = [ "nouveau" ];
    # Do not list these in boot.kernelModules. The first switch often still
    # has nouveau resident, and a failed modprobe fails nixos-rebuild before
    # the reboot that unloads it. The units below load the modules once
    # nouveau is gone.
    boot.extraModulePackages = lib.optional (manage != null) manage;
    hardware.firmware = lib.optional (manage != null) manage;
    environment.systemPackages = lib.optional (manage != null) manage;

    environment.etc."kcore/vgpu-hosts".text = lib.concatMapStrings (
      addr: "${lib.toLower addr}\n"
    ) cfg.hostCards;

    systemd.services.nvidia-modprobe = lib.mkIf (manage != null) {
      description = "Create NVIDIA device nodes";
      after = [ "systemd-modules-load.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        if grep -q '^nouveau ' /proc/modules; then
          echo "nouveau is still loaded; NVIDIA vGPU starts after reboot"
          exit 0
        fi
        ${pkgs.kmod}/bin/modprobe nvidia
        ${pkgs.kmod}/bin/modprobe nvidia_vgpu_vfio
        ${manage}/bin/nvidia-modprobe -c 0 -u || true
      '';
    };

    systemd.services.nvidia-vgpud = lib.mkIf (manage != null) {
      description = "NVIDIA vGPU Daemon";
      after = [
        "systemd-modules-load.service"
        "nvidia-modprobe.service"
      ];
      wants = [ "nvidia-modprobe.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStartPre = "${pkgs.coreutils}/bin/mkdir -p /usr/share/nvidia/vgpu";
        BindReadOnlyPaths = "${manage}/share/nvidia/vgpu/vgpuConfig.xml:/usr/share/nvidia/vgpu/vgpuConfig.xml";
      };
      script = ''
        if grep -q '^nouveau ' /proc/modules; then
          echo "nouveau is still loaded; NVIDIA vGPU starts after reboot"
          exit 0
        fi
        ${manage}/bin/nvidia-vgpud
      '';
    };

    systemd.services.nvidia-vgpu-mgr = lib.mkIf (manage != null) {
      description = "NVIDIA vGPU Manager Daemon";
      after = [ "nvidia-vgpud.service" ];
      requires = [ "nvidia-vgpud.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStopPost = "${pkgs.coreutils}/bin/rm -rf /var/run/nvidia-vgpu-mgr";
      };
      script = ''
        if grep -q '^nouveau ' /proc/modules; then
          echo "nouveau is still loaded; NVIDIA vGPU starts after reboot"
          exit 0
        fi
        ${manage}/bin/nvidia-vgpu-mgr
      '';
    };

    systemd.services.kcore-vgpu-sriov = lib.mkIf (manage != null) {
      description = "Create NVIDIA vGPU virtual functions";
      after = [
        "systemd-modules-load.service"
        "nvidia-vgpu-mgr.service"
      ];
      requires = [ "nvidia-vgpu-mgr.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        set -eu
        if grep -q '^nouveau ' /proc/modules; then
          echo "nouveau is still loaded; NVIDIA vGPU starts after reboot"
          exit 0
        fi
        manage=""
        for candidate in \
          ${manage}/bin/sriov-manage \
          ${manage}/lib/nvidia/sriov-manage \
          ${manage}/libexec/nvidia/sriov-manage
        do
          if [ -x "$candidate" ]; then
            manage=$candidate
            break
          fi
        done
        sys="${config.ch-vm.vms.sysfsRoot}"
        for card in ${lib.concatStringsSep " " (map lib.toLower cfg.hostCards)}; do
          dev="$sys/bus/pci/devices/$card"
          if [ ! -e "$dev" ]; then
            echo "vGPU host card $card does not exist"
            exit 1
          fi
          ${
            if cfg.numVfs == null then
              ''
                if [ -z "$manage" ]; then
                  echo "sriov-manage was not found in the Virtual GPU Manager package"
                  exit 1
                fi
                "$manage" -e "$card"
              ''
            else
              ''
                echo ${toString cfg.numVfs} > "$dev/sriov_numvfs"
              ''
          }
          for _ in 1 2 3 4 5 6 7 8 9 10; do
            if [ -e "$dev/virtfn0" ]; then
              break
            fi
            sleep 0.2
          done
          if [ ! -e "$dev/virtfn0" ]; then
            echo "vGPU host card $card did not create virtfn0"
            exit 1
          fi
        done
      '';
    };
  };
}
