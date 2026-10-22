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

      # LVM and ZFS guests open a stable device-mapper name so a live migration
      # can reload the table (linear → mirror) without Cloud Hypervisor seeing
      # a path that only exists on one node. `direct=on` keeps guest writes
      # synchronous, which is what makes the mirror a complete copy.
      vmDiskArg =
        if isLvm || isZfs then
          "path=/dev/mapper/kcore-vm-${vmName},image_type=raw,direct=on,serial=root-${vmName}"
        else
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

      # Runs right before every qemu-img convert onto a volume. The node agent
      # hashes the download once; this catches a cached file that changed
      # since (bit rot, a partial re-download, an operator swapping files).
      verifySourceFn = ''
        verify_source() {
          ${
            if vmCfg.imageSha256 != null then
              ''
                local expected actual
                expected="$(echo "${vmCfg.imageSha256}" | ${pkgs.coreutils}/bin/tr 'A-F' 'a-f')"
                echo "Verifying $1 against sha256 $expected..."
                actual="$(${pkgs.coreutils}/bin/sha256sum "$1" | ${pkgs.coreutils}/bin/cut -d' ' -f1)"
                if [ "$actual" != "$expected" ]; then
                  echo "ERROR: source image $1 has sha256 $actual, expected $expected; refusing to seed ${vmName} from it"
                  exit 1
                fi
              ''
            else
              "true"
          }
        }
      '';

      lvmProvisionScript = pkgs.writeShellScript "lvm-provision-${vmName}" ''
        set -e
        ${srcFormatFn}
        ${verifySourceFn}
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
          verify_source ${toString vmCfg.image}
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
        ${verifySourceFn}
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
        REPLICA_MARKER="/var/lib/kcore/replica-ready/${vmName}"
        if [ -f "$REPLICA_MARKER" ]; then
          SNAP="$(${pkgs.coreutils}/bin/tr -d '[:space:]' < "$REPLICA_MARKER")"
          if [ -n "$SNAP" ] && ${pkgs.zfs}/bin/zfs list -H -t snapshot "$ZVOL_DATASET@$SNAP" >/dev/null 2>&1; then
            echo "Rolling $ZVOL_DATASET back to replica snapshot $SNAP"
            ${pkgs.zfs}/bin/zfs rollback -r "$ZVOL_DATASET@$SNAP"
          fi
          mkdir -p "$(dirname "$SEED_MARKER")"
          touch "$SEED_MARKER"
          rm -f "$REPLICA_MARKER"
        fi
        if [ ! -f "$SEED_MARKER" ]; then
          verify_source ${toString vmCfg.image}
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
        ${verifySourceFn}
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
          # A device left from an earlier start can still be listed after its
          # Ceph session died. Reusing it boots the guest on a disk with no
          # watcher, so a failover looks successful while the guest cannot write.
          if [ -n "$DEV" ]; then
            local watchers
            watchers="$(rbd status "$IMAGE" --format json 2>/dev/null | jq -r '(.watchers // []) | length' || true)"
            if [ "$watchers" = "0" ]; then
              echo "RBD $IMAGE is mapped at $DEV with no watcher; mapping it again"
              rbd device unmap -t nbd "$DEV" || true
              DEV=""
            fi
          fi
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
        # Vault boot tests restore the guest into this RBD before the VM
        # exists. The sentinel image path means the bytes are already there.
        if [ "$SOURCE" = "/var/lib/kcore/vault-restore" ]; then
          SEEDED=1
        fi
        if [ "$SEEDED" -eq 0 ]; then
          test -e "$SOURCE" || { echo "missing source image: $SOURCE"; exit 1; }
          verify_source "$SOURCE"
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
      vfioDevices = builtins.filter (dev: dev.vgpuProfile == null) normalizedPci;
      vgpuDevices = builtins.filter (dev: dev.vgpuProfile != null) normalizedPci;
      sysfs = config.ch-vm.vms.sysfsRoot;

      # Live migration requires MAP_SHARED guest RAM (`shared=on`). Every
      # block backend can move: Ceph because the disk is shared, LVM and ZFS
      # because the disk is mirrored onto the destination first.
      memoryArg =
        if isBlockBackend then
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
        requested="${lib.concatStringsSep " " (map (dev: dev.address) vfioDevices)}"
        sys="${sysfs}"
        for addr in $requested; do
          dev="$sys/bus/pci/devices/$addr"
          if [ ! -e "$dev" ]; then
            echo "PCI device $addr does not exist on this node"
            exit 1
          fi
          if [ -e "$dev/physfn" ]; then
            echo "PCI device $addr is a virtual function; a vGPU slice is not bound to vfio-pci"
            exit 1
          fi
          if [ -e "$dev/sriov_numvfs" ] && [ "$(cat "$dev/sriov_numvfs")" -gt 0 ] && [ -e "$dev/driver" ]; then
            drv=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$dev/driver")")
            if [ "$drv" = "nvidia" ]; then
              echo "PCI device $addr is a vGPU host card; request a slice with --vgpu"
              exit 1
            fi
          fi
          if [ -f /etc/kcore/vgpu-hosts ] && ${pkgs.gnugrep}/bin/grep -qx "$addr" /etc/kcore/vgpu-hosts; then
            echo "PCI device $addr is listed in /etc/kcore/vgpu-hosts; request a slice with --vgpu"
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
            echo "$addr" > "$sys/bus/pci/drivers_probe"
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

      vgpuBindScript = pkgs.writeShellScript "vgpu-bind-${vmName}" ''
        set -euo pipefail
        if [ -f "${liveMigratedMarker}" ]; then
          echo "live-migrated marker present; not programming vGPU profiles"
          exit 0
        fi
        sys="${sysfs}"
        ${lib.concatMapStrings (dev: ''
          addr=${lib.toLower dev.address}
          dev="$sys/bus/pci/devices/$addr"
          if [ ! -e "$dev/physfn" ]; then
            echo "vGPU slice $addr is not a virtual function"
            exit 1
          fi
          parent=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$dev/physfn")")
          parent_drv=""
          if [ -e "$sys/bus/pci/devices/$parent/driver" ]; then
            parent_drv=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$sys/bus/pci/devices/$parent/driver")")
          fi
          if [ "$parent_drv" != "nvidia" ]; then
            echo "vGPU slice $addr parent $parent driver is ''${parent_drv:-unbound}, not nvidia"
            exit 1
          fi
          current=""
          if [ -e "$dev/driver" ]; then
            current=$(${pkgs.coreutils}/bin/basename "$(${pkgs.coreutils}/bin/readlink "$dev/driver")")
          fi
          if [ "$current" = "vfio-pci" ]; then
            echo "vGPU slice $addr is bound to vfio-pci; a vGPU slice stays on nvidia-vgpu-vfio"
            exit 1
          fi
          if [ "$current" != "nvidia-vgpu-vfio" ]; then
            echo "vGPU slice $addr driver is ''${current:-unbound}; a vGPU slice must use nvidia-vgpu-vfio"
            exit 1
          fi
          types="$dev/nvidia/creatable_vgpu_types"
          current_file="$dev/nvidia/current_vgpu_type"
          id=$(${pkgs.gnugrep}/bin/grep -E '^[[:space:]]*[0-9]+[[:space:]]*:[[:space:]]*NVIDIA[[:space:]]+${dev.vgpuProfile}[[:space:]]*$' "$types" | ${pkgs.gawk}/bin/awk '{print $1}' | ${pkgs.coreutils}/bin/head -n1)
          if [ -z "$id" ]; then
            echo "vGPU slice $addr cannot take profile ${dev.vgpuProfile}"
            exit 1
          fi
          now=$(${pkgs.coreutils}/bin/cat "$current_file")
          if [ "$now" = "$id" ]; then
            :
          elif [ "$now" != "0" ]; then
            echo "vGPU slice $addr is already profile id $now, not ${dev.vgpuProfile} ($id)"
            exit 1
          else
            echo "$id" > "$current_file"
          fi
        '') vgpuDevices}
      '';

      vgpuClearScript = pkgs.writeShellScript "vgpu-clear-${vmName}" ''
        set -eu
        if [ -f "${liveMigratedMarker}" ]; then
          exit 0
        fi
        sys="${sysfs}"
        ${lib.concatMapStrings (dev: ''
          echo 0 > "$sys/bus/pci/devices/${lib.toLower dev.address}/nvidia/current_vgpu_type" || true
        '') vgpuDevices}
      '';

      # A VMM for this VM may already run outside the unit's cgroup: the
      # receiver of a live migration (spawned by the node agent), or one the
      # unit adopted earlier and lost when systemd restarted the unit because
      # its file changed. Such a process owns the sockets and the disk, so a
      # start must never wipe its sockets, reseed its volume or launch a second
      # writer next to it.
      vmmFunctions = ''
        vmm_pid() {
          ${pkgs.procps}/bin/pgrep -f -- "cloud-hypervisor.*--api-socket[= ]${socketPath}( |$)" \
            | ${pkgs.gnugrep}/bin/grep -vx "$$" | ${pkgs.coreutils}/bin/head -n1 || true
        }
        vm_state() {
          ${pkgs.curl}/bin/curl -s --max-time 5 --unix-socket ${socketPath} http://localhost/api/v1/vm.info 2>/dev/null \
            | ${pkgs.gnused}/bin/sed -n 's/.*"state":"\([A-Za-z]*\)".*/\1/p'
        }
      '';
      # Exit status of ExecStart when another VMM serves the socket but has no
      # guest to adopt. It is the main process' status, so
      # RestartPreventExitStatus stops systemd from retrying into the same wall.
      vmmAlreadyRunningStatus = 75;

      # After a live receive, CH is already running outside systemd. Skip
      # destructive socket cleanup so the handoff ExecStart can adopt it.
      # `/dev/mapper/kcore-vm-<name>` is the path Cloud Hypervisor records. It is
      # a linear map over the LV or zvol until a migration reloads it. Creating
      # it is idempotent: a live-migrated guest, or one already running, is
      # left alone.
      localMapperScript = pkgs.writeShellScript "local-mapper-${vmName}" ''
        set -euo pipefail
        if [ -f "${liveMigratedMarker}" ]; then
          exit 0
        fi
        name="kcore-vm-${vmName}"
        mapper="/dev/mapper/$name"
        backing="${if isLvm then lvDevice else zvolDevice}"
        if [ -b "$mapper" ]; then
          exit 0
        fi
        ${pkgs.kmod}/bin/modprobe dm-mod || true
        sectors="$(${pkgs.util-linux}/bin/blockdev --getsz "$backing")"
        printf '0 %s linear %s 0\n' "$sectors" "$backing" | ${pkgs.lvm2.bin}/bin/dmsetup create "$name"
        test -b "$mapper"
      '';

      localMapperRemoveScript = pkgs.writeShellScript "local-mapper-remove-${vmName}" ''
        name="kcore-vm-${vmName}"
        mapper="/dev/mapper/$name"
        if [ ! -b "$mapper" ]; then
          exit 0
        fi
        open="$(${pkgs.lvm2.bin}/bin/dmsetup info -c --noheadings -o open "$name" 2>/dev/null || echo 0)"
        if [ "$open" != "0" ]; then
          exit 0
        fi
        ${pkgs.lvm2.bin}/bin/dmsetup remove "$name" || true
      '';

      startPreScript = pkgs.writeShellScript "kcore-vm-${vmName}-pre" ''
        set -e
        if [ -f "${liveMigratedMarker}" ]; then
          echo "live-migrated marker present; skipping socket wipe / cold provision"
          exit 0
        fi
        ${vmmFunctions}
        other_vmm="$(vmm_pid)"
        if [ -n "$other_vmm" ]; then
          echo "cloud-hypervisor pid $other_vmm already serves ${socketPath}; leaving its sockets and volume alone"
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

      # Adopt a running CH (tail --pid) or cold-start one. Adoption covers the
      # live-migration handoff (marker + pid file) and every later restart of
      # the unit while that CH still runs the guest; only a VMM without a
      # running guest (a receiver whose migration never completed) is refused.
      startScript = pkgs.writeShellScript "kcore-vm-${vmName}-start" ''
        set -e
        ${vmmFunctions}
        adopt() {
          echo "$2 cloud-hypervisor pid $1"
          # The node-agent spawns this hypervisor, so the process is still in
          # the agent's cgroup. Restarting that unit kills every process there,
          # the tail below exits, and systemd cold-boots the guest. Move the
          # hypervisor into this unit's cgroup before waiting on it.
          cg="$(${pkgs.gnused}/bin/sed -n 's/^0:://p' /proc/self/cgroup | ${pkgs.coreutils}/bin/head -n1)"
          if [ -n "$cg" ]; then
            echo "$1" > "/sys/fs/cgroup$cg/cgroup.procs" \
              || echo "warning: could not move cloud-hypervisor pid $1 into $cg"
          fi
          exec ${pkgs.coreutils}/bin/tail --pid="$1" -f /dev/null
        }
        if [ -f "${liveMigratedMarker}" ]; then
          pid="$(${pkgs.coreutils}/bin/cat "${migratePidFile}" 2>/dev/null || true)"
          ${pkgs.coreutils}/bin/rm -f "${liveMigratedMarker}" "${migratePidFile}"
          if [ -n "$pid" ] && ${pkgs.coreutils}/bin/kill -0 "$pid" 2>/dev/null; then
            adopt "$pid" "Adopting live-migrated"
          fi
          echo "live-migrated marker present but pid '$pid' is not running; checking the API socket"
        fi
        other_vmm="$(vmm_pid)"
        if [ -n "$other_vmm" ]; then
          state="$(vm_state)"
          case "$state" in
            Running | Paused)
              adopt "$other_vmm" "Adopting already-running (guest $state)"
              ;;
            *)
              echo "ERROR: cloud-hypervisor pid $other_vmm serves ${socketPath} without a running guest (state '$state'); refusing to start a second instance for ${vmName}. Finish or reset the migration, or stop that process explicitly."
              exit ${toString vmmAlreadyRunningStatus}
              ;;
          esac
        fi
        ${srcFormatFn}
        ROOT_FMT="$(src_format ${toString vmCfg.image} ${vmCfg.imageFormat})"
        exec ${chBin} ${chArgs}
      '';

      # Press the ACPI power button, then wait for the VMM to exit. Adoption
      # moves the hypervisor into this unit's cgroup; if that move failed it is
      # still outside, and systemd would report the unit stopped while the
      # guest kept running. Wait here so a restart does not adopt the old VMM
      # again instead of applying whatever the restart was meant to apply.
      stopGraceSec = 25;
      stopScript = pkgs.writeShellScript "kcore-vm-${vmName}-stop" ''
        ${vmmFunctions}
        pid="$(vmm_pid)"
        ${pkgs.curl}/bin/curl -s --max-time 5 --unix-socket ${socketPath} -X PUT http://localhost/api/v1/vm.power-button || true
        [ -n "$pid" ] || exit 0
        for _ in $(${pkgs.coreutils}/bin/seq 1 ${toString stopGraceSec}); do
          ${pkgs.coreutils}/bin/kill -0 "$pid" 2>/dev/null || exit 0
          ${pkgs.coreutils}/bin/sleep 1
        done
        echo "guest ${vmName} did not power off within ${toString stopGraceSec}s; terminating cloud-hypervisor pid $pid"
        ${pkgs.coreutils}/bin/kill -TERM "$pid" 2>/dev/null || true
        for _ in 1 2 3; do
          ${pkgs.coreutils}/bin/kill -0 "$pid" 2>/dev/null || exit 0
          ${pkgs.coreutils}/bin/sleep 1
        done
        exit 0
      '';
    in
    {
      description = "kcore VM ${vmName}";
      requires =
        map (iface: "${iface.unit}.service") ifaces
        ++ lib.optional (vgpuDevices != [ ] && config.ch-vm.vgpu.enable) "kcore-vgpu-sriov.service"
        ++ [ "kcore-placement-confirmed.target" ];
      after =
        map (iface: "${iface.unit}.service") ifaces
        ++ lib.optional (vgpuDevices != [ ] && config.ch-vm.vgpu.enable) "kcore-vgpu-sriov.service"
        ++ [ "kcore-placement-confirmed.target" ];
      wantedBy = lib.optionals (vmCfg.autoStart && !vmCfg.incomingMigration) [ "multi-user.target" ];
      # stopIfChanged/restartIfChanged keep their NixOS defaults so VM spec
      # updates (cpu/memory/extraArgs) take effect for Ceph-backed VMs too. The
      # destination unit of a live migration already exists (it was pushed
      # with incomingMigration = true before the receive), so any difference
      # between that file and the settled one restarts it right after the
      # handoff: the start script then re-adopts the running VMM instead of
      # starting a second one, and the stop script sees the guest through to
      # power-off when a restart really is meant to apply a change.

      serviceConfig = {
        Type = "simple";
        ExecStartPre =
          lib.optionals (vfioDevices != [ ]) [ "${vfioBindScript}" ]
          ++ lib.optionals (vgpuDevices != [ ]) [ "${vgpuBindScript}" ]
          ++ [ "${startPreScript}" ]
          ++ lib.optionals (isLvm || isZfs) [ "${localMapperScript}" ];
        ExecStart = "${startScript}";
        ExecStop = "${stopScript}";
        ExecStopPost =
          lib.optional (vgpuDevices != [ ]) "-${vgpuClearScript}"
          ++ lib.optional isCeph "-${cephUnmapScript}"
          ++ lib.optional (isLvm || isZfs) "-${localMapperRemoveScript}";
        TimeoutStopSec = 30;
        # ExecStartPre copies the image onto the volume. The 90s default is
        # shorter than a Windows qcow2 convert, which then fails the unit.
        TimeoutStartSec = "15min";
        # Must not depend on incomingMigration: the config that flips a VM from
        # incoming to settled would otherwise change this unit, and the switch
        # would restart the unit that just adopted the live-migrated guest.
        Restart = if vmCfg.autoStart then "always" else "no";
        RestartSec = 5;
        # "another VMM owns this VM's socket and has nothing to adopt" is not
        # transient; retrying would keep running the failure path
        # (ExecStopPost) next to that process every few seconds.
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
    # name<space>backend, one VM per line. The node agent masks every Ceph
    # unit the controller no longer places here before it starts
    # kcore-placement-confirmed.target, which these units require.
    environment.etc."kcore/vm-placement".text = lib.concatStrings (
      lib.mapAttrsToList (name: vm: "${name} ${vm.storageBackend}\n") cfg.virtualMachines
    );
    # Options the controller may only emit once this module version is
    # installed on the node (an unknown option fails the whole evaluation).
    # Append here when adding a per-VM option the controller renders.
    environment.etc."kcore/ch-vm-features".text = ''
      imageSha256
      cloudHypervisorPatches
      vgpuProfile
      localLiveMigrate
      cloudInitGuestOpsKeys
    '';

    # qemu-nbd and nbd-client replicate an LV/zvol during migration. dmsetup
    # and lvcreate come from lvm2; zfs is already on the system when a guest
    # uses it. The node agent finds them on PATH.
    environment.systemPackages = [
      pkgs.qemu-utils
      pkgs.nbd
      pkgs.lvm2
    ]
    ++ lib.optional anyVmUsesZfs pkgs.zfs;

    systemd.services = lib.mapAttrs' (
      vmName: vmCfg: lib.nameValuePair "kcore-vm-${vmName}" (mkVmService vmName vmCfg)
    ) cfg.virtualMachines;

    # Not wanted by multi-user. The node agent starts it after the controller
    # lists the VMs that still belong here, so a host that reboots cannot
    # start a Ceph guest the cluster has already moved.
    systemd.targets.kcore-placement-confirmed = {
      description = "Controller confirmed which VMs belong on this host";
    };
  };
}
