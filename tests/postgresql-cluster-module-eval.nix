# Eval-only guard for kcore.postgresqlCluster. The fragments under
# tests/fixtures/postgresql-cluster/ are the node agent's rendered output
# (crates/kcore-postgresql/src/nixgen.rs checks them byte for byte), so a
# renderer change that breaks evaluation fails here and a module option rename
# fails the Rust golden test.
{
  pkgs,
  lib,
  nixosSystem,
}:
let
  base = {
    networking.hostName = "pg-eval";
    system.stateVersion = "25.11";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "nodev";
      fsType = "tmpfs";
    };
  };

  eval =
    fragment:
    nixosSystem {
      inherit (pkgs) system;
      modules = [
        ../modules/kcore-postgresql-cluster.nix
        (_: base)
        fragment
      ];
    };

  member = (eval ./fixtures/postgresql-cluster/member.nix).config;
  witness = (eval ./fixtures/postgresql-cluster/witness.nix).config;
  disabled = (eval ./fixtures/postgresql-cluster/disabled.nix).config;

  failing =
    extra:
    let
      sys = eval {
        imports = [ ./fixtures/postgresql-cluster/member.nix ];
        config = extra;
      };
    in
    map (a: a.message) (builtins.filter (a: !a.assertion) sys.config.assertions);

  patroni = member.services.patroni.settings;
  etcdUnit = member.systemd.services."kcore-pg-etcd-orders";
  patroniUnit = member.systemd.services.patroni;
  fw = member.networking.firewall.extraCommands;
  etcText = lib.concatStrings (
    lib.mapAttrsToList (_: f: if f.text or null == null then "" else f.text) member.environment.etc
  );
in
assert member.kcore.postgresqlCluster.enable;
assert member.services.patroni.enable;
assert !member.services.postgresql.enable;
assert patroni.scope == "orders";
assert patroni.name == "node-105";
assert patroni.etcd3.protocol == "https";
assert
  patroni.etcd3.hosts == [
    "192.168.40.105:2479"
    "192.168.40.106:2479"
    "192.168.40.108:2479"
  ];
assert patroni.postgresql.listen == "192.168.40.105,127.0.0.1,192.168.40.150:5432";
assert patroni.restapi.listen == "0.0.0.0:8008";
assert patroni.postgresql.authentication.replication.sslmode == "verify-full";
assert lib.all (line: !lib.hasInfix "trust" line) patroni.postgresql.pg_hba;
assert lib.elem "hostssl replication replicator 192.168.40.106/32 scram-sha-256"
  patroni.postgresql.pg_hba;
assert lib.elem "hostssl all all 192.168.40.0/24 scram-sha-256" patroni.postgresql.pg_hba;
assert patroni.bootstrap.dcs.synchronous_mode == "quorum";
assert patroni.bootstrap.dcs.postgresql.parameters.max_connections == "300";
assert lib.elem "data-checksums" patroni.bootstrap.initdb;
assert !patroni.tags.nofailover;
assert member.services.patroni.postgresqlDataDir == "/var/lib/kcore-postgresql/orders/pgdata/17";
assert
  member.services.patroni.environmentFiles.PATRONI_SUPERUSER_PASSWORD
  == "/var/lib/kcore-postgresql/orders/secrets/superuser.password";
assert !patroniUnit.restartIfChanged;
assert !patroniUnit.stopIfChanged;
assert patroniUnit.serviceConfig.MemoryMax == "34359738368";
assert lib.elem "kcore-pg-etcd-orders.service" patroniUnit.after;
assert !etcdUnit.restartIfChanged;
assert lib.hasInfix "--client-cert-auth" etcdUnit.serviceConfig.ExecStart;
assert lib.hasInfix "node-106=https://192.168.40.106:2480" etcdUnit.serviceConfig.ExecStart;
assert member.systemd.services ? "keepalived-pg-orders";
assert member.fileSystems."/var/lib/kcore-postgresql/orders/pgdata".device == "/dev/vg0/pg-orders";
assert member.fileSystems."/var/lib/kcore-postgresql/orders/pgdata".fsType == "xfs";
assert lib.elem "/var/lib/kcore-postgresql/orders/pgdata/17"
  patroniUnit.unitConfig.RequiresMountsFor;
assert lib.hasInfix "-i enp6s0 -s 192.168.40.106" fw;
assert lib.hasInfix "--dport 5432" fw;
assert !lib.hasInfix "192.168.40.105'" fw;
assert !lib.hasInfix "password" (lib.toLower etcText);
assert witness.systemd.services ? "kcore-pg-etcd-orders";
assert !witness.services.patroni.enable;
assert !(witness.systemd.services ? "keepalived-pg-orders");
assert !disabled.services.patroni.enable;
assert !(disabled.systemd.services ? "kcore-pg-etcd-orders");
assert !lib.hasInfix "5432" disabled.networking.firewall.extraCommands;
assert lib.any (lib.hasInfix "odd number") (failing {
  kcore.postgresqlCluster.etcd.peers = lib.mkForce [
    {
      name = "node-105";
      host = "192.168.40.105";
    }
    {
      name = "node-106";
      host = "192.168.40.106";
    }
  ];
});
assert lib.any (lib.hasInfix "advertiseAddress") (failing {
  kcore.postgresqlCluster.advertiseAddress = lib.mkForce "";
});
pkgs.runCommand "postgresql-cluster-module-eval-ok" { } ''
  echo ok > "$out"
''
