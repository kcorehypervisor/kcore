# PostgreSQL cluster member on a kcoreOS node: etcd, Patroni, PostgreSQL,
# and an optional writer VIP held by keepalived on the Patroni leader only.
#
# The node agent writes the values file (/etc/nixos/kcore-postgresql-cluster.nix).
# Patroni, not the controller, decides automatic failover through etcd. Units
# have restartIfChanged = false: a rebuild never bounces the database; the
# controller restarts members one at a time through the node agent.
#
# Secrets are files under secretsDir written by the agent. Nothing secret is
# interpolated into this module or the values file.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.postgresqlCluster;
  inherit (lib)
    concatMapStringsSep
    concatStringsSep
    mkEnableOption
    mkForce
    mkIf
    mkMerge
    mkOption
    optional
    optionalAttrs
    optionals
    types
    ;

  user = "kcore-pg";
  root = "/var/lib/kcore-postgresql/${cfg.clusterName}";
  runDir = "/run/kcore-postgresql";
  tlsDir = "${root}/tls";
  caFile = "${tlsDir}/ca.crt";
  certFile = "${tlsDir}/node.crt";
  keyFile = "${tlsDir}/node.key";
  scheme = if cfg.tls.enable then "https" else "http";

  majorOf = attr: lib.removePrefix "postgresql_" attr;
  major = majorOf cfg.package;
  dataPath = "${root}/pgdata/${major}";
  walPath = "${root}/pgwal/${major}";
  etcdPath = "${root}/etcd";

  withExtensions =
    attr:
    let
      base = pkgs.${attr};
    in
    if cfg.postgres.extensionPackages == [ ] then
      base
    else
      base.withPackages (ps: map (name: ps.${name}) cfg.postgres.extensionPackages);
  pgPackage = withExtensions cfg.package;
  previousPackage =
    if cfg.postgres.previousPackage == null then null else withExtensions cfg.postgres.previousPackage;

  etcdUnit = "kcore-pg-etcd-${cfg.clusterName}";
  keepalivedUnit = "keepalived-pg-${cfg.clusterName}";
  keepalivedNotrack = import ./kcore-keepalived-notrack.nix { inherit pkgs; };

  peerHosts = map (peer: peer.host) cfg.etcd.peers;
  memberHosts = lib.unique (peerHosts ++ cfg.postgres.members);
  otherHosts = builtins.filter (host: host != cfg.advertiseAddress) memberHosts;
  cidr = net: if lib.hasInfix "/" net then net else "${net}/32";
  hostType = if cfg.tls.enable then "hostssl" else "host";

  pgHba = [
    "local all all scram-sha-256"
    "${hostType} all all 127.0.0.1/32 scram-sha-256"
  ]
  ++ lib.concatMap (host: [
    "${hostType} replication replicator ${cidr host} scram-sha-256"
    "${hostType} all rewind ${cidr host} scram-sha-256"
    "${hostType} all all ${cidr host} scram-sha-256"
  ]) cfg.postgres.members
  ++ map (net: "${hostType} all all ${cidr net} scram-sha-256") cfg.postgres.clientNetworks;

  listenAddresses = concatStringsSep "," (
    [
      cfg.advertiseAddress
      "127.0.0.1"
    ]
    ++ optional cfg.vip.enable cfg.vip.address
  );

  sslParameters = optionalAttrs cfg.tls.enable {
    ssl = "on";
    ssl_cert_file = certFile;
    ssl_key_file = keyFile;
    ssl_ca_file = caFile;
  };

  clientSsl = optionalAttrs cfg.tls.enable {
    sslmode = "verify-full";
    sslrootcert = caFile;
  };

  etcdTls = optionalAttrs cfg.tls.enable {
    cacert = caFile;
    cert = certFile;
    key = keyFile;
  };

  initialCluster = concatMapStringsSep "," (
    peer: "${peer.name}=${scheme}://${peer.host}:${toString cfg.ports.etcdPeer}"
  ) cfg.etcd.peers;

  etcdFlags = [
    "--name=${cfg.etcd.name}"
    "--data-dir=${etcdPath}"
    "--listen-client-urls=${scheme}://${cfg.advertiseAddress}:${toString cfg.ports.etcdClient},${scheme}://127.0.0.1:${toString cfg.ports.etcdClient}"
    "--advertise-client-urls=${scheme}://${cfg.advertiseAddress}:${toString cfg.ports.etcdClient}"
    "--listen-peer-urls=${scheme}://${cfg.advertiseAddress}:${toString cfg.ports.etcdPeer}"
    "--initial-advertise-peer-urls=${scheme}://${cfg.advertiseAddress}:${toString cfg.ports.etcdPeer}"
    "--initial-cluster=${initialCluster}"
    "--initial-cluster-state=${cfg.etcd.initialClusterState}"
    "--initial-cluster-token=kcore-pg-${cfg.clusterName}"
    "--auto-compaction-mode=periodic"
    "--auto-compaction-retention=1h"
  ]
  ++ optionals cfg.tls.enable [
    "--cert-file=${certFile}"
    "--key-file=${keyFile}"
    "--trusted-ca-file=${caFile}"
    "--client-cert-auth"
    "--peer-cert-file=${certFile}"
    "--peer-key-file=${keyFile}"
    "--peer-trusted-ca-file=${caFile}"
    "--peer-client-cert-auth"
  ];

  primaryCheck =
    "${pkgs.curl}/bin/curl -fsS --max-time 2 -o /dev/null "
    + lib.optionalString cfg.tls.enable "--cacert ${caFile} "
    + "${scheme}://127.0.0.1:${toString cfg.ports.patroniApi}/primary";

  # Without a weight a failing check puts the instance in FAULT, so only the
  # member whose Patroni answers /primary is eligible for the address.
  keepalivedConf = pkgs.writeText "keepalived-pg-${cfg.clusterName}.conf" ''
    global_defs {
      router_id ${cfg.etcd.name}
      enable_script_security
      script_user root
    }
    vrrp_script kcore_pg_primary {
      script "${primaryCheck}"
      interval 2
      fall 2
      rise 1
    }
    vrrp_instance kcore_pg_${cfg.clusterName} {
      state BACKUP
      interface ${cfg.iface}
      virtual_router_id ${toString cfg.vip.virtualRouterId}
      priority 100
      advert_int 1
      unicast_src_ip ${cfg.advertiseAddress}
      unicast_peer {
    ${lib.concatMapStrings (ip: "    ${ip}\n") otherHosts}
      }
      track_script {
        kcore_pg_primary
      }
      virtual_ipaddress {
        ${cfg.vip.address}/${toString cfg.vip.prefixLength}
      }
    }
  '';

  memberPorts = [
    cfg.ports.postgres
    cfg.ports.patroniApi
    cfg.ports.etcdClient
    cfg.ports.etcdPeer
  ];

  iptablesRules =
    concatMapStringsSep "\n" (source: ''
      iptables -A nixos-fw -i ${lib.escapeShellArg cfg.iface} -s ${lib.escapeShellArg source} -p tcp -m multiport --dports ${
        concatMapStringsSep "," toString memberPorts
      } -j nixos-fw-accept
    '') otherHosts
    + concatMapStringsSep "\n" (net: ''
      iptables -A nixos-fw -i ${lib.escapeShellArg cfg.iface} -s ${lib.escapeShellArg net} -p tcp --dport ${toString cfg.ports.postgres} -j nixos-fw-accept
    '') cfg.postgres.clientNetworks
    + lib.optionalString cfg.vip.enable ''
      iptables -C nixos-fw -p 112 -j nixos-fw-accept 2>/dev/null || iptables -A nixos-fw -p 112 -j nixos-fw-accept
    '';

  nftRules =
    concatMapStringsSep "\n" (
      source:
      ''iifname "${cfg.iface}" ip saddr ${source} tcp dport { ${
        concatMapStringsSep ", " toString memberPorts
      } } accept comment "kcore-postgresql"''
    ) otherHosts
    + "\n"
    + concatMapStringsSep "\n" (
      net:
      ''iifname "${cfg.iface}" ip saddr ${net} tcp dport ${toString cfg.ports.postgres} accept comment "kcore-postgresql-client"''
    ) cfg.postgres.clientNetworks
    + lib.optionalString cfg.vip.enable ''

      ip protocol vrrp accept comment "kcore-postgresql-vrrp"
    '';

  # The agent writes secrets and TLS material as root before the first
  # rebuild creates the kcore-pg user. Each unit takes ownership with full
  # privileges ("+") before it starts.
  ownSecrets = pkgs.writeShellScript "kcore-pg-own-secrets" ''
    for dir in ${lib.escapeShellArg cfg.secretsDir} ${lib.escapeShellArg tlsDir}; do
      [ -d "$dir" ] || continue
      chown -R ${user}:${user} "$dir"
      chmod 0700 "$dir"
      find "$dir" -type f -exec chmod 0400 {} +
    done
    [ -f ${lib.escapeShellArg caFile} ] && chmod 0444 ${lib.escapeShellArg caFile}
    [ -f ${lib.escapeShellArg certFile} ] && chmod 0444 ${lib.escapeShellArg certFile}
    [ -d ${lib.escapeShellArg tlsDir} ] && chmod 0750 ${lib.escapeShellArg tlsDir}
    true
  '';

  underRoot = path: lib.hasPrefix "/var/lib/kcore-postgresql/" path;
  peerNames = map (peer: peer.name) cfg.etcd.peers;
  ports = lib.attrValues cfg.ports;

  peerType = types.submodule {
    options = {
      name = mkOption {
        type = types.str;
        description = "etcd member name (the kcore node id).";
      };
      host = mkOption {
        type = types.str;
        description = "Advertise address of the member.";
      };
    };
  };
in
{
  options.kcore.postgresqlCluster = {
    enable = mkEnableOption "a kcore PostgreSQL cluster member";

    clusterName = mkOption {
      type = types.strMatching "[a-z_][a-z0-9_]{0,62}";
      default = "pg";
      description = "Cluster name. Patroni scope and the directory under /var/lib/kcore-postgresql.";
    };

    generation = mkOption {
      type = types.int;
      default = 0;
      description = "Controller generation that rendered this file. Informational.";
    };

    package = mkOption {
      type = types.enum [
        "postgresql_14"
        "postgresql_15"
        "postgresql_16"
        "postgresql_17"
        "postgresql_18"
      ];
      default = "postgresql_17";
      description = "Versioned nixpkgs attribute. The unversioned alias is not accepted: a pin move would be an unplanned major upgrade.";
    };

    advertiseAddress = mkOption {
      type = types.str;
      default = "";
      description = "Static address on iface that other members dial.";
    };

    iface = mkOption {
      type = types.str;
      default = "";
      description = "Interface that carries cluster and client traffic. Firewall holes open on it only.";
    };

    secretsDir = mkOption {
      type = types.str;
      default = "${root}/secrets";
      defaultText = lib.literalExpression ''"/var/lib/kcore-postgresql/<cluster>/secrets"'';
      description = "Directory holding one password per file, written by the node agent.";
    };

    tls.enable = mkOption {
      type = types.bool;
      default = true;
      description = "TLS for PostgreSQL, replication, etcd and the Patroni REST API, from the cluster CA.";
    };

    ports = {
      postgres = mkOption {
        type = types.port;
        default = 5432;
        description = "PostgreSQL port.";
      };
      patroniApi = mkOption {
        type = types.port;
        default = 8008;
        description = "Patroni REST API port.";
      };
      etcdClient = mkOption {
        type = types.port;
        default = 2479;
        description = "etcd client port. Not 2379, which a Kubernetes control-plane member may hold.";
      };
      etcdPeer = mkOption {
        type = types.port;
        default = 2480;
        description = "etcd peer port.";
      };
    };

    etcd = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Run an etcd voter for this cluster on this node.";
      };
      name = mkOption {
        type = types.str;
        default = "";
        description = "This node's etcd member name and Patroni member name (the kcore node id).";
      };
      peers = mkOption {
        type = types.listOf peerType;
        default = [ ];
        description = "Every etcd voter. Identical on every member; Patroni dials all of them.";
      };
      initialClusterState = mkOption {
        type = types.enum [
          "new"
          "existing"
        ];
        default = "new";
        description = "\"existing\" when this voter joins through a replace.";
      };
    };

    postgres = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Run Patroni and PostgreSQL. false with etcd.enable is a witness.";
      };
      walVolume = mkOption {
        type = types.bool;
        default = false;
        description = "pg_wal lives under ${root}/pgwal/<major> (a separate volume mounted by the agent).";
      };
      members = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Advertise addresses of every PostgreSQL member, for pg_hba and the firewall.";
      };
      clientNetworks = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = "Addresses or CIDRs allowed to reach the PostgreSQL port.";
      };
      memoryLimitBytes = mkOption {
        type = types.ints.unsigned;
        default = 0;
        description = "MemoryMax for patroni.service (PostgreSQL included). 0 leaves it unset.";
      };
      watchdog = mkOption {
        type = types.enum [
          "off"
          "automatic"
          "required"
        ];
        default = "off";
        description = "Patroni watchdog. A reset reboots the whole host, guests included.";
      };
      tags = {
        nofailover = mkOption {
          type = types.bool;
          default = false;
          description = "Never promote this member.";
        };
        noloadbalance = mkOption {
          type = types.bool;
          default = false;
          description = "Leave this member out of read load balancing.";
        };
        failoverPriority = mkOption {
          type = types.ints.unsigned;
          default = 1;
          description = "Patroni failover_priority.";
        };
      };
      extensionPackages = mkOption {
        type = types.listOf (types.strMatching "[a-z][a-z0-9_]*");
        default = [ ];
        description = "Attributes of postgresqlPackages added with withPackages (contrib needs none).";
      };
      previousPackage = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Package of the previous major during a major upgrade, kept until finalize.";
      };
      bootstrapDcs = mkOption {
        inherit (pkgs.formats.yaml { }) type;
        default = { };
        description = "Patroni bootstrap.dcs. Only used by the first bootstrap; later changes go through PATCH /config.";
      };
    };

    volumes = mkOption {
      type = types.listOf (
        types.submodule {
          options = {
            mountPoint = mkOption {
              type = types.str;
              description = "Mount point under /var/lib/kcore-postgresql/<cluster>/.";
            };
            device = mkOption {
              type = types.str;
              description = "Block device (/dev/<vg>/<lv> or /dev/zvol/<pool>/<name>), created and formatted by the agent.";
            };
            fsType = mkOption {
              type = types.enum [
                "xfs"
                "ext4"
              ];
              default = "xfs";
              description = "Filesystem on the device.";
            };
          };
        }
      );
      default = [ ];
      description = "Data, WAL and etcd volumes of this member.";
    };

    vip = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Hold a writer VIP on the Patroni leader.";
      };
      address = mkOption {
        type = types.str;
        default = "";
        description = "VIP address without prefix.";
      };
      prefixLength = mkOption {
        type = types.ints.between 1 32;
        default = 24;
        description = "Prefix length of the VIP.";
      };
      virtualRouterId = mkOption {
        type = types.ints.between 1 255;
        default = 61;
        description = "VRRP router id. Must differ from the ingress and Kubernetes instances on the segment.";
      };
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = [
        {
          assertion = cfg.advertiseAddress != "" && cfg.iface != "";
          message = "kcore.postgresqlCluster needs advertiseAddress and iface.";
        }
        {
          assertion = cfg.etcd.enable || cfg.postgres.enable;
          message = "kcore.postgresqlCluster: enable etcd, postgres, or both.";
        }
        {
          assertion = lib.length cfg.etcd.peers == 1 || lib.mod (lib.length cfg.etcd.peers) 2 == 1;
          message = "kcore.postgresqlCluster.etcd.peers must have an odd number of voters.";
        }
        {
          assertion = !cfg.etcd.enable || lib.elem cfg.etcd.name peerNames;
          message = "kcore.postgresqlCluster.etcd.name must be one of etcd.peers.";
        }
        {
          assertion = cfg.etcd.name != "";
          message = "kcore.postgresqlCluster.etcd.name (the node id) is required.";
        }
        {
          assertion = lib.allUnique ports;
          message = "kcore.postgresqlCluster.ports must be pairwise distinct.";
        }
        {
          assertion = lib.all (v: lib.hasPrefix "${root}/" v.mountPoint) cfg.volumes;
          message = "kcore.postgresqlCluster.volumes must mount under ${root}/.";
        }
        {
          assertion = underRoot cfg.secretsDir;
          message = "kcore.postgresqlCluster.secretsDir must be under /var/lib/kcore-postgresql/.";
        }
        {
          assertion = !cfg.postgres.enable || !config.services.postgresql.enable;
          message = "kcore.postgresqlCluster cannot share a node with services.postgresql.";
        }
        {
          assertion = !cfg.vip.enable || (cfg.vip.address != "" && cfg.postgres.enable);
          message = "kcore.postgresqlCluster.vip needs an address and a PostgreSQL member.";
        }
      ];

      users.users.${user} = {
        isSystemUser = true;
        group = user;
        home = root;
        description = "kcore PostgreSQL cluster (Patroni, PostgreSQL, etcd)";
      };
      users.groups.${user} = { };

      systemd.tmpfiles.rules = [
        "d /var/lib/kcore-postgresql 0755 root root -"
        "d ${root} 0750 ${user} ${user} -"
        "d ${cfg.secretsDir} 0700 ${user} ${user} -"
        "d ${tlsDir} 0750 ${user} ${user} -"
      ];

      environment.systemPackages = [ pkgs.etcd ];

      fileSystems = lib.listToAttrs (
        map (v: {
          name = v.mountPoint;
          value = {
            inherit (v) device fsType;
            options = [
              "nofail"
              "noatime"
            ];
          };
        }) cfg.volumes
      );

      networking.firewall.extraCommands = mkIf (!config.networking.nftables.enable) iptablesRules;
      networking.firewall.extraInputRules = mkIf config.networking.nftables.enable nftRules;
    }

    (mkIf cfg.etcd.enable {
      systemd.tmpfiles.rules = [ "d ${etcdPath} 0700 ${user} ${user} -" ];

      systemd.services.${etcdUnit} = {
        description = "etcd for the kcore PostgreSQL cluster ${cfg.clusterName}";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        restartIfChanged = false;
        stopIfChanged = false;
        unitConfig.RequiresMountsFor = [ etcdPath ];
        serviceConfig = {
          Type = "notify";
          ExecStartPre = "+${ownSecrets}";
          User = user;
          Group = user;
          ExecStart = "${pkgs.etcd}/bin/etcd ${concatStringsSep " " etcdFlags}";
          Restart = "on-failure";
          RestartSec = "5s";
          LimitNOFILE = 40000;
        };
      };
    })

    (mkIf cfg.postgres.enable {
      systemd.tmpfiles.rules = [
        "d ${root}/patroni 0700 ${user} ${user} -"
        "d ${root}/pgdata 0700 ${user} ${user} -"
        "d ${dataPath} 0700 ${user} ${user} -"
        "d ${runDir} 2775 ${user} ${user} -"
      ]
      ++ optionals cfg.postgres.walVolume [
        "d ${root}/pgwal 0700 ${user} ${user} -"
        "d ${walPath} 0700 ${user} ${user} -"
      ];

      # Binaries the node agent calls (psql, pg_ctl, pg_controldata,
      # pg_upgrade, patronictl). The previous major stays reachable at a
      # fixed path until the upgrade is finalized.
      environment.systemPackages = [
        pgPackage
        pkgs.patroni
        pkgs.curl
      ];
      # Patroni re-reads the file it was started with on SIGHUP. Upstream
      # starts it with a store path, so a reload after a rebuild would read the
      # old settings. A stable /etc path makes POST /reload pick up pg_hba,
      # tags and etcd hosts without a restart.
      environment.etc = {
        "kcore-postgresql/current".source = pgPackage;
        "kcore-postgresql/patroni.yml".source =
          (pkgs.formats.yaml { }).generate "patroni-${cfg.clusterName}.yml"
            config.services.patroni.settings;
      }
      // optionalAttrs (previousPackage != null) {
        "kcore-postgresql/previous".source = previousPackage;
      };

      services.patroni = {
        enable = true;
        postgresqlPackage = pgPackage;
        postgresqlDataDir = dataPath;
        postgresqlPort = cfg.ports.postgres;
        inherit user;
        group = user;
        dataDir = "${root}/patroni";
        scope = cfg.clusterName;
        inherit (cfg.etcd) name;
        namespace = "/kcore-postgresql/";
        nodeIp = cfg.advertiseAddress;
        otherNodesIps = otherHosts;
        restApiPort = cfg.ports.patroniApi;
        softwareWatchdog = cfg.postgres.watchdog == "required";
        environmentFiles = {
          PATRONI_SUPERUSER_PASSWORD = "${cfg.secretsDir}/superuser.password";
          PATRONI_REPLICATION_PASSWORD = "${cfg.secretsDir}/replication.password";
          PATRONI_REWIND_PASSWORD = "${cfg.secretsDir}/rewind.password";
          PATRONI_RESTAPI_PASSWORD = "${cfg.secretsDir}/restapi.password";
        };
        settings = {
          etcd3 = {
            hosts = map (host: "${host}:${toString cfg.ports.etcdClient}") peerHosts;
            protocol = scheme;
          }
          // etcdTls;
          restapi = {
            listen = mkForce "0.0.0.0:${toString cfg.ports.patroniApi}";
            authentication.username = "kcore";
          }
          // optionalAttrs cfg.tls.enable {
            certfile = certFile;
            keyfile = keyFile;
            cafile = caFile;
          };
          ctl = optionalAttrs cfg.tls.enable { cacert = caFile; };
          postgresql = {
            listen = mkForce "${listenAddresses}:${toString cfg.ports.postgres}";
            use_unix_socket = true;
            use_unix_socket_repl = false;
            pg_hba = pgHba;
            authentication = {
              superuser.username = "postgres";
              replication = {
                username = "replicator";
              }
              // clientSsl;
              rewind = {
                username = "rewind";
              }
              // clientSsl;
            };
            parameters = {
              unix_socket_directories = runDir;
              password_encryption = "scram-sha-256";
            }
            // sslParameters;
            basebackup = [ { checkpoint = "fast"; } ];
          };
          bootstrap = {
            dcs = cfg.postgres.bootstrapDcs;
            initdb = [
              { encoding = "UTF8"; }
              { locale = "C.UTF-8"; }
              "data-checksums"
            ]
            ++ optional cfg.postgres.walVolume { waldir = walPath; };
          };
          tags = {
            inherit (cfg.postgres.tags) nofailover noloadbalance;
            failover_priority = cfg.postgres.tags.failoverPriority;
            clonefrom = false;
            nosync = false;
          };
        }
        // optionalAttrs (cfg.postgres.watchdog == "automatic") {
          watchdog = {
            mode = "automatic";
            device = "/dev/watchdog";
            safety_margin = 5;
          };
        };
      };

      boot.kernelModules = optional (cfg.postgres.watchdog != "off") "softdog";

      systemd.services.patroni = {
        script = mkForce ''
          ${concatStringsSep "\n" (
            lib.mapAttrsToList (
              name: path: ''export ${name}="$(< ${lib.escapeShellArg path})"''
            ) config.services.patroni.environmentFiles
          )}
          exec ${lib.getExe pkgs.patroni} /etc/kcore-postgresql/patroni.yml
        '';
        restartIfChanged = false;
        stopIfChanged = false;
        after = optional cfg.etcd.enable "${etcdUnit}.service";
        wants = optional cfg.etcd.enable "${etcdUnit}.service";
        unitConfig.RequiresMountsFor = [ dataPath ] ++ optional cfg.postgres.walVolume walPath;
        serviceConfig = {
          # A clean shutdown of a large shared_buffers can exceed upstream's 30 s.
          TimeoutSec = mkForce 300;
          ExecStartPre = "+${ownSecrets}";
          LimitNOFILE = 65536;
        }
        // optionalAttrs (cfg.postgres.memoryLimitBytes > 0) {
          MemoryMax = toString cfg.postgres.memoryLimitBytes;
        };
      };
    })

    (mkIf cfg.vip.enable {
      # PostgreSQL lists the VIP in listen_addresses before keepalived adds it.
      boot.kernel.sysctl."net.ipv4.ip_nonlocal_bind" = 1;
      # Read by the node agent to report which member holds the address.
      environment.etc."kcore-postgresql/vip".text = cfg.vip.address;

      systemd.services.${keepalivedUnit} = {
        description = "Keepalived for the kcore PostgreSQL writer VIP (${cfg.clusterName})";
        wantedBy = [ "multi-user.target" ];
        after = [
          "network-online.target"
          "patroni.service"
        ];
        wants = [ "network-online.target" ];
        serviceConfig = {
          Type = "forking";
          PIDFile = "/run/${keepalivedUnit}.pid";
          ExecStartPre = keepalivedNotrack;
          ExecStart = "${pkgs.keepalived}/bin/keepalived -f ${keepalivedConf} -p /run/${keepalivedUnit}.pid -r /run/vrrp-pg-${cfg.clusterName}.pid";
          Restart = "always";
          RestartSec = "5s";
        };
      };
    })
  ]);
}
