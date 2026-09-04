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
        MARKER="/var/lib/kcore/rbd-seeded/${rbdImage}"
        if [ ! -f "$MARKER" ]; then
          test -e "$SOURCE" || { echo "missing source image: $SOURCE"; exit 1; }
          echo "Seeding RBD $IMAGE from $SOURCE..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f ${vmCfg.imageFormat} -O raw \
            "$SOURCE" "$RBD_DEV"
          mkdir -p "$(dirname "$MARKER")"
          touch "$MARKER"
        fi
      '';

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
        ++ vmCfg.extraArgs
      );

      liveMigratedMarker = "${cfg.socketDir}/${vmName}.live-migrated";
      migratePidFile = "${cfg.socketDir}/${vmName}.migrate.pid";

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
          ${pkgs.coreutils}/bin/rm -f "${liveMigratedMarker}" "${migratePidFile}"
          if ! ${pkgs.coreutils}/bin/kill -0 "$pid" 2>/dev/null; then
            echo "ERROR: live-migrated cloud-hypervisor pid $pid is not running"
            exit 1
          fi
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
      # Keep a live-migrated CH alive across the nixos-rebuild that first
      # installs this unit on the destination node.
      stopIfChanged = !isCeph;
      restartIfChanged = !isCeph;

      serviceConfig = {
        Type = "simple";
        ExecStartPre = [ "${startPreScript}" ];
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
    ];

    boot.supportedFilesystems = lib.mkIf anyVmUsesZfs [ "zfs" ];

    services.lvm.enable = lib.mkIf anyVmUsesLvm true;

    systemd.services = lib.mapAttrs' (
      vmName: vmCfg: lib.nameValuePair "kcore-vm-${vmName}" (mkVmService vmName vmCfg)
    ) cfg.virtualMachines;
  };
}
