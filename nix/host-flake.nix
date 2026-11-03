# /etc/nixos/flake.nix on every installed kcore node. The ISO installer and
# the node agent's version updates both write this file, so a node installed
# from the vX.Y.Z ISO and a node updated to vX.Y.Z build the same system.
# Only the kcore URL changes between releases; flake.lock pins the commit and
# every input behind it, including nixpkgs.
{
  description = "kcore node";

  inputs.kcore.url = "@KCORE_URL@";

  outputs =
    { kcore, ... }:
    {
      nixosConfigurations.kcore = kcore.lib.mkNode { root = ./.; };
    };
}
