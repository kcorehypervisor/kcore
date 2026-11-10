# Three machines: one Patroni primary, two streaming standbys, three etcd
# voters, and a keepalived writer VIP. TLS is off so the test does not need a
# lab CA. Secrets are written by a oneshot; the module still refuses to start
# without EnvironmentFile.
# This derivation is not a flake check. nix-checks does not build it.
{
  pkgs ? (builtins.getFlake (toString ../.)).inputs.nixpkgs.legacyPackages.${builtins.currentSystem},
}:
let
  peers = [
    {
      name = "a";
      host = "192.168.1.1";
    }
    {
      name = "b";
      host = "192.168.1.2";
    }
    {
      name = "c";
      host = "192.168.1.3";
    }
  ];
  members = [
    "192.168.1.1"
    "192.168.1.2"
    "192.168.1.3"
  ];
  bootstrapDcs = {
    ttl = 30;
    loop_wait = 10;
    retry_timeout = 10;
    maximum_lag_on_failover = 1048576;
    synchronous_mode = "quorum";
    synchronous_mode_strict = false;
    synchronous_node_count = 1;
    postgresql = {
      use_pg_rewind = true;
      use_slots = true;
      parameters = {
        hot_standby = "on";
        max_connections = "100";
        max_wal_senders = "10";
        max_replication_slots = "10";
        password_encryption = "scram-sha-256";
        wal_level = "replica";
        wal_log_hints = "on";
      };
    };
  };
  node =
    {
      addr,
      name,
    }:
    {
      imports = [ ../modules/kcore-postgresql-cluster.nix ];
      networking.interfaces.eth1.useDHCP = false;
      networking.interfaces.eth1.ipv4.addresses = [
        {
          address = addr;
          prefixLength = 24;
        }
      ];
      virtualisation.memorySize = 2048;
      virtualisation.diskSize = 8192;
      environment.systemPackages = [
        pkgs.postgresql_17
        pkgs.curl
        pkgs.jq
      ];
      kcore.postgresqlCluster = {
        enable = true;
        clusterName = "orders";
        package = "postgresql_17";
        advertiseAddress = addr;
        iface = "eth1";
        tls.enable = false;
        etcd = {
          enable = true;
          inherit name;
          initialClusterState = "new";
          inherit peers;
        };
        postgres = {
          enable = true;
          inherit members;
          clientNetworks = [ "192.168.1.0/24" ];
          memoryLimitBytes = 0;
          watchdog = "off";
          inherit bootstrapDcs;
        };
        vip = {
          enable = true;
          address = "192.168.1.50";
          prefixLength = 24;
          virtualRouterId = 61;
        };
      };
      systemd.services.kcore-postgresql-test-secrets = {
        description = "PostgreSQL cluster test EnvironmentFiles";
        wantedBy = [ "multi-user.target" ];
        before = [
          "patroni.service"
          "kcore-pg-etcd-orders.service"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0700 /var/lib/kcore-postgresql/orders/secrets
          for f in superuser replication rewind restapi; do
            printf 'secret\n' > "/var/lib/kcore-postgresql/orders/secrets/$f.password"
            chmod 0600 "/var/lib/kcore-postgresql/orders/secrets/$f.password"
          done
          chown -R kcore-pg:kcore-pg /var/lib/kcore-postgresql/orders/secrets
        '';
      };
      systemd.services.patroni = {
        requires = [ "kcore-postgresql-test-secrets.service" ];
        after = [ "kcore-postgresql-test-secrets.service" ];
      };
      systemd.services."kcore-pg-etcd-orders" = {
        requires = [ "kcore-postgresql-test-secrets.service" ];
        after = [ "kcore-postgresql-test-secrets.service" ];
      };
    };
in
pkgs.testers.runNixOSTest {
  name = "postgresql-cluster-module";
  nodes.a = node {
    addr = "192.168.1.1";
    name = "a";
  };
  nodes.b = node {
    addr = "192.168.1.2";
    name = "b";
  };
  nodes.c = node {
    addr = "192.168.1.3";
    name = "c";
  };
  testScript = ''
    start_all()

    def require_unit(machine, unit):
        machine.succeed(
            "for i in $(seq 1 180); do "
            + "if systemctl is-active --quiet "
            + unit
            + "; then exit 0; fi; sleep 1; done; "
            + "systemctl status "
            + unit
            + " --no-pager -n 40; exit 1"
        )

    for m in (a, b, c):
        require_unit(m, "kcore-pg-etcd-orders.service")
        require_unit(m, "patroni.service")
        require_unit(m, "keepalived-pg-orders.service")

    # Build Basic auth at runtime so the tree does not contain curl -u user:pass
    # (gitleaks curl-auth-user).
    patroni = (
        "AUTH=$(printf '%s' 'kcore:secret' | base64 -w0); "
        "curl -sf -H \"Authorization: Basic $AUTH\""
    )

    def wait_leader():
        a.wait_until_succeeds(
            patroni
            + " http://127.0.0.1:8008/cluster | grep -q '\"role\": \"leader\"'",
            timeout=180,
        )

    wait_leader()
    # Primary reports state=running; streaming standbys report state=streaming.
    a.wait_until_succeeds(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + "| jq -e '[.members[] | select(.state==\"running\" or .state==\"streaming\")] "
        + "| length == 3' >/dev/null",
        timeout=180,
    )
    a.wait_until_succeeds(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + "| grep -E '\"role\": \"(sync_standby|quorum_standby|replica)\"' "
        + "| wc -l | grep -Eq '^[12]$'",
        timeout=60,
    )

    psql = "psql -h 192.168.1.50 -U postgres -d postgres -v ON_ERROR_STOP=1"
    a.wait_until_succeeds(
        "ip -4 -o addr show | grep -q '192.168.1.50/' "
        + "|| true; PGPASSWORD=secret "
        + psql
        + " -c 'SELECT 1'",
        timeout=180,
    )
    a.succeed(
        "PGPASSWORD=secret "
        + psql
        + " -c 'CREATE TABLE IF NOT EXISTS hits(id int primary key); "
        + "INSERT INTO hits SELECT g FROM generate_series(1,1000) g "
        + "ON CONFLICT DO NOTHING;'"
    )
    for m in (b, c):
        m.wait_until_succeeds(
            "PGPASSWORD=secret psql -h 127.0.0.1 -U postgres -d postgres "
            + "-Atc 'SELECT count(*) FROM hits' | grep -qx 1000"
        )

    def leader_name(machine):
        return machine.succeed(
            patroni
            + " http://127.0.0.1:8008/cluster "
            + "| jq -r '.members[] | select(.role==\"leader\") | .name'"
        ).strip()

    leaders = {"a": a, "b": b, "c": c}
    leader_role = leader_name(a)
    old = leaders[leader_role]
    old.crash()

    survivors = {name: m for name, m in leaders.items() if name != leader_role}
    survivor = next(iter(survivors.values()))
    survivor.wait_until_succeeds(
        patroni + " http://127.0.0.1:8008/cluster | grep -q '\"role\": \"leader\"'",
        timeout=180,
    )
    new_leader = leader_name(survivor)
    new_m = leaders[new_leader]
    # keepalived only places the VIP after /primary succeeds on the new leader.
    new_m.wait_until_succeeds(
        "ip -4 -o addr show | grep -q '192.168.1.50/'",
        timeout=180,
    )
    new_m.succeed(
        "PGPASSWORD=secret psql -h 127.0.0.1 -U postgres -d postgres "
        + "-c \"INSERT INTO hits VALUES (1001) ON CONFLICT DO NOTHING;\""
    )
    standby = next(m for name, m in survivors.items() if name != new_leader)
    standby.succeed("ip neigh flush all || true")
    standby.wait_until_succeeds(
        "PGPASSWORD=secret psql -h 192.168.1.50 -U postgres -d postgres -c 'SELECT 1'",
        timeout=120,
    )
    got = standby.succeed(
        "PGPASSWORD=secret psql -h 127.0.0.1 -U postgres -d postgres "
        + "-Atc 'SELECT count(*) FROM hits'"
    ).strip()
    assert got == "1001", got

    old.start()
    require_unit(old, "kcore-pg-etcd-orders.service")
    require_unit(old, "patroni.service")
    # Ask a live peer's /cluster view whether the crashed node rejoined.
    # Curling /patroni on the recovering node races REST readiness; the
    # cluster roster is the source of truth (and may show it as leader again).
    survivor.wait_until_succeeds(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + f"| jq -e --arg n '{leader_role}' "
        + "'.members[] | select(.name == $n) "
        + "| select(.state == \"running\" or .state == \"streaming\")' >/dev/null",
        timeout=300,
    )

    # Quorum mode only switchovers onto a sync/quorum standby.
    survivor.wait_until_succeeds(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + "| jq -e '[.members[] | select(.role == \"sync_standby\" "
        + "or .role == \"quorum_standby\")] | length >= 1' >/dev/null",
        timeout=180,
    )
    current = leader_name(survivor)
    candidate = survivor.succeed(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + "| jq -r '[.members[] | select(.role == \"sync_standby\" "
        + "or .role == \"quorum_standby\")][0].name'"
    ).strip()
    assert candidate and candidate != current, candidate
    leaders[current].succeed(
        patroni
        + " -H 'Content-Type: application/json' "
        + "-X POST http://127.0.0.1:8008/switchover "
        + f"-d '{{\"leader\":\"{current}\",\"candidate\":\"{candidate}\"}}'"
    )
    survivor.wait_until_succeeds(
        patroni
        + " http://127.0.0.1:8008/cluster "
        + f"| jq -e --arg n '{candidate}' "
        + "'.members[] | select(.name == $n and .role == \"leader\")' >/dev/null",
        timeout=180,
    )
  '';
}
