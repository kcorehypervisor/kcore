{
  imports = [
    ./options.nix
    ./networking.nix
    ./vm-service.nix
    ./vfio.nix
    ./vgpu.nix
    ./cloud-init.nix
  ];

  meta.maintainers = [ ];
}
