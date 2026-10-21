# ClickHouse server and Keeper on a kcoreOS node.
# Secrets stay in EnvironmentFile. Nothing under environment.etc carries a password.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.kcore.clickhouse;
  format = pkgs.formats.yaml { };
  pkg = pkgs.${cfg.package};
  dataRoot = "/var/lib/kcore-clickhouse/${cfg.clusterName}";
  votingPeers = lib.filter (peer: !peer.learner) cfg.keeper.raftPeers;
  memberHosts = lib.unique (
    map (peer: peer.host) cfg.keeper.raftPeers
    ++ lib.concatMap (shard: map (replica: replica.host) shard.replicas) cfg.server.remoteServers
  );
  clientNets =
    if cfg.server.clientNetworks == [ ] then [ "127.0.0.1" ] else cfg.server.clientNetworks;
  portList =
    if cfg.tls.enable then
      [
        cfg.ports.tcpSecure
        cfg.ports.https
        cfg.ports.interserver
      ]
      ++ lib.optionals cfg.keeper.enable [
        cfg.ports.keeper
        cfg.ports.keeperRaft
      ]
      ++ lib.optional cfg.server.metricsPublic cfg.ports.prometheus
    else
      [
        cfg.ports.tcp
        cfg.ports.http
        cfg.ports.interserver
      ]
      ++ lib.optionals cfg.keeper.enable [
        cfg.ports.keeper
        cfg.ports.keeperRaft
      ]
      ++ lib.optional cfg.server.metricsPublic cfg.ports.prometheus;
  distinctPorts = lib.allUnique portList;
  underRoot = path: lib.hasPrefix "/var/lib/kcore-clickhouse/" path;
  logDays = default: days: if days == 0 then default else days;
  engine =
    days:
    "ENGINE = MergeTree PARTITION BY toYYYYMM(event_date) ORDER BY (event_date, event_time) TTL event_date + INTERVAL ${toString days} DAY";
  fromEnv = var: {
    "@from_env" = var;
    "@hide_in_preprocessed" = "true";
  };
  removeKey = {
    "@remove" = "remove";
  };
  secretKey =
    var:
    fromEnv var
    // {
      "@overridable" = false;
    };
  userEnv = name: "KCORE_CH_USER_${lib.toUpper (lib.replaceStrings [ "-" ] [ "_" ] name)}";
  profileAttrs = lib.listToAttrs (
    map (profile: {
      inherit (profile) name;
      value = profile.settings;
    }) cfg.server.profiles
  );
  quotaAttrs = lib.listToAttrs (
    map (quota: {
      inherit (quota) name;
      value = {
        interval = map (interval: {
          duration = interval.durationSeconds;
          inherit (interval) queries errors;
          result_rows = interval.resultRows;
          read_rows = interval.readRows;
          execution_time = interval.executionTimeSeconds;
        }) quota.intervals;
      };
    }) cfg.server.quotas
  );
  userAttrs = lib.listToAttrs (
    map (user: {
      inherit (user) name;
      value = {
        password_sha256_hex = fromEnv (userEnv user.name);
        networks.ip = if user.networks == [ ] then clientNets else user.networks;
        profile = if user.profile == "" then "default" else user.profile;
        quota = if user.quota == "" then "default" else user.quota;
        access_management = if user.accessManagement then 1 else 0;
      };
    }) cfg.server.users
  );
  dictionaryAttrs = map (dict: {
    inherit (dict) name;
    source =
      if dict.source == "https" then
        {
          http = {
            inherit (dict) url;
            format = "TabSeparated";
          };
        }
      else
        {
          clickhouse = {
            host = "127.0.0.1";
            port = if cfg.tls.enable then cfg.ports.tcpSecure else cfg.ports.tcp;
            secure = if cfg.tls.enable then 1 else 0;
            inherit (dict) query;
          };
        };
    layout.${dict.layout} = { };
    structure = {
      key = {
        name = dict.primaryKey;
      };
      attribute = map (attr: {
        inherit (attr) name type;
        null_value = if attr.nullable then "" else "0";
      }) dict.attributes;
    };
    lifetime = dict.lifetimeSeconds;
  }) (lib.filter (dict: dict.database == "") cfg.server.dictionaries);
  collectionAttrs = lib.listToAttrs (
    map (collection: {
      inherit (collection) name;
      value =
        collection.keys
        // lib.mapAttrs (key: _: secretKey "KCORE_CH_NC_${collection.name}_${key}") collection.secretRefs;
    }) cfg.server.namedCollections
  );
  serverConfig = {
    logger = {
      level = "information";
      log = "${cfg.server.dataPath}/logs/clickhouse-server.log";
      errorlog = "${cfg.server.dataPath}/logs/clickhouse-server.err.log";
    };
    listen_host = [
      "127.0.0.1"
      cfg.advertiseAddress
    ];
    path = cfg.server.dataPath;
    tmp_path = "${cfg.server.dataPath}/tmp/";
    user_files_path = "${cfg.server.dataPath}/user_files/";
    format_schema_path = "${cfg.server.dataPath}/format_schemas/";
    access_control_path = "${cfg.server.dataPath}/access/";
    max_server_memory_usage_to_ram_ratio = cfg.server.maxServerMemoryRatio;
    macros = {
      inherit (cfg.server) shard replica;
      cluster = cfg.clusterName;
    };
    remote_servers.${cfg.clusterName} = {
      secret = fromEnv "KCORE_CH_CLUSTER_SECRET";
      shard = map (shard: {
        inherit (shard) weight;
        internal_replication = true;
        replica = map (replica: {
          inherit (replica) host port priority;
          secure = if replica.secure then 1 else 0;
        }) shard.replicas;
      }) cfg.server.remoteServers;
    };
    zookeeper = {
      node = map (peer: {
        inherit (peer) host;
        port = cfg.ports.keeper;
        secure = if cfg.tls.enable then 1 else 0;
      }) cfg.keeper.raftPeers;
      identity = fromEnv "KCORE_CH_KEEPER_IDENTITY";
    };
    interserver_http_credentials = {
      user = "kcore-interserver";
      password = fromEnv "KCORE_CH_INTERSERVER_PASSWORD";
    };
    user_directories = {
      users_xml = {
        path = "users.xml";
      };
      replicated = {
        zookeeper_path = "/clickhouse/access/";
      };
    };
    prometheus = {
      endpoint = "/metrics";
      port = cfg.ports.prometheus;
      listen_host = if cfg.server.metricsPublic then cfg.advertiseAddress else "127.0.0.1";
    };
    http_handlers.rule = {
      url = "/clickhouse/metrics";
      methods = "GET";
      handler = {
        type = "predefined";
        name = "prometheus";
      };
    };
    query_log.engine = engine (logDays 14 cfg.server.systemLogs.queryLogDays);
    part_log.engine = engine (logDays 14 cfg.server.systemLogs.partLogDays);
    trace_log.engine = engine (logDays 3 cfg.server.systemLogs.traceLogDays);
    text_log.engine = engine (logDays 7 cfg.server.systemLogs.textLogDays);
  }
  // lib.optionalAttrs cfg.tls.enable {
    tcp_port = removeKey;
    http_port = removeKey;
    interserver_http_port = removeKey;
    tcp_port_secure = cfg.ports.tcpSecure;
    https_port = cfg.ports.https;
    interserver_https_port = cfg.ports.interserver;
    interserver_https_host = cfg.advertiseAddress;
    openSSL.server = {
      certificateFile = cfg.tls.certFile;
      privateKeyFile = cfg.tls.keyFile;
      caConfig = cfg.tls.caFile;
      verificationMode = "strict";
    };
  }
  // lib.optionalAttrs (!cfg.tls.enable) {
    tcp_port = cfg.ports.tcp;
    http_port = cfg.ports.http;
    interserver_http_port = cfg.ports.interserver;
  }
  // cfg.server.settings
  // lib.optionalAttrs (dictionaryAttrs != [ ]) {
    dictionaries.dictionary = dictionaryAttrs;
  }
  // lib.optionalAttrs (collectionAttrs != { }) {
    named_collections = collectionAttrs;
  };
  usersConfig = {
    profiles = profileAttrs;
    quotas = quotaAttrs;
    users = {
      default = {
        password_sha256_hex = fromEnv "KCORE_CH_DEFAULT_PASSWORD";
        networks.ip = [ "127.0.0.1" ] ++ cfg.server.clientNetworks;
        profile = "default";
        quota = "default";
      };
      "kcore-metrics" = {
        password_sha256_hex = fromEnv "KCORE_CH_METRICS_PASSWORD";
        networks.ip = clientNets;
        profile = "default";
        quota = "default";
      };
    }
    // userAttrs;
  };
  keeperConfig = {
    logger = {
      level = "information";
      log = "${cfg.keeper.dataPath}/logs/clickhouse-keeper.log";
      errorlog = "${cfg.keeper.dataPath}/logs/clickhouse-keeper.err.log";
    };
    listen_host = cfg.advertiseAddress;
    server_id = cfg.keeper.serverId;
    storage_path = cfg.keeper.dataPath;
    raft_configuration = {
      server = map (peer: {
        inherit (peer) id port;
        hostname = peer.host;
        can_become_leader = !peer.learner;
      }) cfg.keeper.raftPeers;
      secure = cfg.tls.enable;
    };
  }
  // lib.optionalAttrs cfg.tls.enable {
    tcp_port = removeKey;
    tcp_port_secure = cfg.ports.keeper;
    openSSL.server = {
      certificateFile = cfg.tls.certFile;
      privateKeyFile = cfg.tls.keyFile;
      caConfig = cfg.tls.caFile;
      verificationMode = "strict";
    };
  }
  // lib.optionalAttrs (!cfg.tls.enable) {
    tcp_port = cfg.ports.keeper;
  };
  iptablesRule = port: source: ''
    iptables -A nixos-fw -p tcp --dport ${toString port} -s ${source} -j nixos-fw-accept
  '';
  extraFw =
    lib.concatMapStrings (host: iptablesRule cfg.ports.interserver host) memberHosts
    + lib.concatMapStrings (host: iptablesRule cfg.ports.keeper host) memberHosts
    + lib.concatMapStrings (host: iptablesRule cfg.ports.keeperRaft host) memberHosts
    + lib.concatMapStrings (net: iptablesRule cfg.ports.tcpSecure net) clientNets
    + lib.concatMapStrings (net: iptablesRule cfg.ports.https net) clientNets;
in
{
  options.kcore.clickhouse = {
    enable = lib.mkEnableOption "kcore ClickHouse";
    clusterName = lib.mkOption {
      type = lib.types.str;
      default = "clickhouse";
    };
    generation = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 0;
    };
    package = lib.mkOption {
      type = lib.types.enum [
        "clickhouse"
        "clickhouse-lts"
      ];
      default = "clickhouse";
    };
    advertiseAddress = lib.mkOption {
      type = lib.types.str;
      default = "";
    };
    iface = lib.mkOption {
      type = lib.types.str;
      default = "";
    };
    secretsDir = lib.mkOption {
      type = lib.types.str;
      default = "${dataRoot}/secrets";
    };
    tls = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };
      certFile = lib.mkOption {
        type = lib.types.str;
        default = "${dataRoot}/tls/node.crt";
      };
      keyFile = lib.mkOption {
        type = lib.types.str;
        default = "${dataRoot}/tls/node.key";
      };
      caFile = lib.mkOption {
        type = lib.types.str;
        default = "${dataRoot}/tls/ca.crt";
      };
    };
    ports = {
      tcp = lib.mkOption {
        type = lib.types.port;
        default = 9000;
      };
      tcpSecure = lib.mkOption {
        type = lib.types.port;
        default = 9440;
      };
      http = lib.mkOption {
        type = lib.types.port;
        default = 8123;
      };
      https = lib.mkOption {
        type = lib.types.port;
        default = 8443;
      };
      interserver = lib.mkOption {
        type = lib.types.port;
        default = 9010;
      };
      keeper = lib.mkOption {
        type = lib.types.port;
        default = 9281;
      };
      keeperRaft = lib.mkOption {
        type = lib.types.port;
        default = 9234;
      };
      prometheus = lib.mkOption {
        type = lib.types.port;
        default = 9363;
      };
    };
    server = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      shard = lib.mkOption {
        type = lib.types.str;
        default = "1";
      };
      replica = lib.mkOption {
        type = lib.types.str;
        default = "";
      };
      readOnly = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      dataPath = lib.mkOption {
        type = lib.types.str;
        default = "${dataRoot}/data";
      };
      maxServerMemoryRatio = lib.mkOption {
        type = lib.types.float;
        default = 0.5;
      };
      metricsPublic = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      clientNetworks = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
      };
      databases = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
      };
      settings = lib.mkOption {
        type = lib.types.attrs;
        default = { };
      };
      remoteServers = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      users = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      profiles = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      quotas = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      dictionaries = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      namedCollections = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      functions = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
      systemLogs = {
        queryLogDays = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 14;
        };
        partLogDays = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 14;
        };
        traceLogDays = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 3;
        };
        textLogDays = lib.mkOption {
          type = lib.types.ints.unsigned;
          default = 7;
        };
      };
    };
    keeper = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      serverId = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
      };
      learner = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      dataPath = lib.mkOption {
        type = lib.types.str;
        default = "${dataRoot}/keeper";
      };
      raftPeers = lib.mkOption {
        type = lib.types.listOf lib.types.attrs;
        default = [ ];
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.advertiseAddress != "";
        message = "kcore.clickhouse.advertiseAddress is required when ClickHouse is enabled";
      }
      {
        assertion = cfg.server.enable || cfg.keeper.enable;
        message = "kcore.clickhouse needs server.enable or keeper.enable";
      }
      {
        assertion = cfg.iface != "" && cfg.iface != "br0" && !lib.hasPrefix "kbr-" cfg.iface;
        message = "kcore.clickhouse.iface must be set and must not be br0 or a kbr bridge";
      }
      {
        assertion =
          !cfg.keeper.enable || lib.elem cfg.keeper.serverId (map (peer: peer.id) cfg.keeper.raftPeers);
        message = "kcore.clickhouse.keeper.serverId must be one of raftPeers";
      }
      {
        assertion = !cfg.keeper.enable || lib.mod (lib.length votingPeers) 2 == 1;
        message = "kcore.clickhouse voting Keeper count must be odd";
      }
      {
        assertion =
          !cfg.server.enable
          || (lib.any (
            shard:
            shard.shard == cfg.server.shard
            && lib.any (replica: replica.replica == cfg.server.replica) shard.replicas
          ) cfg.server.remoteServers);
        message = "kcore.clickhouse server shard and replica must appear in remoteServers";
      }
      {
        assertion =
          !cfg.tls.enable || (cfg.tls.certFile != "" && cfg.tls.keyFile != "" && cfg.tls.caFile != "");
        message = "kcore.clickhouse.tls.enable requires certFile, keyFile, and caFile";
      }
      {
        assertion = distinctPorts;
        message = "kcore.clickhouse ports must be pairwise distinct";
      }
      {
        assertion = !cfg.server.enable || underRoot cfg.server.dataPath;
        message = "kcore.clickhouse server dataPath must be under /var/lib/kcore-clickhouse/";
      }
      {
        assertion = !cfg.keeper.enable || underRoot cfg.keeper.dataPath;
        message = "kcore.clickhouse keeper dataPath must be under /var/lib/kcore-clickhouse/";
      }
    ];

    users.users.clickhouse = lib.mkIf (!cfg.server.enable) {
      isSystemUser = true;
      group = "clickhouse";
      uid = config.ids.uids.clickhouse;
    };
    users.groups.clickhouse = lib.mkIf (!cfg.server.enable) {
      gid = config.ids.gids.clickhouse;
    };

    services.clickhouse = lib.mkIf cfg.server.enable {
      enable = true;
      package = pkg;
      inherit serverConfig usersConfig;
    };

    systemd.services.clickhouse = lib.mkIf cfg.server.enable {
      restartIfChanged = false;
      stopIfChanged = false;
      serviceConfig = {
        EnvironmentFile = "${cfg.secretsDir}/server.env";
        TimeoutStopSec = "300";
      };
      unitConfig.RequiresMountsFor = cfg.server.dataPath;
    };

    systemd.services.clickhouse-keeper = lib.mkIf cfg.keeper.enable {
      description = "ClickHouse Keeper";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      restartIfChanged = false;
      stopIfChanged = false;
      serviceConfig = {
        Type = "simple";
        User = "clickhouse";
        Group = "clickhouse";
        ExecStart = "${pkg}/bin/clickhouse-keeper --config-file=/etc/clickhouse-keeper/keeper_config.yaml";
        EnvironmentFile = "${cfg.secretsDir}/keeper.env";
        TimeoutStopSec = "300";
        Restart = "on-failure";
      };
      unitConfig.RequiresMountsFor = cfg.keeper.dataPath;
    };

    environment.etc = lib.mkMerge [
      (lib.mkIf cfg.keeper.enable {
        "clickhouse-keeper/keeper_config.yaml".source = format.generate "keeper.yaml" keeperConfig;
      })
      (lib.mkIf cfg.server.enable {
        "clickhouse-client/config.xml".text = ''
          <config>
            <secure>${if cfg.tls.enable then "1" else "0"}</secure>
            <host>127.0.0.1</host>
            <port>${toString (if cfg.tls.enable then cfg.ports.tcpSecure else cfg.ports.tcp)}</port>
            <openSSL>
              <client>
                <caConfig>${cfg.tls.caFile}</caConfig>
                <verificationMode>strict</verificationMode>
              </client>
            </openSSL>
          </config>
        '';
      })
    ];

    systemd.tmpfiles.rules = lib.mkMerge [
      (lib.mkIf cfg.server.enable [
        "d ${cfg.server.dataPath} 0750 clickhouse clickhouse - -"
        "d ${cfg.secretsDir} 0700 root root - -"
        "d ${dirOf cfg.tls.certFile} 0750 clickhouse clickhouse - -"
      ])
      (lib.mkIf cfg.keeper.enable [
        "d ${cfg.keeper.dataPath} 0750 clickhouse clickhouse - -"
      ])
    ];

    networking.firewall.interfaces.${cfg.iface}.allowedTCPPorts = portList;
    networking.firewall.extraCommands = extraFw;
  };
}
