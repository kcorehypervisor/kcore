# NVIDIA vGPU host. The Virtual GPU Manager stays bound to the physical
# card and creates SR-IOV virtual functions. kcore does not ship the manager;
# the operator supplies a build for this kernel.
{
  config,
  lib,
  ...
}:
let
  cfg = config.ch-vm.vgpu;
  manage = cfg.managerPackage;
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
        kcore does not distribute it.
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
        message = "ch-vm.vgpu.enable requires ch-vm.vgpu.managerPackage, the NVIDIA Virtual GPU Manager for this kernel";
      }
    ];

    boot.kernelParams = [
      "iommu=pt"
      "amd_iommu=on"
      "intel_iommu=on"
    ];
    boot.blacklistedKernelModules = [ "nouveau" ];
    boot.extraModulePackages = lib.optional (manage != null) manage;
    environment.systemPackages = lib.optional (manage != null) manage;

    environment.etc."kcore/vgpu-hosts".text = lib.concatMapStrings (
      addr: "${lib.toLower addr}\n"
    ) cfg.hostCards;

    systemd.services.kcore-vgpu-sriov = lib.mkIf (manage != null) {
      description = "Create NVIDIA vGPU virtual functions";
      after = [ "systemd-modules-load.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        set -eu
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
