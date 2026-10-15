# Shared fixture for nixosConfigurations.kcore-node. stateVersion is the
# caller's; this file is not what an installed machine uses.
{ pkgs, ... }:
{
  networking.hostName = "kcore-node-fixture";
  system.stateVersion = "25.05";
  nixpkgs.overlays = [ (_final: prev: { ceph = prev.hello; }) ];
  boot.loader.grub.enable = false;
  fileSystems."/" = {
    device = "nodev";
    fsType = "tmpfs";
  };
  services.kcore.version = "0.4.2";
  services.kcore.nodeAgent = {
    enable = true;
    package = pkgs.hello;
  };
  services.kcore.controller.package = pkgs.hello;
  services.kcore.dashboard.package = pkgs.hello;
  services.kcore.console = {
    enable = true;
    package = pkgs.hello;
  };
  services.kcore.kctl.package = pkgs.hello;
  kcore.metrics.enable = true;
  ch-vm.vms = {
    enable = true;
    cloudHypervisorPackage = pkgs.cloud-hypervisor;
    gatewayInterface = "eth0";
    networks.default = {
      externalIP = "10.0.2.15";
      gatewayIP = "192.168.100.1";
    };
    virtualMachines.guest = {
      image = "/var/lib/kcore/images/guest.img";
      cores = 1;
      memorySize = 256;
      network = "default";
      autoStart = true;
    };
  };
}
