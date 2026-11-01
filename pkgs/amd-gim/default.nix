# AMD GPU-IOV Module (MxGPU) for Radeon PRO V710 and Instinct SR-IOV hosts.
# MIT licensed and built from AMD's public repository. GIM owns the physical
# function and creates the virtual functions at load time (vf_num).
{
  stdenv,
  lib,
  fetchFromGitHub,
  kernel,
  autoconf,
  automake,
}:

stdenv.mkDerivation (finalAttrs: {
  pname = "amd-gim";
  version = "9.3.0.K";

  src = fetchFromGitHub {
    owner = "amd";
    repo = "MxGPU-Virtualization";
    rev = "78ffbb3468394dd1f3d90bf56334101c8b124598";
    hash = "sha256-DYrhwEwkTl0nxw7ws2MIVR3+uQASM9zo6Fi3YW7Pb3c=";
  };

  nativeBuildInputs = [
    autoconf
    automake
  ]
  ++ kernel.moduleBuildDependencies;

  hardeningDisable = [
    "pic"
    "format"
  ];

  postPatch = ''
    patchShebangs gim/dkms
  '';

  # gim/Makefile passes only the build tree to configure. In a Nix kernel the
  # headers sit in source/, so every header probe would fail.
  buildPhase =
    let
      modules = "${kernel.dev}/lib/modules/${kernel.modDirVersion}";
    in
    ''
      runHook preBuild
      echo ${finalAttrs.version} > VERSION
      gim/dkms/pre-build.sh --with-linux=${modules}/source --with-linux-obj=${modules}/build
      make -C ${modules}/build M="$PWD/gim" -j"$NIX_BUILD_CORES" modules
      runHook postBuild
    '';

  installPhase = ''
    runHook preInstall
    dest=$out/lib/modules/${kernel.modDirVersion}/updates
    install -Dm444 gim/gim.ko "$dest/gim.ko"
    install -Dm444 gim/amd-vfio-pci/amd-vfio-pci.ko "$dest/amd-vfio-pci.ko"
    runHook postInstall
  '';

  meta = {
    description = "AMD GPU-IOV Module for SR-IOV GPU virtualization";
    homepage = "https://github.com/amd/MxGPU-Virtualization";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
  };
})
