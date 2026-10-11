{ pkgs, ... }:
let
  testImage = pkgs.runCommand "test-disk.raw" { } ''
    ${pkgs.qemu}/bin/qemu-img create -f raw "$out" 64M
  '';
in
pkgs.testers.runNixOSTest {
  name = "ch-vm-basic";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../modules/ch-vm ];

      ch-vm.vms = {
        enable = true;
        cloudHypervisorPackage = pkgs.cloud-hypervisor;
        gatewayInterface = "eth0";

        networks.default = {
          externalIP = "10.0.2.15";
          gatewayIP = "192.168.100.1";
        };

        virtualMachines."this-name-is-way-too-long" = {
          image = testImage;
          cores = 1;
          memorySize = 256;
          network = "default";
          autoStart = false;
        };

        virtualMachines.gpu-guest = {
          image = testImage;
          cores = 1;
          memorySize = 256;
          network = "default";
          autoStart = false;
          pciDevices = [
            { address = "0000:03:00.0"; }
            { address = "0000:03:00.1"; }
          ];
        };

        # Seed-time digest check: the LV is only written when the cached
        # image still hashes to what the operator pinned.
        lvmVgName = "vg_test";
        virtualMachines.lvm-badsha = {
          image = testImage;
          imageSha256 = "0000000000000000000000000000000000000000000000000000000000000000";
          cores = 1;
          memorySize = 256;
          network = "default";
          storageBackend = "lvm";
          storageSizeBytes = 67108864;
          autoStart = false;
        };
        virtualMachines.lvm-goodsha = {
          image = testImage;
          imageSha256 = builtins.hashFile "sha256" testImage;
          cores = 1;
          memorySize = 256;
          network = "default";
          storageBackend = "lvm";
          storageSizeBytes = 67108864;
          autoStart = false;
        };
      };

      virtualisation.memorySize = 2048;
      virtualisation.emptyDiskImages = [ 512 ];
    };

  testScript = ''
    machine.wait_for_unit("kcore-bridge-default.service")
    machine.wait_for_unit("kcore-dhcp-default.service")
    machine.succeed("ip link show kbr-default")

    status = machine.get_unit_info("kcore-vm-this-name-is-way-too-long.service")
    assert status["ActiveState"] != "active", "VM should not auto-start"

    machine.succeed("systemctl start kcore-tap-this-name-is-way-too-long.service")
    machine.wait_for_unit("kcore-tap-this-name-is-way-too-long.service")
    machine.succeed("test $(ip -o link show | awk -F': ' '/tap-[0-9a-f]{8}/ {print $2; exit}' | wc -c) -le 16")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -E 'tap-[0-9a-f]{8}'")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F -- '--firmware /nix/store/'")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F 'CLOUDHV.fd'")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F 'image_type=raw'")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F 'kvm_hyperv=on'")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F 'TimeoutStartSec=15min'")

    machine.succeed("test -d /run/kcore")
    machine.succeed("test -f /etc/kcore/seeds/this-name-is-way-too-long.iso")

    machine.succeed("systemctl cat kcore-vm-gpu-guest.service | grep -F -- '--device path=/sys/bus/pci/devices/0000:03:00.0,iommu=on'")
    machine.succeed("systemctl cat kcore-vm-gpu-guest.service | grep -F -- '--device path=/sys/bus/pci/devices/0000:03:00.1,iommu=on'")
    machine.succeed("grep -q iommu=pt /proc/cmdline")
    machine.succeed("systemctl cat kcore-vm-gpu-guest.service | grep -F vfio-bind-gpu-guest")

    # The node agent spawns the live-migration receiver from this file; it
    # must be the same binary the units exec.
    ch_bin = machine.succeed("cat /etc/kcore/cloud-hypervisor").strip()
    machine.succeed(f"test -x {ch_bin}")
    machine.succeed(f"grep -F -- '{ch_bin}' $(systemctl show -p ExecStart --value kcore-vm-gpu-guest.service | sed -n 's/.*path=\\([^ ;]*\\).*/\\1/p')")
    machine.succeed("test \"$(cat /etc/kcore/vm-socket-dir)\" = /run/kcore")

    # A pinned digest that does not match the cached image must stop the seed
    # before anything is written to the volume.
    machine.succeed("pvcreate /dev/vdb && vgcreate vg_test /dev/vdb")
    machine.fail("systemctl start kcore-vm-lvm-badsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-badsha.service | grep -F 'refusing to seed lvm-badsha'")
    machine.fail("test -e /var/lib/kcore/volume-seeded/lvm-badsha")
    # The matching digest seeds; ExecStart then fails only because the test
    # VM has no KVM, which is after the part under test.
    machine.execute("systemctl start kcore-vm-lvm-goodsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-goodsha.service | grep -F 'Verifying'")
    machine.succeed("test -e /var/lib/kcore/volume-seeded/lvm-goodsha")

    # A VMM already serving this VM's API socket makes a start refuse with a
    # status systemd does not retry, instead of wiping sockets and reseeding.
    machine.succeed("systemctl cat kcore-vm-lvm-goodsha.service | grep -F 'RestartPreventExitStatus=75'")
    # argv carries the same `cloud-hypervisor ... --api-socket <path>` shape
    # the units and the node agent use; the process itself just sleeps.
    machine.succeed("systemd-run --unit fake-vmm --quiet -- sh -c 'sleep 600' cloud-hypervisor --api-socket /run/kcore/lvm-goodsha.sock")
    machine.succeed("pgrep -f -- 'cloud-hypervisor.*--api-socket /run/kcore/lvm-goodsha.sock'")
    machine.fail("systemctl start kcore-vm-lvm-goodsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-goodsha.service | grep -F 'refusing to start a second instance for lvm-goodsha'")
    machine.succeed("systemctl show -p ExecStartPre kcore-vm-lvm-goodsha.service | grep -q 'status=75'")
    machine.succeed("systemctl stop fake-vmm.service")
  '';
}
