# Installed kcore node. Packages come from the host flake's kcore input.
# Identity (hostname, LUKS, stateVersion) stays in the caller's node.nix.
{
  config,
  lib,
  options,
  pkgs,
  ...
}:
let
  cfg = config.services.kcore;
  candidateFiles = "flake.nix flake.lock legacy-configuration.nix";
  systemctl = "${pkgs.systemd}/bin/systemctl";
  bootctl = "${pkgs.systemd}/bin/bootctl";
  # Longer than the agent's RestartSec, so a crash loop never counts.
  healthyUpSeconds = 20;

  # boot-pending is written by the node agent when it stages a generation:
  # toplevel, previous (the generation that was running), staging (the host
  # flake copy with the target pins), and boot_id (the boot that staged it).
  pendingLib = ''
    pending=/var/lib/kcore/updates/boot-pending
    field() { sed -n "s/^$1=//p" "$pending" | head -n 1; }
    staged_this_boot() {
      [ "$(field boot_id)" = "$(cat /proc/sys/kernel/random/boot_id)" ]
    }
    # Boot the generation that ran before the update and record why.
    revert() {
      expected=$(field toplevel)
      previous=$(field previous)
      printf 'toplevel=%s\nreason=%s\n' "$expected" "$1" > /var/lib/kcore/updates/boot-failed
      rm -f "$pending"
      boot_into "$previous"
      ${systemctl} reboot
      exit 0
    }
    # Make $1 the newest generation and drop the pin the agent set for the
    # staged boot, so loader.conf names it and later generations still boot.
    boot_into() {
      if [ -n "$1" ] && [ -x "$1/bin/switch-to-configuration" ]; then
        ${config.nix.package}/bin/nix-env -p /nix/var/nix/profiles/system --set "$1" \
          && "$1/bin/switch-to-configuration" boot \
          && ${bootctl} set-default "" || true
      fi
    }
  '';

  # A unit that crash-loops under Restart=always is "active" for a moment
  # after every restart, so a unit must also have stayed up for a while.
  healthy = ''
    up_for() {
      ${systemctl} is-active --quiet "$1" || return 1
      since=$(${systemctl} show -p ActiveEnterTimestampMonotonic --value "$1")
      now=$(cut -d. -f1 /proc/uptime)
      [ $(( now - ''${since:-0} / 1000000 )) -ge ${toString healthyUpSeconds} ]
    }
    healthy() {
      up_for kcore-node-agent.service || return 1
      restarts=$(${systemctl} show -p NRestarts --value kcore-node-agent.service)
      [ "''${restarts:-0}" -lt 3 ] || return 1
      if ${systemctl} is-enabled --quiet kcore-controller.service 2>/dev/null; then
        up_for kcore-controller.service || return 1
      fi
      return 0
    }
  '';

  confirmScript = pkgs.writeShellScript "kcore-boot-confirm" ''
    set -u
    ${pendingLib}
    ${healthy}
    if [ ! -f "$pending" ] || staged_this_boot; then
      exit 0
    fi
    expected=$(field toplevel)
    booted=$(readlink -f /run/booted-system || true)
    if [ "$booted" != "$expected" ]; then
      # systemd-boot fell back after the one-shot boot failed (panic, hang
      # reset), or someone picked another entry.
      printf 'toplevel=%s\nreason=%s\n' "$expected" "booted $booted instead of the staged generation" \
        > /var/lib/kcore/updates/boot-failed
      rm -f "$pending"
      boot_into "$booted"
      exit 0
    fi
    deadline=$(( $(date +%s) + ${toString cfg.bootConfirm.timeoutSeconds} ))
    ok=0
    while [ "$ok" -lt 3 ]; do
      if healthy; then
        ok=$((ok + 1))
      else
        ok=0
        if [ "$(date +%s)" -ge "$deadline" ]; then
          revert "kcore services were not healthy within ${toString cfg.bootConfirm.timeoutSeconds}s"
        fi
      fi
      sleep 10
    done
    # The agent pinned the previous entry for the staged boot. Pinning this
    # one instead would override every later generation, so drop the pin:
    # loader.conf already names the staged generation.
    ${bootctl} set-default "" || true
    # The host flake moves to the update's pins only once its generation has
    # proven itself. Same file list and backup layout as system_update.rs.
    staging=$(field staging)
    if [ -n "$staging" ] && [ -f "$staging/flake.nix" ]; then
      previous="$(dirname "$staging")/previous"
      if [ ! -d "$previous" ]; then
        mkdir -p "$previous"
        for f in ${candidateFiles}; do
          if [ -f "/etc/nixos/$f" ]; then
            cp "/etc/nixos/$f" "$previous/$f"
          else
            touch "$previous/$f.absent"
          fi
        done
      fi
      for f in ${candidateFiles}; do
        if [ -f "$staging/$f" ]; then
          cp "$staging/$f" "/etc/nixos/.$f.kcore-tmp"
          mv -f "/etc/nixos/.$f.kcore-tmp" "/etc/nixos/$f"
        fi
      done
    fi
    mkdir -p /run/kcore
    touch /run/kcore/boot-blessed
    rm -f "$pending"
  '';

  # Safety net when kcore-boot-confirm itself never finishes.
  timeoutScript = pkgs.writeShellScript "kcore-boot-confirm-timeout" ''
    set -u
    ${pendingLib}
    if [ ! -f "$pending" ] || [ -f /run/kcore/boot-blessed ] || staged_this_boot; then
      exit 0
    fi
    if [ "$(readlink -f /run/booted-system || true)" = "$(field toplevel)" ]; then
      revert "the staged generation was not confirmed in time"
    fi
  '';
in
{
  imports = [
    ./ch-vm
    ./kcore-branding.nix
    ./kcore-ceph.nix
    ./kcore-ingress.nix
    ./kcore-metrics.nix
    ./kcore-postgresql-cluster.nix
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
      enable = lib.mkOption {
        type = lib.types.bool;
        # Host flakes written before this default may not set the package.
        default = cfg.controller.enable && options.services.kcore.dashboard.package.isDefined;
        defaultText = lib.literalExpression "config.services.kcore.controller.enable";
        description = "Serve the web dashboard on the controller's gRPC-web listener.";
      };
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
        description = ''
          After a staged update boots, wait for the node agent (and the controller
          when enabled) to stay up. Keep the generation when they do; otherwise make
          the previous generation the boot default and reboot into it.
        '';
      };
      timeoutSeconds = lib.mkOption {
        type = lib.types.ints.positive;
        default = 600;
        description = "How long the staged generation has to become healthy.";
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
        # Version updates resolve a release tag with `nix flake metadata`.
        path = [
          config.nix.package
          pkgs.git
        ];
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
        description = "Confirm a staged kcore generation or boot the previous one";
        wantedBy = [ "multi-user.target" ];
        after = [
          "kcore-node-agent.service"
          "kcore-controller.service"
        ];
        # Only boots matter. A switch that changes this unit must not re-run it.
        restartIfChanged = false;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = "${confirmScript}";
      };
      systemd.timers.kcore-boot-confirm-timeout = {
        description = "Boot the previous generation when a staged one was never confirmed";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "${toString (cfg.bootConfirm.timeoutSeconds + 300)}s";
          Unit = "kcore-boot-confirm-timeout.service";
        };
      };
      systemd.services.kcore-boot-confirm-timeout = {
        description = "Boot the previous generation when a staged one was never confirmed";
        serviceConfig.Type = "oneshot";
        script = "${timeoutScript}";
      };
    })
  ];
}
