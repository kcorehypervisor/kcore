{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.ch-vm.vms;
  helpers = import ./helpers.nix { inherit lib; };
  inherit (helpers) vmInterfaces;

  chPkg =
    if cfg.cloudHypervisorPatches == [ ] then
      cfg.cloudHypervisorPackage
    else
      cfg.cloudHypervisorPackage.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ cfg.cloudHypervisorPatches;
      });
  chBin = "${chPkg}/bin/cloud-hypervisor";

  mkVmService =
    vmName: vmCfg:
    let
      ifaces = vmInterfaces vmName vmCfg;

      socketPath = "${cfg.socketDir}/${vmName}.sock";
      serialSocket = "${cfg.socketDir}/${vmName}.serial.sock";
      seedIso = "/etc/kcore/seeds/${vmName}.iso";
      firmwarePath =
        if cfg.firmwarePath != null then cfg.firmwarePath else "${pkgs.OVMF-cloud-hypervisor.firmware}";

      isLvm = vmCfg.storageBackend == "lvm";
      isZfs = vmCfg.storageBackend == "zfs";
      isCeph = vmCfg.storageBackend == "ceph";
      isBlockBackend = isLvm || isZfs || isCeph;

      lvName = "kcore-${vmName}";
      lvDevice = "/dev/${cfg.lvmVgName}/${lvName}";
      # Written only after qemu-img convert finishes. A timeout or crash leaves
      # the volume in place without this file, so the next start seeds it again
      # instead of booting a half-written Windows disk.
      seedMarker = "/var/lib/kcore/volume-seeded/${vmName}";

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
      # Block backends are always seeded as raw. For an in-place image the real
      # format is sniffed in the start script (see srcFormatFn).
      actualFormat = if isBlockBackend then "raw" else "$ROOT_FMT";

      vmDiskArg =
        "path=${actualDisk},image_type=${actualFormat}"
        + lib.optionalString isCeph ",direct=on,serial=root-${vmName}";
      seedDiskArg = "path=${seedIso},readonly=on,image_type=raw";
      dataDiskArgs = lib.concatMapStringsSep " " (
        disk:
        let
          ro = lib.optionalString disk.readonly ",readonly=on";
          path =
            if disk.encrypted then
              "/dev/mapper/${disk.mapperName}"
            else
              "/dev/rbd/${cfg.rbdPool}/${disk.rbdImage}";
        in
        "path=${path},image_type=raw,direct=on,serial=${disk.serial}${ro}"
      ) vmCfg.dataDisks;

      # The declared format comes from the image URL/extension, which is wrong for
      # qcow2 cloud images named *.img. A qcow2 magic header always wins over a
      # declared "raw", otherwise the qcow2 bytes would be copied onto the disk verbatim.
      srcFormatFn = ''
        src_format() {
          local PATH="${pkgs.coreutils}/bin:$PATH"
          if [ "$(head -c 4 "$1" 2>/dev/null | od -An -tx1 | tr -d ' \n')" = "514649fb" ]; then
            echo qcow2
          else
            echo "$2"
          fi
        }
      '';

      lvmProvisionScript = pkgs.writeShellScript "lvm-provision-${vmName}" ''
        set -e
        ${srcFormatFn}
        LV_DEVICE="${lvDevice}"
        VG="${cfg.lvmVgName}"
        LV="${lvName}"
        SIZE_BYTES="${toString vmCfg.storageSizeBytes}"
        SEED_MARKER="${seedMarker}"

        if [ ! -b "$LV_DEVICE" ]; then
          echo "Creating LV $VG/$LV (''${SIZE_BYTES} bytes)..."
          ${pkgs.lvm2.bin}/bin/lvcreate -y -L "''${SIZE_BYTES}B" -n "$LV" "$VG"
          rm -f "$SEED_MARKER"
        fi
        if [ ! -f "$SEED_MARKER" ]; then
          echo "Converting source image to LV..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f "$(src_format ${toString vmCfg.image} ${vmCfg.imageFormat})" -O raw \
            ${toString vmCfg.image} "$LV_DEVICE"
          mkdir -p "$(dirname "$SEED_MARKER")"
          touch "$SEED_MARKER"
          echo "LVM volume provisioned: $LV_DEVICE"
        else
          echo "LV $LV_DEVICE already seeded, skipping provision"
        fi
      '';

      zfsProvisionScript = pkgs.writeShellScript "zfs-provision-${vmName}" ''
        set -e
        ${srcFormatFn}
        ZVOL_DATASET="${zvolDataset}"
        ZVOL_DEVICE="${zvolDevice}"
        SIZE_BYTES="${toString vmCfg.storageSizeBytes}"
        SEED_MARKER="${seedMarker}"

        if ! ${pkgs.zfs}/bin/zfs list -H "$ZVOL_DATASET" >/dev/null 2>&1; then
          echo "Creating zvol $ZVOL_DATASET (''${SIZE_BYTES} bytes)..."
          ${pkgs.zfs}/bin/zfs create -V "''${SIZE_BYTES}" -o volmode=dev "$ZVOL_DATASET"
          for i in $(seq 1 30); do
            [ -b "$ZVOL_DEVICE" ] && break
            sleep 0.2
          done
          if [ ! -b "$ZVOL_DEVICE" ]; then
            echo "ERROR: zvol device $ZVOL_DEVICE did not appear after create"
            exit 1
          fi
          rm -f "$SEED_MARKER"
        fi
        if [ ! -f "$SEED_MARKER" ]; then
          echo "Converting source image to zvol..."
          ${pkgs.qemu-utils}/bin/qemu-img convert \
            -f "$(src_format ${toString vmCfg.image} ${vmCfg.imageFormat})" -O raw \
            ${toString vmCfg.image} "$ZVOL_DEVICE"
          mkdir -p "$(dirname "$SEED_MARKER")"
          touch "$SEED_MARKER"
          echo "ZFS volume provisioned: $ZVOL_DEVICE"
        else
          echo "zvol $ZVOL_DATASET already seeded, skipping provision"
        fi
      '';

      # Ceph 20 issues aes256k cephx keys, which the kernel libceph client
      # rejects ("secret too big 32"), so images are mapped with rbd-nbd and
      # exposed under the stable /dev/rbd/<pool>/<image> symlink. The nbd daemon
      # runs in its own transient scope so it outlives this unit's cgroup.
      rbdNbdFunctions = ''
        export PATH="${pkgs.ceph}/bin:${pkgs.jq}/bin:${pkgs.kmod}/bin:${pkgs.util-linux}/bin:$PATH"
        nbd_dev() {
          rbd device list -t nbd --format json 2>/dev/null \
            | jq -r --arg p "''${1%%/*}" --arg i "''${1#*/}" \
              '[.[] | select(.pool == $p and .image == $i and ((.snap // "") | . == "" or . == "-"))][0].device // empty'
        }
      '';

      cephMapScript = pkgs.writeShellScript "ceph-map-${vmName}" ''
        set -e
        ${srcFormatFn}
        ${rbdNbdFunctions}
        map_one() {
          local IMAGE="$1"
          local RBD_DEV="/dev/rbd/$IMAGE"
          if ! rbd info "$IMAGE" >/dev/null 2>&1; then
            echo "ERROR: RBD image $IMAGE does not exist; create/attach the volume via kctl first"
            exit 1
          fi
          local DEV
          DEV="$(nbd_dev "$IMAGE")"
          if [ -z "$DEV" ]; then
            modprobe nbd || true
            DEV="$(${pkgs.systemd}/bin/systemd-run --scope --quiet --collect -- \
              rbd device map -t nbd --rbd-cache=true --rbd-cache-policy=writeback \
                --rbd-readahead-trigger-requests=1 --rbd-readahead-max-bytes=4194304 \
                --rbd-readahead-disable-after-bytes=0 "$IMAGE" | tail -1)"
          fi
          mkdir -p "$(dirname "$RBD_DEV")"
          ln -sfn "$DEV" "$RBD_DEV"
          test -b "$RBD_DEV"
        }
        IMAGE="${cfg.rbdPool}/${rbdImage}"
        RBD_DEV="${rbdDevice}"
        SOURCE="${toString vmCfg.image}"
        # Controller/CephAdapter owns rbd create; this script only maps and
        # seeds the guest image once onto the root block device (like LVM/ZFS).
        map_one "$IMAGE"
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
            -f "$(src_format ${toString vmCfg.image} ${vmCfg.imageFormat})" -O raw \
            "$SOURCE" "$RBD_DEV"
          ${pkgs.ceph}/bin/rbd image-meta set "$IMAGE" kcore.seeded 1
          mkdir -p "$(dirname "$LOCAL_MARKER")"
          touch "$LOCAL_MARKER"
        fi
        ${lib.concatMapStrings (disk: ''
          map_one "${cfg.rbdPool}/${disk.rbdImage}"
        '') vmCfg.dataDisks}
      '';

      cephUnmapScript = pkgs.writeShellScript "ceph-unmap-${vmName}" ''
        ${rbdNbdFunctions}
        # A live-migrated guest runs outside this unit. If the unit's own
        # start fails (and ExecStopPost runs), the device is still in use by
        # that guest, and unmapping it would pull the disk from under it.
        held_by_vmm() {
          local fd pid
          for fd in /proc/[0-9]*/fd/*; do
            [ "$(readlink "$fd" 2>/dev/null)" = "$1" ] || continue
            pid="''${fd#/proc/}"
            pid="''${pid%%/*}"
            [ "$(cat "/proc/$pid/comm" 2>/dev/null)" = "rbd-nbd" ] && continue
            return 0
          done
          return 1
        }
        unmap_one() {
          local DEV
          DEV="$(nbd_dev "$1")"
          if [ -n "$DEV" ] && held_by_vmm "$DEV"; then
            return 0
          fi
          # An open LUKS mapper stacked on the nbd keeps it mapped; the node
          # agent unmaps it when the volume is locked on detach.
          if [ -n "$DEV" ] && [ -n "$(ls -A "/sys/block/$(basename "$DEV")/holders" 2>/dev/null)" ]; then
            return 0
          fi
          if [ -n "$DEV" ]; then
            blockdev --flushbufs "$DEV" || true
            rbd device unmap -t nbd "$DEV" || true
          fi
          rm -f "/dev/rbd/$1"
        }
        unmap_one "${cfg.rbdPool}/${rbdImage}"
        ${lib.concatMapStrings (disk: ''
          unmap_one "${cfg.rbdPool}/${disk.rbdImage}"
        '') vmCfg.dataDisks}
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
          # Windows needs Hyper-V enlightenments. Linux guests tolerate them, so every VM gets them.
          "--cpus boot=${toString vmCfg.cores},kvm_hyperv=on"
          memoryArg
          "--firmware ${firmwarePath}"
          "--serial socket=${serialSocket}"
          "--disk ${vmDiskArg} ${seedDiskArg}${lib.optionalString (dataDiskArgs != "") " ${dataDiskArgs}"}"
        ]
        ++ map (dev: "--device path=/sys/bus/pci/devices/${dev.address},iommu=on") normalizedPci
        ++ map (iface: "--net tap=${iface.tap},mac=${iface.macAddress}") ifaces
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

      # A guest that is already running for this VM outside the unit (a
      # live-migrated VMM the unit failed to adopt, or one left behind when the
      # unit crashed) owns the sockets and the disk. Starting again would wipe
      # its sockets, reseed its volume and launch a second writer, and the
      # unit's failure path would then unmap the device under the live guest.
      # Refuse with a status systemd will not retry.
      refuseIfVmmRunning = ''
        other_vmm="$(${pkgs.procps}/bin/pgrep -f -- "cloud-hypervisor.*--api-socket[= ]${socketPath}( |$)" | ${pkgs.gnugrep}/bin/grep -vx "$$" | ${pkgs.coreutils}/bin/head -n1 || true)"
        if [ -n "$other_vmm" ]; then
          echo "ERROR: a cloud-hypervisor (pid $other_vmm) already serves ${socketPath}; refusing to start a second instance for ${vmName}. Adopt it (live-migrated marker) or stop it explicitly."
          exit ${toString vmmAlreadyRunningStatus}
        fi
      '';
      vmmAlreadyRunningStatus = 75;

      # After a live receive, CH is already running outside systemd. Skip
      # destructive socket cleanup so the handoff ExecStart can adopt it.
      startPreScript = pkgs.writeShellScript "kcore-vm-${vmName}-pre" ''
        set -e
        if [ -f "${liveMigratedMarker}" ]; then
          echo "live-migrated marker present; skipping socket wipe / cold provision"
          exit 0
        fi
        ${refuseIfVmmRunning}
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
        ${refuseIfVmmRunning}
        ${srcFormatFn}
        ROOT_FMT="$(src_format ${toString vmCfg.image} ${vmCfg.imageFormat})"
        exec ${chBin} ${chArgs}
      '';
    in
    {
      description = "kcore VM ${vmName}";
      requires = map (iface: "${iface.unit}.service") ifaces;
      after = map (iface: "${iface.unit}.service") ifaces;
      wantedBy = lib.optionals (vmCfg.autoStart && !vmCfg.incomingMigration) [ "multi-user.target" ];
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
        ExecStopPost = lib.optionalString isCeph "-${cephUnmapScript}";
        TimeoutStopSec = 30;
        # ExecStartPre copies the image onto the volume. The 90s default is
        # shorter than a Windows qcow2 convert, which then fails the unit.
        TimeoutStartSec = "15min";
        # Must not depend on incomingMigration: the config that flips a VM from
        # incoming to settled would otherwise change this unit, and the switch
        # would restart the unit that just adopted the live-migrated guest.
        Restart = if vmCfg.autoStart then "always" else "no";
        RestartSec = 5;
        # "another VMM already owns this VM" is not transient; retrying would
        # keep running the failure path (ExecStopPost) next to a live guest.
        RestartPreventExitStatus = toString vmmAlreadyRunningStatus;

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

    # The node agent spawns the receiving VMM for a live migration itself, so
    # it must run the exact binary the units use (patched, same build), or the
    # unit later adopts a process it could not have started.
    environment.etc."kcore/cloud-hypervisor".text = chBin + "\n";
    environment.etc."kcore/vm-socket-dir".text = cfg.socketDir + "\n";

    systemd.services = lib.mapAttrs' (
      vmName: vmCfg: lib.nameValuePair "kcore-vm-${vmName}" (mkVmService vmName vmCfg)
    ) cfg.virtualMachines;
  };
}
