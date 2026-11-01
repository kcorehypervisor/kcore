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
        inherit (config.boot.kernelPackages) kernel;
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
  splitHostDriver = {
    intel = "xe";
    amd = "gim";
  };
  # GIM numbers the cards it probes in PCI address order and reads vf_num by
  # that number, so the list follows the address order, not the option order.
  amdCards = lib.sort (a: b: lib.toLower a.address < lib.toLower b.address) (
    builtins.filter (card: card.vendor == "amd") cfg.sriovCards
  );
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

    sriovCards = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            address = lib.mkOption {
              type = lib.types.str;
              example = "0000:03:00.0";
              description = "Physical function that stays on the vendor host driver.";
            };
            vendor = lib.mkOption {
              type = lib.types.enum [
                "intel"
                "amd"
              ];
              description = ''
                intel: the in-kernel xe driver creates the virtual functions
                through sriov_numvfs. amd: the GIM module creates them when it
                loads, with vf_num.
              '';
            };
            numVfs = lib.mkOption {
              type = lib.types.ints.positive;
              example = 2;
              description = ''
                Virtual functions to create. The card is split evenly, so each
                function gets an equal share. Intel supports 2 on the Arc Pro
                B50 and 4 on the B70; AMD supports 1 to 12 on the Radeon PRO V710.
              '';
            };
          };
        }
      );
      default = [ ];
      description = ''
        Intel and AMD cards split with SR-IOV. Each virtual function is bound to
        vfio-pci and passed to one VM. No licence or profile programming is
        involved; NVIDIA cards use hostCards instead.
      '';
    };

    amd = {
      gimPackage = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = pkgs.callPackage ../../pkgs/amd-gim {
          inherit (config.boot.kernelPackages) kernel;
        };
        defaultText = lib.literalExpression "pkgs.callPackage ../../pkgs/amd-gim { inherit (config.boot.kernelPackages) kernel; }";
        description = ''
          AMD GPU-IOV Module (github.com/amd/MxGPU-Virtualization) built for
          this kernel. kcore builds release 9.3.0.K from source.
        '';
      };
      schedulePolicy = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.unsigned;
        default = null;
        description = "GIM sch_policy. Null keeps the driver default (Solid Mode, equal time slices).";
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf (cfg.sriovCards != [ ]) {
      assertions = [
        {
          assertion = amdCards == [ ] || cfg.amd.gimPackage != null;
          message = "ch-vm.vgpu.sriovCards has an amd card; ch-vm.vgpu.amd.gimPackage must not be null";
        }
        {
          assertion =
            !cfg.enable
            ||
              lib.intersectLists (map lib.toLower cfg.hostCards) (
                map (card: lib.toLower card.address) cfg.sriovCards
              ) == [ ];
          message = "a card cannot be in both ch-vm.vgpu.hostCards and ch-vm.vgpu.sriovCards";
        }
      ];

      boot.kernelParams = [
        "iommu=pt"
        "amd_iommu=on"
        "intel_iommu=on"
      ];
      # amdgpu would claim the physical function before GIM.
      boot.blacklistedKernelModules = lib.optional (amdCards != [ ]) "amdgpu";
      boot.extraModulePackages = lib.optional (amdCards != [ ]) cfg.amd.gimPackage;
      boot.kernelModules = lib.optional (amdCards != [ ]) "gim";
      boot.extraModprobeConfig = lib.optionalString (amdCards != [ ]) ''
        options gim vf_num=${lib.concatMapStringsSep "," (card: toString card.numVfs) amdCards}${
          lib.optionalString (cfg.amd.schedulePolicy != null)
            " sch_policy=${lib.concatMapStringsSep "," (_: toString cfg.amd.schedulePolicy) amdCards}"
        }
      '';

      environment.etc."kcore/vgpu-hosts".text = lib.concatMapStrings (
        card: "${lib.toLower card.address}\n"
      ) cfg.sriovCards;

      # Not restarted by a switch: writing sriov_numvfs again removes every
      # virtual function, including ones a running VM holds. A changed split
      # takes effect at the next boot.
      systemd.services.kcore-gpu-sriov = {
        description = "Create Intel and AMD GPU virtual functions";
        after = [ "systemd-modules-load.service" ];
        wantedBy = [ "multi-user.target" ];
        restartIfChanged = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          set -eu
          export PATH="${pkgs.kmod}/bin:${pkgs.coreutils}/bin:$PATH"
          modprobe vfio-pci
          sys="${config.ch-vm.vms.sysfsRoot}"
          driver_of() {
            if [ -e "$1/driver" ]; then
              basename "$(readlink "$1/driver")"
            fi
          }
          split_card() {
            card=$1 vendor=$2 want=$3 host_driver=$4
            dev="$sys/bus/pci/devices/$card"
            if [ ! -e "$dev" ]; then
              echo "$vendor card $card does not exist"
              exit 1
            fi
            drv=$(driver_of "$dev")
            if [ "$drv" != "$host_driver" ]; then
              echo "$vendor card $card driver is ''${drv:-unbound}, not $host_driver"
              exit 1
            fi
            total=$(cat "$dev/sriov_totalvfs" 2>/dev/null || echo 0)
            if [ "$want" -gt "$total" ]; then
              echo "$vendor card $card supports $total virtual functions, not $want"
              exit 1
            fi
            have=$(cat "$dev/sriov_numvfs")
            if [ "$vendor" = intel ]; then
              # Keep xe from probing each new function as a host GPU.
              echo 0 > "$dev/sriov_drivers_autoprobe"
              if [ "$have" != "$want" ]; then
                if [ "$have" != 0 ]; then
                  echo 0 > "$dev/sriov_numvfs"
                fi
                echo "$want" > "$dev/sriov_numvfs"
              fi
            elif [ "$have" != "$want" ]; then
              echo "GIM created $have virtual functions on $card, not $want; reboot after changing vf_num. GIM numbers every AMD GPU on the node in PCI address order, so an AMD GPU missing from ch-vm.vgpu.sriovCards shifts the counts."
              exit 1
            fi
            last=$((want - 1))
            for _ in $(seq 1 50); do
              [ -e "$dev/virtfn$last" ] && break
              sleep 0.2
            done
            for index in $(seq 0 "$last"); do
              if [ ! -e "$dev/virtfn$index" ]; then
                echo "$vendor card $card did not create virtfn$index"
                exit 1
              fi
              vf=$(basename "$(readlink "$dev/virtfn$index")")
              vf_dev="$sys/bus/pci/devices/$vf"
              current=$(driver_of "$vf_dev")
              if [ "$current" != vfio-pci ]; then
                echo vfio-pci > "$vf_dev/driver_override"
                if [ -n "$current" ]; then
                  echo "$vf" > "$vf_dev/driver/unbind"
                fi
                echo "$vf" > "$sys/bus/pci/drivers_probe"
              fi
              if [ "$(driver_of "$vf_dev")" != vfio-pci ]; then
                echo "virtual function $vf of $card did not bind to vfio-pci"
                exit 1
              fi
            done
          }
          ${lib.concatMapStrings (card: ''
            split_card ${lib.toLower card.address} ${card.vendor} ${toString card.numVfs} ${splitHostDriver.${card.vendor}}
          '') cfg.sriovCards}
        '';
      };
    })
    (lib.mkIf cfg.enable {
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
          "kcore-vgpu-sriov.service"
        ];
        wants = [
          "nvidia-modprobe.service"
          "kcore-vgpu-sriov.service"
        ];
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
          # nvidia-vgpu-mgr dlopens libnvidia-vgpu.so by that name. The package
          # ships a versioned file, and dlopen does not search the binary rpath.
          libdir=/run/kcore/nvidia-vgpu-lib
          ${pkgs.coreutils}/bin/mkdir -p "$libdir"
          for base in libnvidia-vgpu libnvidia-vgxcfg libnvidia-ml; do
            src=$(${pkgs.coreutils}/bin/ls ${manage}/lib/$base.so.[0-9]*.[0-9]*.[0-9]* | ${pkgs.coreutils}/bin/head -n1)
            ${pkgs.coreutils}/bin/ln -sfn "$src" "$libdir/$base.so"
          done
          export LD_LIBRARY_PATH="$libdir:${manage}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
          ${manage}/bin/nvidia-vgpu-mgr
        '';
      };

      systemd.services.kcore-vgpu-sriov = lib.mkIf (manage != null) {
        description = "Create NVIDIA vGPU virtual functions";
        # sriov-manage unbinds the PF, which fails once vgpud or vgpu-mgr
        # holds the device open.
        after = [
          "systemd-modules-load.service"
          "nvidia-modprobe.service"
        ];
        wants = [ "nvidia-modprobe.service" ];
        before = [
          "nvidia-vgpud.service"
          "nvidia-vgpu-mgr.service"
        ];
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          set -eu
          # sriov-manage calls modprobe, lspci, and setpci by name.
          export PATH="${pkgs.kmod}/bin:${pkgs.pciutils}/bin:${pkgs.gawk}/bin:${pkgs.coreutils}/bin:$PATH"
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
    })
  ];
}
