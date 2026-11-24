{ lib, ... }:
{
  networking.hostName = "node-2";
  networking.nftables.enable = true;
  system.stateVersion = "25.11";
  boot.loader.grub.enable = false;
  fileSystems."/" = {
    device = "nodev";
    fsType = "tmpfs";
  };
  services.kcore.version = "0.0.0";
  services.kcore.nodeAgent.enable = lib.mkForce false;
  services.kcore.console.enable = lib.mkForce false;
}
