# Two-node rehearsal of an LVM live copy and the cold fallback.
#
# Source and destination each have a disk. The checker calls the same
# volume-mirror functions the node agent uses: NBD export, dm-mirror until
# the kernel reports in sync, abort back to a linear map, then a quiescent
# qemu-img copy. One machine cannot host both sides, because both use
# /dev/mapper/kcore-vm-<name>.
{ pkgs }:
let
  mirrorCheck = pkgs.rustPlatform.buildRustPackage {
    pname = "kcore-volume-mirror";
    version = "0.5.0";
    src = pkgs.lib.cleanSourceWith {
      src = ../.;
      filter =
        path: type:
        let
          base = baseNameOf path;
          rel = pkgs.lib.removePrefix (toString ../. + "/") (toString path);
        in
        (type == "regular" || type == "directory" || type == "symlink")
        && base != "target"
        && base != ".git"
        && base != ".playwright-mcp"
        && base != "result"
        && !pkgs.lib.hasPrefix "target/" rel
        && !pkgs.lib.hasPrefix ".git/" rel
        && !pkgs.lib.hasPrefix ".playwright-mcp/" rel;
    };
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = [
      "--package"
      "kcore-volume-mirror"
      "--bin"
      "kcore-volume-mirror-check"
    ];
    cargoTestFlags = [
      "--package"
      "kcore-volume-mirror"
    ];
    doCheck = true;
    meta.mainProgram = "kcore-volume-mirror-check";
  };
  checker = pkgs.lib.getExe mirrorCheck;
in
pkgs.testers.runNixOSTest {
  name = "local-disk-migrate";

  defaults = {
    networking.firewall.enable = false;
    boot.kernelModules = [
      "dm_mod"
      "dm_mirror"
      "nbd"
    ];
    virtualisation.diskSize = 4096;
    virtualisation.memorySize = 2048;
    virtualisation.emptyDiskImages = [ 256 ];
    environment.systemPackages = [
      mirrorCheck
      pkgs.lvm2
      pkgs.qemu-utils
      pkgs.nbd
      pkgs.util-linux
    ];
  };

  nodes.src = { };
  nodes.dst = { };

  testScript = ''
    src.wait_for_unit("multi-user.target")
    dst.wait_for_unit("multi-user.target")
    dst_ip = dst.succeed("ip -4 -o addr show dev eth1 | awk '{print $4}' | cut -d/ -f1").strip()
    size = src.succeed("${checker} setup-source /dev/vdb").strip()
    port, export = dst.succeed(
        f"${checker} prepare-target /dev/vdb {dst_ip} {size}"
    ).split()
    raw = src.succeed(f"${checker} sync-source {dst_ip} {port} {export}")
    assert " 1 AA" in raw, raw
    dst.succeed("${checker} expect-dest a5 3c 5a")

    src.succeed("${checker} abort-source")
    src.succeed("dmsetup status kcore-vm-itmig | grep -w linear")
    dst.succeed("${checker} abort-target")
    dst.fail("test -b /dev/migdst/kcore-itmig")
    dst.fail("pgrep -x qemu-nbd")

    port, export = dst.succeed(
        f"${checker} prepare-target /dev/vdb {dst_ip} {size}"
    ).split()
    src.succeed(f"${checker} cold-copy {dst_ip} {port} {export}")
    dst.succeed("${checker} expect-dest a5 3c 5a")
    dst.succeed("${checker} reject-source-delete")
    dst.succeed("${checker} release-target")
    dst.fail("pgrep -x qemu-nbd")
    dst.succeed("${checker} expect-dest a5 3c 5a")
    src.succeed("${checker} destroy-source")
    src.fail("test -b /dev/migsrc/kcore-itmig")

    src.succeed("${checker} cleanup-source /dev/vdb")
    dst.succeed("${checker} cleanup-dest /dev/vdb")
    src.fail("dmsetup info kcore-vm-itmig")
    dst.fail("dmsetup info kcore-vm-itmig")
  '';
}
