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
          # Every rule shape the firewall unit can render goes through a real
          # `nft -f` here, so a syntax slip fails the test instead of a node.
          eastWestFirewall = true;
          allowedTCPPorts = [ 2222 ];
          allowedUDPPorts = [ 5353 ];
          securityGroupRules = [
            {
              protocol = "tcp";
              hostPort = 8080;
              targetPort = 80;
              sourceCidr = "10.0.0.0/8";
              targetIp = "192.168.100.20";
              enableDnat = true;
            }
            {
              protocol = "udp";
              hostPort = 514;
              targetPort = 514;
            }
          ];
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

    # The kcore tables are owned by their own unit, applied as one atomic
    # transaction, and come back after a reload or restart even when something
    # flushed them from under the bridge.
    machine.wait_for_unit("kcore-firewall-default.service")
    machine.succeed("nft list table ip kcore-default | grep -F 'oifname \"eth0\" masquerade'")
    machine.succeed("nft list table ip kcore-default | grep -F 'tcp dport 2222 dnat to 192.168.100.1'")
    machine.succeed("nft list table ip kcore-default | grep -F 'udp dport 5353 dnat to 192.168.100.1'")
    machine.succeed("nft list table ip kcore-default | grep -F 'dnat to 192.168.100.20:80'")
    machine.succeed("nft list table ip kcore-default | grep -F 'udp dport 514 ip saddr 0.0.0.0/0 accept'")
    machine.succeed("nft list table bridge kcoreew-default | grep -F 'ip saddr 192.168.100.0/24 ip daddr 192.168.100.0/24 drop'")
    machine.succeed("nft list table bridge kcoreew-default | grep -F 'ip daddr 192.168.100.20 tcp dport 80 accept'")
    machine.succeed("nft delete table ip kcore-default && nft delete table bridge kcoreew-default")
    machine.fail("nft list table ip kcore-default")
    machine.succeed("systemctl reload kcore-firewall-default.service")
    machine.succeed("nft list table ip kcore-default | grep -F masquerade")
    machine.succeed("nft list table bridge kcoreew-default | grep -F drop")
    # A reload over existing tables replaces them instead of appending
    # duplicates.
    machine.succeed("systemctl reload kcore-firewall-default.service")
    machine.succeed("test $(nft list table ip kcore-default | grep -c masquerade) -eq 1")
    machine.succeed("systemctl restart kcore-firewall-default.service")
    machine.succeed("test $(nft list table ip kcore-default | grep -c masquerade) -eq 1")
    machine.succeed("ip link show kbr-default")

    status = machine.get_unit_info("kcore-vm-this-name-is-way-too-long.service")
    assert status["ActiveState"] != "active", "VM should not auto-start"

    machine.succeed("systemctl start kcore-tap-this-name-is-way-too-long.service")
    machine.wait_for_unit("kcore-tap-this-name-is-way-too-long.service")
    machine.succeed("test $(ip -o link show | awk -F': ' '/tap-[0-9a-f]{8}/ {print $2; exit}' | wc -c) -le 16")
    # ExecStart is a generated script; the VMM command line lives there.
    def start_script(unit):
        return machine.succeed(
            f"systemctl show -p ExecStart --value {unit} | sed -n 's/.*path=\\([^ ;]*\\).*/\\1/p'"
        ).strip()

    long_start = start_script("kcore-vm-this-name-is-way-too-long.service")
    machine.succeed(f"grep -E 'tap-[0-9a-f]{{8}}' {long_start}")
    machine.succeed(f"grep -F -- '--firmware /nix/store/' {long_start}")
    machine.succeed(f"grep -F 'CLOUDHV.fd' {long_start}")
    machine.succeed(f"grep -F 'image_type=raw' {long_start}")
    machine.succeed(f"grep -F 'kvm_hyperv=on' {long_start}")
    machine.succeed("systemctl cat kcore-vm-this-name-is-way-too-long.service | grep -F 'TimeoutStartSec=15min'")

    machine.succeed("test -d /run/kcore")
    machine.succeed("test -f /etc/kcore/seeds/this-name-is-way-too-long.iso")

    gpu_start = start_script("kcore-vm-gpu-guest.service")
    machine.succeed(f"grep -F -- '--device path=/sys/bus/pci/devices/0000:03:00.0,iommu=on' {gpu_start}")
    machine.succeed(f"grep -F -- '--device path=/sys/bus/pci/devices/0000:03:00.1,iommu=on' {gpu_start}")
    machine.succeed("grep -q iommu=pt /proc/cmdline")
    machine.succeed("systemctl cat kcore-vm-gpu-guest.service | grep -F vfio-bind-gpu-guest")

    # The node agent spawns the live-migration receiver from this file; it
    # must be the same binary the units exec.
    ch_bin = machine.succeed("cat /etc/kcore/cloud-hypervisor").strip()
    machine.succeed(f"test -x {ch_bin}")
    machine.succeed(f"grep -F -- '{ch_bin}' {gpu_start}")
    machine.succeed("test \"$(cat /etc/kcore/vm-socket-dir)\" = /run/kcore")

    # A pinned digest that does not match the cached image must stop the seed
    # before anything is written to the volume.
    machine.succeed("pvcreate /dev/vdb && vgcreate vg_test /dev/vdb")
    machine.fail("systemctl start kcore-vm-lvm-badsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-badsha.service | grep -F 'refusing to seed lvm-badsha'")
    machine.fail("test -e /var/lib/kcore/volume-seeded/lvm-badsha")
    # The matching digest seeds. Whether ExecStart then runs the guest depends
    # on whether the test VM has nested KVM; the seed is the part under test.
    machine.execute("systemctl start kcore-vm-lvm-goodsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-goodsha.service | grep -F 'Verifying'")
    machine.succeed("test -e /var/lib/kcore/volume-seeded/lvm-goodsha")
    machine.succeed("systemctl stop kcore-vm-lvm-goodsha.service; systemctl reset-failed kcore-vm-lvm-goodsha.service || true")
    machine.fail("pgrep -f -- 'cloud-hypervisor.*--api-socket /run/kcore/lvm-goodsha.sock'")

    # A VMM already serving this VM's API socket makes a start refuse with a
    # status systemd does not retry, instead of wiping sockets and reseeding.
    machine.succeed("systemctl cat kcore-vm-lvm-goodsha.service | grep -F 'RestartPreventExitStatus=75'")
    # argv carries the same `cloud-hypervisor ... --api-socket <path>` shape
    # the units and the node agent use; the process itself just sleeps. The
    # loop keeps the shell from exec'ing sleep and losing that argv.
    machine.succeed("systemd-run --unit fake-vmm --quiet -- /run/current-system/sw/bin/sh -c 'while :; do /run/current-system/sw/bin/sleep 5; done' cloud-hypervisor --api-socket /run/kcore/lvm-goodsha.sock")
    machine.succeed("pgrep -f -- 'cloud-hypervisor.*--api-socket /run/kcore/lvm-goodsha.sock'")
    machine.fail("systemctl start kcore-vm-lvm-goodsha.service")
    machine.succeed("journalctl -u kcore-vm-lvm-goodsha.service | grep -F 'refusing to start a second instance for lvm-goodsha'")
    machine.succeed("systemctl show -p ExecStartPre kcore-vm-lvm-goodsha.service | grep -q 'status=75'")
    machine.succeed("systemctl stop fake-vmm.service")
  '';
}
