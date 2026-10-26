# Ingress gateway: Caddy with the Coraza WAF as an HTTPS load balancer on the
# host. Nix owns the rarely-changing parts (package, unit hardening, firewall,
# VIP, state directories). The routing table is Caddy JSON rendered by the
# controller and loaded through the admin socket, so a route change never needs
# a nixos-rebuild. See docs/ingress-waf-load-balancer-plan.md.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.ingress;

  stateDir = "/var/lib/kcore/ingress";
  runtimeDir = "/run/kcore-ingress";
  adminSocket = "${runtimeDir}/admin.sock";
  configPath = "${stateDir}/config.json";
  certStore = "${stateDir}/certmagic";

  # Used until the controller pushes the first rendered configuration. It
  # carries the admin endpoint so the agent can reach a freshly started Caddy.
  bootstrapConfig = pkgs.writeText "kcore-ingress-bootstrap.json" (
    builtins.toJSON {
      admin.listen = "unix/${adminSocket}|0600";
    }
  );

  keepalivedNotrack = import ./kcore-keepalived-notrack.nix { inherit pkgs; };

  # Caddy's metrics listen on every address; only the hosts that may scrape
  # node_exporter can reach them. The metrics module is optional.
  scrapeSources = lib.unique (
    builtins.filter (s: s != "") (
      lib.attrByPath [ "kcore" "metrics" "allowedScrapeSources" ] [ ] config
    )
  );
  listenerAddresses = lib.unique cfg.listenAddresses;
  inherit (cfg) tcpListeners;
  tcpPorts = [
    80
    443
  ];
  inputRules =
    lib.concatMapStrings (addr: ''
      ip daddr ${addr} tcp dport { ${
        lib.concatMapStringsSep ", " toString tcpPorts
      } } accept comment "kcore-ingress"
    '') listenerAddresses
    + lib.optionalString cfg.http3 (
      lib.concatMapStrings (addr: ''
        ip daddr ${addr} udp dport 443 accept comment "kcore-ingress-http3"
      '') listenerAddresses
    )
    + lib.concatMapStrings (l: ''
      ip daddr ${l.address} tcp dport ${toString l.port} accept comment "kcore-ingress-tcp"
    '') tcpListeners
    + lib.concatMapStrings (src: ''
      ip saddr ${src} tcp dport ${toString cfg.metricsPort} accept comment "kcore-ingress-metrics"
    '') scrapeSources;
  iptablesRules =
    verb:
    lib.concatMapStrings (
      addr:
      lib.concatMapStrings (port: ''
        iptables -${verb} nixos-fw -d ${lib.escapeShellArg addr} -p tcp --dport ${toString port} -j nixos-fw-accept ${
          lib.optionalString (verb == "D") "|| true"
        }
      '') tcpPorts
      + lib.optionalString cfg.http3 ''
        iptables -${verb} nixos-fw -d ${lib.escapeShellArg addr} -p udp --dport 443 -j nixos-fw-accept ${
          lib.optionalString (verb == "D") "|| true"
        }
      ''
    ) listenerAddresses
    + lib.concatMapStrings (l: ''
      iptables -${verb} nixos-fw -d ${lib.escapeShellArg l.address} -p tcp --dport ${toString l.port} -j nixos-fw-accept ${
        lib.optionalString (verb == "D") "|| true"
      }
    '') tcpListeners
    + lib.concatMapStrings (src: ''
      iptables -${verb} nixos-fw -s ${lib.escapeShellArg src} -p tcp --dport ${toString cfg.metricsPort} -j nixos-fw-accept ${
        lib.optionalString (verb == "D") "|| true"
      }
    '') scrapeSources;

  peers = lib.filter (ip: ip != cfg.vip.sourceAddress) cfg.vip.peers;
  keepalivedConf = pkgs.writeText "keepalived-ingress.conf" ''
    global_defs {
      router_id ${config.networking.hostName}
      enable_script_security
      script_user root
    }
    vrrp_script kcore_ingress_caddy {
      script "${pkgs.curl}/bin/curl -fsS --max-time 2 --unix-socket ${adminSocket} http://localhost/config/ -o /dev/null"
      interval 2
      fall 2
      rise 2
    }
    vrrp_instance kcore_ingress {
      state BACKUP
      interface ${cfg.vip.interface}
      virtual_router_id ${toString cfg.vip.virtualRouterId}
      priority ${toString cfg.vip.priority}
      advert_int 1
      nopreempt
      unicast_src_ip ${cfg.vip.sourceAddress}
      unicast_peer {
    ${lib.concatMapStrings (ip: "    ${ip}\n") peers}
      }
      track_script {
        kcore_ingress_caddy
      }
      virtual_ipaddress {
        ${cfg.vip.address}/${toString cfg.vip.prefixLength}
      }
    }
  '';
in
{
  options.kcore.ingress = {
    enable = lib.mkEnableOption "the kcore ingress gateway (Caddy + Coraza WAF)";

    package = lib.mkOption {
      type = lib.types.package;
      default = import ./kcore-ingress-caddy.nix { inherit pkgs; };
      defaultText = lib.literalExpression "import ./kcore-ingress-caddy.nix { inherit pkgs; }";
      description = "Caddy build with the Coraza, rate-limit, layer-4 and DNS-01 plugins.";
    };

    listenAddresses = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "203.0.113.10" ];
      description = ''
        Addresses the gateway listens on (a VIP or an address on this node).
        The firewall opens ports 80 and 443 on exactly these destinations.
      '';
    };

    tcpListeners = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            address = lib.mkOption {
              type = lib.types.str;
              description = "Address the TCP route listens on.";
            };
            port = lib.mkOption {
              type = lib.types.port;
              description = "TCP port of the route.";
            };
          };
        }
      );
      default = [ ];
      description = "Address and port pairs carried by layer-4 (TCP) routes; opened in the firewall.";
    };

    http3 = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Open UDP 443 on the listener addresses for HTTP/3.";
    };

    vip = {
      address = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Shared address held by one gateway node through keepalived. Null disables VRRP.";
      };
      prefixLength = lib.mkOption {
        type = lib.types.ints.between 1 32;
        default = 24;
        description = "Prefix length of the VIP.";
      };
      interface = lib.mkOption {
        type = lib.types.str;
        default = "eth0";
        description = "Interface that carries VRRP and the VIP.";
      };
      virtualRouterId = lib.mkOption {
        type = lib.types.ints.between 1 255;
        default = 61;
        description = "VRRP virtual router id. Must differ from every other VRRP instance on the segment.";
      };
      priority = lib.mkOption {
        type = lib.types.ints.between 1 254;
        default = 100;
        description = "VRRP priority; the other node of a pair uses a lower value.";
      };
      sourceAddress = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "This node's unicast VRRP source address.";
      };
      peers = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Unicast VRRP peer addresses (this node's own address is ignored).";
      };
    };

    acme = {
      email = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "ACME account contact. Informational: the controller renders it into the Caddy JSON.";
      };
      credentialsFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        example = "/etc/kcore/ingress/dns.env";
        description = ''
          Environment file with DNS-01 provider credentials, written by the
          node agent with mode 0600 outside the Nix store. Optional: the unit
          starts without it.
        '';
      };
    };

    storage = {
      mode = lib.mkOption {
        type = lib.types.enum [
          "local"
          "shared-filesystem"
          "replicated"
        ];
        default = "local";
        description = ''
          Where CertMagic keeps certificates. `shared-filesystem` expects
          `sharedFilesystemMount` to be mounted on both gateway nodes;
          `replicated` keeps a local directory the node agent syncs to the standby.
        '';
      };
      sharedFilesystemMount = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Mount point of the CephFS SharedFilesystem used when storage.mode = shared-filesystem.";
      };
    };

    metricsPort = lib.mkOption {
      type = lib.types.port;
      default = 2020;
      description = ''
        Port of Caddy's Prometheus metrics endpoint. It listens on every address
        but the firewall opens it only to kcore.metrics.allowedScrapeSources.
      '';
    };

    wafAuditLogPath = lib.mkOption {
      type = lib.types.str;
      default = "/var/log/kcore/ingress/waf.jsonl";
      description = "Coraza JSON audit log the node agent tails.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions = [
          {
            assertion = cfg.listenAddresses != [ ];
            message = "kcore.ingress.listenAddresses must name at least one address.";
          }
          {
            assertion = cfg.storage.mode != "shared-filesystem" || cfg.storage.sharedFilesystemMount != null;
            message = "kcore.ingress.storage.sharedFilesystemMount is required for storage.mode = shared-filesystem.";
          }
          {
            assertion = cfg.vip.address == null || (cfg.vip.sourceAddress != "" && cfg.vip.peers != [ ]);
            message = "kcore.ingress.vip needs sourceAddress and at least one peer.";
          }
        ];

        # Same rationale as kcore-metrics: stay on the iptables backend.
        networking.nftables.enable = lib.mkForce false;

        systemd.services.kcore-ingress = {
          description = "kcore ingress gateway (Caddy + Coraza WAF)";
          wantedBy = [ "multi-user.target" ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];

          serviceConfig = {
            ExecStartPre = "${pkgs.runtimeShell} -c 'test -s ${configPath} || cp ${bootstrapConfig} ${configPath}'";
            ExecStart = "${cfg.package}/bin/caddy run --config ${configPath}";
            ExecReload = "${pkgs.curl}/bin/curl -fsS --unix-socket ${adminSocket} -X POST -H 'Content-Type: application/json' --data-binary @${configPath} http://localhost/load";
            EnvironmentFile = lib.optional (cfg.acme.credentialsFile != null) "-${cfg.acme.credentialsFile}";
            Restart = "on-failure";
            RestartSec = "2s";
            LimitNOFILE = 1048576;

            DynamicUser = true;
            StateDirectory = "kcore/ingress";
            LogsDirectory = "kcore/ingress";
            RuntimeDirectory = "kcore-ingress";
            AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
            CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
            NoNewPrivileges = true;
            ProtectSystem = "strict";
            ProtectHome = true;
            PrivateTmp = true;
            PrivateDevices = true;
            ProtectKernelTunables = true;
            ProtectKernelModules = true;
            ProtectControlGroups = true;
            RestrictAddressFamilies = [
              "AF_INET"
              "AF_INET6"
              "AF_UNIX"
            ];
            RestrictNamespaces = true;
            LockPersonality = true;
            MemoryDenyWriteExecute = true;
            SystemCallFilter = [ "@system-service" ];
          };
          environment = {
            XDG_DATA_HOME = stateDir;
            XDG_CONFIG_HOME = stateDir;
            KCORE_INGRESS_CERT_STORE =
              if cfg.storage.mode == "shared-filesystem" then cfg.storage.sharedFilesystemMount else certStore;
            KCORE_INGRESS_WAF_AUDIT_LOG = cfg.wafAuditLogPath;
            KCORE_INGRESS_METRICS_ADDR = "0.0.0.0:${toString cfg.metricsPort}";
          };
        };

        networking.firewall.extraCommands = lib.mkIf (!config.networking.nftables.enable) (
          lib.mkAfter (iptablesRules "A")
        );
        networking.firewall.extraStopCommands = lib.mkIf (!config.networking.nftables.enable) (
          lib.mkAfter (iptablesRules "D")
        );
        networking.firewall.extraInputRules = lib.mkIf config.networking.nftables.enable inputRules;
      }

      (lib.mkIf (cfg.vip.address != null) {
        environment.systemPackages = [ pkgs.keepalived ];

        systemd.services.keepalived-ingress = {
          description = "Keepalived for the kcore ingress VIP";
          wantedBy = [ "multi-user.target" ];
          after = [
            "network-online.target"
            "kcore-ingress.service"
          ];
          wants = [ "network-online.target" ];
          serviceConfig = {
            Type = "forking";
            PIDFile = "/run/keepalived-ingress.pid";
            ExecStartPre = keepalivedNotrack;
            ExecStart = "${pkgs.keepalived}/bin/keepalived -f ${keepalivedConf} -p /run/keepalived-ingress.pid -r /run/vrrp-ingress.pid";
            Restart = "always";
            RestartSec = "5s";
          };
        };

        # Leave the raw NOTRACK rules in place when the firewall reloads, as
        # the Kubernetes module does: removing them lets VM masquerade rewrite
        # the adverts.
        networking.firewall.extraCommands = lib.mkIf (!config.networking.nftables.enable) ''
          iptables -C nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null || iptables -A nixos-fw -p 112 -j nixos-fw-accept
        '';
      })
    ]
  );
}
