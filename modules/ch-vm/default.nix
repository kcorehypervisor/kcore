{
  imports = [
    ./options.nix
    ./networking.nix
    ./vm-service.nix
    ./vfio.nix
    ./cloud-init.nix
  ];

  meta.maintainers = [ ];
}
