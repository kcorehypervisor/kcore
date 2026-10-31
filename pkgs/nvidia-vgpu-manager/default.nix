# NVIDIA Virtual GPU Manager for Linux KVM.
#
# kcore does not fetch this. The caller passes `src`, normally
# `pkgs.requireFile` of the operator's
# NVIDIA-Linux-x86_64-<version>-vgpu-kvm-aie.run after
# `nix-store --add-fixed sha256` has put that file in the store.
{
  stdenv,
  lib,
  kernel,
  src,
  pciutils,
  coreutils,
  gnugrep,
  gawk,
  bash,
  nukeReferences,
  patchelf,
}:

let
  version = "595.91.04";
in
stdenv.mkDerivation {
  pname = "nvidia-vgpu-manager";
  inherit version src;

  nativeBuildInputs = [
    nukeReferences
    patchelf
  ]
  ++ kernel.moduleBuildDependencies;

  buildInputs = [
    stdenv.cc.cc.lib
  ];

  hardeningDisable = [
    "pic"
    "format"
  ];
  dontStrip = true;
  dontPatchELF = true;

  unpackPhase = ''
    runHook preUnpack
    sh "$src" -x
    sourceRoot=NVIDIA-Linux-x86_64-${version}-vgpu-kvm-aie
    runHook postUnpack
  '';

  buildPhase = ''
    runHook preBuild
    cd kernel
    unset src
    make -j"$NIX_BUILD_CORES" module \
      IGNORE_PREEMPT_RT_PRESENCE=1 \
      NV_BUILD_SUPPORTS_HMM=1 \
      SYSSRC=${kernel.dev}/lib/modules/${kernel.modDirVersion}/source \
      SYSOUT=${kernel.dev}/lib/modules/${kernel.modDirVersion}/build
    cd ..
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    modDir=$out/lib/modules/${kernel.modDirVersion}/misc
    mkdir -p "$modDir"
    while IFS= read -r ko; do
      nuke-refs "$ko"
      cp "$ko" "$modDir/"
    done < <(find kernel -name '*.ko')

    mkdir -p "$out/lib/firmware/nvidia/${version}"
    cp firmware/gsp_*.bin "$out/lib/firmware/nvidia/${version}/"

    mkdir -p "$out/lib"
    cp -a libnvidia-ml.so.${version} "$out/lib/"
    cp -a libnvidia-vgpu.so.${version} "$out/lib/"
    cp -a libnvidia-vgxcfg.so.${version} "$out/lib/"
    ln -s libnvidia-ml.so.${version} "$out/lib/libnvidia-ml.so.1"
    ln -s libnvidia-vgpu.so.${version} "$out/lib/libnvidia-vgpu.so.1"
    ln -s libnvidia-vgxcfg.so.${version} "$out/lib/libnvidia-vgxcfg.so.1"

    mkdir -p "$out/bin" "$out/lib/nvidia" "$out/share/nvidia/vgpu"
    for bin in nvidia-smi nvidia-debugdump nvidia-modprobe nvidia-vgpud nvidia-vgpu-mgr nvidia-xid-logd; do
      install -Dm755 "$bin" "$out/bin/$bin"
    done
    install -Dm644 vgpuConfig.xml "$out/share/nvidia/vgpu/vgpuConfig.xml"

    # sriov-manage is a bash script that calls lspci and setpci.
    sed "1s|.*|#!${bash}/bin/bash|" sriov-manage > "$out/lib/nvidia/sriov-manage"
    chmod 755 "$out/lib/nvidia/sriov-manage"
    printf '%s\n' \
      '#!${bash}/bin/bash' \
      'export PATH="${
        lib.makeBinPath [
          pciutils
          coreutils
          gnugrep
          gawk
        ]
      }:$PATH"' \
      "exec ${bash}/bin/bash \"$out/lib/nvidia/sriov-manage\" \"\$@\"" \
      > "$out/bin/sriov-manage"
    chmod 755 "$out/bin/sriov-manage"

    interp=$(cat ${stdenv.cc}/nix-support/dynamic-linker)
    for bin in "$out/bin"/*; do
      if ! ${patchelf}/bin/patchelf --print-interpreter "$bin" >/dev/null 2>&1; then
        continue
      fi
      ${patchelf}/bin/patchelf --set-interpreter "$interp" --set-rpath "$out/lib:${
        lib.makeLibraryPath [ stdenv.cc.cc.lib ]
      }" "$bin"
    done
    for lib in "$out/lib"/*.so.*; do
      if [ -L "$lib" ]; then
        continue
      fi
      ${patchelf}/bin/patchelf --set-rpath "$out/lib:${lib.makeLibraryPath [ stdenv.cc.cc.lib ]}" "$lib"
    done
    runHook postInstall
  '';

  meta = {
    description = "NVIDIA Virtual GPU Manager for Linux KVM";
    homepage = "https://docs.nvidia.com/vgpu/";
    license = lib.licenses.unfree;
    platforms = [ "x86_64-linux" ];
    # The .run is not redistributable. Hydra must not build this.
    hydraPlatforms = [ ];
  };
}
