{
  description = "kcore - declarative VM management with Cloud Hypervisor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      perSystem =
        {
          pkgs,
          system,
          ...
        }:
        let
          rustOverlay = inputs.rust-overlay.overlays.default;
          pkgsWithRust = import inputs.nixpkgs {
            inherit system;
            overlays = [ rustOverlay ];
          };
          kcoreVersion = builtins.replaceStrings [ "\n" ] [ "" ] (builtins.readFile ./VERSION);
          rustToolchain = pkgsWithRust.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
            targets = [
              "aarch64-apple-darwin"
              "x86_64-apple-darwin"
            ];
          };
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              (craneLib.filterCargoSources path type)
              || pkgs.lib.hasPrefix "${toString ./.}/proto/" (toString path)
              || pkgs.lib.hasPrefix "${toString ./.}/tests/fixtures/ingress/" (toString path)
              || pkgs.lib.hasPrefix "${toString ./.}/examples/ingress/" (toString path)
              || (toString path) == "${toString ./.}/modules"
              || (toString path) == "${toString ./.}/modules/kcore-ceph.nix"
              || (toString path) == "${toString ./.}/modules/kcore-kubernetes.nix"
              || (toString path) == "${toString ./.}/modules/kcore-slurm.nix"
              || pkgs.lib.hasPrefix "${toString ./.}/crates/kcore-console/" (toString path);
          };

          commonArgs = {
            inherit src;
            pname = "kcore-workspace";
            version = kcoreVersion;
            strictDeps = true;
            nativeBuildInputs = [
              pkgs.protobuf
              pkgs.cmake
              pkgs.perl
            ];
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          kcore-node-agent = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kcore-node-agent";
              cargoExtraArgs = "-p kcore-node-agent";
              # guest_ops tests and the agent itself call ssh-keygen.
              nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.openssh ];
            }
          );

          kcore-controller = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kcore-controller";
              cargoExtraArgs = "-p kcore-controller";
            }
          );

          kctl = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kctl";
              cargoExtraArgs = "-p kctl";
            }
          );

          # Static files only. The controller's web listener serves them and
          # answers the dashboard's gRPC-web calls on the same port.
          kcore-dashboard = pkgs.buildNpmPackage {
            pname = "kcore-dashboard";
            version = kcoreVersion;
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter =
                path: _type:
                let
                  p = toString path;
                in
                p == "${toString ./.}/proto"
                || pkgs.lib.hasPrefix "${toString ./.}/proto/" p
                || p == "${toString ./.}/dashboard"
                || (
                  pkgs.lib.hasPrefix "${toString ./.}/dashboard/" p
                  && !pkgs.lib.hasInfix "/node_modules" p
                  && !pkgs.lib.hasInfix "/dist" p
                  && !pkgs.lib.hasInfix "/src/gen" p
                );
            };
            sourceRoot = "source/dashboard";
            npmDepsHash = "sha256-EOcPawCPoY4RWRv/TIQJ0yIBxWOzfSrZhIwZ3vPFHV4=";
            nativeBuildInputs = [ pkgs.protobuf ];
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              npx vitest run
              runHook postCheck
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out/share
              cp -r dist $out/share/kcore-dashboard
              runHook postInstall
            '';
          };

          kcoreGitRev = inputs.self.shortRev or inputs.self.dirtyShortRev or "dev";

          kcore-console = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kcore-console";
              cargoExtraArgs = "-p kcore-console";
              KCORE_GIT_REV = kcoreGitRev;
            }
          );

          kcore-disk-verify = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "kcore-disk-verify";
              cargoExtraArgs = "-p kcore-disk-verify";
            }
          );

          # The Virtual GPU Manager .run is unfree and is not fetched. This
          # output exists so a workstation that already ran
          # `nix-store --add-fixed sha256` on the file can `nix build` it.
          nvidia-vgpu-manager =
            let
              unfreePkgs = import inputs.nixpkgs {
                inherit system;
                config.allowUnfree = true;
              };
            in
            unfreePkgs.callPackage ./pkgs/nvidia-vgpu-manager {
              kernel = unfreePkgs.linuxPackages.kernel;
              src = unfreePkgs.requireFile {
                name = "NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run";
                sha256 = "2bc7f9be263d45a92331d781df33c14af1af01fcfa45e80102e80caf1ffaa74d";
                message = ''
                  Add NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run to the Nix store:
                    nix-store --add-fixed sha256 NVIDIA-Linux-x86_64-595.91.04-vgpu-kvm-aie.run
                '';
              };
            };
        in
        {
          packages = {
            default = kcore-node-agent;
            caddy-kcore = import ./modules/kcore-ingress-caddy.nix { inherit pkgs; };
            inherit
              kcore-node-agent
              kcore-controller
              kctl
              kcore-dashboard
              kcore-console
              kcore-disk-verify
              nvidia-vgpu-manager
              ;
          };

          checks = {
            inherit
              kcore-node-agent
              kcore-controller
              kctl
              kcore-dashboard
              kcore-console
              kcore-disk-verify
              ;
            clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );
            fmt-rust = craneLib.cargoFmt { inherit src; };
            vm-module = import ./tests/vm-module.nix { inherit pkgs; };
            local-disk-migrate = import ./tests/local-disk-migrate.nix { inherit pkgs; };
            disko-module = import ./tests/disko-module.nix { inherit pkgs; };
            ceph-module-eval = import ./tests/ceph-module-eval.nix {
              inherit pkgs;
              inherit (pkgs) lib;
              nixosSystem = inputs.nixpkgs.lib.nixosSystem;
            };
            slurm-module-eval = import ./tests/slurm-module-eval.nix {
              inherit pkgs;
              inherit (pkgs) lib;
              nixosSystem = inputs.nixpkgs.lib.nixosSystem;
            };
            kcore-slurm = import ./tests/kcore-slurm.nix { inherit pkgs; };
            ch-vm-module-eval = import ./tests/ch-vm-module-eval.nix {
              inherit pkgs;
              inherit (pkgs) lib;
              nixosSystem = inputs.nixpkgs.lib.nixosSystem;
            };
            kcore-backup-host-eval = import ./tests/kcore-backup-host-eval.nix {
              inherit pkgs;
              inherit (pkgs) lib;
              nixosSystem = inputs.nixpkgs.lib.nixosSystem;
              consoleModule = ./modules/kcore-branding.nix;
            };
            kcore-ingress = import ./tests/kcore-ingress.nix { inherit pkgs; };
            # The renderer's golden configurations must load in the real Caddy
            # build, with every plugin it uses.
            kcore-ingress-config = import ./tests/kcore-ingress-config.nix {
              inherit pkgs;
              caddy = import ./modules/kcore-ingress-caddy.nix { inherit pkgs; };
              fixtures = ./tests/fixtures/ingress;
            };
            kcore-host-eval = import ./tests/kcore-host-eval.nix {
              inherit pkgs;
              inherit (pkgs) lib;
              nixosSystem = inputs.nixpkgs.lib.nixosSystem;
            };
            # The stdenv shell is bash, but the `bash` name is not on PATH.
            kcore-infect = pkgs.runCommand "kcore-infect-test" { } ''
              export PATH="${pkgs.lib.removeSuffix "/bash" pkgs.stdenv.shell}:$PATH"
              bash ${./scripts/kcore-infect-test.sh} ${./scripts/kcore-infect}
              touch $out
            '';
            fmt-nix =
              pkgs.runCommand "check-nix-fmt"
                {
                  nativeBuildInputs = [
                    pkgs.nixfmt
                    pkgs.findutils
                  ];
                }
                ''
                  find ${./.} -name '*.nix' -exec nixfmt --check {} + 2>&1 || {
                    echo "Run 'nixfmt' to fix formatting"
                    exit 1
                  }
                  touch $out
                '';
          };

          devShells.default = craneLib.devShell {
            packages = [
              pkgs.cargo-audit
              pkgs.protobuf
              pkgs.grpcurl
              pkgs.gh
              pkgs.cmake
              pkgs.perl
              pkgs.statix
              pkgs.deadnix
              pkgs.nixfmt
              pkgs.cargo-zigbuild
              pkgs.zig
              # Release supply-chain tooling (see docs/release-verification.md).
              # Versions are pinned by flake.lock; sbomnix is deliberately
              # absent -- its ~1.3 GB closure is fetched on demand by
              # scripts/sbom.sh via `nix run --inputs-from .`.
              pkgs.cargo-cyclonedx
              pkgs.cyclonedx-cli
              pkgs.cosign
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              pkgs.cloud-hypervisor
            ];
            shellHook = ''
              if [ -d .git ] && [ -d scripts/hooks ]; then
                for hook in scripts/hooks/*; do
                  name="$(basename "$hook")"
                  target=".git/hooks/$name"
                  if [ ! -L "$target" ] || [ "$(readlink "$target")" != "../../$hook" ]; then
                    ln -sf "../../$hook" "$target"
                  fi
                done
              fi
            '';
          };
        };

      flake =
        let
          kcoreVersion = builtins.replaceStrings [ "\n" ] [ "" ] (builtins.readFile ./VERSION);
          chVmModule = ./modules/ch-vm;
          cephModule = ./modules/kcore-ceph.nix;
          slurmModule = ./modules/kcore-slurm.nix;
          metricsModule = ./modules/kcore-metrics.nix;
        in
        {
          nixosModules = {
            ch-vm = chVmModule;
            default = chVmModule;
            kcore-disko = ./modules/kcore-disko.nix;
            kcore-ceph = cephModule;
            kcore-slurm = slurmModule;
            kcore-metrics = metricsModule;
            kcore-host = ./modules/kcore-host.nix;
          };

          nixosConfigurations =
            let
              # Shared by the ISO and the netboot image; media-specific options live in
              # the per-target modules below.
              kcoreInstallerModule =
                {
                  pkgs,
                  lib,
                  ...
                }:
                let
                  inherit (inputs.self.packages.x86_64-linux) kctl;
                  nodeAgent = inputs.self.packages.x86_64-linux.kcore-node-agent;
                  controller = inputs.self.packages.x86_64-linux.kcore-controller;
                  kcoreConsole = inputs.self.packages.x86_64-linux.kcore-console;
                  diskoPackage = inputs.disko.packages.x86_64-linux.default;
                in
                {
                  imports = [
                    (import ./modules/kcore-disk-check.nix {
                      diskVerify = inputs.self.packages.x86_64-linux.kcore-disk-verify;
                    })
                  ];

                  system.stateVersion = "25.05";
                  nixpkgs.config.allowUnfree = true;
                  nixpkgs.overlays = [
                    (_final: _prev: {
                      kcore-console = kcoreConsole;
                    })
                  ];

                  boot.loader.timeout = lib.mkForce 0;
                  boot.loader.systemd-boot.editor = false;
                  boot.kernelParams = [ "quiet" ];
                  # The branding module turns local login off. Without the
                  # console unit, tty1 stays on the kernel log (USB -71,
                  # kvm_amd) and the machine looks crashed.
                  boot.consoleLogLevel = lib.mkForce 1;
                  services.kcore-console = {
                    enable = true;
                    package = kcoreConsole;
                  };
                  boot.kernelModules = [
                    "kvm"
                    "kvm-intel"
                    "kvm-amd"
                    "tap"
                    "tun"
                    "br_netfilter"
                  ];
                  services.qemuGuest.enable = true;

                  networking.hostName = "kvm-node";
                  networking.useDHCP = true;
                  networking.firewall.enable = true;
                  networking.firewall.allowedTCPPorts = [
                    22
                    9090
                    9091
                    9443
                  ];

                  users.users.root.initialPassword = "kcore";
                  users.mutableUsers = true;

                  services.openssh = {
                    enable = true;
                    listenAddresses = [
                      {
                        addr = "0.0.0.0";
                        port = 22;
                      }
                    ];
                    settings = {
                      PermitRootLogin = "yes";
                      PasswordAuthentication = true;
                    };
                  };

                  systemd.services.kcore-node-agent = {
                    description = "kcore Node Agent";
                    wantedBy = [ "multi-user.target" ];
                    after = [ "network-online.target" ];
                    wants = [ "network-online.target" ];
                    serviceConfig = {
                      Type = "simple";
                      # Live ISO has no bootstrap TLS yet; allow insecure node admin RPCs.
                      ExecStart = "${nodeAgent}/bin/kcore-node-agent --allow-insecure";
                      Environment = "PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
                      Restart = "always";
                      RestartSec = "10s";
                      User = "root";
                      LimitNOFILE = 65536;
                    };
                  };

                  systemd.tmpfiles.rules = [
                    "d /var/lib/kcore 0755 root root -"
                    "d /var/lib/kcore/images 0755 root root -"
                    "d /var/lib/kcore/volumes 0755 root root -"
                    "d /opt/kcore 0755 root root -"
                    "d /opt/kcore/bin 0755 root root -"
                    "d /etc/kcore 0755 root root -"
                    "d /run/kcore 0755 root root -"
                  ];

                  environment.systemPackages = [
                    pkgs.nixos-install-tools
                    pkgs.dosfstools
                    pkgs.e2fsprogs
                    pkgs.cloud-hypervisor
                    pkgs.qemu-utils
                    pkgs.cloud-utils
                    pkgs.iproute2
                    pkgs.jq
                    pkgs.parted
                    pkgs.lvm2
                    pkgs.cryptsetup
                    pkgs.tpm2-tools
                    pkgs.openssl
                    kcoreConsole
                    nodeAgent
                    controller
                    kctl
                    diskoPackage
                    (pkgs.writeShellScriptBin "install-to-disk" ''
                                                                set -euo pipefail

                                                                DISK=""
                                                                AUTO_YES="false"
                                                                NON_INTERACTIVE="false"
                                                                FORCE_WIPE="false"
                                                                REBOOT_AFTER_INSTALL="false"
                                                                CONTROLLER_ENDPOINTS=()
                                                                RUN_CONTROLLER="false"
                                                                DISABLE_VXLAN="false"
                                                                DC_ID="DC1"
                                                                LUKS_METHOD=""
                                                                DATA_DISKS=()
                                                                INSTALL_HOSTNAME=""
                                                                INSTALL_NODE_ID=""
                                                                DATA_DISK_MODE="filesystem"
                                                                LVM_VG_NAME=""
                                                                LVM_LV_PREFIX="kcore-"
                                                                ZFS_POOL_NAME=""
                                                                ZFS_DATASET_PREFIX="kcore-"
                                                                RECOVERY_KEY_OUTPUT=""

                                                                while [[ $# -gt 0 ]]; do
                                                                  case "$1" in
                                                                    --disk)
                                                                      DISK="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --yes)
                                                                      AUTO_YES="true"
                                                                      shift
                                                                      ;;
                                                                    --non-interactive)
                                                                      NON_INTERACTIVE="true"
                                                                      shift
                                                                      ;;
                                                                    --wipe)
                                                                      FORCE_WIPE="true"
                                                                      shift
                                                                      ;;
                                                                    --reboot)
                                                                      REBOOT_AFTER_INSTALL="true"
                                                                      shift
                                                                      ;;
                                                                    --controller)
                                                                      _CTRL_VAL="''${2:-}"
                                                                      if [[ -n "$_CTRL_VAL" && "$_CTRL_VAL" != *:* ]]; then
                                                                        _CTRL_VAL="''${_CTRL_VAL}:9090"
                                                                      fi
                                                                      CONTROLLER_ENDPOINTS+=("$_CTRL_VAL")
                                                                      shift 2
                                                                      ;;
                                                                    --dc-id)
                                                                      DC_ID="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --run-controller)
                                                                      RUN_CONTROLLER="true"
                                                                      shift
                                                                      ;;
                                                                    --data-disk)
                                                                      DATA_DISKS+=("''${2:-}")
                                                                      shift 2
                                                                      ;;
                                                                    --disable-vxlan)
                                                                      DISABLE_VXLAN="true"
                                                                      shift
                                                                      ;;
                                                                    --luks-method)
                                                                      LUKS_METHOD="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --hostname)
                                                                      INSTALL_HOSTNAME="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --node-id)
                                                                      INSTALL_NODE_ID="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --data-disk-mode)
                                                                      DATA_DISK_MODE="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --lvm-vg-name)
                                                                      LVM_VG_NAME="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --lvm-lv-prefix)
                                                                      LVM_LV_PREFIX="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --zfs-pool-name)
                                                                      ZFS_POOL_NAME="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --zfs-dataset-prefix)
                                                                      ZFS_DATASET_PREFIX="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    --recovery-key-output)
                                                                      RECOVERY_KEY_OUTPUT="''${2:-}"
                                                                      shift 2
                                                                      ;;
                                                                    *)
                                                                      echo "Unknown argument: $1"
                                                                      echo "Usage: install-to-disk [--disk /dev/sda] [--data-disk /dev/nvme0n1] [--hostname HOSTNAME] [--node-id ID] [--data-disk-mode filesystem|lvm|zfs] [--lvm-vg-name VG] [--lvm-lv-prefix PREFIX] [--zfs-pool-name POOL] [--zfs-dataset-prefix PREFIX] [--recovery-key-output /path/file.txt] [--controller 192.168.40.135[:9090]]... [--dc-id DC1] [--run-controller] [--disable-vxlan] [--luks-method tpm2|key-file] [--yes --wipe --non-interactive --reboot]"
                                                                      exit 1
                                                                      ;;
                                                                  esac
                                                                done

                                                                if [ "$NON_INTERACTIVE" = "true" ] && [ "$AUTO_YES" != "true" ]; then
                                                                  echo "Error: --non-interactive requires --yes"
                                                                  exit 1
                                                                fi

                                                                echo "======================================================"
                                                                echo "  kcoreOS Node - Automated Disk Installer"
                                                                echo "======================================================"
                                                                echo ""
                                                                echo "WARNING: This will ERASE the selected disk and install kcoreOS!"
                                                                echo ""

                                                                print_install_overview() {
                                                                  ESC="$(printf '\033')"
                                                                  RESET="$ESC[0m"
                                                                  HEADER_BG1="$ESC[48;2;99;102;241m"
                                                                  HEADER_BG2="$ESC[48;2;123;95;246m"
                                                                  PANEL_BG="$ESC[48;2;13;20;45m"
                                                                  SEPARATOR="$ESC[38;2;99;102;241m"
                                                                  TEXT="$ESC[38;2;235;236;255m"
                                                                  WARN="$ESC[38;2;255;190;130m"
                                                                  cols="$(tput cols 2>/dev/null || echo 80)"
                                                                  [ "$cols" -lt 80 ] && cols=80
                                                                  left_w=$(( (cols - 3) / 2 ))
                                                                  right_w=$(( cols - left_w - 3 ))

                                                                  fit() {
                                                                    text="$1"
                                                                    width="$2"
                                                                    printf "%-''${width}.''${width}s" "$text"
                                                                  }

                                                                  print_band() {
                                                                    color="$1"
                                                                    msg="$2"
                                                                    printf "%b" "$color"
                                                                    printf "%-''${cols}.''${cols}s" " $msg "
                                                                    printf "%b\n" "$RESET"
                                                                  }

                                                                  get_primary_ip() {
                                                                    ip -o -4 route show default 2>/dev/null | awk 'NR==1 {print $9}'
                                                                  }

                                                                  service_status() {
                                                                    svc="$1"
                                                                    port="$2"
                                                                    if systemctl is-active --quiet "$svc"; then
                                                                      ip_addr="$(get_primary_ip)"
                                                                      [ -z "$ip_addr" ] && ip_addr="-"
                                                                      printf "%s:%s" "$ip_addr" "$port"
                                                                    else
                                                                      printf "not installed"
                                                                    fi
                                                                  }

                                                                  left_file="$(mktemp)"
                                                                  right_file="$(mktemp)"
                                                                  cleanup_overview() { rm -f "$left_file" "$right_file"; }
                                                                  trap cleanup_overview RETURN

                                                                  {
                                                                    printf "Network Interfaces\n"
                                                                    printf "%-12s %-8s %-17s %s\n" "NAME" "STATE" "MAC" "PRIMARY IPv4"
                                                                    iface_count=0
                                                                    while read -r iface; do
                                                                      [ -z "$iface" ] && continue
                                                                      [ "$iface" = "lo" ] && continue
                                                                      state="$(cat "/sys/class/net/$iface/operstate" 2>/dev/null || echo "unknown")"
                                                                      mac="$(cat "/sys/class/net/$iface/address" 2>/dev/null || echo "-")"
                                                                      addr="$(ip -o -4 addr show dev "$iface" scope global 2>/dev/null | awk 'NR==1 {print $4}')"
                                                                      [ -z "$addr" ] && addr="-"
                                                                      printf "%-12s %-8s %-17s %s\n" "$iface" "$state" "$mac" "$addr"
                                                                      iface_count=$((iface_count + 1))
                                                                    done < <(ls /sys/class/net 2>/dev/null | sort)
                                                                    if [ "$iface_count" -eq 0 ]; then
                                                                      printf "(no network interfaces detected)\n"
                                                                    fi
                                                                    printf "\nDisks\n"
                                                                    printf "%-14s %-8s %-12s %s\n" "PATH" "SIZE" "LABEL/UUID" "MODEL"
                                                                    disk_count=0
                                                                    while read -r row; do
                                                                      path="$(printf "%s\n" "$row" | sed -n 's/.*PATH=\"\([^\"]*\)\".*/\1/p')"
                                                                      size="$(printf "%s\n" "$row" | sed -n 's/.*SIZE=\"\([^\"]*\)\".*/\1/p')"
                                                                      label="$(printf "%s\n" "$row" | sed -n 's/.*LABEL=\"\([^\"]*\)\".*/\1/p')"
                                                                      uuid="$(printf "%s\n" "$row" | sed -n 's/.*UUID=\"\([^\"]*\)\".*/\1/p')"
                                                                      model="$(printf "%s\n" "$row" | sed -n 's/.*MODEL=\"\([^\"]*\)\".*/\1/p')"
                                                                      dtype="$(printf "%s\n" "$row" | sed -n 's/.*TYPE=\"\([^\"]*\)\".*/\1/p')"
                                                                      [ "$dtype" != "disk" ] && continue
                                                                      label_uuid="$label"
                                                                      [ -z "$label_uuid" ] && label_uuid="$uuid"
                                                                      [ -z "$label_uuid" ] && label_uuid="-"
                                                                      [ -z "$model" ] && model="-"
                                                                      printf "%-14s %-8s %-12s %s\n" "$path" "$size" "$label_uuid" "$model"
                                                                      disk_count=$((disk_count + 1))
                                                                    done < <(lsblk -dn -P -o PATH,SIZE,LABEL,UUID,MODEL,TYPE 2>/dev/null || true)
                                                                    if [ "$disk_count" -eq 0 ]; then
                                                                      printf "(no disks detected)\n"
                                                                    fi
                                                                  } > "$left_file"

                                                                  {
                                                                    printf "Services\n"
                                                                    printf "%-14s %s\n" "NAME" "ENDPOINT / STATUS"
                                                                    printf "%-14s %s\n" "node-agent" "$(service_status kcore-node-agent.service 9091)"
                                                                    printf "%-14s %s\n" "controller" "$(service_status kcore-controller.service 9090)"
                                                                    printf "%-14s %s\n" "dashboard" "$(service_status kcore-controller.service 9443)"
                                                                    printf "%-14s %s\n" "metrics" "$(service_status prometheus-node-exporter.service 9100)"
                                                                    printf "%-14s %s\n" "victoriametrics" "$(service_status victoriametrics.service 8428)"
                                                                    printf "\nAccess\n"
                                                                    printf "%s\n" "SSH only (no local login)"
                                                                    printf "%s\n" "Use kctl from operator host"
                                                                  } > "$right_file"

                                                                  mapfile -t left_rows < "$left_file"
                                                                  mapfile -t right_rows < "$right_file"
                                                                  max_rows="''${#left_rows[@]}"
                                                                  if [ "''${#right_rows[@]}" -gt "$max_rows" ]; then
                                                                    max_rows="''${#right_rows[@]}"
                                                                  fi

                                                                  print_band "$HEADER_BG1$TEXT" "kcoreOS Installer"
                                                                  print_band "$HEADER_BG1$TEXT" "Declarative Virtualization Platform"
                                                                  print_band "$HEADER_BG2$TEXT" "for Edge & Datacenters"
                                                                  printf "\n"
                                                                  for ((i=0; i<max_rows; i++)); do
                                                                    left="''${left_rows[$i]:-}"
                                                                    right="''${right_rows[$i]:-}"
                                                                    if [ -z "$left" ] && [ -z "$right" ]; then
                                                                      printf "\n"
                                                                    else
                                                                      printf "%b%b" "$PANEL_BG" "$TEXT"
                                                                      fit "$left" "$left_w"
                                                                      printf "%b│%b" "$SEPARATOR" "$TEXT"
                                                                      fit "$right" "$right_w"
                                                                      printf "%b\n" "$RESET"
                                                                    fi
                                                                  done
                                                                  printf "\n"
                                                                }
                                                                print_install_overview

                                                                echo "Available disks:"
                                                                lsblk -d -o NAME,SIZE,TYPE,MODEL | grep disk
                                                                echo ""

                                                                if [ -z "$DISK" ]; then
                                                                  read -p "Enter target disk (e.g., sda, nvme0n1, vda): " DISK
                                                                fi

                                                                if [[ "$DISK" == /dev/* ]]; then
                                                                  DISK_PATH="$DISK"
                                                                else
                                                                  DISK_PATH="/dev/$DISK"
                                                                fi

                                                                if [ ! -b "$DISK_PATH" ]; then
                                                                  echo "Error: $DISK_PATH is not a valid block device"
                                                                  exit 1
                                                                fi
                                                                NORMALIZED_DATA_DISKS=()
                                                                DISK_REAL=$(readlink -f "$DISK_PATH")
                                                                for dd in "''${DATA_DISKS[@]}"; do
                                                                  if [[ "$dd" != /dev/* ]]; then
                                                                    dd="/dev/$dd"
                                                                  fi
                                                                  if [ ! -b "$dd" ]; then
                                                                    echo "Error: data disk $dd is not a block device"
                                                                    exit 1
                                                                  fi
                                                                  DD_REAL=$(readlink -f "$dd")
                                                                  if [ "$DD_REAL" = "$DISK_REAL" ]; then
                                                                    echo "Error: data disk $dd is the same device as the OS disk"
                                                                    exit 1
                                                                  fi
                                                                  NORMALIZED_DATA_DISKS+=("$dd")
                                                                done
                                                                DATA_DISKS=("''${NORMALIZED_DATA_DISKS[@]}")

                                                                echo "Selected: $DISK_PATH"
                                                                lsblk "$DISK_PATH"
                                                                echo ""

                                                                if [ "$NON_INTERACTIVE" = "true" ]; then
                                                                  if [ "$FORCE_WIPE" != "true" ]; then
                                                                    echo "Error: --non-interactive requires --wipe"
                                                                    exit 1
                                                                  fi
                                                                  echo "Non-interactive mode: continuing with forced disk wipe."
                                                                else
                                                                  read -p "THIS WILL ERASE ALL DATA ON $DISK_PATH! Type 'yes' to continue: " CONFIRM
                                                                  if [ "$CONFIRM" != "yes" ]; then
                                                                    echo "Installation cancelled."
                                                                    exit 0
                                                                  fi
                                                                fi

                                                                echo ""
                                                                echo "Preparing disk..."

                                                                # Best-effort cleanup from prior interrupted runs so retries
                                                                # remain idempotent in automation environments.
                                                                swapoff -a 2>/dev/null || true
                                                                umount -R /mnt 2>/dev/null || true
                                                                if [ -e /dev/mapper/cryptroot ]; then
                                                                  cryptsetup close cryptroot 2>/dev/null || true
                                                                fi

                                                                # Non-interactive destructive wipe for install targets.
                                                                # This intentionally removes signatures and common residual
                                                                # metadata regions so downstream disko/LVM steps never prompt.
                                                                deep_wipe_device() {
                                                                  local target="$1"
                                                                  if [ ! -b "$target" ]; then
                                                                    echo "skip wipe: not a block device: $target"
                                                                    return 0
                                                                  fi
                                                                  echo "==> deep wipe: $target"
                                                                  wipefs -a "$target" 2>/dev/null || true
                                                                  timeout 180s blkdiscard -f "$target" 2>/dev/null || true
                                                                  dd if=/dev/zero of="$target" bs=1M count=128 conv=fsync,notrunc status=none || true
                                                                  local total_sectors tail_sectors
                                                                  total_sectors=$(blockdev --getsz "$target" 2>/dev/null || echo 0)
                                                                  tail_sectors=$((128 * 1024 * 1024 / 512))
                                                                  if [ "$total_sectors" -gt "$tail_sectors" ]; then
                                                                    dd if=/dev/zero of="$target" bs=512 seek=$((total_sectors - tail_sectors)) count="$tail_sectors" conv=fsync,notrunc status=none || true
                                                                  fi
                                                                  partprobe "$target" 2>/dev/null || true
                                                                }

                                                                unmount_disk_holders() {
                                                                  local disk="$1"
                                                                  local dev vg
                                                                  while read -r dev; do
                                                                    [ -z "$dev" ] && continue
                                                                    [ "$dev" = "$disk" ] && continue
                                                                    umount "$dev" 2>/dev/null || true
                                                                  done < <(lsblk -ln -o PATH "$disk" 2>/dev/null || true)
                                                                  # Only VGs whose physical volumes sit on this disk.
                                                                  while read -r vg; do
                                                                    [ -z "$vg" ] && continue
                                                                    vgchange -an "$vg" 2>/dev/null || true
                                                                  done < <(pvs --noheadings -o vg_name,pv_name 2>/dev/null | awk -v disk="$disk" 'index($2, disk)==1 { n=length(disk); rest=substr($2, n+1); if (rest=="" || (disk ~ /[A-Za-z]$/ && rest ~ /^[0-9]+$/) || rest ~ /^p[0-9]+$/) print $1 }' | sort -u)
                                                                }

                                                                unmount_disk_holders "$DISK_PATH"
                                                                for dd in "''${DATA_DISKS[@]}"; do
                                                                  unmount_disk_holders "$dd"
                                                                done

                                                                # Always wipe signatures on target disks for clean re-installs.
                                                                # This prevents stale LUKS/LVM metadata from previous attempts.
                                                                if [ "$FORCE_WIPE" = "true" ]; then
                                                                  deep_wipe_device "$DISK_PATH"
                                                                  for dd in "''${DATA_DISKS[@]}"; do
                                                                    deep_wipe_device "$dd"
                                                                  done
                                                                fi

                                                                # Auto-detect LUKS method if not provided
                                                                if [ -z "$LUKS_METHOD" ]; then
                                                                  if [ -d /sys/class/tpm/tpm0 ]; then
                                                                    LUKS_METHOD="tpm2"
                                                                  else
                                                                    LUKS_METHOD="key-file"
                                                                  fi
                                                                fi

                                                                echo "Disk encryption method: $LUKS_METHOD"

                                                                if [ "$DATA_DISK_MODE" = "lvm" ] && [ -z "$LVM_VG_NAME" ]; then
                                                                  LVM_VG_NAME="vg_kcore"
                                                                fi
                                                                if [ "$DATA_DISK_MODE" = "zfs" ] && [ -z "$ZFS_POOL_NAME" ]; then
                                                                  ZFS_POOL_NAME="tank0"
                                                                fi

                                                                # Generate LUKS passphrase for disko (hex avoids shell-special chars)
                                                                LUKS_PASSPHRASE=$(${pkgs.openssl}/bin/openssl rand -hex 32)
                                                                mkdir -p /tmp/luks
                                                                printf "%s" "$LUKS_PASSPHRASE" > /tmp/luks/password
                                                                chmod 0400 /tmp/luks/password

                                                                # --- Build disko device configuration ---
                                                                DATA_DISK_NIX=""
                                                                EXTRA_DEVICES_NIX=""
                                                                for i in "''${!DATA_DISKS[@]}"; do
                                                                  dd="''${DATA_DISKS[$i]}"
                                                                  case "$DATA_DISK_MODE" in
                                                                    filesystem)
                                                                      MOUNT="/var/lib/kcore/volumes"
                                                                      if [ "$i" -gt 0 ]; then MOUNT="/var/lib/kcore/volumes$i"; fi
                                                                      DATA_DISK_NIX="$DATA_DISK_NIX
                            data$i = {
                              type = \"disk\";
                              device = \"$dd\";
                              content = {
                                type = \"gpt\";
                                partitions = {
                                  data = {
                                    size = \"100%\";
                                    content = {
                                      type = \"filesystem\";
                                      format = \"ext4\";
                                      mountpoint = \"$MOUNT\";
                                    };
                                  };
                                };
                              };
                            };"
                                                                      ;;
                                                                    lvm)
                                                                      DATA_DISK_NIX="$DATA_DISK_NIX
                            data$i = {
                              type = \"disk\";
                              device = \"$dd\";
                              content = {
                                type = \"gpt\";
                                partitions = {
                                  lvm = {
                                    size = \"100%\";
                                    content = {
                                      type = \"lvm_pv\";
                                      vg = \"$LVM_VG_NAME\";
                                    };
                                  };
                                };
                              };
                            };"
                                                                      if [ -z "$EXTRA_DEVICES_NIX" ]; then
                                                                        EXTRA_DEVICES_NIX="
                          lvm_vg = {
                            $LVM_VG_NAME = {
                              type = \"lvm_vg\";
                              lvs = {};
                            };
                          };"
                                                                      fi
                                                                      ;;
                                                                    zfs)
                                                                      DATA_DISK_NIX="$DATA_DISK_NIX
                            data$i = {
                              type = \"disk\";
                              device = \"$dd\";
                              content = {
                                type = \"gpt\";
                                partitions = {
                                  zfs = {
                                    size = \"100%\";
                                    content = {
                                      type = \"zfs\";
                                      pool = \"$ZFS_POOL_NAME\";
                                    };
                                  };
                                };
                              };
                            };"
                                                                      if [ -z "$EXTRA_DEVICES_NIX" ]; then
                                                                        EXTRA_DEVICES_NIX="
                          zpool = {
                            $ZFS_POOL_NAME = {
                              type = \"zpool\";
                              datasets = {};
                            };
                          };"
                                                                      fi
                                                                      ;;
                                                                  esac
                                                                done

                                                                echo "Generating disko configuration..."
                                                                cat > /tmp/disko-config.nix << DISKOEOF
                      {
                        disko.devices = {
                          disk = {
                            os = {
                              type = "disk";
                              device = "$DISK_PATH";
                              content = {
                                type = "gpt";
                                partitions = {
                                  ESP = {
                                    size = "512M";
                                    type = "EF00";
                                    content = {
                                      type = "filesystem";
                                      format = "vfat";
                                      mountpoint = "/boot";
                                      mountOptions = [ "umask=0077" ];
                                    };
                                  };
                                  root = {
                                    size = "100%";
                                    content = {
                                      type = "luks";
                                      name = "cryptroot";
                                      passwordFile = "/tmp/luks/password";
                                      settings = {
                                        allowDiscards = true;
                                      };
                                      content = {
                                        type = "filesystem";
                                        format = "ext4";
                                        mountpoint = "/";
                                      };
                                    };
                                  };
                                };
                              };
                            };$DATA_DISK_NIX
                          };$EXTRA_DEVICES_NIX
                        };
                      }
                      DISKOEOF

                                                                echo "Running disko (partition, format, mount)..."
                                                                disko --mode format,mount --root-mountpoint /mnt /tmp/disko-config.nix

                                                                # Partition names are not a function of the disk path: nvme uses
                                                                # p2, virtio uses 2, by-id uses -part2. cryptsetup reports the
                                                                # device it actually opened.
                                                                ROOT_PART=""
                                                                if [ -e /dev/mapper/cryptroot ]; then
                                                                  ROOT_PART="$(cryptsetup status cryptroot 2>/dev/null | awk '/device:/ {print $2; exit}')"
                                                                fi
                                                                if [ -z "$ROOT_PART" ] || [ ! -b "$ROOT_PART" ]; then
                                                                  echo "Error: could not find the LUKS device behind /dev/mapper/cryptroot"
                                                                  exit 1
                                                                fi
                                                                echo "LUKS root partition: $ROOT_PART"

                                                                # The initrd is built on the live system, so the key file has to
                                                                # exist at the path named in configuration.nix here AND on the
                                                                # installed system (the same absolute path after reboot).
                                                                if [ "$LUKS_METHOD" = "key-file" ]; then
                                                                  install -d -m 0700 /etc/kcore/recovery /mnt/etc/kcore/recovery
                                                                  install -m 0400 /tmp/luks/password /etc/kcore/recovery/crypto_keyfile.bin
                                                                  install -m 0400 /tmp/luks/password /mnt/etc/kcore/recovery/crypto_keyfile.bin
                                                                fi

                                                                if [ "''${#DATA_DISKS[@]}" -gt 0 ]; then
                                                                  echo "Data disks formatted via disko (backend: $DATA_DISK_MODE)."
                                                                fi

                                                                echo "Generating kcoreOS hardware configuration..."
                                                                nixos-generate-config --root /mnt

                                                                echo "kcore packages come from the host flake; not copying /opt/kcore/bin."
                                                                printf '{ ... }: {}\n' > /mnt/etc/nixos/kcore-ceph.nix
                                                                printf '{ ... }: {}\n' > /mnt/etc/nixos/kcore-slurm.nix
                                                                # The format already happened. The saved graph must not point at
                                                                # /tmp, which is gone after reboot.
                                                                sed 's|passwordFile = "/tmp/luks/password"|passwordFile = "/etc/kcore/recovery/crypto_keyfile.bin"|' /tmp/disko-config.nix > /mnt/etc/nixos/disko-config.nix

                                                                echo "Copying kcore config and certificates..."
                                                                mkdir -p /mnt/etc/kcore
                                                                if [ -d /etc/kcore ]; then
                                                                  cp -r /etc/kcore/* /mnt/etc/kcore/ 2>/dev/null || true
                                                                fi
                                                                # Ensure correct permissions on copied cert material
                                                                if [ -d /mnt/etc/kcore/certs ]; then
                                                                  find /mnt/etc/kcore/certs -name '*.key' -exec chmod 0600 {} + 2>/dev/null || true
                                                                  find /mnt/etc/kcore/certs -name '*.crt' -exec chmod 0644 {} + 2>/dev/null || true
                                                                fi
                                                                if [ "''${#DATA_DISKS[@]}" -gt 0 ]; then
                                                                  printf "%s\n" "''${DATA_DISKS[@]}" > /mnt/etc/kcore/data-disks
                                                                fi
                                                                # Safe split default: installer owns disk layout until explicitly promoted.
                                                                echo "installer-only" > /mnt/etc/kcore/disk-management-mode
                                                                # Compatibility symlink so node-agents on the previous release still see the mode.
                                                                ln -sf disk-management-mode /mnt/etc/kcore/disko-management-mode

                                                                if [ "$DISABLE_VXLAN" = "true" ]; then
                                                                  touch /mnt/etc/kcore/disable-vxlan
                                                                fi

                                                                SSH_KEYS=""
                                                                if [ -f /root/.ssh/authorized_keys ]; then
                                                                  SSH_KEYS=$(cat /root/.ssh/authorized_keys | sed 's/^/      "/' | sed 's/$/"/' | paste -sd '\n')
                                                                fi

                                                                GATEWAY_INTERFACE=$(ip -4 route show default 2>/dev/null | awk 'NR==1 {print $5}')
                                                                INTERNAL_GATEWAY_IP="10.240.0.1"
                                                                EXTERNAL_IP=""
                                                                if [ -n "$GATEWAY_INTERFACE" ]; then
                                                                  EXTERNAL_IP=$(ip -4 -o addr show dev "$GATEWAY_INTERFACE" scope global 2>/dev/null | awk 'NR==1 {print $4}' | cut -d/ -f1)
                                                                fi
                                                                if [ -z "$EXTERNAL_IP" ]; then
                                                                  EXTERNAL_IP=$(hostname -I 2>/dev/null | awk '{print $1}')
                                                                fi
                                                                if [ -z "$GATEWAY_INTERFACE" ]; then
                                                                  echo "WARNING: no default route detected; defaulting gateway interface to eno1"
                                                                  GATEWAY_INTERFACE="eno1"
                                                                fi
                                                                if [[ "$EXTERNAL_IP" == 10.240.* ]]; then
                                                                  INTERNAL_GATEWAY_IP="10.241.0.1"
                                                                fi
                                                                if [ -z "$EXTERNAL_IP" ]; then
                                                                  echo "ERROR: could not detect external IP address."
                                                                  echo "  No default route, no addresses from 'hostname -I'."
                                                                  echo "  Fix networking before installing, or set the IP manually."
                                                                  exit 1
                                                                fi

                                                                # Auto-generate hostname and nodeId if not provided
                                                                if [ -z "$INSTALL_HOSTNAME" ]; then
                                                                  IP_SUFFIX="''${EXTERNAL_IP//./-}"
                                                                  INSTALL_HOSTNAME="kvm-node-$IP_SUFFIX"
                                                                fi
                                                                if [ -z "$INSTALL_NODE_ID" ]; then
                                                                  INSTALL_NODE_ID="$INSTALL_HOSTNAME"
                                                                fi
                                                                if [ -z "$RECOVERY_KEY_OUTPUT" ]; then
                                                                  RECOVERY_KEY_OUTPUT="/var/log/kcore/recovery-keys/$INSTALL_NODE_ID-$(date +%Y%m%d%H%M%S).txt"
                                                                fi

                                                                # Controller is opt-in only. By default, install as node-agent that joins an existing controller.
                                                                CONTROLLER_ADDR=""
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  CONTROLLER_ADDR="$EXTERNAL_IP:9090"
                                                                  if [ "''${#CONTROLLER_ENDPOINTS[@]}" -eq 0 ]; then
                                                                    CONTROLLER_ENDPOINTS=("$CONTROLLER_ADDR")
                                                                  else
                                                                    CONTROLLER_ENDPOINTS=("$CONTROLLER_ADDR" "''${CONTROLLER_ENDPOINTS[@]}")
                                                                  fi
                                                                elif [ "''${#CONTROLLER_ENDPOINTS[@]}" -eq 0 ]; then
                                                                  echo "Error: provide --controller <host:9090> or pass --run-controller"
                                                                  exit 1
                                                                else
                                                                  CONTROLLER_ADDR="''${CONTROLLER_ENDPOINTS[0]}"
                                                                fi

                                                                # Deduplicate endpoints (preserving order, first occurrence wins)
                                                                _SEEN=""
                                                                _DEDUPED=()
                                                                for ctrl in "''${CONTROLLER_ENDPOINTS[@]}"; do
                                                                  if [[ "|$_SEEN|" != *"|$ctrl|"* ]]; then
                                                                    _DEDUPED+=("$ctrl")
                                                                    _SEEN="$_SEEN|$ctrl"
                                                                  fi
                                                                done
                                                                CONTROLLER_ENDPOINTS=("''${_DEDUPED[@]}")

                                                                # Write bootstrap controller endpoint (after merge, so correct for all modes)
                                                                echo "''${CONTROLLER_ENDPOINTS[0]}" > /mnt/etc/kcore/bootstrap-controller-endpoint

                                                                CONTROLLERS_YAML=""
                                                                for ctrl in "''${CONTROLLER_ENDPOINTS[@]}"; do
                                                                  CONTROLLERS_YAML="$CONTROLLERS_YAML
                                              - \"$ctrl\""
                                                                done

                                                                STORAGE_YAML="storage:
                                              backend: $DATA_DISK_MODE
                                              imageCacheDir: /var/lib/kcore/images"
                                                                case "$DATA_DISK_MODE" in
                                                                  filesystem)
                                                                    STORAGE_YAML="$STORAGE_YAML
                                              filesystemVolumeDir: /var/lib/kcore/volumes"
                                                                    ;;
                                                                  lvm)
                                                                    STORAGE_YAML="$STORAGE_YAML
                                              lvm:
                                                vgName: $LVM_VG_NAME
                                                lvPrefix: $LVM_LV_PREFIX"
                                                                    ;;
                                                                  zfs)
                                                                    STORAGE_YAML="$STORAGE_YAML
                                              zfs:
                                                poolName: $ZFS_POOL_NAME
                                                datasetPrefix: $ZFS_DATASET_PREFIX"
                                                                    ;;
                                                                  *)
                                                                    echo "Error: unknown --data-disk-mode: $DATA_DISK_MODE (expected filesystem, lvm, or zfs)"
                                                                    exit 1
                                                                    ;;
                                                                esac

                                                                cat > /mnt/etc/kcore/node-agent.yaml <<AGENTEOF
                      nodeId: $INSTALL_NODE_ID
                      listenAddr: "0.0.0.0:9091"
                      controllerAddr: "$CONTROLLER_ADDR"
                      controllers:$CONTROLLERS_YAML
                      dcId: "$DC_ID"
                      vmSocketDir: /run/kcore
                      nixConfigPath: /etc/nixos/kcore-vms.nix
                      tls:
                        caFile: /etc/kcore/certs/ca.crt
                        certFile: /etc/kcore/certs/node.crt
                        keyFile: /etc/kcore/certs/node.key
                      $STORAGE_YAML
                      AGENTEOF

                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  cat > /mnt/etc/kcore/controller.yaml <<CTRLEOF
                      listenAddr: "0.0.0.0:9090"
                      dbPath: /var/lib/kcore/controller.db
                      defaultNetwork:
                        gatewayInterface: br0
                        externalIp: $EXTERNAL_IP
                        gatewayIp: $INTERNAL_GATEWAY_IP
                      tls:
                        caFile: /etc/kcore/certs/ca.crt
                        certFile: /etc/kcore/certs/controller.crt
                        keyFile: /etc/kcore/certs/controller.key
                        subCaCertFile: /etc/kcore/certs/sub-ca.crt
                        subCaKeyFile: /etc/kcore/certs/sub-ca.key
                      metrics:
                        enabled: true
                        fileSdPath: /etc/kcore/metrics/file_sd/nodes.json
                        scrapePort: 9100
                      CTRLEOF

                                                                  # Always write replication config so the controller emits
                                                                  # controller.register and runs peer discovery even when
                                                                  # starting as the first (solo) controller.
                                                                  REPL_PEERS_YAML=""
                                                                  for ctrl in "''${CONTROLLER_ENDPOINTS[@]}"; do
                                                                    if [ "$ctrl" != "$CONTROLLER_ADDR" ]; then
                                                                      REPL_PEERS_YAML="''${REPL_PEERS_YAML}
                        - \"$ctrl\""
                                                                    fi
                                                                  done
                                                                  if [ -z "$REPL_PEERS_YAML" ]; then
                                                                    REPL_PEERS_YAML=" []"
                                                                  fi
                                                                  cat >> /mnt/etc/kcore/controller.yaml <<REPLEOF
                      replication:
                        controllerId: "kcore-controller-$EXTERNAL_IP"
                        dcId: "$DC_ID"
                        peers:$REPL_PEERS_YAML
                      REPLEOF
                                                                fi

                                                                cat > /mnt/etc/nixos/kcore-vms.nix <<'VMSEOF'
                      { ... }:
                      {
                      }
                      VMSEOF

                                                                # Build firewall and ordering blocks before configuration.nix heredoc
                                                                EXTRA_TCP_PORTS=""
                                                                EXTRA_UDP_PORTS=""
                                                                NODE_AGENT_AFTER='"network-online.target"'
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  EXTRA_TCP_PORTS="9090 9443"
                                                                  NODE_AGENT_AFTER='"network-online.target" "kcore-controller.service"'
                                                                fi
                                                                if [ "$DISABLE_VXLAN" != "true" ]; then
                                                                  EXTRA_UDP_PORTS="4789"
                                                                fi

                                                                # node_exporter scrape allowlist: known controller IPs + this node's /24.
                                                                # Never open 9100 on allowedTCPPorts (world-reachable).
                                                                METRICS_SCRAPE_SOURCES="\"127.0.0.1\""
                                                                _METRICS_SEEN="|127.0.0.1|"
                                                                for ctrl in "''${CONTROLLER_ENDPOINTS[@]}"; do
                                                                  ctrl_host="''${ctrl%%:*}"
                                                                  if [ -n "$ctrl_host" ] && [[ "$_METRICS_SEEN" != *"|$ctrl_host|"* ]]; then
                                                                    METRICS_SCRAPE_SOURCES="$METRICS_SCRAPE_SOURCES \"$ctrl_host\""
                                                                    _METRICS_SEEN="$_METRICS_SEEN$ctrl_host|"
                                                                  fi
                                                                done
                                                                if [[ "$EXTERNAL_IP" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
                                                                  _LAN_CIDR="''${BASH_REMATCH[1]}.''${BASH_REMATCH[2]}.''${BASH_REMATCH[3]}.0/24"
                                                                  if [[ "$_METRICS_SEEN" != *"|$_LAN_CIDR|"* ]]; then
                                                                    METRICS_SCRAPE_SOURCES="$METRICS_SCRAPE_SOURCES \"$_LAN_CIDR\""
                                                                  fi
                                                                fi
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  METRICS_SERVER_ENABLE="true"
                                                                else
                                                                  METRICS_SERVER_ENABLE="false"
                                                                fi

                                                                # Build controller service block conditionally
                                                                CONTROLLER_SERVICE=""
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  CONTROLLER_SERVICE=$(cat <<'CTRLSVC'
                                              systemd.services.kcore-controller = {
                                                description = "kcore Controller";
                                                wantedBy = [ "multi-user.target" ];
                                                after = [ "network-online.target" ];
                                                wants = [ "network-online.target" ];
                                                serviceConfig = {
                                                  Type = "simple";
                                                  ExecStart = "/opt/kcore/bin/kcore-controller --config /etc/kcore/controller.yaml";
                                                  Restart = "always";
                                                  RestartSec = "10s";
                                                  User = "root";
                                                  LimitNOFILE = 65536;
                                                };
                                              };
                      CTRLSVC
                                                                  )
                                                                fi

                                                                # Build LUKS boot config for kcoreOS
                                                                ROOT_PART_UUID=$(blkid -s UUID -o value "$ROOT_PART")
                                                                if [ -z "$ROOT_PART_UUID" ]; then
                                                                  echo "Error: $ROOT_PART has no UUID; refusing to write a crypttab that cannot unlock"
                                                                  exit 1
                                                                fi
                                                                LUKS_BOOT_CONFIG=""
                                                                if [ "$LUKS_METHOD" = "tpm2" ]; then
                                                                  LUKS_BOOT_CONFIG=$(cat <<'LUKSEOF'
                                              boot.initrd.luks.devices.cryptroot = {
                                                device = "/dev/disk/by-uuid/ROOT_PART_UUID_PLACEHOLDER";
                                                preLVM = true;
                                                crypttabExtraOpts = [ "tpm2-device=auto" ];
                                              };
                                              boot.initrd.systemd.enable = true;
                      LUKSEOF
                                                                  )
                                                                  LUKS_BOOT_CONFIG="''${LUKS_BOOT_CONFIG//ROOT_PART_UUID_PLACEHOLDER/$ROOT_PART_UUID}"
                                                                else
                                                                  LUKS_BOOT_CONFIG=$(cat <<'LUKSEOF'
                                              boot.initrd.luks.devices.cryptroot = {
                                                device = "/dev/disk/by-uuid/ROOT_PART_UUID_PLACEHOLDER";
                                                preLVM = true;
                                                keyFile = "/crypto_keyfile.bin";
                                              };
                                              boot.initrd.secrets."/crypto_keyfile.bin" = "/etc/kcore/recovery/crypto_keyfile.bin";
                      LUKSEOF
                                                                  )
                                                                  LUKS_BOOT_CONFIG="''${LUKS_BOOT_CONFIG//ROOT_PART_UUID_PLACEHOLDER/$ROOT_PART_UUID}"
                                                                fi

                                                                # --- Storage-specific kcoreOS configuration ---
                                                                STORAGE_BOOT_CONFIG=""
                                                                HOST_ID=$(echo "$INSTALL_HOSTNAME" | md5sum | head -c 8)
                                                                case "$DATA_DISK_MODE" in
                                                                  lvm)
                                                                    STORAGE_BOOT_CONFIG=$(cat <<'STOREOF'
                        services.lvm.enable = true;
                      STOREOF
                                                                    )
                                                                    ;;
                                                                  zfs)
                                                                    STORAGE_BOOT_CONFIG=$(cat <<STOREOF
                        boot.supportedFilesystems = [ "zfs" ];
                        boot.zfs.extraPools = [ "$ZFS_POOL_NAME" ];
                        networking.hostId = "$HOST_ID";
                      STOREOF
                                                                    )
                                                                    ;;
                                                                esac

                                                                RUN_CONTROLLER_NIX="false"
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  RUN_CONTROLLER_NIX="true"
                                                                fi
                                                                echo "Writing node identity..."
                                                                cat > /mnt/etc/nixos/node.nix <<NIXEOF
                      { ... }:
                      {
                      $STORAGE_BOOT_CONFIG

                      $LUKS_BOOT_CONFIG

                        networking.hostName = "$INSTALL_HOSTNAME";
                        networking.useDHCP = false;
                        networking.bridges.br0.interfaces = [ "$GATEWAY_INTERFACE" ];
                        networking.interfaces.br0.useDHCP = true;
                        networking.interfaces."$GATEWAY_INTERFACE".useDHCP = false;
                        networking.dhcpcd.extraConfig = "timeout 60";
                        networking.firewall.enable = true;
                        networking.firewall.allowedTCPPorts = [ 22 9091 $EXTRA_TCP_PORTS ];
                        networking.firewall.allowedUDPPorts = [ $EXTRA_UDP_PORTS ];

                        kcore.metrics = {
                          enable = true;
                          allowedScrapeSources = [ $METRICS_SCRAPE_SOURCES ];
                          server.enable = $METRICS_SERVER_ENABLE;
                        };

                        users.users.root = {
                          initialPassword = "kcore";
                          openssh.authorizedKeys.keys = [
                      $SSH_KEYS
                          ];
                        };
                        users.mutableUsers = true;

                        services.openssh = {
                          enable = true;
                          listenAddresses = [ { addr = "0.0.0.0"; port = 22; } ];
                          settings = {
                            PermitRootLogin = "yes";
                            PasswordAuthentication = true;
                          };
                        };

                        services.kcore.nodeAgent.enable = true;
                        services.kcore.controller.enable = $RUN_CONTROLLER_NIX;
                        services.kcore.dashboard.enable = $RUN_CONTROLLER_NIX;
                        services.kcore.console.enable = true;

                        system.stateVersion = "25.05";
                      }
                      NIXEOF

                                                                cp /etc/kcore/installed-flake.nix /mnt/etc/nixos/flake.nix
                                                                echo "Building kcoreOS on the installer store (this will take 10-20 minutes)..."
                                                                export NIX_CONFIG="experimental-features = nix-command flakes"
                                                                # Build in this store, copy the closure onto the target, then install
                                                                # that system. Evaluating the flake in the target store hits the Nix
                                                                # 2.31 copyInputToStore assertion once these path inputs have a nar
                                                                # hash. Substituters stay enabled: the installed configuration is not
                                                                # the live ISO closure, and fetching missing paths from the binary
                                                                # cache is what makes the build finish.
                                                                SYSTEM=$(nix build --impure --no-link --print-out-paths \
                                                                  '/mnt/etc/nixos#nixosConfigurations.kcore.config.system.build.toplevel')
                                                                nix copy --no-check-sigs --to /mnt \
                                                                  "$SYSTEM" \
                                                                  "${inputs.nixpkgs.outPath}" \
                                                                  "${./.}"
                                                                nixos-install --system "$SYSTEM" --root /mnt --no-root-passwd

                                                                # Enroll TPM2 and add recovery key
                                                                if [ "$LUKS_METHOD" = "tpm2" ]; then
                                                                  echo "Enrolling TPM2 for LUKS..."
                                                                  if ! timeout 120s systemd-cryptenroll --unlock-key-file /tmp/luks/password --tpm2-device=auto --tpm2-pcrs=7 "$ROOT_PART"; then
                                                                    echo "Error: TPM2 enrollment failed or timed out"
                                                                    exit 1
                                                                  fi
                                                                  echo ""
                                                                  echo "Adding and persisting LUKS recovery key..."
                                                                  if ! RECOVERY_ENROLL_OUTPUT=$(timeout 120s systemd-cryptenroll --unlock-key-file /tmp/luks/password --recovery-key "$ROOT_PART" 2>&1); then
                                                                    echo "$RECOVERY_ENROLL_OUTPUT"
                                                                    echo "Error: failed to add LUKS recovery key"
                                                                    exit 1
                                                                  fi
                                                                  RECOVERY_KEY=$(printf "%s\n" "$RECOVERY_ENROLL_OUTPUT" | grep -Eo '[a-z0-9]{8}(-[a-z0-9]{8}){7}' | head -n 1 || true)
                                                                  if [ -z "$RECOVERY_KEY" ]; then
                                                                    echo "Warning: could not parse recovery key from systemd-cryptenroll output."
                                                                    echo "$RECOVERY_ENROLL_OUTPUT"
                                                                    RECOVERY_KEY="UNAVAILABLE"
                                                                    RECOVERY_KEY_FINGERPRINT="unavailable"
                                                                  else
                                                                    RECOVERY_KEY_FINGERPRINT=$(printf "%s" "$RECOVERY_KEY" | sha256sum | awk '{print $1}')
                                                                  fi
                                                                  RECOVERY_KEY_TMP=$(mktemp)
                                                                  cat > "$RECOVERY_KEY_TMP" <<EOF
                      nodeId: $INSTALL_NODE_ID
                      hostname: $INSTALL_HOSTNAME
                      disk: $DISK_PATH
                      rootPart: $ROOT_PART
                      rootUuid: $ROOT_PART_UUID
                      luksMethod: tpm2
                      createdAtUtc: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
                      recoveryKey: $RECOVERY_KEY
                      recoveryKeySha256: $RECOVERY_KEY_FINGERPRINT
                      recoveryEnrollOutput: |
                      $(printf "%s\n" "$RECOVERY_ENROLL_OUTPUT" | sed 's/^/  /')
                      EOF
                                                                  chmod 0400 "$RECOVERY_KEY_TMP"
                                                                  install -d -m 0700 /mnt/etc/kcore/recovery
                                                                  install -m 0400 "$RECOVERY_KEY_TMP" /mnt/etc/kcore/recovery/luks-recovery-key.txt
                                                                  install -d -m 0700 "$(dirname "$RECOVERY_KEY_OUTPUT")"
                                                                  install -m 0400 "$RECOVERY_KEY_TMP" "$RECOVERY_KEY_OUTPUT"
                                                                  rm -f "$RECOVERY_KEY_TMP"
                                                                  if ! timeout 120s systemd-cryptenroll --unlock-key-file /tmp/luks/password --wipe-slot=password "$ROOT_PART"; then
                                                                    echo "Warning: failed to remove temporary LUKS password slot"
                                                                  fi
                                                                  rm -f /tmp/luks/password
                                                                  unset LUKS_PASSPHRASE
                                                                  echo "TPM2 enrolled, temporary passphrase replaced with recovery key."
                                                                  echo "Recovery key artifact saved on installed node: /etc/kcore/recovery/luks-recovery-key.txt"
                                                                  echo "Recovery key artifact saved on live env: $RECOVERY_KEY_OUTPUT"
                                                                  echo "Recovery key fingerprint (sha256): $RECOVERY_KEY_FINGERPRINT"
                                                                fi

                                                                echo ""
                                                                echo "======================================================"
                                                                echo "  Installation complete!"
                                                                echo "======================================================"
                                                                echo ""
                                                                echo "Login credentials:"
                                                                echo "  Username: root"
                                                                echo "  Password: kcore"
                                                                echo ""
                                                                echo "Disk encryption: LUKS2 ($LUKS_METHOD)"
                                                                echo ""
                                                                if [ "$RUN_CONTROLLER" = "true" ]; then
                                                                  echo "This node is configured as a controller + agent."
                                                                  echo "To add more nodes, use:"
                                                                  echo "  kctl node install --node <new-node-ip>:9091 --os-disk /dev/sda --join-controller <this-ip>"
                                                                else
                                                                  echo "This node is configured as an agent joining controller at: $CONTROLLER_ADDR"
                                                                fi
                                                                echo ""
                                                                if [ "$REBOOT_AFTER_INSTALL" = "true" ]; then
                                                                  echo "Rebooting now..."
                                                                  sync
                                                                  reboot
                                                                else
                                                                  echo "Remove the USB drive and type: reboot"
                                                                fi
                    '')
                  ];

                  environment.etc."kcore/installed-flake.nix".text = ''
                    {
                      description = "kcore node";
                      inputs = {
                        nixpkgs.url = "path:${inputs.nixpkgs.outPath}";
                        kcore.url = "path:${./.}";
                      };
                      outputs =
                        { nixpkgs, kcore, ... }:
                        {
                          nixosConfigurations.kcore = nixpkgs.lib.nixosSystem {
                            system = "x86_64-linux";
                            specialArgs = {
                              inherit kcore;
                            };
                            modules = [
                              kcore.nixosModules.kcore-host
                              (
                                { kcore, ... }:
                                {
                                  services.kcore.version = "${kcoreVersion}";
                                  services.kcore.nodeAgent.package = kcore.packages.x86_64-linux.kcore-node-agent;
                                  services.kcore.controller.package = kcore.packages.x86_64-linux.kcore-controller;
                                  services.kcore.dashboard.package = kcore.packages.x86_64-linux.kcore-dashboard;
                                  services.kcore.console.package = kcore.packages.x86_64-linux.kcore-console;
                                  services.kcore.kctl.package = kcore.packages.x86_64-linux.kctl;
                                }
                              )
                              ./hardware-configuration.nix
                              ./node.nix
                              ./kcore-vms.nix
                              ./kcore-ceph.nix
                              ./kcore-slurm.nix
                            ];
                          };
                        };
                    }
                  '';

                  environment.etc."kcore/node-agent.yaml" = {
                    text = builtins.concatStringsSep "\n" [
                      "nodeId: kvm-node-01"
                      ''listenAddr: "0.0.0.0:9091"''
                      "vmSocketDir: /run/kcore"
                      "nixConfigPath: /etc/nixos/kcore-vms.nix"
                      ""
                    ];
                    mode = "0644";
                  };
                };

              kcoreIsoModule = _: {
                isoImage.volumeID = "KCORE";
                isoImage.makeUsbBootable = true;
                isoImage.makeEfiBootable = true;
                image.fileName = "kcoreos-${kcoreVersion}-x86_64-linux.iso";
              };

              # PXE / UEFI HTTP Boot: kernel + one initrd that embeds the squashfs nix store
              # (the ISO initrd looks for it on a LABEL=KCORE volume, which a network boot
              # does not have). The provisioning server serves vmlinuz and initrd.img and
              # must pass `init=<contents of init>` on the kernel command line.
              kcoreNetbootModule =
                {
                  config,
                  pkgs,
                  ...
                }:
                {
                  image.fileName = "kcoreos-${kcoreVersion}-x86_64-netboot";
                  # zstd -19 over the full closure is slow to build; -10 keeps the
                  # image size within ~10% and cuts build time substantially.
                  netboot.squashfsCompression = "zstd -Xcompression-level 10";
                  # The squashfs is already compressed; do not compress it again.
                  boot.initrd.compressor = "cat";
                  boot.initrd.compressorArgs = [ ];
                  # Serial console first so iDRAC/IPMI Serial-over-LAN shows stage 1 errors.
                  boot.kernelParams = [
                    "console=tty0"
                    "console=ttyS0,115200n8"
                  ];

                  system.build.kcoreNetbootTree = pkgs.runCommand "kcore-netboot-tree" { } ''
                    mkdir -p $out
                    ln -s ${config.system.build.kernel}/${config.system.boot.loader.kernelFile} $out/vmlinuz
                    ln -s ${config.system.build.netbootRamdisk}/initrd $out/initrd.img
                    printf '%s\n' "${config.system.build.toplevel}/init" > $out/init
                    printf '%s\n' "${toString config.boot.kernelParams}" > $out/kernel-params
                  '';
                };
            in
            {
              kcore-iso = inputs.nixpkgs.lib.nixosSystem {
                system = "x86_64-linux";
                modules = [
                  "${inputs.nixpkgs}/nixos/modules/installer/cd-dvd/iso-image.nix"
                  ./modules/kcore-minimal.nix
                  ./modules/kcore-branding.nix
                  kcoreInstallerModule
                  kcoreIsoModule
                ];
              };

              kcore-netboot = inputs.nixpkgs.lib.nixosSystem {
                system = "x86_64-linux";
                modules = [
                  "${inputs.nixpkgs}/nixos/modules/installer/netboot/netboot.nix"
                  ./modules/kcore-minimal.nix
                  ./modules/kcore-branding.nix
                  kcoreInstallerModule
                  kcoreNetbootModule
                ];
              };

              # Fixture installed node. Packages are hello so evaluating this
              # configuration does not build the Rust workspace. A real host
              # sets the packages from its kcore flake input.
              kcore-node = inputs.nixpkgs.lib.nixosSystem {
                system = "x86_64-linux";
                modules = [
                  ./modules/kcore-host.nix
                  ./tests/fixtures/installed-node.nix
                ];
              };
              kcore-controller-node = inputs.nixpkgs.lib.nixosSystem {
                system = "x86_64-linux";
                modules = [
                  ./modules/kcore-host.nix
                  ./tests/fixtures/installed-node.nix
                  (_: {
                    services.kcore.controller.enable = true;
                    services.kcore.dashboard.enable = true;
                  })
                ];
              };
            };
        };
    };
}
