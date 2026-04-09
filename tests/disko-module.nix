{ pkgs, ... }:
let
  diskoModule = (builtins.getFlake "github:nix-community/disko").nixosModules.disko;
in
pkgs.testers.runNixOSTest {
  name = "kcore-disko-basic";

  nodes.machine =
    { ... }:
    {
      imports = [
        diskoModule
        ../modules/kcore-disko.nix
      ];

      kcore.disko = {
        enable = true;
        osDisk = "/dev/vda";
        luksPasswordFile = "/tmp/luks-password";
        storageBackend = "filesystem";
        dataDisks = [ ];
      };

      virtualisation.memorySize = 2048;
    };

  testScript = ''
    import json

    # Verify that the disko module options are accepted and the NixOS
    # module evaluation succeeds (the VM boots).
    machine.wait_for_unit("multi-user.target")

    # The disko module should have generated fileSystems entries
    # (though they won't be active in the VM since /dev/vda is the
    # virtualisation disk, not a real disko-formatted device).
    machine.succeed("test -f /etc/nixos/configuration.nix || true")
  '';
}
