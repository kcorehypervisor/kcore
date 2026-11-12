# Multi-network DHCP and DNS. Kea and PowerDNS come from nixpkgs.
# kcore-provisioning is the gRPC API that renders one subnet per network.
{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.services.kcore.provisioning;
  seedJson = builtins.toJSON {
    upstream = cfg.upstreamDns;
    inherit (cfg) authoritative;
    networks = lib.sort (a: b: a.name < b.name) (
      lib.mapAttrsToList (name: network: {
        inherit name;
        inherit (network)
          interface
          cidr
          gateway
          domain
          ;
        dns_servers = network.dnsServers;
        pools = map (pool: {
          inherit (pool) start end;
        }) network.pools;
        dhcp_enabled = network.dhcpEnabled;
        valid_lifetime = network.validLifetime;
        next_server = network.nextServer;
        boot_file_name = network.bootFileName;
      }) cfg.networks
    );
    records = map (record: {
      inherit (record)
        zone
        name
        content
        ttl
        rrtype
        ;
    }) cfg.records;
  };
  pdnsInit = pkgs.writeShellScript "kcore-pdns-init" ''
    set -eu
    db=/var/lib/pdns/pdns.sqlite3
    if [ ! -s "$db" ]; then
      ${pkgs.sqlite}/bin/sqlite3 "$db" < ${pkgs.pdns}/share/doc/pdns/schema.sqlite3.sql
    fi
  '';

  # network-online.target alone is not enough here: lan10g can still be
  # operstate!=up (or missing its address) when Kea/pdns start after reboot.
  dhcpInterfaces = lib.unique (
    lib.filter (iface: iface != "") (lib.mapAttrsToList (_: network: network.interface) cfg.networks)
  );
  waitAddresses = lib.unique (
    lib.filter (addr: addr != "127.0.0.1" && addr != "::1" && addr != "") (
      (lib.concatLists (lib.mapAttrsToList (_: network: network.dnsServers) cfg.networks))
      ++ map (lib.head) (
        builtins.filter (parts: builtins.length parts > 0) (map (lib.splitString ":") cfg.listenAddresses)
      )
    )
  );
  waitNetwork = pkgs.writeShellScript "kcore-provisioning-wait-network" ''
    set -eu
    timeout_sec=90
    deadline=$((SECONDS + timeout_sec))
    ip=${pkgs.iproute2}/bin/ip

    for iface in ${lib.escapeShellArgs dhcpInterfaces}; do
      echo "waiting for interface ''${iface} (operstate=up)"
      while true; do
        if [ -r "/sys/class/net/''${iface}/operstate" ]; then
          state=$(cat "/sys/class/net/''${iface}/operstate")
          if [ "$state" = "up" ]; then
            break
          fi
        else
          state=missing
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
          echo "timed out after ''${timeout_sec}s waiting for ''${iface} (operstate=$state)" >&2
          exit 1
        fi
        sleep 0.5
      done
    done

    for addr in ${lib.escapeShellArgs waitAddresses}; do
      echo "waiting for address ''${addr}"
      while true; do
        if "$ip" -4 -o addr show | ${pkgs.gnugrep}/bin/grep -F -q " ''${addr}/"; then
          break
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
          echo "timed out after ''${timeout_sec}s waiting for address ''${addr}" >&2
          exit 1
        fi
        sleep 0.5
      done
    done
  '';
in
{
  options.services.kcore.provisioning = {
    enable = lib.mkEnableOption "kcore PowerDNS and Kea provisioner";

    package = lib.mkOption {
      type = lib.types.package;
      description = "kcore-provisioning package.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/kcore-provisioning";
      description = "Catalog, Kea configuration, and recursor files.";
    };

    listenAddresses = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "127.0.0.1:9443" ];
      example = [
        "127.0.0.1:9443"
        "192.168.40.10:9443"
      ];
      description = "gRPC listen addresses. Clients send Authorization: Bearer and the token in the state directory.";
    };

    upstreamDns = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "1.1.1.1"
        "8.8.8.8"
      ];
      description = "Resolvers the recursor forwards to for names outside the local zones.";
    };

    authoritative = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:5300";
      description = "Authoritative PowerDNS address. The recursor forwards local zones here.";
    };

    records = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            zone = lib.mkOption {
              type = lib.types.str;
              description = "Zone the record belongs to, without a trailing dot.";
            };
            name = lib.mkOption {
              type = lib.types.str;
              description = "Record name. A relative name is stored as given.";
            };
            rrtype = lib.mkOption {
              type = lib.types.str;
              default = "A";
              description = "DNS record type.";
            };
            content = lib.mkOption {
              type = lib.types.str;
              description = "Record data.";
            };
            ttl = lib.mkOption {
              type = lib.types.ints.positive;
              default = 3600;
              description = "Time to live, in seconds.";
            };
          };
        }
      );
      default = [ ];
      description = "Extra records inserted when they are not already in the catalog.";
    };

    networks = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            interface = lib.mkOption {
              type = lib.types.str;
              default = "";
              description = "Host interface Kea binds when DHCP is enabled.";
            };
            cidr = lib.mkOption {
              type = lib.types.str;
              example = "192.168.40.0/24";
              description = "IPv4 subnet.";
            };
            gateway = lib.mkOption {
              type = lib.types.str;
              description = "Default router advertised to clients.";
            };
            domain = lib.mkOption {
              type = lib.types.str;
              default = "home.arpa";
              description = "Forward zone for this network.";
            };
            dnsServers = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              description = "DNS servers advertised by DHCP.";
            };
            pools = lib.mkOption {
              type = lib.types.listOf (
                lib.types.submodule {
                  options = {
                    start = lib.mkOption {
                      type = lib.types.str;
                      description = "First dynamic address.";
                    };
                    end = lib.mkOption {
                      type = lib.types.str;
                      description = "Last dynamic address.";
                    };
                  };
                }
              );
              default = [ ];
              description = "Dynamic address pools. Reservations may sit outside them.";
            };
            dhcpEnabled = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = "Serve DHCP for this network. Disabled networks stay in the catalog until an interface is attached.";
            };
            validLifetime = lib.mkOption {
              type = lib.types.ints.positive;
              default = 1800;
              description = "DHCP lease lifetime in seconds.";
            };
            nextServer = lib.mkOption {
              type = lib.types.str;
              default = "";
              description = "DHCP next-server address.";
            };
            bootFileName = lib.mkOption {
              type = lib.types.str;
              default = "";
              description = ''
                DHCP boot file name (option 67). An http:// or https:// URL, such as
                `http://192.168.40.10:8081/boot/efi/x86_64/grub.efi`, goes only to UEFI HTTP boot
                clients (vendor class HTTPClient) on this network, and Kea answers them with the
                HTTPClient vendor class. Any other value is offered to every client.
              '';
            };
          };
        }
      );
      default = { };
      description = "One entry per L3 network. A second interface is another entry; the daemon does not need a different build.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.networks != { };
        message = "services.kcore.provisioning.networks needs at least one network";
      }
      {
        assertion = cfg.upstreamDns != [ ];
        message = "services.kcore.provisioning.upstreamDns needs at least one resolver";
      }
    ]
    ++ lib.mapAttrsToList (name: network: {
      assertion =
        !network.dhcpEnabled
        || (network.interface != "" && network.pools != [ ] && network.dnsServers != [ ]);
      message = "network ${name} serves DHCP and needs an interface, a pool, and DNS servers";
    }) cfg.networks;

    environment.etc."kcore-provisioning/seed.json".text = seedJson;
    environment.systemPackages = [
      cfg.package
      pkgs.kea
      pkgs.pdns
      pkgs.pdns-recursor
    ];

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0750 root root -"
      "d /var/lib/kea 0750 root root -"
      "d /var/lib/pdns 0750 root root -"
      "d /run/kea 0750 root root -"
      "d /run/pdns-recursor 0755 root root -"
    ];

    systemd.services.kcore-provisioning-prepare = {
      description = "Render Kea and PowerDNS configuration";
      wantedBy = [ "multi-user.target" ];
      before = [
        "kcore-pdns.service"
        "kcore-pdns-recursor.service"
        "kcore-kea-dhcp4.service"
        "kcore-provisioning.service"
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = utils.escapeSystemdExecArgs [
          "${cfg.package}/bin/kcore-provisioning"
          "prepare"
          "--state-dir"
          cfg.stateDir
          "--seed"
          "/etc/kcore-provisioning/seed.json"
        ];
      };
    };

    systemd.services.kcore-pdns = {
      description = "PowerDNS authoritative server";
      wantedBy = [ "multi-user.target" ];
      after = [ "kcore-provisioning-prepare.service" ];
      requires = [ "kcore-provisioning-prepare.service" ];
      serviceConfig = {
        ExecStartPre = pdnsInit;
        ExecStart = utils.escapeSystemdExecArgs [
          "${pkgs.pdns}/bin/pdns_server"
          "--config-dir=${cfg.stateDir}"
          "--guardian=no"
          "--daemon=no"
          "--disable-syslog"
          "--log-timestamp=no"
          "--write-pid=no"
        ];
        Restart = "on-failure";
        RestartSec = 2;
      };
    };

    systemd.services.kcore-pdns-recursor = {
      description = "PowerDNS recursor";
      wantedBy = [ "multi-user.target" ];
      after = [
        "kcore-provisioning-prepare.service"
        "kcore-pdns.service"
        "network-online.target"
      ];
      requires = [ "kcore-provisioning-prepare.service" ];
      wants = [ "network-online.target" ];
      startLimitBurst = 10;
      startLimitIntervalSec = 120;
      serviceConfig = {
        RuntimeDirectory = "pdns-recursor";
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        ExecStartPre = [ "${waitNetwork}" ];
        ExecStart = utils.escapeSystemdExecArgs [
          "${pkgs.pdns-recursor}/bin/pdns_recursor"
          "--config-dir=${cfg.stateDir}"
        ];
        ExecReload = [
          "${pkgs.pdns-recursor}/bin/rec_control --socket-dir=/run/pdns-recursor reload-zones"
          "${pkgs.pdns-recursor}/bin/rec_control --socket-dir=/run/pdns-recursor reload-lua-script"
        ];
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    systemd.services.kcore-kea-dhcp4 = {
      description = "Kea DHCPv4";
      wantedBy = [ "multi-user.target" ];
      after = [
        "kcore-provisioning-prepare.service"
        "network-online.target"
      ];
      requires = [ "kcore-provisioning-prepare.service" ];
      wants = [ "network-online.target" ];
      startLimitBurst = 10;
      startLimitIntervalSec = 120;
      serviceConfig = {
        RuntimeDirectory = "kea";
        RuntimeDirectoryMode = "0750";
        AmbientCapabilities = [
          "CAP_NET_BIND_SERVICE"
          "CAP_NET_RAW"
        ];
        CapabilityBoundingSet = [
          "CAP_NET_BIND_SERVICE"
          "CAP_NET_RAW"
        ];
        ExecStartPre = [
          "${waitNetwork}"
          (utils.escapeSystemdExecArgs [
            "${pkgs.kea}/bin/kea-dhcp4"
            "-t"
            "${cfg.stateDir}/kea-dhcp4.json"
          ])
        ];
        ExecStart = utils.escapeSystemdExecArgs [
          "${pkgs.kea}/bin/kea-dhcp4"
          "-c"
          "${cfg.stateDir}/kea-dhcp4.json"
        ];
        ExecReload = utils.escapeSystemdExecArgs [
          "${cfg.package}/bin/kcore-provisioning"
          "reload-kea"
        ];
        Restart = "on-failure";
        RestartSec = 5;
      };
    };

    systemd.services.kcore-provisioning = {
      description = "kcore network provisioner";
      wantedBy = [ "multi-user.target" ];
      after = [
        "kcore-provisioning-prepare.service"
        "kcore-pdns.service"
        "network-online.target"
      ];
      requires = [ "kcore-provisioning-prepare.service" ];
      wants = [
        "kcore-pdns.service"
        "network-online.target"
      ];
      serviceConfig = {
        ExecStartPre = [ "${waitNetwork}" ];
        ExecStart = utils.escapeSystemdExecArgs (
          [
            "${cfg.package}/bin/kcore-provisioning"
            "serve"
            "--state-dir"
            cfg.stateDir
          ]
          ++ lib.concatMap (addr: [
            "--listen"
            addr
          ]) cfg.listenAddresses
        );
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
