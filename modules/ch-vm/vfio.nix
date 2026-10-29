{
  config,
  lib,
  ...
}:
let
  vms = config.ch-vm.vms;
  anyPci = lib.any (vm: vm.pciDevices != [ ]) (lib.attrValues vms.virtualMachines);
  enabled = config.ch-vm.vfio.enable || (vms.enable && anyPci);
  slurm = config.kcore.slurm or null;
  slurmGpuIds = if slurm != null && slurm.gpu.driver then slurm.gpu.pciIds else [ ];
in
{
  options.ch-vm.vfio.enable = lib.mkEnableOption ''
    VFIO PCI passthrough. Loads vfio-pci and sets IOMMU kernel parameters.
    A reboot is required before a guest can claim a device.
  '';

  options.ch-vm.vfio.excludePciIds = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = slurmGpuIds;
    description = ''
      PCI ids that must not be bound to vfio-pci. When a Slurm compute member
      enables the host NVIDIA driver, its GPU ids are excluded automatically.
    '';
  };

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
