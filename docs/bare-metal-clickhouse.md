# Bare-metal ClickHouse on kcoreOS

Status: **implemented in tree**. The NixOS module, controller API, node agent, kctl, MCP kind, and dashboard card are in this revision. Lab apply is still blocked.

Reviewed against ClickHouse commit [`ae3bcc01`](https://github.com/ClickHouse/ClickHouse/commit/ae3bcc01c681d35f0d778b310c0022407b65efd6) (2026-10-16, `v26.9.20.23-stable`) and against nixpkgs `b40629efe5d6`, whose `pkgs.clickhouse` is still `26.2.4.23-stable` (ClickHouse `94012846`). The module targets the pin. The upgrade section below is what changes when the pin moves to 26.7 or later. Latest LTS on that date is `v26.8.28.14-lts`.

ClickHouse joins the Bare-Metal line next to Kubernetes and PostgreSQL: a declarative NixOS service on kcoreOS nodes, applied the same way as the rest of the host, orchestrated by the Rust controller over gRPC. It does not run inside a VM and it does not run on the Kubernetes cluster.

The gRPC surface for this design is in [`proto-snippets-clickhouse.proto`](./proto-snippets-clickhouse.proto). That file compiles with `protoc` on its own so field numbers and comments can be reviewed before they are spliced into `proto/controller.proto` and `proto/node.proto`.

## What “done” means

An operator declares a `ClickhouseCluster` with shards, replicas and a Keeper ensemble. The controller validates it, issues certificates and secrets, and drives each member through the node agent. The node agent writes a Nix fragment, runs `nixos-rebuild`, and the `clickhouse` and `clickhouse-keeper` units come up. A `ReplicatedMergeTree` table created on one replica is readable on the other replicas of its shard, and `system.clusters` lists the whole topology. Every later change (settings, users, scale, package upgrade, node replacement, delete) runs as a rolling operation that never takes two replicas of one shard down at once and never breaks Keeper quorum.

## Pattern to copy

Ceph and Kubernetes are the host-service precedents. The split is the same: NixOS owns packages, units and files; the controller owns membership, secrets and anything that has to be identical on every member; the node agent owns the rebuild, the preflight, and local ClickHouse administration.

| Kubernetes today | ClickHouse |
|---|---|
| [`modules/kcore-kubernetes.nix`](../modules/kcore-kubernetes.nix) wraps `services.kubernetes` | `modules/kcore-clickhouse.nix` wraps `services.clickhouse` and adds a `clickhouse-keeper` unit |
| Installer seeds an empty `/etc/nixos/kcore-kubernetes.nix` and copies the module (`flake.nix`) | Same for `kcore-clickhouse.nix` |
| `ApplyKubernetesConfig` renders Nix on the agent from a typed request (`nixgen_k8s.rs`) | `ApplyClickhouseConfig` renders from a typed request (`nixgen_clickhouse.rs`) |
| `kubernetes_pki.rs` issues member certificates | `clickhouse_pki.rs` issues member certificates from the same cluster CA |
| `KubernetesHostVolume` carves LVM/ZFS volumes | `ClickhouseHostVolume`, same node RPCs |
| `KubernetesOrchestrator` with `BootstrapPhase` | `ClickhouseReconciler` with a persisted operation log |
| `kctl kubernetes plan / apply / get / health / scale / delete` | Same verbs under `kctl clickhouse` |
| MCP kind `kubernetes-cluster` | MCP kind `clickhouse-cluster` |

Two things differ from Kubernetes and are the reason this design has its own rollout planner:

- ClickHouse has no scheduler to drain to. Taking a replica down is safe only when another replica of the same shard is healthy and caught up, and only one replica per shard may be in flight.
- Keeper is a Raft ensemble. A member restart is safe only while a leader exists and the remaining followers are in sync; the leader rolls last.

## Topology

- A **cluster** has a name, a package attribute (`clickhouse` or `clickhouse-lts` from the node’s nixpkgs pin), shards, Keeper members, users, profiles, databases and server settings.
- A **shard** has a `shardName` (the `{shard}` macro) and one or more replicas.
- A **replica** is one `clickhouse-server` on one kcoreOS node with a `replicaName` (the `{replica}` macro), an advertise address, an interface and an optional data volume. A node hosts at most one server per cluster. In v1 a node belongs to at most one ClickHouse cluster; the unit name `clickhouse.service` is fixed by the NixOS module and the data path, ports and firewall rules are per node.
- A **Keeper member** is one `clickhouse-keeper` process with a Raft `serverId`. Voting members are one, three or five. A `learner: true` member sets `can_become_leader: false` and does not vote, so it does not count toward that number and does not sit in the quorum. Learners are how a second datacenter can hold a Keeper copy. Members may share a node with a replica; a dedicated Keeper node is also valid. Keeper membership, including who is a learner, is immutable in v1; plan returns `replace` when it changes. Keeper cannot flip `can_become_leader` on an existing `serverId`. Dynamic `reconfig` is a later phase.
- A **read replica** (`readOnly: true`) is in `remote_servers` at priority 100. The cluster uses `load_balancing: first_or_random`, so a Distributed insert lands on a writable replica (priority 1) and ReplicatedMergeTree copies the parts. A shard with no writable replica is a plan error. A read replica does not satisfy the rolling gate for a writable replica.
- **Datacenter** is an optional label on replicas and keepers. If any member sets it, every member must. It is not a ClickHouse macro. With more than one datacenter, no datacenter may hold a majority of the voting Keepers. Plan warns when every writable replica of a shard sits in one datacenter.
- `remote_servers.<cluster>` is rendered identically on every member from the shard list. `macros` are per replica. `zookeeper` lists every Keeper member.

### Defaults the module will ship

| Field | Default | Why |
|---|---|---|
| `package` | `clickhouse` | The pinned nixpkgs (`b40629ef`) resolves it to `26.2.4.23-stable`. `clickhouse-lts` is the other accepted value. Upstream stable on 2026-10-16 is `26.9.20.23`; the pin does not move with it. |
| `tls.disabled` | `false` | Member certs from the kcore CA. The packaged `config.xml` already opens plaintext `9000`, `8123` and `9009`, so the module removes those keys. It does not leave them bound to localhost. |
| `ports` | `tcpSecure 9440`, `https 8443`, `interserver 9010`, `keeper 9281`, `keeperRaft 9234`, `prometheus 9363` | Upstream secure ports. Raft `9234` is TLS because `raft_configuration.secure` is true. Keeper’s compiled default client port, if the key is omitted, is `2181`, so the module always sets `tcp_port_secure` and removes `tcp_port`. Opened on `iface` only. Prometheus stays plaintext and is limited to `clientNetworks`. |
| `maxServerMemoryRatio` | `0.5` | `max_server_memory_usage_to_ram_ratio`. Upstream’s 0.9 would starve guests and the agent; validation refuses values above 0.8. |
| `maxReplicaDelaySeconds` | `30` | Rolling gate on `system.replicas.absolute_delay`. |
| `stepTimeoutSeconds` | `600` | A rolling step that does not pass its gate in this time fails the operation. |
| `replicaName` | the node id | Stable and unique inside a kcore cluster. |
| `shardName` | 1-based position | `"1"`, `"2"`, … |
| `keeper.serverId` | 1-based position | Raft ids. |
| Data path | `/var/lib/kcore-clickhouse/<cluster>/data` | On the data volume when one is declared, else the OS disk. |
| Keeper path | `/var/lib/kcore-clickhouse/<cluster>/keeper` | Same rule. Keeper fsyncs every append; a separate device is recommended on nodes that also run a replica. |
| `default` user | kcore-generated SHA-256 password, `networks` = `127.0.0.1` plus `clientNetworks` | Retrieved with `GetClickhouseClusterCredentials`. Cannot be redefined in `users`. |
| Interserver credentials | kcore-generated | Written outside the Nix store. |
| Keeper identity | `kcore:<random password>` | Written as `zookeeper.identity`. ClickHouse hashes it for the digest ACL. The value is the plaintext `user:password`, not a precomputed digest. |
| Cluster secret | kcore-generated | `remote_servers.<cluster>.secret`, via `from_env`. Distributed queries and, from 26.7, interserver `TablesStatus` use it. |

Preflight rejects a spec whose advertise address is not present on `iface`, whose `iface` is DHCP-managed, `br0`, a `kbr-*` bridge, or the node’s Ceph cluster interface, whose ports are in use by another unit, or whose Keeper member count is even.

### Example manifest

```yaml
kind: ClickhouseCluster
metadata:
  name: analytics
spec:
  package: clickhouse
  shards:
    - shardName: "1"
      replicas:
        - nodeId: node-105
          advertiseAddress: 192.168.40.105
          iface: enp6s0
          dataVolume: { name: ch-analytics, storageClass: lvm, volumeGroup: vg0, sizeBytes: 536870912000, fsType: xfs }
        - nodeId: node-106
          advertiseAddress: 192.168.40.106
          iface: enp6s0
          dataVolume: { name: ch-analytics, storageClass: lvm, volumeGroup: vg0, sizeBytes: 536870912000, fsType: xfs }
    - shardName: "2"
      replicas:
        - nodeId: node-108
          advertiseAddress: 192.168.40.108
          iface: enp6s0
        - nodeId: node-109
          advertiseAddress: 192.168.40.109
          iface: enp6s0
  keepers:
    - { nodeId: node-105, advertiseAddress: 192.168.40.105, iface: enp6s0 }
    - { nodeId: node-106, advertiseAddress: 192.168.40.106, iface: enp6s0 }
    - { nodeId: node-108, advertiseAddress: 192.168.40.108, iface: enp6s0 }
  databases:
    - name: events
  users:
    - name: ingest
      passwordSha256Hex: 36dd292533174299fb0c34665df468bb881756ca9eaf9757d0cfde38f9ededa1
      networks: [192.168.40.0/24]
      profile: ingest
  profiles:
    - name: ingest
      settings:
        max_memory_usage: "10000000000"
        async_insert: "1"
  serverSettings:
    max_concurrent_queries: "200"
  clientNetworks: [192.168.40.0/24]
```

Canonical YAML is camelCase and snake_case aliases are accepted, the same rule as the release-train YAML. `kctl` converts the manifest to `ClickhouseClusterSpec` and never interprets fields itself.

The example nodes are placeholders. `node-107` is the kcore controller in the garage lab and stays out of `spec` until the operator puts it there.

## API

Controller RPCs (`service Controller`): `PlanClickhouseCluster`, `CreateClickhouseCluster`, `UpdateClickhouseCluster`, `GetClickhouseCluster`, `ListClickhouseClusters`, `DeleteClickhouseCluster`, `ScaleClickhouseCluster`, `ReplaceClickhouseReplica`, `RestoreClickhouseReplica`, `UpgradeClickhouseCluster`, `GetClickhouseClusterHealth`, `Get/List/CancelClickhouseOperation`, `GetClickhouseClusterCredentials`, `Backup/Restore/ListClickhouseBackups`.

Node RPCs (`service NodeAdmin`): `CheckClickhousePreflight`, `ApplyClickhouseConfig`, `GetClickhouseStatus`, `SetClickhouseUnitState`, `RunClickhouseAdmin`. The existing `GetNixApplyStatus` reports on the rebuild.

Messages and comments are in the proto file. Rules that the proto cannot express:

- **Immutable after create:** `name`, `shardName`, `replicaName` once the replica has joined, `keepers` (count, ids, addresses), `tls.disabled`, `ports`. Plan returns `replace`; create of a new cluster is the path.
- **Upgrade, not update:** `package`. `UpdateClickhouseCluster` with a different `package` returns `INVALID_ARGUMENT` pointing at `UpgradeClickhouseCluster`. A lower resolved version is refused without `force`. ClickHouse on-disk metadata does not downgrade across any component, not only the first.
- **Scale, not update:** adding or removing shards or replicas through `Update` is accepted when the diff is only membership; the controller runs it as a scale operation and plan says so. `Scale` exists for the CLI verb and for MCP.
- **Mutable with reload only:** `users`, `profiles`, `quotas`, `databases`, `clientNetworks`, config dictionaries, named collections, `serverSettings` keys ClickHouse reloads, shard `weight`, `readOnly`. ClickHouse re-reads `config.d` and `users.d`; no unit restart. Weight and read-replica priority live on the shard inside `remote_servers`. SQL functions are applied with `CREATE OR REPLACE FUNCTION` on one writable replica and replicate through the access storage. System-log retention is an `ALTER TABLE ... MODIFY TTL` on each server, not a unit restart: a config change does not alter an existing system log.
- **Mutable with rolling restart:** `serverSettings` keys ClickHouse does not reload (the agent keeps the list: `max_server_memory_usage`, `mark_cache_size`, `max_connections`, …), `maxServerMemoryRatio`, `dataVolume` (refused while the replica has data; `replace` otherwise).
- **Controller-only:** `maxReplicaDelaySeconds` and `stepTimeoutSeconds` change how the next operation runs and touch no node.
- **Owned keys:** `serverSettings` may not set `listen_host`, any `*_port`, `path`, `macros`, `remote_servers`, `zookeeper`, `openSSL`, `interserver_*`, `users_config`, `include_from`, `user_directories`, `prometheus`, `http_handlers`, `query_log`, `part_log`, `trace_log`, `text_log`, `dictionaries_config`, `named_collections`, `user_defined_executable_functions`. Plan reports the key. `learner` and Keeper membership stay immutable. `datacenter` is controller-only.
- **`confirm`:** every mutating RPC requires `confirm: true`. Dry-run is `Plan`.
- **`force`:** delete with unreachable members, removal of a shard’s last replica, removal of a shard that still holds parts, major-version downgrade. Force never skips the operation log.

### SQLite

- `clickhouse_clusters(name PK, spec_json, generation, observed_generation, phase, message, active_operation_id, created_at, updated_at)`
- `clickhouse_cluster_members(cluster_name FK, node_id, roles, shard_name, replica_name, keeper_server_id, advertise_address, iface, joined_generation, state, datacenter, read_only, learner)` with PK `(cluster_name, node_id)`
- `clickhouse_cluster_secrets(cluster_name PK, default_password_sha256_hex, default_password_enc, interserver_user, interserver_password_enc, keeper_secret_enc, cluster_secret_enc, metrics_password_enc, named_collection_secrets_enc, ca_pem)` with the same at-rest handling as `ceph_dashboard_credentials`
- `clickhouse_member_certs(cluster_name, node_id, cert_pem, not_after)` read by `cert_rotation_reconciler.rs`
- `clickhouse_operations(id PK, cluster_name FK, kind, state, target_generation, steps_json, created_at, updated_at)`

Each table gets a `schema_down.rs` entry. Phases: `pending`, `bootstrapping`, `ready`, `degraded`, `updating`, `upgrading`, `scaling`, `deleting`, `failed`.

## Nix module

`modules/kcore-clickhouse.nix`, options under `kcore.clickhouse`:

- `enable`, `clusterName`, `generation`, `package` (`"clickhouse"` or `"clickhouse-lts"`, resolved with `pkgs.${name}`)
- `advertiseAddress`, `iface`, `secretsDir`
- `tls.enable`, `tls.certFile`, `tls.keyFile`, `tls.caFile`
- `ports.{tcp,tcpSecure,http,https,interserver,keeper,keeperRaft,prometheus}`
- `server.enable`, `server.shard`, `server.replica`, `server.readOnly`, `server.dataPath`, `server.remoteServers`, `server.users`, `server.profiles`, `server.quotas`, `server.databases`, `server.dictionaries`, `server.namedCollections`, `server.functions`, `server.settings`, `server.systemLogs`, `server.clientNetworks`, `server.maxServerMemoryRatio`, `server.metricsPublic`
- `keeper.enable`, `keeper.serverId`, `keeper.learner`, `keeper.dataPath`, `keeper.raftPeers`

When `enable` is false the module turns `services.clickhouse` off, defines no Keeper unit and opens no ports. A Keeper-only member keeps `kcore.clickhouse.enable` true and `server.enable` false. That combination still creates the `clickhouse` user and group. `services.clickhouse.enable` stays false, so the NixOS module would not create the user on its own.

A node that runs both roles uses one `advertiseAddress` and one `iface`. Plan rejects a spec that disagrees. The Keeper volume name, when set, must differ from the server volume name on that node.

### What the module renders

- `services.clickhouse.enable`, `package`, and `serverConfig` (the NixOS module’s YAML attrset, written to `config.d/100-nixos-module-config.yaml`). The packaged `config.xml` is still installed beside it, so keys that must disappear use ClickHouse’s `remove` attribute (`"@remove" = "remove"` in YAML). The attrset:
  - replaces `listen_host` with `127.0.0.1` and the advertise address (the agent dials `127.0.0.1`, replicas dial the advertise address)
  - removes `tcp_port`, `http_port` and `interserver_http_port` when TLS is on, and sets `tcp_port_secure`, `https_port`, `interserver_https_port`, `interserver_https_host`
  - sets `path` and the absolute children the packaged file hard-codes (`tmp_path`, `user_files_path`, `format_schema_path`, `access_control_path`, logger files) under `dataPath`
  - sets `macros`, `remote_servers.<clusterName>` with `internal_replication = true` on every shard, a `secret` taken from the environment, and `user_directories` (stock `users_xml` plus `replicated` at `/clickhouse/access/`)
  - sets `zookeeper` nodes to Keeper port `9281` with `secure: 1`, and `identity` from the environment
  - sets `openSSL`, `logger`, `server.settings`, and the system-log engine strings (`query_log`, `part_log`, `trace_log`, `text_log`) with the TTL in the engine
  - binds `prometheus` to `127.0.0.1` only. `metrics_public` is the only way that port is also opened on `iface`. An `http_handlers` rule serves `GET /clickhouse/metrics` on the HTTPS port; the native port has no authentication
  - renders config dictionaries and named collections. Named-collection secret keys use `@from_env` and `overridable: false`
  - `usersConfig` carries profiles, quotas and users. Password hashes are `@from_env` references, never the hex digest. The `kcore-metrics` user is one of those rows. The name `default` is reserved for both users and quotas.
- `systemd.tmpfiles` rules create `dataPath`, the Keeper path, `secretsDir` and the TLS directory. Data and Keeper directories are `0750` `clickhouse:clickhouse`. The secrets directory is `0700` root.
- `systemd.services.clickhouse-keeper` running `${pkg}/bin/clickhouse-keeper --config-file=/etc/clickhouse-keeper/keeper_config.yaml`, `User=clickhouse`, `Type=simple`, its own YAML generated with `pkgs.formats.yaml`, `RequiresMountsFor = keeper.dataPath`. `Type=simple` so a keeper that never sends systemd `READY` still becomes active; the reconciler’s quorum gate is the readiness check. Use `Type=notify` only after the packaged binary is shown to send `READY`. The YAML sets `tcp_port_secure`, removes `tcp_port` (compiled default `2181`), sets `raft_configuration.secure = true`, and includes the same `openSSL` material. Without that section a standalone keeper has no certificate and the secure port does not open.
- `systemd.services.clickhouse.serviceConfig.RequiresMountsFor = server.dataPath`.
- `restartIfChanged = false` and `stopIfChanged = false` on both units. `nixos-rebuild switch` never restarts ClickHouse; the controller restarts members one at a time through `SetClickhouseUnitState`. The `ExecStop` timeout covers `shutdown_wait_unfinished_queries`.
- A `/etc/clickhouse-client/config.xml` with `secure`, `host = 127.0.0.1`, and an `openSSL.client` block (`caConfig` pointing at the CA, `verificationMode = strict`), so `clickhouse-client` on the host and in the agent needs no flags. The certificate SAN list includes `127.0.0.1`.
- Firewall: `networking.firewall.interfaces.<iface>.allowedTCPPorts` for the port set, and `extraCommands` iptables rules that restrict `interserver`, `keeper` and `keeperRaft` to member addresses and the client ports to `clientNetworks`. Prometheus `9363` is not in that set unless `metrics_public` is set, and then plan warns that the port has no authentication. Nothing opens on `br0`, `kbr-*` or the Ceph interface.
- Assertions: `advertiseAddress` set when enabled; `server.enable || keeper.enable`; odd `raftPeers` count; `serverId` present in `raftPeers`; `shard`/`replica` present in `remoteServers`; `tls.enable` implies the three file paths; `ports` pairwise distinct; `dataPath` under `/var/lib/kcore-clickhouse/`.

### Secrets stay out of the store

Nothing secret is interpolated into the Nix fragment. The agent writes `${secretsDir}/server.env` and `${secretsDir}/keeper.env` with mode `0400`, owner root, and the TLS material with owner `clickhouse`, mode `0400`. The units get `EnvironmentFile=` (systemd reads it as root) and the YAML uses `from_env` attributes. ClickHouse copies substituted values into `preprocessed_configs`, so each secret element also sets `hide_in_preprocessed`. The attribute form below is what the YAML parser turns into XML attributes:

```yaml
interserver_http_credentials:
  user: kcore-interserver
  password:
    "@from_env": KCORE_CH_INTERSERVER_PASSWORD
    "@hide_in_preprocessed": "true"
remote_servers:
  analytics:
    secret:
      "@from_env": KCORE_CH_CLUSTER_SECRET
      "@hide_in_preprocessed": "true"
zookeeper:
  node:
    - host: 192.168.40.105
      port: 9281
      secure: 1
  identity:
    "@from_env": KCORE_CH_KEEPER_IDENTITY
    "@hide_in_preprocessed": "true"
```

`KCORE_CH_KEEPER_IDENTITY` is `kcore:<password>`. User rows use the same shape for `password_sha256_hex`, with one env var per user. The hex digest never appears in the Nix fragment or in `environment.etc`.

The eval test asserts that rendered `environment.etc` text contains `@from_env` and `@hide_in_preprocessed`, and that it contains neither a `password_sha256_hex` hex digest nor the secret values. `@from_env` is the attribute name; its presence is required.

### Rendered fragment

```nix
{ ... }:
{
  kcore.clickhouse = {
    enable = true;
    clusterName = "analytics";
    generation = 3;
    package = "clickhouse";
    advertiseAddress = "192.168.40.105";
    iface = "enp6s0";
    secretsDir = "/var/lib/kcore-clickhouse/analytics/secrets";
    tls = {
      enable = true;
      certFile = "/var/lib/kcore-clickhouse/analytics/tls/node.crt";
      keyFile = "/var/lib/kcore-clickhouse/analytics/tls/node.key";
      caFile = "/var/lib/kcore-clickhouse/analytics/tls/ca.crt";
    };
    server = {
      enable = true;
      shard = "1";
      replica = "node-105";
      dataPath = "/var/lib/kcore-clickhouse/analytics/data";
      remoteServers = [
        {
          shard = "1";
          weight = 1;
          internalReplication = true;
          replicas = [
            { replica = "node-105"; host = "192.168.40.105"; port = 9440; secure = true; }
            { replica = "node-106"; host = "192.168.40.106"; port = 9440; secure = true; }
          ];
        }
      ];
      databases = [ "events" ];
      users = [ { name = "ingest"; passwordEnv = "KCORE_CH_USER_INGEST"; networks = [ "192.168.40.0/24" ]; profile = "ingest"; } ];
      profiles = [ { name = "ingest"; settings = { max_memory_usage = "10000000000"; }; } ];
      settings = { max_concurrent_queries = "200"; };
      clientNetworks = [ "192.168.40.0/24" ];
    };
    keeper = {
      enable = true;
      serverId = 1;
      dataPath = "/var/lib/kcore-clickhouse/analytics/keeper";
      raftPeers = [
        { id = 1; host = "192.168.40.105"; port = 9234; }
        { id = 2; host = "192.168.40.106"; port = 9234; }
        { id = 3; host = "192.168.40.108"; port = 9234; }
      ];
    };
  };
}
```

Every interpolated string passes through `kcore_sanitize::nix_escape`. Names split into two grammars, because the defaults do not fit one:

- Database, user, profile, quota and cluster names are `[A-Za-z_][A-Za-z0-9_]*`. Setting keys are `[a-z_][a-z0-9_]*`. These become unquoted ClickHouse identifiers or Nix attribute keys and also pass through `sanitize_nix_attr_key`.
- Shard and replica names are `[A-Za-z0-9][A-Za-z0-9._-]{0,63}`. The default shard name is `"1"` and the default replica name is the node id (`node-105`). Neither matches the identifier grammar. They are macro values and YAML scalars, not Nix attribute keys. Quotes, backticks, slashes, spaces and `${` are rejected.

An empty `networks` list on a user means `clientNetworks`, or `127.0.0.1` when that list is also empty. `::/0` is never the default.

If the pinned nixpkgs module changes shape (it is maintained by one person upstream), the fallback is the Ceph overlay approach: pin a nixpkgs revision for the package and keep the module in-tree.

## Node agent

Files: `crates/node-agent/src/nixgen_clickhouse.rs` (pure render), `crates/node-agent/src/clickhouse_ops.rs` (unit control, `mntr`, `clickhouse-client` calls, SQL builder), handlers in `crates/node-agent/src/grpc/admin.rs`.

`CheckClickhousePreflight`:

- nixpkgs pin resolves `pkgs.<package>`; the resolved version is returned
- `nixos-rebuild dry-build` of the rendered fragment succeeds
- `iface` exists, is not `br0`, `kbr-*`, or the Ceph cluster interface, and is not DHCP-managed
- `advertiseAddress` is present on `iface`
- every port in the set is free or already owned by this cluster’s units
- declared volumes: VG or pool exists and has room, or the LV/zvol already exists with the expected size and filesystem
- clock is synchronised (`timedatectl show -p NTPSynchronized`); Keeper and replication both depend on it
- memory: warn when `max_server_memory_usage_to_ram_ratio` × RAM leaves less than 2 GiB for the host and running guests
- when the node already serves this cluster: `nix_action` is `no-change`, `rebuild`, or `restart` depending on whether the fragment differs and whether a differing key is reloadable

`ApplyClickhouseConfig`:

1. Create, format and mount the declared volumes through the existing storage path when they are not mounted yet. Mount points are fixed by convention and refused outside `/var/lib/kcore-clickhouse/`.
2. Write the secrets and TLS files atomically (temp file, `fsync`, rename).
3. Refresh `/etc/nixos/modules/kcore-clickhouse.nix` from `include_str!` and write `/etc/nixos/kcore-clickhouse.nix`.
4. `nixos-rebuild test` then `switch` under the node apply lock, through the transient unit the agent already uses, so the rebuild cannot overlap a VM, Ceph or Kubernetes apply and survives the agent restart.
5. Return `apply_id`; the controller polls `GetNixApplyStatus`.

`enable = false` stops both units, runs the rebuild, then removes the secrets directory. Volumes are released by a separate storage call so `retain_data` is honoured.

`GetClickhouseStatus` reads `systemctl is-active`. Keeper’s client port is TLS, so a raw `mntr` write does not complete a handshake. The agent runs `openssl s_client -connect 127.0.0.1:9281 -CAfile <ca> -quiet` and writes `mntr`, then parses the body. It runs three fixed queries over `clickhouse-client --secure --host 127.0.0.1`: version, `system.replicas` aggregates (`countIf(is_readonly)`, `max(absolute_delay)`, `sum(queue_size)`), and a `system.zookeeper` probe for the session. `max()` over no rows is `NULL`; the parser treats that as zero. An empty `system.replicas` is not health. The Keeper session probe is.

`RunClickhouseAdmin` accepts a `oneof` of commands. The agent builds each statement with `quote_identifier` and `quote_literal` from `kcore-sanitize`; the controller never sends SQL. A new command is a proto change plus a builder plus tests.

## Controller

Files: `clickhouse_cluster_spec.rs` (normalise, validate, defaults, diff → action and changed fields), `clickhouse_pki.rs`, `clickhouse_rollout.rs` (pure planner), `clickhouse_cluster_reconciler.rs` (executes operations), `db.rs` additions, gRPC handlers.

### Rollout planner

`plan_rollout(spec, members, health, kind) -> Vec<Step>` is a pure function. The reconciler executes steps; the planner decides order. Invariants the planner guarantees and the tests check:

1. At most one replica per shard is in flight.
2. A writable replica is restarted only when another writable replica of the same shard is healthy (`unit_active`, `readonly_tables == 0`, delay below threshold). A read replica does not satisfy that gate. A read replica is restarted when any other replica of the shard is healthy. A shard whose only writable replica is restarting produces a warning in plan and a downtime step for inserts; the operator sees it before confirming.
3. Keeper steps come before server steps in an upgrade. Voting followers first, leader last, learners after the voting members; the leader is re-read from `mntr` at execution time because it may move. A learner is never elected leader.
4. A voting Keeper member is restarted only when the ensemble has a leader and every other voting member is synced. A learner is restarted when a leader exists. Quorum is computed from voting members only.
5. Every restart step is followed by gate steps: `wait-nix`, unit active, Keeper session ok, `wait-replica-caught-up`.
6. Reload-only changes produce `apply-nix` on every node in parallel and no restart steps.
7. Steps are idempotent. A controller restart resumes the operation from the first step not in `succeeded`.

### Operations

Create, Update, Upgrade, Scale, Replace, Delete, Backup and Restore each create one row in `clickhouse_operations` and return its id. One active operation per cluster; a second request returns `FAILED_PRECONDITION` with the active id. `CancelClickhouseOperation` stops before the next step.

**Create**

1. Plan every node. Refuse on blocking issues.
2. Persist spec, members and generation 1. Generate secrets. Issue one certificate per node with SANs `advertiseAddress`, node hostname, `localhost`, `127.0.0.1`.
3. `ApplyClickhouseConfig` on all nodes in parallel. There is no data yet, so parallel is safe.
4. `wait-nix` on each node.
5. `wait-keeper-quorum`: a leader exists and all followers are synced.
6. Servers: unit active and Keeper session ok on every replica.
7. `create-databases`: on the first writable replica of the lowest shard, `CREATE DATABASE IF NOT EXISTS <db> ENGINE = Replicated('/clickhouse/databases/<db>', '{shard}', '{replica}')`, then wait until `EXISTS DATABASE` on every other replica. Parallel `CREATE` against the same Keeper path races. The macros stay quoted so ClickHouse expands them per replica.
8. On that same replica, `CREATE FUNCTION` for each spec function and `CREATE DICTIONARY` for each dictionary whose `database` is set. Config dictionaries and named collections arrived with the Nix apply.
9. `set-system-log-ttl` is a no-op on create: the engine string already has the TTL, and the tables are created on first start.
10. Verify `system.clusters` on one replica per shard lists every declared replica. Phase `ready`.

**Update**: diff → reload-only or rolling. Reload-only applies in parallel. Rolling follows the planner. `observed_generation` moves when the last step succeeds; a failure leaves the cluster in `degraded` with the mixed generations recorded per member, and plan on the same spec shows the remaining nodes.

**Upgrade**: preflight resolves the version on each node and refuses any lower version without `force`. Order: voting Keeper followers, the leader, learners, then one writable replica per shard at a time, then read replicas. Each node: `apply-nix` (new package, no restart because of `restartIfChanged = false`), then `restart` through `SetClickhouseUnitState`, then gates. `running_version` reports `a,b` while mixed.

Keeper on-disk feature flags are a static table in `clickhouse_rollout.rs`, reviewed when the pin moves:

| Flag introduced | Minimum step already required | What 26.2 → 26.9 does |
|---|---|---|
| 25.7 | 24.9 | Nothing. 26.2 is already past 25.7. |

A jump whose running version is below the minimum step and whose target is at or above the flag is refused until `intermediate_package` resolves inside the window (at least the minimum step, below the flag). The operation then upgrades every Keeper to that intermediate package, waits for quorum, upgrades every Keeper to the target, then rolls servers. `force` does not skip the window. The 26.7 server setting below is a separate gate and is not a Keeper flag.

From 26.7, `interserver_tables_status_require_auth` defaults to true and a node still on an older protocol does not send the cluster-secret hash. `TablesStatus` then fails for `Distributed` queries during the roll. The upgrade emits that setting as `false` only on members whose resolved version is at least 26.7, and drops the override with a reload once every server is past that line. The key is omitted on 26.2, which does not know it. The cluster secret itself is present from the first create, so a cluster that never mixes versions needs no override.

**Scale out a replica**: apply the new `remote_servers` to existing members (reload only), apply the new node, `create-databases` on it (the Replicated engine brings the schema, `ReplicatedMergeTree` fetches parts), `wait-replica-caught-up`. **Scale out a shard**: same, with a plan warning that kcore does not redistribute existing data.

**Scale in a replica**: refuse the last replica of a shard without `force`. Stop the unit, then `SYSTEM DROP REPLICA '<replica>' FROM DATABASE <db>` on a surviving replica for every database in the spec. `shard_name` chooses the survivor; it is not a SQL argument. Apply `enable = false` or a Keeper-only fragment on the node, apply the new `remote_servers` everywhere, release the volume unless `retain_data`. **Scale in a shard**: `local-parts-count` on each replica must be zero, or `force`.

**Replace replica**: scale in the old (its node may be unreachable; `force` skips the stop) then scale out the new. The new replica may reuse the old name only after `drop-replica` succeeded. `ReplaceClickhouseReplica` carries `force` for the unreachable case.

**Delete**: stop every server, stop every Keeper, apply `enable = false` on every node, release volumes unless `retain_data`, drop rows. `force` skips unreachable nodes; the next successful apply of the empty fragment on such a node removes the units.

**Restore replica**: `RestoreClickhouseReplica` runs `SYSTEM RESTORE REPLICA` for each readonly replicated table on that node. It is the recovery for lost Keeper metadata, not a backup restore. Without `force` the replica must be reporting readonly tables or a failed Keeper session, Keeper must have quorum, and another replica of the shard must be healthy. One such operation at a time. Then `wait-replica-caught-up`.

**Backup / Restore**: `BACKUP DATABASE … TO Disk('kcore_backups', '<name>/<shard>') SETTINGS …` or the S3 form, run on one writable replica per shard through `RunClickhouseAdmin`, polled with `BackupStatus`. The S3 target may be a kcore ObjectStore bucket; the access key is a controller-stored credential reference and never travels to the client. Unless `skip_access` is set, one writable replica also runs:

```sql
BACKUP
  TABLE system.users,
  TABLE system.roles,
  TABLE system.row_policies,
  TABLE system.quotas,
  TABLE system.settings_profiles,
  TABLE system.masking_policies
TO Disk('kcore_backups', '<name>/access')
```

`user_directories.replicated` keeps the default `allow_backup: true`. `users_xml` stays `allow_backup: false`, so the kcore-managed users in the Nix config are not dumped into the backup; those secrets stay in the controller. Restore of access runs on one writable replica after Keeper quorum. Running it on every replica would apply the entities twice. The Phase-1 hooks in [backup.md](./backup.md) are where the later backup service picks this up.

### Health and cert rotation

A `clickhouse_health` loop polls `GetClickhouseStatus` from every member every 30 s, computes `phase` (`ready`, `degraded` when any replica has readonly replicated tables or is lagging or a voting Keeper is down with quorum intact, `failed` when a shard has no writable replica or Keeper lost quorum), and feeds `metrics_targets.rs` from the agent’s scrape of `127.0.0.1:9363`. External Prometheus uses `https://<advertise>:8443/clickhouse/metrics` with the `kcore-metrics` user from `GetClickhouseClusterCredentials`. A spec read replica that is caught up is healthy; `system.replicas.is_readonly` is a different signal. `cert_rotation_reconciler.rs` reissues a member cert `rotate_before_days` before expiry. The agent rewrites the files, runs `ReloadConfig` for `clickhouse-server`, and rolls Keeper with the same quorum rules. Keeper has no documented config reload that picks up a new certificate.

### Interaction with kcore cluster updates

`ClusterUpdate` rolls host OS generations one node at a time and may reboot. The reconciler publishes a per-node “safe to take down” answer (another replica of each of this node’s shards is healthy, Keeper quorum survives without this member) that `cluster_update_reconciler.rs` consults before each node. That is a later phase; until then the operator checks `kctl clickhouse health` before approving an update.

## Operator surface

- `kctl clickhouse plan -f analytics.yaml` prints the action, changed fields, per-node preflight, resolved versions and the ordered rollout steps.
- `kctl clickhouse apply -f analytics.yaml --confirm` creates or updates; refused without a clean plan.
- `kctl clickhouse get analytics`, `list`, `health analytics`, `credentials analytics`, `operations analytics`, `operation <id>`, `cancel <id>`.
- `kctl clickhouse scale analytics --add-replica 1:node-110:192.168.40.110:enp6s0`, `--remove-replica 1/node-106`, `--add-shard -f shard.yaml`, `--remove-shard 2 --force`.
- `kctl clickhouse replace-replica analytics --shard 1 --old node-106 -f new-replica.yaml`.
- `kctl clickhouse restore-replica analytics --shard 1 --replica node-106 [--database events] [--force]`.
- `kctl clickhouse upgrade analytics --package clickhouse-lts [--intermediate-package clickhouse-lts]`.
- `kctl clickhouse backup analytics --disk /mnt/backups`, `restore`, `backups`. `--skip-access` omits SQL users and grants.
- `kctl clickhouse delete analytics --confirm [--force] [--retain-data]`.
- MCP kind `clickhouse-cluster` with advise questions: shard count, replicas per shard, which replicas are read replicas, member node ids, addresses, interfaces and datacenters, Keeper placement and which members are learners, data volumes, client networks, quota and log retention. Apply stays behind `confirm: true`.
- Dashboard: one status card (phase, replicas, Keeper quorum, running version, active operation) with the same weight as the Kubernetes card. The dashboard test mocks stub the new RPCs.

## Tests

Every layer gets its own test type, and the test lives next to the code it covers. The list is the exit criterion of the phases below.

### Static checks (CI `rust-checks`, `nix-checks`)

`cargo fmt --check`, `cargo clippy --all-targets -- --deny warnings`, `cargo audit`, `statix`, `deadnix`, `nixfmt --check`, `nix flake show`. Statix forbids aliases like `name = cfg.name;` in the module; write `inherit (cfg) name;`.

### Unit tests (Rust, `cargo test --workspace`)

- `clickhouse_cluster_spec.rs`: defaults fill (`replicaName`, `shardName`, `serverId`, ports, timeouts, system-log days); validation errors for even voting-Keeper count, a learner counted as a voter, one datacenter holding a majority of voters, a shard with no writable replica, a partial `datacenter` labeling, duplicate names, owned `serverSettings` keys, `default` in `users` or `quotas`, executable dictionary or UDF sources, a named-collection secret left in `keys`, names outside the two grammars (including shard `"1"` and replica `node-105` accepted, and `node-105` rejected as a database name), a user `networks` list left empty resolving to `clientNetworks`, the same node advertised on two interfaces, a shared volume name for server and Keeper, overlapping ports; diff classification (`unchanged`, `update` reload-only, `update` restart, `upgrade`, `scale`, `replace`) and `changed_fields` paths. A lower version of any component is `upgrade` refused without `force`. A Keeper flag jump without `intermediate_package` is a blocking issue. Flipping `learner` on an existing id is `replace`.
- `clickhouse_rollout.rs`: fixed topologies (1×1, 1×2, 2×2 with 3 Keepers, 3×3 with 5 Keepers, 2×2 with one read replica and two learners in a second datacenter) produce the expected step order; leader-last; learners after voters; a read replica does not unlock a writable restart; single writable replica yields a warning step; an intermediate Keeper package inserts a full Keeper pass before the target pass; resume from a half-finished operation produces only the remaining steps.
- `nixgen_clickhouse.rs`: escaping of `"`, `${`, newlines and non-ASCII in every string field; `enable = false` fragment is exactly the empty module shape; reloadable-vs-restart key list.
- `clickhouse_ops.rs`: SQL builders for every `RunClickhouseAdmin` command with identifier and literal quoting, including names containing `'`, `` ` ``, `;` and `--`; `DROP REPLICA … FROM DATABASE` once per database; `mntr` parser on recorded output from leader, follower, a TLS transcript that never finishes the handshake, and a port that answers nothing; `system.replicas` aggregate parser on TSV fixtures including a `NULL` delay.
- `db.rs`: CRUD for the five tables, cascade on cluster delete, up and down migration round-trip (`schema_down.rs`).
- `kctl`: manifest parsing in camelCase and snake_case, unknown-field rejection, output rendering for plan/get/health from canned responses.

### Property tests (proptest, same `cargo test`)

- For any spec from a generator of valid topologies: `plan_rollout` output satisfies invariants 1–6 above, every member appears exactly once in restart steps, and the plan is finite.
- For any string: the rendered Nix fragment never contains an unescaped `"` or `${` outside a string literal, and `nix-instantiate --parse` accepts it (run only when `nix` is on `PATH`, skipped otherwise).
- For any identifier from the accepted grammar: `quote_identifier` round-trips through a ClickHouse-style unquote; for any string, the quoted literal contains no unescaped `'`.
- Proto ↔ internal spec: `from_proto(to_proto(spec)) == spec` for generated specs.
- Diff is a function of the pair: `diff(a, a) == unchanged`, `diff(a, b)` classifies `replace` whenever an immutable field differs.

### Kani harnesses (`make kani`)

`quote_identifier`, `quote_literal` and `validate_clickhouse_name` move into `kcore-sanitize` next to `nix_escape` and get bounded harnesses with the existing `MAX_INPUT_LEN`: no output byte sequence ever closes the quote early; the validator accepts exactly the grammar.

### Golden and contract tests

`tests/fixtures/clickhouse/` holds manifests and their rendered fragments (`two-shards-three-keepers.yaml` → `.nix`, `keeper-only.nix`, `disabled.nix`). A Rust snapshot test renders each manifest and compares byte for byte. The Nix eval check imports those same `.nix` files, so a renderer change that breaks evaluation fails in the Nix job and a module option rename fails in the Rust job. Updating a golden file is a reviewed diff.

### Nix evaluation checks (`nix build .#checks.x86_64-linux.clickhouse-module-eval`)

`tests/clickhouse-module-eval.nix`, copied from `ceph-module-eval.nix`, evaluates every golden fragment and asserts:

- `systemd.services.clickhouse` and `clickhouse-keeper` exist with `restartIfChanged = false`, `EnvironmentFile` set, `RequiresMountsFor` set
- the generated `config.d` YAML has the right `macros`, `remote_servers` (`internal_replication`, cluster secret via `@from_env`), `zookeeper` (`secure: 1`, port 9281) and port set; `tcp_port`, `http_port` and `interserver_http_port` carry `@remove` when TLS is on; Keeper YAML removes `tcp_port`, sets `tcp_port_secure`, and sets `raft_configuration.secure`
- firewall ports are on `enp6s0` only and `iptables` rules mention the member addresses
- `environment.etc` text contains `@from_env` and `@hide_in_preprocessed`, and contains no `password_sha256_hex` hex digest and no secret value
- `kcore.clickhouse.enable = false` leaves `services.clickhouse.enable` false and no Keeper unit
- the module’s assertions fire on an even Keeper count and on a missing `advertiseAddress` (evaluated with `lib.evalModules` and checked for the expected message)

### NixOS VM integration test (`make test-clickhouse-vm`)

`tests/clickhouse-module.nix` with `pkgs.testers.runNixOSTest`, three machines, each running one Keeper member, two running a replica of one shard, with a static CA and certs generated in the test derivation. The script:

1. waits for Keeper quorum (`mntr` over the TLS client port reports one leader, two synced followers)
2. creates a `Replicated` database and a `ReplicatedMergeTree` table on replica A, inserts 1 000 rows, reads 1 000 rows on replica B
3. stops the Keeper leader, confirms a new leader within 30 s and that inserts still succeed
4. restarts replica A while inserting on B, confirms `system.replicas` catches up and row counts match
5. switches to a configuration with a changed reloadable setting and asserts the unit was not restarted (`systemctl show -p NRestarts`)

ClickHouse’s closure comes from `cache.nixos.org`, so the test downloads instead of compiling. It runs as its own job (`nix build .#checks.x86_64-linux.clickhouse-module`) because QEMU with three machines does not fit the `nix-checks` time budget; it is required on changes under `modules/kcore-clickhouse.nix`, `tests/clickhouse*`, and `crates/node-agent/src/nixgen_clickhouse.rs`.

### Controller integration tests with a fake node agent (Rust, `cargo test -p kcore-controller`)

An in-process tonic `NodeAdmin` implementation (served over a `tokio::io::duplex` channel, using the `tower` dev-dependency the crate already has) records calls and answers from a scripted state machine: preflight ok or failing, `GetNixApplyStatus` progression, `mntr` roles, replica lag that drops over time, a node that never answers. Scenarios:

- plan with one failing node returns the issue and persists nothing
- create on 2×2 + 3 Keepers calls apply on every node once, waits for quorum before database creation, ends `ready`
- update with a reload-only diff never calls `SetClickhouseUnitState`
- update with a restart diff restarts one replica per shard at a time and waits for the lag gate; a replica that never catches up fails the operation at `stepTimeoutSeconds` and leaves `degraded`
- upgrade restarts voting Keeper followers before the leader, learners after voters, and servers after Keeper; a jump across a Keeper flag boundary runs the intermediate package on Keepers before the target
- restore-replica runs `SYSTEM RESTORE REPLICA` on the named node and waits for the lag gate; a healthy replica is refused without `force`
- scale in refuses the last replica without `force`; with `force` it drops the replica from the survivor
- delete with an unreachable node fails without `force` and drops rows with it
- controller restart in the middle of an operation resumes at the first non-succeeded step
- a second mutating request while an operation runs returns `FAILED_PRECONDITION`

### gRPC contract tests

- `protoc`/`buf` build of both proto files in `build.rs` (existing).
- `buf breaking --against .git#branch=main` on `proto/` in CI once the RPCs are spliced; field renumbering or type changes fail the job.
- A Rust test that constructs every request message with every field set, encodes, decodes, and compares, so a field dropped from the conversion layer is caught.
- `kctl` manifest → proto → manifest round-trip for the fixtures.

### Formal model (`make test-tla`, `formal-checks.yml`)

`specs/tla/ClickhouseRollout.tla` models shards, replicas, Keeper members with a leader, and the rollout steps. Safety: every shard has at least one serving replica and Keeper keeps a majority at every state; at most one replica per shard is restarting. Liveness: with a fair scheduler every operation terminates in `succeeded` or `failed`. The bounded TLC config covers up to 3 shards × 3 replicas and 5 Keeper members. `scripts/test-clickhouse-rollout-trace.sh` runs the Rust planner on the fixtures, emits the step trace, and checks it against the model’s action labels the way `test-replication-trace.sh` does for replication, so the implementation cannot drift from the spec silently.

### Soak test (`scripts/soak-clickhouse.sh`, manual or nightly)

Against the VM test cluster or the lab: a writer inserts batches continuously while the controller runs an update, an upgrade and a replica replacement. The run passes when every batch is acknowledged exactly once (`count()` and `uniqExact(batch_id)` match the writer’s log) and `system.replicas` ends with zero read-only tables.

### Security tests

- Injection strings in cluster, shard, replica, database, user and setting names (`'; DROP`, `${…}`, `../`, Unicode quotes) are rejected by validation and, where the grammar allows them, rendered safely; asserted in the unit and property tests above.
- Volume names and mount paths outside `/var/lib/kcore-clickhouse/` are refused (`validate_path_under_root`).
- Eval check asserts firewall scope and the absence of secrets in `/etc`.
- `GetClickhouseClusterCredentials` requires the operator role and writes an audit row; tested in the gRPC auth tests like the Ceph dashboard credential RPC.
- Semgrep runs when the token is set.

### Lab end-to-end (phase gate, manual)

Real nodes: `kctl clickhouse apply`, `clickhouse-client --secure` from an operator host with the CA from `credentials`, a `ReplicatedMergeTree` write and cross-replica read, a reload-only update, a rolling update, a Keeper leader power-off, and a replica replacement. Runs only after the blocking questions below are answered.

### Where each test runs

| Test type | Location | Gate |
|---|---|---|
| Static checks | repo | `rust-checks`, `nix-checks` |
| Unit, property, golden, fake-agent, contract | `crates/*` | `rust-checks` |
| Kani | `kcore-sanitize` | `make kani` (manual; CI job later) |
| Nix eval | `tests/clickhouse-module-eval.nix` | `nix-checks` |
| NixOS VM | `tests/clickhouse-module.nix` | own job, path-filtered |
| TLA+ and trace drift | `specs/tla/`, `scripts/` | `formal-checks` |
| Soak | `scripts/soak-clickhouse.sh` | nightly / manual |
| Lab e2e | garage lab | manual, after questions |

## Phases

### 1. Module and eval check

- `modules/kcore-clickhouse.nix`, `tests/clickhouse-module-eval.nix`, flake check `clickhouse-module-eval`, installer seeding of the empty fragment and module import in `flake.nix`.
- Golden fragments written by hand for now.

Exit: `nix build .#checks.x86_64-linux.clickhouse-module-eval` passes including the disabled and Keeper-only fragments.

### 2. Proto, spec, render, planner (no node contacted)

- Splice the proto. Stub handlers return `UNIMPLEMENTED` so the build is green.
- `clickhouse_cluster_spec.rs`, `nixgen_clickhouse.rs`, `clickhouse_rollout.rs`, `kcore-sanitize` quoting, SQLite tables and down migration.
- Unit, property, golden and Kani tests above.

Exit: `cargo test --workspace -- clickhouse` passes; golden `.nix` now generated, and the eval check still passes on them.

### 3. Node agent RPCs

- Preflight, apply, status, unit state, admin commands, `clickhouse_ops.rs`.
- `mntr` and `system.replicas` parsers with recorded fixtures.

Exit: `cargo test -p kcore-node-agent -- clickhouse` passes; the NixOS VM test boots the module and runs steps 1–2.

### 4. Controller reconciler

- PKI, secrets, operations table, planner execution, create/update/upgrade/scale/replace/delete, health loop, metrics targets.
- Fake node agent integration tests.

Exit: fake-agent scenarios pass; TLA+ model and trace check added and green in `formal-checks`.

### 5. kctl, MCP, dashboard

- CLI verbs, MCP kind, dashboard card and mock stubs.

Exit: `kctl clickhouse plan` against a fake controller prints the rollout steps; apply is refused without a clean plan; MCP plan tests pass.

### 6. VM test complete and soak

- NixOS VM test steps 3–5, soak script.

Exit: VM job green in CI; soak passes against the VM cluster.

### 7. Lab apply

Blocked on the questions below. Rebuilds NixOS on live hypervisors.

Exit: the lab e2e list above.

### 8. Day-2 and later

- Backup and restore RPCs wired to the Disk and S3 targets, including the access-entity backup specified above. ObjectStore bucket as target.
- Cluster-update gate (“safe to take down”).
- Keeper dynamic `reconfig` behind a flag, lifting the immutable-membership rule.
- Tiered storage: an S3 disk in `storage_configuration` pointing at kcore ObjectStore.
- More than one ClickHouse cluster per node (needs instanced units and per-cluster paths in the module).

## Risks

- `nixos-rebuild switch` on a hypervisor restarts host units. The apply lock serialises it with VM, Ceph and Kubernetes applies, and `restartIfChanged = false` keeps ClickHouse itself out of the switch; still, plan names every node so the operator can drain guests first.
- Keeper on the same device as MergeTree merges adds fsync latency to every Raft append. The default warns when a node runs both without a separate Keeper volume.
- ClickHouse memory: upstream’s `max_server_memory_usage_to_ram_ratio` of 0.9 starves guests. `maxServerMemoryRatio` defaults to 0.5 and preflight warns when the remainder is below 2 GiB.
- A single-replica shard has downtime on every restart. Plan prints that as a warning; the operator confirms.
- The upstream NixOS module is small and maintained by one person; its `serverConfig` option is the integration point. If it changes, the fallback is an in-tree copy of the module with a pinned package, as with Ceph.
- Package versions follow the node’s nixpkgs pin. A kcore release that moves the pin moves ClickHouse. The upgrade operation handles the restart order, the 26.7 `TablesStatus` window, and any Keeper feature-flag window in the static table. The release notes name the ClickHouse version so an operator can hold a `ClusterUpdate`. 26.2 is already past the 25.7 flag line, so that pin jump needs no intermediate package.
- Firewall rules use iptables `extraCommands`, matching the Kubernetes module. A move to nftables on the host must carry these along.

## Quotas, dictionaries, functions, collections

Checked against `v26.9.20.23`. These objects are part of the spec.

**Quotas.** `spec.quotas` is a list of `{ name, intervals: [{ durationSeconds, queries, errors, resultRows, readRows, executionTimeSeconds }] }`. A user `quota` names one of them. The stock quota `default` stays the one from the packaged `users.xml` and cannot be redefined. A counter of 0 is unlimited. Quotas render into `usersConfig` and reload.

**Dictionaries.** `source: clickhouse` runs `query` against `127.0.0.1` over TLS. `source: https` fetches `url`; the URL has no userinfo and uses `https`. `source: executable` is a validation error. `layout` is `flat`, `hashed` or `cache`. With `database` empty the dictionary is XML in `config.d` and identical on every server. With `database` set, one writable replica runs `CREATE DICTIONARY` in that replicated database and Keeper copies it. Attribute types are a ClickHouse type name plus an optional length, and `nullable`.

**Functions.** `spec.functions` is `{ name, parameters, expression }` applied as `CREATE FUNCTION name AS (parameters) -> expression` on one writable replica. The expression is one expression: a semicolon is rejected. `executable` and `executable_pool` UDFs are refused. They would run a host binary as the `clickhouse` user on the hypervisor. SQL functions land in the replicated access storage, so a new replica receives them, and the access backup includes them.

**Named collections.** `keys` are non-secret values in the config. `secretRefs` maps a key to a controller credential ref. The agent writes the resolved value to the env file and the YAML uses `@from_env` with `overridable: false` and `@hide_in_preprocessed`. A secret value in `keys` is a validation error. DDL `CREATE NAMED COLLECTION` is not the path for these; the config copy is what every replica must have before startup.

## Metrics, system logs, restore, access backup

**Prometheus.** The native port 9363 does not authenticate. That is still true in 26.9. The module binds it to `127.0.0.1` and the agent scrapes it for `metrics_targets.rs`. Operators scrape `https://<advertise>:8443/clickhouse/metrics` as `kcore-metrics`, whose password is in `GetClickhouseClusterCredentials`. `metricsPublic: true` also opens 9363 on `iface` toward `clientNetworks` and plan warns that the scrape is unauthenticated.

**System logs.** Defaults are query log 14 days, part log 14 days, trace log 3 days, text log 7 days. The TTL is inside the engine string so the first start creates the table with it. A later change runs `ALTER TABLE system.<log> MODIFY TTL event_date + INTERVAL N DAY` through `SetSystemLogTtl` on every server. Reloading the config does not change the TTL of a table that already exists.

**Restore replica.** `RestoreClickhouseReplica` is the controller operation for `SYSTEM RESTORE REPLICA`. The node command was already in `RunClickhouseAdmin`. The operation refuses a replica whose Keeper session is healthy and whose replicated tables are writable, unless `force` is set. It restores one replica, then waits until that replica is caught up.

**Access backup.** Database backup is per shard, on one writable replica of that shard. Access backup is once per cluster, because the entities live in Keeper under `/clickhouse/access/`. `skip_access` omits it. Restore of access is also once.

## Blocking questions before phase 7

Code phases 1–6 do not need these. A lab apply does.

1. Which nodes host the first cluster, and which of them run Keeper? Placeholders are `node-105`, `node-106`, `node-108`; `node-107` is the controller and stays out unless you say otherwise.
2. Advertise addresses and interfaces per node. Confirm they are static or reserved.
3. Does each data node have a VG or pool with room for a dedicated data volume, and should Keeper get its own small volume on nodes that also run a replica?
4. Client networks allowed to reach 9440/8443, and whether anything but the lab management network needs access.
5. May phase 7 run `nixos-rebuild switch` on those nodes while they may still be hosting guests?
