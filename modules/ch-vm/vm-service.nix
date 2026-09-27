{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.ch-vm.vms;
  helpers = import ./helpers.nix { inherit lib; };
  inherit (helpers) tapName generateMac;

  mkVmService =
    vmName: vmCfg:
    let
      mac = if vmCfg.macAddress != null then vmCfg.macAddress else generateMac vmName;

      socketPath = "${cfg.socketDir}/${vmName}.sock";
      serialSocket = "${cfg.socketDir}/${vmName}.serial.sock";
      seedIso = "/etc/kcore/seeds/${vmName}.iso";
      firmwarePath =
        if cfg.firmwarePath != null then cfg.firmwarePath else "${pkgs.OVMF-cloud-hypervisor.firmware}";
      chBin = "${cfg.cloudHypervisorPackage}/bin/cloud-hypervisor";

      isLvm = vmCfg.storageBackend == "lvm";
      isZfs = vmCfg.storageBackend == "zfs";
      isCeph = vmCfg.storageBackend == "ceph";
      isBlockBackend = isLvm || isZfs || isCeph;

      lvName = "kcore-${vmName}";
      lvDevice = "/dev/${cfg.lvmVgName}/${lvName}";

      zvolDataset = "${cfg.zfsPoolName}/kcore-${vmName}";
      zvolDevice = "/dev/zvol/${zvolDataset}";
      rbdImage = if vmCfg.rbdImage != "" then vmCfg.rbdImage else "kcore-${vmName}";
      rbdDevice = "/dev/rbd/${cfg.rbdPool}/${rbdImage}";

      actualDisk =
        if isLvm then
          lvDevice
        else if isZfs then
          zvolDevice
        else if isCeph then
          rbdDevice
        else
          toString vmCfg.image;
      actualFormat = if isBlockBackend then "raw" else vmCfg.imageFormat;

      vmDiskArg = "path=${actualDisk},image_type=${actualFormat}";
      seedDiskArg = "path=${seedIso},readonly=on,image_type=raw";

      lvmProvisionScript = pkgs.writeShellScript "lvm-provision-${vmName}" ''
        set -e
        LV_DEVICE="${lvDevice}"
        VG="${cfg.lvmVgName}"
        LV="${lvName}"
        SIZE_BYTES="${toString vmCfg.storageSizeBytes}"

        if [ ! -b "$LV_DEVICE" ]; then
          echo "Creating LV $VG/$LV (''${SIZE_BYTES} bytes)..."
          ${pkgs.lvm2.bin}/bin/lvcreate -y -L "''${SIZE_BYTES}B" -n "$LV" "$VG"
          echo "Converting source image to LV..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f ${vmCfg.imageFormat} -O raw \
            ${toString vmCfg.image} "$LV_DEVICE"
          echo "LVM volume provisioned: $LV_DEVICE"
        else
          echo "LV $LV_DEVICE already exists, skipping provision"
        fi
      '';

      zfsProvisionScript = pkgs.writeShellScript "zfs-provision-${vmName}" ''
        set -e
        ZVOL_DATASET="${zvolDataset}"
        ZVOL_DEVICE="${zvolDevice}"
        SIZE_BYTES="${toString vmCfg.storageSizeBytes}"

        if ! ${pkgs.zfs}/bin/zfs list -H "$ZVOL_DATASET" >/dev/null 2>&1; then
          echo "Creating zvol $ZVOL_DATASET (''${SIZE_BYTES} bytes)..."
          ${pkgs.zfs}/bin/zfs create -V "''${SIZE_BYTES}" -o volmode=dev "$ZVOL_DATASET"
          # Wait for the device node to appear
          for i in $(seq 1 30); do
            [ -b "$ZVOL_DEVICE" ] && break
            sleep 0.2
          done
          if [ ! -b "$ZVOL_DEVICE" ]; then
            echo "ERROR: zvol device $ZVOL_DEVICE did not appear after create"
            exit 1
          fi
          echo "Converting source image to zvol..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f ${vmCfg.imageFormat} -O raw \
            ${toString vmCfg.image} "$ZVOL_DEVICE"
          echo "ZFS volume provisioned: $ZVOL_DEVICE"
        else
          echo "zvol $ZVOL_DATASET already exists, skipping provision"
        fi
      '';

      cephMapScript = pkgs.writeShellScript "ceph-map-${vmName}" ''
        set -e
        IMAGE="${cfg.rbdPool}/${rbdImage}"
        RBD_DEV="${rbdDevice}"
        SOURCE="${toString vmCfg.image}"
        # Controller/CephAdapter owns rbd create; this script only maps and
        # seeds the guest image once onto the block device (like LVM/ZFS).
        if ! ${pkgs.ceph}/bin/rbd info "$IMAGE" >/dev/null 2>&1; then
          echo "ERROR: RBD image $IMAGE does not exist; create the VM via kctl first"
          exit 1
        fi
        if [ ! -b "$RBD_DEV" ]; then
          ${pkgs.ceph}/bin/rbd map "$IMAGE"
        fi
        test -b "$RBD_DEV"
        # Cluster-visible seed flag so cold drain/migrate to another node does
        # not re-run qemu-img convert and wipe the shared RBD.
        LOCAL_MARKER="/var/lib/kcore/rbd-seeded/${rbdImage}"
        SEEDED=0
        if ${pkgs.ceph}/bin/rbd image-meta get "$IMAGE" kcore.seeded >/dev/null 2>&1; then
          SEEDED=1
        elif [ -f "$LOCAL_MARKER" ]; then
          SEEDED=1
          ${pkgs.ceph}/bin/rbd image-meta set "$IMAGE" kcore.seeded 1 || true
        fi
        if [ "$SEEDED" -eq 0 ]; then
          test -e "$SOURCE" || { echo "missing source image: $SOURCE"; exit 1; }
          echo "Seeding RBD $IMAGE from $SOURCE..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f ${vmCfg.imageFormat} -O raw \
            "$SOURCE" "$RBD_DEV"
          ${pkgs.ceph}/bin/rbd image-meta set "$IMAGE" kcore.seeded 1
          mkdir -p "$(dirname "$LOCAL_MARKER")"
          touch "$LOCAL_MARKER"
        fi
      '';

      normalizedPci = map (dev: dev // { address = lib.toLower dev.address; }) vmCfg.pciDevices;

      # Live migration requires MAP_SHARED guest RAM (`shared=on`).
      memoryArg =
        if isCeph then
          "--memory size=${toString vmCfg.memorySize}M,shared=on"
        else
          "--memory size=${toString vmCfg.memorySize}M";

      chArgs = lib.concatStringsSep " " (
        [
          "--api-socket ${socketPath}"
          "--cpus boot=${toString vmCfg.cores}"
          memoryArg
          "--firmware ${firmwarePath}"
          "--serial socket=${serialSocket}"
          "--disk ${vmDiskArg} ${seedDiskArg}"
          "--net tap=${tapName vmName},mac=${mac}"
        ]
        ++ map (dev: "--device path=/sys/bus/pci/devices/${dev.address},iommu=on") normalizedPci
        ++ vmCfg.extraArgs
      );

      liveMigratedMarker = "${cfg.socketDir}/${vmName}.live-migrated";
      migratePidFile = "${cfg.socketDir}/${vmName}.migrate.pid";

      # Bind the VM's PCI devices to vfio-pci. The guest gets each one via
      # `--device`. Live migration does not carry these devices, so a
      # receive-mode handoff skips the bind (the unit only adopts the pid).
      vfioBindScript = pkgs.writeShellScript "vfio-bind-${vmName}" ''
        set -euo pipefail
        if [ -f "${liveMigratedMarker}" ]; then
          echo "live-migrated marker present; not rebinding PCI devices"
          exit 0
        fi
        ${pkgs.kmod}/bin/modprobe vfio-pci
        requested="${lib.concatStringsSep " " (map (dev: dev.address) normalizedPci)}"
        for addr in $requested; do
          dev="/sys/bus/pci/devices/$addr"
          if [ ! -e "$dev" ]; then
            echo "PCI device $addr does not exist on this node"
            exit 1
          fi
          if [ ! -e "$dev/iommu_group" ]; then
            echo "PCI device $addr has no IOMMU group. Boot with ch-vm.vfio.enable and reboot before attaching it."
            exit 1
          fi
          group=$(${pkgs.coreutils}/bin/readlink -f "$dev/iommu_group")
          for member in "$group"/devices/*; do
            m=$(${pkgs.coreutils}/bin/basename "$member")
            case " $requested " in
              *" $m "*) ;;
              *)
                echo "IOMMU group of $addr also contains $m. Pass every device in that group."
                exit 1
                ;;
            esac
          done
          current=""
          if [ -e "$dev/driver" ]; then
            current=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$dev/driver")")
          fi
          if [ "$current" != "vfio-pci" ]; then
            echo "Binding $addr to vfio-pci (was ''${current:-unbound})"
            echo vfio-pci > "$dev/driver_override"
            if [ -n "$current" ]; then
              echo "$addr" > "$dev/driver/unbind"
            fi
            echo "$addr" > /sys/bus/pci/drivers_probe
          fi
          if [ ! -e "$dev/driver" ]; then
            echo "Failed to bind $addr to vfio-pci"
            exit 1
          fi
          current=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$dev/driver")")
          if [ "$current" != "vfio-pci" ]; then
            echo "Failed to bind $addr to vfio-pci (current driver: $current)"
            exit 1
          fi
        done
      '';

      # After a live receive, CH is already running outside systemd. Skip
      # destructive socket cleanup so the handoff ExecStart can adopt it.
      startPreScript = pkgs.writeShellScript "kcore-vm-${vmName}-pre" ''
        set -e
        if [ -f "${liveMigratedMarker}" ]; then
          echo "live-migrated marker present; skipping socket wipe / cold provision"
          exit 0
        fi
        ${pkgs.coreutils}/bin/rm -f ${socketPath} ${serialSocket}
        ${pkgs.bash}/bin/bash -euc 'test -f ${seedIso} || { echo "missing cloud-init seed: ${seedIso}"; exit 1; }'
        ${pkgs.bash}/bin/bash -euc 'test -f ${firmwarePath} || { echo "missing firmware: ${firmwarePath}"; exit 1; }'
        ${pkgs.bash}/bin/bash -euc 'test -e ${toString vmCfg.image} || { echo "missing source image: ${toString vmCfg.image}"; exit 1; }'
        ${
          if isLvm then
            "${lvmProvisionScript}"
          else if isZfs then
            "${zfsProvisionScript}"
          else if isCeph then
            "${cephMapScript}"
          else
            "true"
        }
      '';

      # Adopt an in-flight receive-mode CH (tail --pid) or cold-start CH.
      startScript = pkgs.writeShellScript "kcore-vm-${vmName}-start" ''
        set -e
        if [ -f "${liveMigratedMarker}" ]; then
          if [ ! -f "${migratePidFile}" ]; then
            echo "ERROR: ${liveMigratedMarker} present but ${migratePidFile} missing"
            exit 1
          fi
          pid="$(${pkgs.coreutils}/bin/cat "${migratePidFile}")"
          if ! ${pkgs.coreutils}/bin/kill -0 "$pid" 2>/dev/null; then
            echo "ERROR: live-migrated cloud-hypervisor pid $pid is not running"
            exit 1
          fi
          ${pkgs.coreutils}/bin/rm -f "${liveMigratedMarker}" "${migratePidFile}"
          echo "Adopting live-migrated cloud-hypervisor pid $pid"
          exec ${pkgs.coreutils}/bin/tail --pid="$pid" -f /dev/null
        fi
        exec ${chBin} ${chArgs}
      '';
    in
    {
      description = "kcore VM ${vmName}";
      requires = [ "kcore-tap-${vmName}.service" ];
      after = [ "kcore-tap-${vmName}.service" ];
      wantedBy = lib.optionals vmCfg.autoStart [ "multi-user.target" ];
      # A live-migrated CH survives the destination rebuild because the unit is
      # *new* there: switch-to-configuration only consults
      # stopIfChanged/restartIfChanged for units that already existed and
      # changed, so the handoff needs no override here. Leaving the defaults on
      # keeps VM spec updates (cpu/memory/extraArgs) actually taking effect for
      # Ceph-backed VMs.

      serviceConfig = {
        Type = "simple";
        ExecStartPre = lib.optionals (normalizedPci != [ ]) [ "${vfioBindScript}" ] ++ [
          "${startPreScript}"
        ];
        ExecStart = "${startScript}";
        ExecStop = "${pkgs.curl}/bin/curl --unix-socket ${socketPath} -s -X PUT http://localhost/api/v1/vm.power-button";
        ExecStopPost = lib.optionalString isCeph "-${pkgs.ceph}/bin/rbd unmap ${rbdDevice}";
        TimeoutStopSec = 30;
        Restart = if vmCfg.autoStart then "always" else "no";
        RestartSec = 5;

        Group = "kvm";
        LimitMEMLOCK = "infinity";
      };
    };
  anyVmUsesZfs = lib.any (vm: vm.storageBackend == "zfs") (lib.attrValues cfg.virtualMachines);
  anyVmUsesLvm = lib.any (vm: vm.storageBackend == "lvm") (lib.attrValues cfg.virtualMachines);
in
{
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.virtualMachines != { } -> cfg.gatewayInterface != "";
        message = "ch-vm.vms.gatewayInterface must be set when virtualMachines are defined.";
      }
    ]
    ++ lib.concatLists (
      lib.mapAttrsToList (
        vmName: vmCfg:
        map (dev: {
          assertion =
            builtins.match "[0-9a-fA-F]{4}:[0-9a-fA-F]{2}:[0-9a-fA-F]{2}\\.[0-7]" dev.address != null;
          message = "VM '${vmName}' pciDevices address '${dev.address}' must look like 0000:03:00.0";
        }) vmCfg.pciDevices
      ) cfg.virtualMachines
    );

    boot.supportedFilesystems = lib.mkIf anyVmUsesZfs [ "zfs" ];

    services.lvm.enable = lib.mkIf anyVmUsesLvm true;

    systemd.services = lib.mapAttrs' (
      vmName: vmCfg: lib.nameValuePair "kcore-vm-${vmName}" (mkVmService vmName vmCfg)
    ) cfg.virtualMachines;
  };
}
