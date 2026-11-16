# Written by the kcore node agent. Do not edit: the next apply replaces it.
_: {
  kcore.postgresqlCluster = {
    enable = true;
    clusterName = "orders";
    generation = 1;
    package = "postgresql_17";
    advertiseAddress = "192.168.40.105";
    iface = "enp6s0";
    tls.enable = true;
    ports = {
      postgres = 5432;
      patroniApi = 8008;
      etcdClient = 2479;
      etcdPeer = 2480;
    };
    etcd = {
      enable = true;
      name = "node-105";
      initialClusterState = "new";
      peers = [
        {
          name = "node-105";
          host = "192.168.40.105";
        }
        {
          name = "node-106";
          host = "192.168.40.106";
        }
        {
          name = "node-108";
          host = "192.168.40.108";
        }
      ];
    };
    postgres = {
      enable = true;
      walVolume = false;
      members = [
        "192.168.40.105"
        "192.168.40.106"
        "192.168.40.108"
      ];
      clientNetworks = [
        "192.168.40.0/24"
      ];
      memoryLimitBytes = 34359738368;
      watchdog = "off";
      tags = {
        nofailover = false;
        noloadbalance = false;
        failoverPriority = 1;
      };
      extensionPackages = [ ];
      bootstrapDcs = builtins.fromJSON "{\"loop_wait\":10,\"maximum_lag_on_failover\":1048576,\"postgresql\":{\"parameters\":{\"effective_cache_size\":\"20132659kB\",\"hot_standby\":\"on\",\"max_connections\":\"300\",\"max_replication_slots\":\"10\",\"max_wal_senders\":\"10\",\"password_encryption\":\"scram-sha-256\",\"shared_buffers\":\"8388608kB\",\"shared_preload_libraries\":\"pg_stat_statements\",\"wal_keep_size\":\"1GB\",\"wal_level\":\"replica\",\"wal_log_hints\":\"on\",\"work_mem\":\"16MB\"},\"use_pg_rewind\":true,\"use_slots\":true},\"retry_timeout\":10,\"synchronous_mode\":\"quorum\",\"synchronous_mode_strict\":false,\"synchronous_node_count\":1,\"ttl\":30}";
    };
    vip = {
      enable = true;
      address = "192.168.40.150";
      prefixLength = 24;
      virtualRouterId = 61;
    };
    volumes = [
      {
        mountPoint = "/var/lib/kcore-postgresql/orders/pgdata";
        device = "/dev/vg0/pg-orders";
        fsType = "xfs";
      }
    ];
  };
}
