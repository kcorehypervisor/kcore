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
    ./kcore-dashboard.nix
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
      enable = lib.mkEnableOption "kcore dashboard";
      package = lib.mkOption {
        type = lib.types.package;
        description = "Package providing bin/kcore-dashboard.";
      };
      environmentFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = "/etc/kcore/dashboard.env";
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
      nix.settings.experimental-features = [
        "nix-command"
        "flakes"
      ];
      # A later nixpkgs default of broker makes nixos-rebuild refuse to switch
      # a running node. Callers do not set this.
      services.dbus.implementation = lib.mkDefault "dbus";

      boot.loader.systemd-boot.enable = lib.mkDefault true;
      boot.loader.systemd-boot.configurationLimit = lib.mkDefault 8;
      boot.loader.timeout = lib.mkDefault 5;
      boot.loader.efi.canTouchEfiVariables = lib.mkDefault true;
      boot.kernel.sysctl."kernel.panic" = lib.mkDefault 10;
      boot.kernel.sysctl."net.ipv4.ip_forward" = lib.mkDefault 1;
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
      };
      services.kcore-dashboard = lib.mkIf cfg.dashboard.enable {
        enable = true;
        inherit (cfg.dashboard) package environmentFile;
        openFirewall = true;
      };
    })
    (lib.mkIf cfg.console.enable {
      environment.systemPackages = [ cfg.console.package ];
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
