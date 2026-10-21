# Eval-only guard: the ClickHouse module must evaluate without building the package.
{
  pkgs,
  nixosSystem,
}:
let
  eval = nixosSystem {
    inherit (pkgs) system;
    modules = [
      ../modules/kcore-clickhouse.nix
      (_: {
        networking.hostName = "clickhouse-eval";
        system.stateVersion = "25.11";
        boot.loader.grub.enable = false;
        fileSystems."/" = {
          device = "nodev";
          fsType = "tmpfs";
        };
        kcore.clickhouse = {
          enable = true;
          clusterName = "analytics";
          advertiseAddress = "192.168.40.105";
          iface = "enp6s0";
          server = {
            enable = true;
            shard = "1";
            replica = "r1";
            remoteServers = [
              {
                shard = "1";
                weight = 1;
                replicas = [
                  {
                    replica = "r1";
                    host = "192.168.40.105";
                    port = 9440;
                    secure = true;
                    priority = 1;
                  }
                ];
              }
            ];
          };
          keeper = {
            enable = true;
            serverId = 1;
            raftPeers = [
              {
                id = 1;
                host = "192.168.40.105";
                port = 9234;
                learner = false;
              }
            ];
          };
        };
      })
    ];
  };
  cfg = eval.config;
  server = cfg.systemd.services.clickhouse;
  keeper = cfg.systemd.services.clickhouse-keeper;
in
assert cfg.kcore.clickhouse.enable;
assert !server.restartIfChanged;
assert !server.stopIfChanged;
assert keeper.serviceConfig.Type == "simple";
assert !keeper.restartIfChanged;
assert !keeper.stopIfChanged;
assert (cfg.services.clickhouse.package.pname or cfg.services.clickhouse.package.name) != "";
pkgs.runCommand "clickhouse-module-eval-ok" { } ''
  echo ok > "$out"
''
