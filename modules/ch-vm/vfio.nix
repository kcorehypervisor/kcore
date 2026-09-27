{
  config,
  lib,
  ...
}:
let
  vms = config.ch-vm.vms;
  anyPci = lib.any (vm: vm.pciDevices != [ ]) (lib.attrValues vms.virtualMachines);
  enabled = config.ch-vm.vfio.enable || (vms.enable && anyPci);
in
{
  options.ch-vm.vfio.enable = lib.mkEnableOption ''
    VFIO PCI passthrough. Loads vfio-pci and sets IOMMU kernel parameters.
    A reboot is required before a guest can claim a device.
  '';

  config = lib.mkIf enabled {
    # Both vendor switches are harmless on the other CPU. `iommu=pt` keeps
    # host devices on the identity map until a guest claims one via VFIO.
    boot.kernelParams = [
      "iommu=pt"
      "amd_iommu=on"
      "intel_iommu=on"
    ];
    boot.kernelModules = [
      "vfio"
      "vfio_pci"
      "vfio_iommu_type1"
    ];
  };
}
