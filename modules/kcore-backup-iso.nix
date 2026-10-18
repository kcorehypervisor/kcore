# Live ISO that installs the backup appliance. Arguments are the product version
# and the store paths baked into the installed flake so nixos-install does not
# fetch nixpkgs or kcore from the network.
{
  kcoreVersion,
  nixpkgsPath,
  kcoreSource,
  diskoPackage,
}:
{
  pkgs,
  ...
}:
let
  installBackup = pkgs.writeShellScriptBin "install-backup" (
    builtins.replaceStrings
      [
        "@KCORE_VERSION@"
        "@NIXPKGS@"
        "@KCORE@"
      ]
      [
        kcoreVersion
        nixpkgsPath
        "${kcoreSource}"
      ]
      (builtins.readFile ./kcore-backup-install.sh)
  );
in
{
  system.stateVersion = "25.05";
  # ZFS is only for the optional repository disk. The live system needs a
  # hostId so the module evaluates; the installed host gets its own.
  networking.hostId = "b1c0b0e0";
  boot.supportedFilesystems = [ "zfs" ];

  isoImage.volumeID = "KCOREBACKUP";
  isoImage.makeUsbBootable = true;
  isoImage.makeEfiBootable = true;
  image.fileName = "kcore-backup-${kcoreVersion}-x86_64-linux.iso";

  networking.useDHCP = true;
  networking.firewall.enable = true;
  networking.firewall.allowedTCPPorts = [ 22 ];

  users.users.root.initialPassword = "kcore";
  users.mutableUsers = true;
  services.getty.autologinUser = "root";

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "yes";
      PasswordAuthentication = true;
    };
  };

  environment.etc."issue".text = ''
    kcoreOS backup installer ${kcoreVersion}

    This image installs a backup server. It does not install the hypervisor.

    Log in as root and run:
      install-backup --disk /dev/disk/by-id/YOUR-DISK --yes

    Add --data-disk to put the repository on a second disk (ZFS pool vault).

  '';

  environment.systemPackages = [
    pkgs.nixos-install-tools
    pkgs.dosfstools
    pkgs.e2fsprogs
    pkgs.cryptsetup
    pkgs.openssl
    pkgs.parted
    pkgs.zfs
    pkgs.iproute2
    pkgs.jq
    diskoPackage
    installBackup
  ];
}
