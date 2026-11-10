# Written by the kcore node agent. Do not edit: the next apply replaces it.
_:
{
  kcore.postgresqlCluster = {
    enable = true;
    clusterName = "orders";
    generation = 1;
    package = "postgresql_17";
    advertiseAddress = "192.168.40.108";
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
      name = "node-108";
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
      enable = false;
    };
  };
}
