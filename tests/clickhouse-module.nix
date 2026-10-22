# Three machines: two ClickHouse replicas of one shard and three Keepers.
# TLS is off so the test does not need a lab CA. Secrets are written by a
# oneshot; the module still refuses to start without EnvironmentFile.
# This derivation is not a flake check. nix-checks does not build it.
{
  pkgs ? (builtins.getFlake (toString ../.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem},
}:
let
  inherit (pkgs) lib;
  # SHA-256 hex of the password string "secret". Computed so the digest is not in the source.
  passwordSha = builtins.hashString "sha256" "secret";
  peers = [
    {
      id = 1;
      host = "192.168.1.1";
      port = 9234;
      learner = false;
    }
    {
      id = 2;
      host = "192.168.1.2";
      port = 9234;
      learner = false;
    }
    {
      id = 3;
      host = "192.168.1.3";
      port = 9234;
      learner = false;
    }
  ];
  remoteServers = [
    {
      shard = "1";
      weight = 1;
      replicas = [
        {
          replica = "a";
          host = "192.168.1.1";
          port = 9000;
          secure = false;
          priority = 1;
        }
        {
          replica = "b";
          host = "192.168.1.2";
          port = 9000;
          secure = false;
          priority = 1;
        }
      ];
    }
  ];
  node =
    {
      addr,
      server,
      keeperId,
      replica ? "",
    }:
    {
      imports = [ ../modules/kcore-clickhouse.nix ];
      networking.interfaces.eth1.useDHCP = false;
      networking.interfaces.eth1.ipv4.addresses = [
        {
          address = addr;
          prefixLength = 24;
        }
      ];
      virtualisation.memorySize = 3072;
      virtualisation.diskSize = 8192;
      environment.systemPackages = [ pkgs.clickhouse ];
      kcore.clickhouse = {
        enable = true;
        clusterName = "analytics";
        advertiseAddress = addr;
        iface = "eth1";
        tls.enable = false;
        server = {
          enable = server;
          shard = "1";
          inherit replica;
          maxServerMemoryRatio = 0.5;
          clientNetworks = [ "192.168.1.0/24" ];
          remoteServers = if server then remoteServers else [ ];
        };
        keeper = {
          enable = true;
          serverId = keeperId;
          raftPeers = peers;
        };
      };
      systemd.services.kcore-clickhouse-test-secrets = {
        description = "ClickHouse test EnvironmentFiles";
        wantedBy = [ "multi-user.target" ];
        before = [
          "clickhouse.service"
          "clickhouse-keeper.service"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0700 /var/lib/kcore-clickhouse/analytics/secrets
          cat > /var/lib/kcore-clickhouse/analytics/secrets/server.env <<'EOF'
          KCORE_CH_DEFAULT_PASSWORD=${passwordSha}
          KCORE_CH_METRICS_PASSWORD=${passwordSha}
          KCORE_CH_CLUSTER_SECRET=cluster-secret
          KCORE_CH_KEEPER_IDENTITY=kcore:secret
          KCORE_CH_INTERSERVER_PASSWORD=interserver-secret
          EOF
          cp /var/lib/kcore-clickhouse/analytics/secrets/server.env \
            /var/lib/kcore-clickhouse/analytics/secrets/keeper.env
        '';
      };
      systemd.services.clickhouse = lib.mkIf server {
        requires = [ "kcore-clickhouse-test-secrets.service" ];
        after = [ "kcore-clickhouse-test-secrets.service" ];
      };
      systemd.services.clickhouse-keeper = {
        requires = [ "kcore-clickhouse-test-secrets.service" ];
        after = [ "kcore-clickhouse-test-secrets.service" ];
      };
    };
in
pkgs.testers.runNixOSTest {
  name = "clickhouse-module";
  nodes.a = node {
    addr = "192.168.1.1";
    server = true;
    keeperId = 1;
    replica = "a";
  };
  nodes.b = node {
    addr = "192.168.1.2";
    server = true;
    keeperId = 2;
    replica = "b";
  };
  nodes.c = node {
    addr = "192.168.1.3";
    server = false;
    keeperId = 3;
  };
  testScript = ''
    start_all()

    def require_unit(machine, unit, log):
        machine.succeed(
            "for i in $(seq 1 120); do "
            + "if systemctl is-active --quiet "
            + unit
            + "; then sleep 2; "
            + "if systemctl is-active --quiet "
            + unit
            + "; then exit 0; fi; "
            + "fi; sleep 1; done; "
            + "echo '--- "
            + unit
            + " ---'; systemctl status "
            + unit
            + " --no-pager -n 20 || true; "
            + "echo '--- "
            + log
            + " ---'; grep -E '<Error>|<Fatal>|<Exception>' "
            + log
            + " | head -8 || true; exit 1"
        )

    keeper_log = "/var/lib/kcore-clickhouse/analytics/keeper/logs/clickhouse-keeper.err.log"
    server_log = "/var/lib/kcore-clickhouse/analytics/data/logs/clickhouse-server.err.log"
    require_unit(a, "clickhouse-keeper.service", keeper_log)
    require_unit(b, "clickhouse-keeper.service", keeper_log)
    require_unit(c, "clickhouse-keeper.service", keeper_log)

    # The server's replicated access storage exits if Keeper is not accepting
    # clients yet. Wait for a leader, then start the servers.
    a.wait_until_succeeds(
        """
        for h in 192.168.1.1 192.168.1.2 192.168.1.3; do
          if clickhouse-keeper-client --host "$h" --port 9281 --query mntr \
              | grep -q $'zk_server_state\tleader'; then
            exit 0
          fi
        done
        exit 1
        """
    )
    a.succeed("systemctl reset-failed clickhouse.service; systemctl restart clickhouse.service")
    b.succeed("systemctl reset-failed clickhouse.service; systemctl restart clickhouse.service")
    require_unit(a, "clickhouse.service", server_log)
    require_unit(b, "clickhouse.service", server_log)

    client = "clickhouse-client --host 127.0.0.1 --port 9000 --password secret"
    a.succeed(
        client
        + " -q \"CREATE DATABASE IF NOT EXISTS events ON CLUSTER analytics "
        + "ENGINE = Replicated('/clickhouse/databases/events', '{shard}', '{replica}')\""
    )
    # A Replicated database copies DDL itself. ON CLUSTER is rejected with
    # INCORRECT_QUERY because the distributed query is not the initial one.
    a.succeed(
        client
        + " -q \"CREATE TABLE IF NOT EXISTS events.hits "
        + "(id UInt64) ENGINE = ReplicatedMergeTree ORDER BY id\""
    )
    a.succeed(client + " -q 'INSERT INTO events.hits VALUES (1)'")
    b.wait_until_succeeds(client + " -q 'SELECT count() FROM events.hits' | grep -qx 1")

    leader = a.succeed(
        """
        for h in 192.168.1.1 192.168.1.2 192.168.1.3; do
          if clickhouse-keeper-client --host "$h" --port 9281 --query mntr \
              | grep -q $'zk_server_state\tleader'; then
            echo "$h"
            exit 0
          fi
        done
        exit 1
        """
    ).strip()
    {"192.168.1.1": a, "192.168.1.2": b, "192.168.1.3": c}[leader].systemctl(
        "stop clickhouse-keeper"
    )
    a.wait_until_succeeds(
        f"""
        for h in 192.168.1.1 192.168.1.2 192.168.1.3; do
          if [ "$h" = "{leader}" ]; then
            continue
          fi
          if clickhouse-keeper-client --host "$h" --port 9281 --query mntr \
              | grep -q $'zk_server_state\\tleader'; then
            exit 0
          fi
        done
        exit 1
        """
    )

    before = a.succeed("systemctl show clickhouse.service -p NRestarts --value").strip()
    a.succeed(
        "mkdir -p /etc/clickhouse-server/config.d && "
        "printf '%s\\n' '<clickhouse><max_concurrent_queries>123</max_concurrent_queries></clickhouse>' "
        "> /etc/clickhouse-server/config.d/reload-test.xml"
    )
    a.succeed(client + " -q 'SYSTEM RELOAD CONFIG'")
    after = a.succeed("systemctl show clickhouse.service -p NRestarts --value").strip()
    assert before == after, f"reload restarted the unit ({before} -> {after})"
    got = a.succeed(
        client + " -q \"SELECT value FROM system.settings WHERE name = 'max_concurrent_queries'\""
    ).strip()
    assert got == "123", got

    a.systemctl("restart clickhouse")
    b.succeed(client + " -q 'SELECT 1'")
  '';
}
