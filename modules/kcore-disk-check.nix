{ diskVerify }:
{ pkgs, lib, ... }:

let
  # Driven entirely by the kernel command line, so the provisioning server can
  # request a check per machine through SetBootTarget boot_params:
  #
  #   kcore.disk_check=write|verify   required; the unit is skipped without it
  #   kcore.disk_nonce=RUN            required; same value for write and verify
  #   kcore.disk_devices=A,B          optional; /dev/... or by-serial:SERIAL
  #                                   (default: every non-removable disk)
  #   kcore.disk_report=http://...    optional; each JSON report is POSTed here
  #
  # The unit never powers the machine off. The caller does the power cycle
  # (BMC ForceOff, then On) between the write and the verify phase, because an
  # OS shutdown lets the drive flush and so proves nothing about power loss.
  diskCheck = pkgs.writeShellApplication {
    name = "kcore-disk-check";
    runtimeInputs = [
      diskVerify
      pkgs.coreutils
      pkgs.curl
      pkgs.gawk
      pkgs.util-linux
    ];
    text = ''
      cmdline=$(cat /proc/cmdline)
      param() {
        local w
        for w in $cmdline; do
          case "$w" in
            "kcore.$1="*) printf '%s' "''${w#*=}"; return 0 ;;
          esac
        done
        return 0
      }

      phase=$(param disk_check)
      nonce=$(param disk_nonce)
      devices=$(param disk_devices)
      report=$(param disk_report)
      machine_id=$(param machine_id)

      case "$phase" in
        write | verify) ;;
        *) echo "kcore.disk_check must be write or verify, got '$phase'" >&2; exit 1 ;;
      esac
      if [ -z "$nonce" ]; then
        echo "kcore.disk_nonce is required" >&2
        exit 1
      fi

      if [ -z "$devices" ]; then
        # Skip removable media (iDRAC virtual media, USB sticks) and RAM disks.
        devices=$(lsblk -dnpo NAME,TYPE,RM,SIZE -b \
          | awk '$2 == "disk" && $3 == 0 && $4 > 0 && $1 !~ /\/(zram|ram|nbd|loop)/ { print $1 }' \
          | paste -sd, -)
      fi
      if [ -z "$devices" ]; then
        echo "no disks to check" >&2
        exit 1
      fi

      outdir=/run/kcore/disk-check
      mkdir -p "$outdir"

      failures=0
      n=0
      IFS=','
      for dev in $devices; do
        n=$((n + 1))
        out="$outdir/$phase-$n.json"
        args=("$phase" --device "$dev" --nonce "$nonce")
        [ -n "$machine_id" ] && args+=(--machine-id "$machine_id")
        [ "$phase" = write ] && args+=(--yes)

        rc=0
        kcore-disk-verify "''${args[@]}" >"$out" || rc=$?
        if [ -s "$out" ]; then
          echo "KCORE-DISK-CHECK rc=$rc $(tr -d '\n' <"$out")"
          if [ -n "$report" ]; then
            curl -fsS --max-time 20 -H 'Content-Type: application/json' \
              --data-binary @"$out" "$report" >/dev/null \
              || echo "could not POST report for $dev to $report" >&2
          fi
        else
          echo "KCORE-DISK-CHECK rc=$rc device=$dev no report produced" >&2
        fi
        [ "$rc" -eq 0 ] || failures=$((failures + 1))
      done

      echo "KCORE-DISK-CHECK-DONE phase=$phase nonce=$nonce devices=$n failures=$failures"
      if [ -n "$report" ]; then
        # Marks the end of the run so the caller knows no more reports are coming.
        printf '{"done":true,"phase":"%s","nonce":"%s","machine_id":"%s","devices":%d,"failures":%d}\n' \
          "$phase" "$nonce" "$machine_id" "$n" "$failures" \
          | curl -fsS --max-time 20 -H 'Content-Type: application/json' \
              --data-binary @- "$report" >/dev/null \
          || echo "could not POST completion marker to $report" >&2
      fi
      [ "$failures" -eq 0 ]
    '';
  };
in
{
  environment.systemPackages = [
    diskVerify
    diskCheck
  ];

  systemd.services.kcore-disk-check = {
    description = "kcore commissioning disk write-verify check";
    wantedBy = [ "multi-user.target" ];
    after = [
      "network-online.target"
      "systemd-udev-settle.service"
    ];
    wants = [ "network-online.target" ];
    unitConfig.ConditionKernelCommandLine = "kcore.disk_check";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = lib.getExe diskCheck;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
      TimeoutStartSec = "2h";
    };
  };
}
