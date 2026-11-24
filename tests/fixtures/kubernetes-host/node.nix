{ lib, ... }:
{
  networking.hostName = "k8s-host-fixture";
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
