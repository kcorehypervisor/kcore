# Installed kcore node. Packages come from the host flake's kcore input.
# Identity (hostname, LUKS, stateVersion) stays in the caller's node.nix.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.kcore;
  confirmScript = pkgs.writeShellScript "kcore-boot-confirm" ''
    set -eu
    pending=/var/lib/kcore/updates/boot-pending
    if [ ! -f "$pending" ]; then
      exit 0
    fi
    expected_toplevel=$(sed -n 's/^toplevel=//p' "$pending" | head -n 1)
    expected_version=$(sed -n 's/^version=//p' "$pending" | head -n 1)
    booted=$(readlink -f /run/booted-system || true)
    if [ -z "$expected_toplevel" ] || [ "$booted" != "$expected_toplevel" ]; then
      exit 0
    fi
    if [ -n "$expected_version" ] && [ -f /etc/kcore/version ]; then
      running=$(tr -d '\n' < /etc/kcore/version)
      if [ "$running" != "$expected_version" ]; then
        exit 0
      fi
    fi
    if ! ${pkgs.systemd}/bin/systemctl is-active --quiet kcore-node-agent.service; then
      exit 0
    fi
    entry=$(${pkgs.systemd}/bin/bootctl status --json=short 2>/dev/null | ${pkgs.jq}/bin/jq -r '.entry // empty' || true)
    if [ -z "$entry" ]; then
      entry=$(${pkgs.systemd}/bin/bootctl status 2>/dev/null | sed -n 's/^.*id: //p' | head -n 1 || true)
    fi
    if [ -n "$entry" ]; then
      ${pkgs.systemd}/bin/bootctl set-default "$entry"
    fi
    mkdir -p /run/kcore
    touch /run/kcore/boot-blessed
    rm -f "$pending"
  '';
  timeoutScript = pkgs.writeShellScript "kcore-boot-confirm-timeout" ''
    set -eu
    pending=/var/lib/kcore/updates/boot-pending
    if [ ! -f "$pending" ]; then
      exit 0
    fi
    if [ -f /run/kcore/boot-blessed ]; then
      exit 0
    fi
    expected_toplevel=$(sed -n 's/^toplevel=//p' "$pending" | head -n 1)
    booted=$(readlink -f /run/booted-system || true)
    if [ -n "$expected_toplevel" ] && [ "$booted" = "$expected_toplevel" ]; then
      ${pkgs.systemd}/bin/systemctl reboot
    fi
  '';
in
{
  imports = [
    ./ch-vm
    ./kcore-branding.nix
    ./kcore-ceph.nix
    ./kcore-metrics.nix
    (lib.mkRemovedOptionModule [
      "services"
      "kcore"
      "dashboard"
      "environmentFile"
    ] "The dashboard is served by the controller's web listener and talks gRPC-web to it.")
  ];

  options.services.kcore = {
    version = lib.mkOption {
      type = lib.types.str;
      default = "";
      description = "Product version written to /etc/kcore/version. The caller supplies it.";
    };

    nodeAgent = {
      enable = lib.mkEnableOption "kcore node agent";
      package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing bin/kcore-node-agent.";
      };
      configFile = lib.mkOption {
        type = lib.types.path;
        default = "/etc/kcore/node-agent.yaml";
      };
    };

    controller = {
      enable = lib.mkEnableOption "kcore controller";
      package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing bin/kcore-controller.";
      };
      configFile = lib.mkOption {
        type = lib.types.path;
        default = "/etc/kcore/controller.yaml";
      };
    };

    dashboard = {
      enable = lib.mkEnableOption "the web dashboard on the controller's gRPC-web listener";
      package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing share/kcore-dashboard, the static dashboard files.";
      };
      listenAddress = lib.mkOption {
        type = lib.types.str;
        default = "0.0.0.0:9443";
        description = "Address of the controller's web listener (gRPC-web, bearer tokens, TLS).";
      };
      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Open the web listener port in the host firewall.";
      };
    };

    console = {
      enable = lib.mkEnableOption "kcore appliance console";
      package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing bin/kcore-console.";
      };
    };

    kctl = {
      package = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Package providing bin/kctl.";
      };
    };

    bootConfirm = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Bless a one-shot boot only after the node agent is up on the staged generation.";
      };
      timeoutSeconds = lib.mkOption {
        type = lib.types.ints.positive;
        default = 600;
      };
    };
  };

  config = lib.mkMerge [
    {
      assertions = [
        {
          assertion = cfg.dashboard.enable -> cfg.controller.enable;
          message = "services.kcore.dashboard is served by the controller; enable services.kcore.controller too.";
        }
      ];
      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];
      # A fresh install must be able to nixos-rebuild without a channel and
      # without a hand-written NIX_PATH. The nixpkgs source stays in the
      # system closure because this path is part of the generation.
      nix.nixPath = [
        "nixos-config=/etc/nixos/configuration.nix"
        "nixpkgs=${pkgs.path}"
      ];
      # A later nixpkgs default of broker makes nixos-rebuild refuse to switch
      # a running node. Callers do not set this.
      services.dbus.implementation = lib.mkDefault "dbus";

      boot.loader.systemd-boot.enable = lib.mkDefault true;
      boot.loader.systemd-boot.configurationLimit = lib.mkDefault 8;
      boot.loader.timeout = lib.mkDefault 5;
      boot.loader.efi.canTouchEfiVariables = lib.mkDefault true;
      boot.kernel.sysctl."kernel.panic" = lib.mkDefault 10;
      # Plain priority outranks the mkDefault copies in the VM networking
      # module and the Kubernetes module. Making this mkDefault too makes
      # those copies conflict with each other.
      boot.kernel.sysctl."net.ipv4.ip_forward" = 1;
      boot.kernelModules = [
        "kvm"
        "kvm-intel"
        "kvm-amd"
        "tap"
        "tun"
        "br_netfilter"
      ];

      environment.etc."kcore/version" = lib.mkIf (cfg.version != "") {
        text = "${cfg.version}\n";
        mode = "0644";
      };

      systemd.tmpfiles.rules = [
        "d /var/lib/kcore 0755 root root -"
        "d /var/lib/kcore/images 0755 root root -"
        "d /var/lib/kcore/volumes 0755 root root -"
        "d /var/lib/kcore/updates 0755 root root -"
        "d /etc/kcore 0755 root root -"
        "d /run/kcore 0755 root root -"
      ];

      systemd.services.kcore-br0-forward-delay = {
        description = "Drop the bridge STP delay before DHCP";
        after = [ "network-setup.service" ];
        before = [ "dhcpcd.service" ];
        wantedBy = [ "multi-user.target" ];
        serviceConfig.Type = "oneshot";
        path = [ pkgs.iproute2 ];
        script = "ip link set br0 type bridge forward_delay 0 stp_state 0 || true";
      };
    }
    (lib.mkIf cfg.nodeAgent.enable {
      systemd.services.kcore-node-agent = {
        description = "kcore Node Agent";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
        ]
        ++ lib.optional cfg.controller.enable "kcore-controller.service";
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = "${cfg.nodeAgent.package}/bin/kcore-node-agent --config ${cfg.nodeAgent.configFile}";
          Environment = "PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin:/usr/bin:/bin";
          Restart = "always";
          RestartSec = "10s";
          User = "root";
          LimitNOFILE = 65536;
        };
      };
    })
    (lib.mkIf cfg.controller.enable {
      systemd.services.kcore-controller = {
        description = "kcore Controller";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = "${cfg.controller.package}/bin/kcore-controller --config ${cfg.controller.configFile}";
          Restart = "always";
          RestartSec = "10s";
          User = "root";
          LimitNOFILE = 65536;
        };
        environment = lib.mkIf cfg.dashboard.enable {
          KCORE_WEB_ENABLED = "true";
          KCORE_WEB_LISTEN_ADDR = cfg.dashboard.listenAddress;
          KCORE_WEB_UI_DIR = "${cfg.dashboard.package}/share/kcore-dashboard";
        };
      };
      networking.firewall.allowedTCPPorts =
        lib.mkIf (cfg.dashboard.enable && cfg.dashboard.openFirewall)
          [
            (lib.toInt (lib.last (lib.splitString ":" cfg.dashboard.listenAddress)))
          ];
    })
    (lib.mkIf cfg.console.enable {
      environment.systemPackages = [ cfg.console.package ];
      # The branding unit otherwise stays off, or falls back to /opt/kcore
      # and fails the switch when that path is not there.
      services.kcore-console = {
        enable = true;
        package = cfg.console.package;
      };
    })
    {
      environment.systemPackages = lib.mkIf (cfg.kctl.package != null) [ cfg.kctl.package ];
      system.activationScripts.kctl-path = lib.mkIf (cfg.kctl.package != null) {
        text = ''
          mkdir -p /usr/local/bin
          ln -sfn ${cfg.kctl.package}/bin/kctl /usr/local/bin/kctl
        '';
      };
    }
    (lib.mkIf cfg.bootConfirm.enable {
      systemd.services.kcore-boot-confirm = {
        description = "Bless the booted kcore generation";
        wantedBy = [ "multi-user.target" ];
        after = [
          "kcore-node-agent.service"
          "network-online.target"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = "${confirmScript}";
      };
      systemd.timers.kcore-boot-confirm-timeout = {
        description = "Reboot an unblessed staged generation";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "${toString cfg.bootConfirm.timeoutSeconds}s";
          Unit = "kcore-boot-confirm-timeout.service";
        };
      };
      systemd.services.kcore-boot-confirm-timeout = {
        description = "Reboot when the staged generation was not blessed";
        serviceConfig.Type = "oneshot";
        script = "${timeoutScript}";
      };
    })
  ];
}
