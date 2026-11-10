# Bare-metal PostgreSQL clusters on kcoreOS

Status: **implemented in tree; lab target is the three Dell hypervisors**. The NixOS module, the pure `kcore-postgresql` crate, the node-agent RPCs, the controller engine and API, `kctl pg-cluster`, the ClusterUpdate switchover gate, the MCP kind, the dashboard card, and the NixOS VM test are in this revision. Apply on the Dells with `examples/postgresql-cluster/dell-lab.yaml`. See [Built and not built](#built-and-not-built).

Checked against nixpkgs `b40629efe5d6` (the flake pin). That pin provides `postgresql_14` 14.22, `postgresql_15` 15.17, `postgresql_16` 16.13, `postgresql_17` 17.9 (also `postgresql`), `postgresql_18` 18.3, `patroni` 4.1.0, `etcd` 3.6.8, `pgbackrest` 2.57.0 and `keepalived`. The NixOS `services.patroni` module is present. Its `raft` option was removed because upstream deprecated Raft, so the DCS has to be etcd. `vip-manager` is not packaged.

A `PostgresqlCluster` is one writable primary and one or more streaming standbys (the target shape is one primary and two read replicas). Members run as NixOS host services on kcoreOS nodes, not in VMs and not on Kubernetes. The Rust controller orchestrates them over gRPC. The node agent renders a Nix fragment, runs `nixos-rebuild`, and does local PostgreSQL and Patroni administration.

The gRPC surface is in `proto/controller.proto` (messages prefixed `Postgresql`, next to the Slurm RPCs) and `proto/node.proto` (`CheckPostgresqlClusterPreflight`, `ApplyPostgresqlClusterConfig`, `GetPostgresqlClusterStatus`, `RunPostgresqlAdmin`).

## What "done" means

An operator declares a `PostgresqlCluster` with three members. The controller validates it, issues certificates and secrets, and drives each member through the node agent. Each node gets an etcd member, Patroni and PostgreSQL. Patroni bootstraps the primary, the standbys clone it with `pg_basebackup`, and a writer VIP follows the primary. A row written through the VIP can be read on both standbys.

If the primary's host is powered off, a standby is promoted within `ttl` (30 s by default) with no controller involved, and the VIP moves to it. When the old host returns, it rejoins as a standby through `pg_rewind`. Every later change runs as a planned operation that keeps the cluster writable, except a major-version upgrade, which has a downtime window that plan states before the operator confirms. Changes are settings, roles, databases, scale, minor upgrade, major upgrade, switchover, member replacement, backup, restore and delete.

## Precedents

The ClickHouse lifecycle is the template. Its implementation is on `cursor/vm-ha-build-d1dd` (PRs #47 to #62) and has not been merged to `main`, so this code does not depend on it; it copies the layout and the rules. The Slurm cluster on `main` is the precedent for the controller-to-agent wiring (values file, `run_test_then_switch`, the failover-lease gate):

| ClickHouse | PostgreSQL |
|---|---|
| `crates/kcore-clickhouse/` (pure: `spec.rs`, `nixgen.rs`, `rollout.rs`, `ops.rs`) | `crates/kcore-postgresql/` with the same four modules |
| `crates/controller/src/clickhouse_{api,convert,db,engine,pki}.rs` | `crates/controller/src/grpc/controller/postgresql.rs` (API and conversion), `db/postgresql_store.rs`, `postgresql_cluster_engine.rs`, `postgresql_cluster_pki.rs` |
| `crates/node-agent/src/clickhouse_agent.rs` | `crates/node-agent/src/postgresql_cluster_agent.rs` |
| `modules/kcore-clickhouse.nix`, values in `/etc/nixos/kcore-clickhouse.nix` | `modules/kcore-postgresql-cluster.nix`, values in `/etc/nixos/kcore-postgresql-cluster.nix` |
| Keeper (Raft) holds coordination state | etcd holds Patroni's leader key and dynamic config |
| `ClickhouseNodes` trait, `GrpcNodes`, fake in tests | `PostgresqlNodes` trait, `GrpcNodes`, fake in tests |
| Operation rows with steps, CAS claim (`clickhouse_operation_claims`) | Operation rows with steps; one actor through the replicated failover lease (`may_act`, as Slurm) |
| `safe_to_take_down`, `keeper_restart_allowed` | `safe_to_take_down`, `etcd_restart_allowed`, `switchover_candidate` |
| `specs/tla/ClickhouseRollout.tla` | `specs/tla/PostgresqlRollout.tla` (not built yet) |
| `tests/clickhouse-module{,-eval}.nix` | `tests/postgresql-cluster-module{,-eval}.nix` |
| `kctl clickhouse …`, MCP `clickhouse-cluster` | `kctl pg-cluster …`, MCP `postgresql-cluster` |

The existing single-node resource stays as it is. That resource is `CreatePostgresql`, which renders `services.postgresql` through `nixgen::with_postgresql`. A node that carries one is refused as a cluster member, and `CreatePostgresql` refuses a node that is a cluster member. The NixOS Patroni module also asserts that `services.postgresql` and Patroni do not share a data directory.

## Who decides failover

**Patroni does, through etcd. The controller does not.**

This is the main design decision, and it is where PostgreSQL differs from the other kcore services.

- Unplanned failover has to work when the controller is down or partitioned. kcore controllers replicate with last-writer-wins and OR-sets ([ha-crdt-replication.md](./ha-crdt-replication.md)). Linearizability is a stated non-goal, and the operation claim is a compare-and-swap on each controller's local SQLite. Two controllers on opposite sides of a partition could each decide to promote a standby. For VMs, kcore can fence by RBD blocklist before it moves a guest. For PostgreSQL, a wrong promotion means two primaries accepting writes, and the second timeline is lost when they are reconciled.
- etcd is a linearizable Raft store. Patroni holds the leader key with a TTL. A primary that cannot renew the key demotes itself, and a standby promotes only after it acquires the key. Three etcd members, one on each PostgreSQL node, keep quorum through the loss of any one node.
- The controller still owns every planned change. A switchover, a restart, maintenance mode, a dynamic-config change or a reinit is a step in a controller operation. The node agent carries out each step by calling Patroni's REST API on `127.0.0.1`. The agent never promotes or demotes PostgreSQL by itself. It asks Patroni to, so the leader key always matches the actual primary.
- The controller observes failovers. The health loop sees the leader change and records a `failover` event with the old and new leader, the timeline and the lag at promotion. It also sends the `postgresql.failover` webhook and tells a running operation to re-plan.

Alternatives considered:

- *Controller-driven promotion with an agent-side fence.* This needs a linearizable lease the controller does not have, and a fence that works when the old primary's host is unreachable. kcore has no such fence for a process on bare metal. Rejected.
- *Patroni with Raft (pysyncobj).* Removed from the pinned NixOS module because upstream deprecated it. Rejected.
- *Reusing a Kubernetes cluster's etcd.* This would couple the database's availability to an unrelated cluster's lifecycle and certificates. Rejected. Each PostgreSQL cluster gets its own etcd.

### Fencing without a hardware watchdog

Patroni's last line of defence is a watchdog. If Patroni hangs while it is primary, the watchdog reboots the host. On a kcore hypervisor that reboot also kills every guest, so `watchdog` defaults to `off`. Two things limit the damage instead:

1. **Synchronous quorum commit** (the default, below). A commit is acknowledged only after a standby confirms it. A hung old primary whose standbys now follow the new leader cannot acknowledge a commit, so no acknowledged write is lost on the old timeline.
2. **Writer VIP on `/primary` only.** keepalived holds the VIP only where Patroni's `GET /primary` returns 200. That endpoint requires Patroni to hold the leader key. A hung Patroni stops answering, the check fails, and the VIP leaves.

`watchdog: required` is available for members on dedicated database hosts. Plan warns when it is set on a node that hosts guests.

## Topology

- A **cluster** has a name, a package attribute (`postgresql_14` to `postgresql_18`), members, a replication mode, PostgreSQL parameters, roles, databases, extensions, an optional writer VIP and an optional backup target.
- A **member** is one node running one etcd member, Patroni and PostgreSQL. `role` in the spec is only the *initial* role. `primary` marks the bootstrap node. After bootstrap, who is primary is live state that Patroni owns. The spec does not change when a failover happens, and plan never reports a failover as a diff.
- **Member count.** PostgreSQL members: 1 to 5 (1 is valid but has no failover and plan warns). etcd voters: 3 or 5, always odd. With 2 PostgreSQL members, the spec must add a `witness` (an etcd-only member, for example on the controller node) to reach 3 voters. In v1 every PostgreSQL member is also an etcd voter. The witness exists only to make the vote odd.
- **Read replica tags.** `noFailover: true` sets the Patroni tag `nofailover` (never promoted, for example in a second datacenter). `noLoadBalance: true` removes the member from the reader endpoint. `failoverPriority` (Patroni 4.0 or later) orders candidates. A cluster with no member that can be promoted is a plan error.
- **Datacenter** is an optional label. If any member sets it, every member must. With more than one datacenter, no datacenter may hold a majority of the etcd voters unless the operator accepts that with `allowDcMajority`.
- **One PostgreSQL cluster per node.** `services.patroni` is a single instance. A node with an existing `Postgresql` resource, another PostgreSQL cluster member, or a Kubernetes etcd on the same ports is refused. The etcd ports default to 2479/2480 so they do not collide with Kubernetes' 2379/2380.

### Replication and RPO

| `replication.mode` | Patroni settings | RPO | Write availability |
|---|---|---|---|
| `quorum` (default with 2 or more standbys) | `synchronous_mode: quorum`, `synchronous_node_count: 1` | 0 for acknowledged commits | Writes continue while any one standby is up |
| `sync` | `synchronous_mode: on` | 0 | Same as above, but Patroni names the sync standby |
| `strict` | `synchronous_mode: on`, `synchronous_mode_strict: true` | 0 | Writes stop when no sync standby is up |
| `async` (default with 1 standby) | `synchronous_mode: off`, `maximum_lag_on_failover` | Up to `maximumLagOnFailoverBytes` | Always |

Plan prints the RPO line. With `async`, plan warns that a failover can drop acknowledged commits.

### Client endpoints

- **Writer.** With `vip` set, keepalived on every member tracks `curl --cacert … https://127.0.0.1:8008/primary`, so the VIP sits on the primary only. The VRRP instance uses `nopreempt`, and its priority comes from the check result rather than the node. The rendering copies `modules/kcore-kubernetes.nix` (the `kcore_api` instance) and reuses `kcore-keepalived-notrack.nix`. Without a VIP, clients use the libpq multi-host string with `target_session_attrs=read-write`.
- **Readers.** A libpq multi-host string over the standbys with `target_session_attrs=prefer-standby&load_balance_hosts=random` (libpq 16 or later on the client). This needs no extra daemon. `GetPostgresqlClusterCredentials` returns both strings. A reader VIP or HAProxy is a later phase.

### Defaults the module ships

| Field | Default | Why |
|---|---|---|
| `package` | `postgresql_17` | 17.9 at the pin. Not the unversioned `postgresql`: that alias moving with a pin bump would be a major upgrade without an operator decision. Validation refuses `postgresql`. |
| `ports` | `postgres 5432`, `patroniApi 8008`, `etcdClient 2479`, `etcdPeer 2480` | etcd moved off 2379/2380 for Kubernetes coexistence. |
| `tls.disabled` | `false` | Member certs from the cluster CA. They cover PostgreSQL `ssl`, replication (`sslmode=verify-full`), etcd client and peer, and the Patroni REST API. |
| `patroni.ttl` / `loopWait` / `retryTimeout` | `30` / `10` / `10` | Upstream defaults. Validation enforces `loopWait + 2*retryTimeout <= ttl`. |
| `replication.maximumLagOnFailoverBytes` | `1048576` | Upstream default. Used by async mode and by the switchover gate. |
| `usePgRewind` | `true` | Needs `wal_log_hints` or data checksums. The module initdb's with `--data-checksums` and also sets `wal_log_hints = on`. |
| `memoryLimitBytes` | 25 % of host RAM | `MemoryMax` on `patroni.service`. `shared_buffers` defaults to 25 % of it and `effective_cache_size` to 60 %. The host still runs guests. |
| `maxReplicationLagBytes` | `16777216` | Rolling gate: a standby is caught up below this. |
| `stepTimeoutSeconds` | `600` | A step that does not pass its gate in this time fails the operation. |
| `watchdog` | `off` | See the fencing section. |
| Data path | `/var/lib/kcore-postgresql/<cluster>/pgdata/<major>` | `pgdata` is the `dataVolume` mount when declared. Each major gets its own directory on the same filesystem, so `pg_upgrade --clone` works and rollback keeps the old one. |
| WAL path | inside the data dir | With `walVolume`: `/var/lib/kcore-postgresql/<cluster>/pgwal/<major>`, via `initdb --waldir`. |
| etcd path | `/var/lib/kcore-postgresql/<cluster>/etcd` | Same volume rule as Keeper: a separate small volume is recommended. |
| `postgres` superuser | kcore-generated password | `GetPostgresqlClusterCredentials`, operator role, audited. |
| `replicator`, `rewind`, Patroni REST user | kcore-generated | Never leave the controller and the member's secrets directory. |

Preflight rejects a spec whose advertise address is not on `iface`, or whose `iface` is DHCP-managed, `br0`, a `kbr-*` bridge or the Ceph cluster interface. It also rejects ports already used by another unit, a node with `services.postgresql` enabled, and a VIP outside the `iface` subnet.

### Example manifest

Lab apply uses the three Dells: `examples/postgresql-cluster/dell-lab.yaml` (`dell-1` / `dell-2` / `dell-3`, `enp6s0`, `vg_kcore`, VIP `192.168.40.160/24`). A generic three-node template remains at `examples/postgresql-cluster/orders.yaml`.

```yaml
kind: PostgresqlCluster
metadata:
  name: orders
spec:
  package: postgresql_17
  members:
    - nodeId: dell-1
      role: primary
      advertiseAddress: 192.168.40.103
      iface: enp6s0
      dataVolume: { name: pg-orders, storageClass: lvm, volumeGroup: vg_kcore, sizeBytes: 107374182400, fsType: xfs }
    - nodeId: dell-2
      advertiseAddress: 192.168.40.113
      iface: enp6s0
      dataVolume: { name: pg-orders, storageClass: lvm, volumeGroup: vg_kcore, sizeBytes: 107374182400, fsType: xfs }
    - nodeId: dell-3
      advertiseAddress: 192.168.40.123
      iface: enp6s0
      dataVolume: { name: pg-orders, storageClass: lvm, volumeGroup: vg_kcore, sizeBytes: 107374182400, fsType: xfs }
  replication:
    mode: quorum
  vip:
    address: 192.168.40.160/24
    virtualRouterId: 61
  clientNetworks: [192.168.40.0/24]
  parameters:
    max_connections: "300"
    work_mem: "16MB"
  roles:
    - name: app
      login: true
  databases:
    - name: orders
      owner: app
      extensions: [pg_stat_statements, pgcrypto]
```

Canonical YAML is camelCase. snake_case keys are accepted (except inside `parameters`, whose keys are PostgreSQL names), and unknown fields are refused. `kctl` parses with `kcore_postgresql::spec::parse_manifest` and never interprets fields itself. A login role without `passwordScram` (a SCRAM-SHA-256 verifier) gets a kcore-generated password, returned by `credentials`. The controller host (`node-107` in the garage lab) stays out of `spec` unless the operator puts it there.

## Declarative split: Nix, DCS and SQL

PostgreSQL configuration lives in three places. The design keeps each fact in exactly one of them.

| Where | What | How it changes |
|---|---|---|
| **Nix fragment** (per node, identical except identity) | Package, Patroni local YAML (`restapi`, `etcd3`, `postgresql.listen`, `data_dir`, `bin_dir`, `pg_hba`, `authentication` env refs, tags), etcd unit, keepalived, firewall, tmpfiles, pgBackRest config | `ApplyPostgresqlClusterConfig` → `nixos-rebuild switch`, then `RELOAD` to Patroni (SIGHUP). Units have `restartIfChanged = false`. |
| **Patroni dynamic config in etcd** (cluster-wide) | `ttl`, `loop_wait`, `retry_timeout`, `maximum_lag_on_failover`, `synchronous_mode*`, `postgresql.parameters`, `use_pg_rewind`, `use_slots` | `bootstrap.dcs` seeds it on create only. Every later change is `PATCH /config` on one member through `RunPostgresqlAdmin.patch_dynamic_config`. Patroni applies it everywhere and sets `pending_restart` where needed. |
| **SQL on the primary** | Roles, databases, owners, extensions, grants kcore manages | `RunPostgresqlAdmin` commands built with `quote_ident` and `quote_literal`. The controller never sends SQL text. |

The spec diff classifies each changed parameter as `reload` or `restart`. The agent reads `pg_settings.context` on the running server (`postmaster` means restart) and returns the list in preflight. A static fallback table in `kcore-postgresql/src/spec.rs` covers plan when no member answers.

**Owned parameters.** `parameters` may not set `listen_addresses`, `port`, `data_directory`, `hba_file`, `ident_file`, `ssl*`, `primary_conninfo`, `primary_slot_name`, `restore_command`, `archive_command`, `archive_mode` (owned when `backup` is set), `hot_standby`, `wal_log_hints` or `cluster_name`. `wal_level` may be raised to `logical` but not lowered below `replica`. `max_connections`, `max_worker_processes`, `max_wal_senders`, `max_prepared_transactions` and `max_locks_per_transaction` must not be lower on a standby than on the primary. Because they are DCS settings they are always identical, and Patroni's restart ordering is used (decrease: primary first; increase: standbys first). Plan reports an owned key by name.

## Nix module

`modules/kcore-postgresql-cluster.nix`, options under `kcore.postgresqlCluster`:

- `enable`, `clusterName`, `generation`, `package` (resolved as `pkgs.${name}`), `advertiseAddress`, `iface`, `secretsDir`
- `tls.{enable,certFile,keyFile,caFile}`
- `ports.{postgres,patroniApi,etcdClient,etcdPeer}`
- `etcd.{enable,name,dataPath,peers}`: peers are `{ name, host }` and the list is identical on every member
- `postgres.{enable,dataPath,walPath,members,tags,pgHba,clientNetworks,memoryLimitBytes,watchdog,bootstrapDcs,extensions}`
- `vip.{enable,address,virtualRouterId,priority}`
- `backup.{enable,repoType,repoPath,s3Endpoint,s3Bucket,s3Region,stanza,schedules}`

`enable = false` turns everything off and opens no ports. An etcd-only witness has `enable = true`, `etcd.enable = true` and `postgres.enable = false`.

### What the module renders

- **Patroni.** `services.patroni` with `postgresqlPackage = pkg.withPackages (ps: <extensionPackages>)`, `postgresqlDataDir = pgdata/<major>`, `scope = clusterName`, `name = nodeId`, `nodeIp = advertiseAddress`, user and group `kcore-pg`, and `settings`:
  - `postgresql.listen` forced to `"<advertise>,127.0.0.1[,<vip>]:<port>"` (with `ip_nonlocal_bind` when there is a VIP), `use_unix_socket` on `/run/kcore-postgresql`.
  - `etcd3: { hosts, protocol, cacert, cert, key }` listing every voter.
  - `restapi.listen` forced to `0.0.0.0:<port>` (the firewall limits it to members), TLS files, and user `kcore` whose password comes from `PATRONI_RESTAPI_PASSWORD`.
  - `postgresql.authentication.{superuser,replication,rewind}`: usernames in the YAML, passwords from `PATRONI_*_PASSWORD`; replication and rewind use `sslmode=verify-full`.
  - `postgresql.pg_hba`: `local all all scram-sha-256`, `127.0.0.1`, and per member `replication replicator`, `all rewind` and `all all`, plus each client network. All `hostssl` with TLS on, all `scram-sha-256`, no `trust`.
  - `bootstrap.initdb`: UTF8, `C.UTF-8`, `data-checksums`, and `waldir` when there is a WAL volume. `bootstrap.dcs` comes from `bootstrapDcs` and is used only by the first bootstrap.
  - `tags` (`nofailover`, `noloadbalance`, `failover_priority`); `watchdog` only when it is not `off`.
- **A stable config path.** Upstream starts Patroni with a store-path YAML, and SIGHUP re-reads the file it was started with, so a reload after a rebuild would read old settings. The module writes the same settings to `/etc/kcore-postgresql/patroni.yml` and starts Patroni with that path, so `POST /reload` picks up `pg_hba`, tags and etcd hosts without a restart.
- **Secrets stay out of the store.** One file per password under `secretsDir`, read by the unit script into `PATRONI_*` variables. The agent writes them as root. `ExecStartPre=+` on both units hands the secrets and TLS directories to `kcore-pg` (keys `0400`, certificates `0444`).
- **etcd.** `systemd.services."kcore-pg-etcd-<cluster>"` running `${pkgs.etcd}/bin/etcd` with flags only, TLS with `--client-cert-auth` and `--peer-client-cert-auth`, `Type = notify`. It is not `services.etcd`, which is a single instance and could collide with a Kubernetes control-plane member.
- **Unit behaviour.** `restartIfChanged = false` and `stopIfChanged = false` on `patroni` and the etcd unit, so `nixos-rebuild switch` never bounces the database. `patroni.service` gets `RequiresMountsFor` on the data and WAL paths, `After`/`Wants` on the local etcd unit, `MemoryMax`, and a 300 s stop timeout. Upstream's `KillMode = process` lets Patroni shut PostgreSQL down cleanly on stop. In maintenance mode, PostgreSQL keeps running when Patroni stops; the major upgrade depends on this.
- **Volumes.** `volumes` entries become `fileSystems` with `nofail,noatime`.
- **keepalived** (when `vip.enable`). One `vrrp_instance kcore_pg_<cluster>` in `BACKUP` state with unicast peers. A `vrrp_script` checks `/primary` every 2 s (`fall 2 rise 1`) with no weight, so a failing check puts the instance in FAULT and only the Patroni leader is eligible for the address. The VIP is published at `/etc/kcore-postgresql/vip` for status.
- **Agent tools.** The package (`psql`, `pg_ctl`, `pg_upgrade`), `patroni`, `etcd`/`etcdctl` and `curl` on the system path. `/etc/kcore-postgresql/current`, and `/etc/kcore-postgresql/previous` during a major upgrade.
- **Firewall.** iptables `extraCommands` (or nftables input rules) on `iface` only: the four ports from other members, `postgres` from client networks, VRRP when there is a VIP.
- **Assertions.** `advertiseAddress` and `iface` set; etcd or postgres enabled; an odd voter count; this node among the voters; distinct ports; secrets and volumes under `/var/lib/kcore-postgresql/`; no `services.postgresql`; a VIP needs an address and a PostgreSQL member.

### Rendered fragment (one member)

The exact output for the example is the golden file `tests/fixtures/postgresql-cluster/member.nix`; `witness.nix` and `disabled.nix` sit next to it. The Patroni dynamic config is embedded as `bootstrapDcs = builtins.fromJSON "…"`, so no nested Nix is generated from operator input.

Every interpolated string passes through `kcore_sanitize::nix_escape`. Cluster, role and database names use `[a-z_][a-z0-9_]{0,62}` (unquoted PostgreSQL identifiers, and also Nix attribute keys through `sanitize_nix_attr_key`). Extension names must be in the table in `spec.rs` (contrib, plus `vector`, `postgis`, `pg_cron`, `pg_partman`, `pgaudit` from `postgresqlPackages`); extensions that need `shared_preload_libraries` are added to it automatically. Parameter keys use `[a-z_][a-z0-9_.]*`, and values are strings without newlines.

## Node agent

Files: `crates/kcore-postgresql/src/nixgen.rs` (pure render), `crates/kcore-postgresql/src/ops.rs` (SQL and Patroni request builders, response parsers), and `crates/node-agent/src/postgresql_cluster_agent.rs` (files, preflight, Patroni REST, `psql`, `pg_upgrade`, `etcdctl`). Thin handlers live in `crates/node-agent/src/grpc/admin.rs`.

Node requests carry the normalized cluster spec as JSON plus this node's id, the etcd voters and a stage. The agent parses it, **re-validates it** with the same `kcore_postgresql::spec::validate` the controller uses, and renders the values file itself. The controller never sends Nix text or SQL.

**`CheckPostgresqlClusterPreflight`** checks:

- the host is flake-based (`/etc/nixos/flake.nix`); legacy `configuration.nix` hosts are refused
- the node is not a member of another cluster and `postgresql.service` is not running (a single-node `Postgresql` resource)
- `iface` exists, carries `advertiseAddress`, and the address is not DHCP-assigned
- the four ports are free unless this cluster already owns them (`ss -ltnH`)
- the clock is NTP-synchronised (warning)
- `memoryLimitBytes` leaves at least 2 GiB on the host (warning); it returns `MemTotal`
- a missing LVM volume fits in its VG
- on a running member: the server version, and which changed parameters have `pg_settings.context = postmaster`
- for an upgrade: whether the data filesystem is XFS with reflink

**`ApplyPostgresqlClusterConfig`:**

1. Write the passwords and TLS files atomically (temp file, `fsync`, rename) as root. The units' privileged `ExecStartPre=+` hands them to `kcore-pg`. This works before the first rebuild has created that user.
2. Create a missing LV (`lvcreate`) or zvol (`zfs create -V`), and format it if it has no filesystem. A device that already has a filesystem is never formatted. Mounting is declarative: the values file lists the volumes and the module turns them into `fileSystems` entries.
3. Write `/etc/nixos/kcore-postgresql-cluster.nix`. The host flake (`mkNode`) imports it as a host file; `kcore-host.nix` imports the module.
4. `nixos-rebuild test` then `switch` under `apply_lock` through `run_test_then_switch`, the path the Slurm apply uses. Return the `apply_id` for `GetNixApplyStatus`.

Stages: `etcd-only` (create, phase 1), `full`, `join` (etcd `--initial-cluster-state existing`, used by replace), `upgrade` (new package plus `previousPackage`), `rollback`, `disable`.

**`GetPostgresqlClusterStatus`** reports the unit states, `GET /patroni` and `GET /cluster` on `127.0.0.1` (role, state, timeline, lag, `pending_restart`, pause), `etcdctl endpoint status` (leader), whether this node holds the VIP (`/etc/kcore-postgresql/vip`), and free bytes on the data mount. Errors become a message, so the controller can tell "down" from "unreachable".

**`RunPostgresqlAdmin`** takes a closed `oneof`:

- Patroni REST: `switchover_candidate`, `failover_candidate`, `reinitialize`, `pause`, `patch_dynamic_config`, `reload`. Credentials are passed to `curl` on stdin, never on the command line.
- Units: `restart_member` (`systemctl restart patroni`), `unit` (start, stop or restart of `patroni` or the etcd unit), `stop_postgres` (`pg_ctl stop -m fast` while Patroni is paused and stopped).
- SQL on the local socket as `postgres`: `ensure_roles`, `ensure_databases`, `ensure_extensions`, `analyze`. Statements come from `ops.rs` with `quote_ident` and `quote_literal`. A role password kcore generated is turned into a SCRAM-SHA-256 verifier on the agent (PBKDF2-HMAC-SHA256, 4096 iterations, random salt). The plaintext never reaches the server or its logs.
- Upgrade: `pg_upgrade_check`, `pg_upgrade_run`, `set_initialize_key`, `remove_data_major`.
- etcd membership: `etcd_member_add`, `etcd_member_remove`.
- `release_volumes` and `verify`.

## Controller

Files: `crates/kcore-postgresql/src/{spec,rollout}.rs` (pure: normalize, validate, diff, planner, gates), `crates/controller/src/postgresql_cluster_engine.rs` (executes operations and refreshes health), `postgresql_cluster_pki.rs`, `db/postgresql_store.rs`, and `grpc/controller/postgresql.rs` (handlers and proto conversion; the `Controller` trait impl only delegates there).

### Rollout planner

`plan_rollout(req) -> Vec<Step>` is pure. The engine executes steps and re-reads the live leader from health before every step, because Patroni can fail over in the middle of an operation. A step whose node is `@leader` resolves to whoever leads at that moment. Invariants checked by the unit tests and a proptest over 1 to 5 members (`rollout_invariants`):

1. **At most one member is down at a time** (a restart or a reinit).
2. **The leader is never restarted while it leads** if a standby exists. A roll goes standbys first (`noFailover` members first), then `switchover` to an already-restarted standby, then the old leader. A one-member cluster gets a `warning` step and an in-place restart.
3. **The sync quorum stays satisfiable** (`safe_to_restart`). A standby restarts only when every other standby is streaming within `maxReplicationLagBytes` and, outside `async`, at least `syncCount` of them remain. Otherwise the step waits until `stepTimeoutSeconds`, then the operation fails.
4. **Switchover candidates** (`switchover_candidate`) are streaming within `maximumLagOnFailoverBytes` and not `noFailover`. They are ranked by `failoverPriority`, then lag. With no candidate the step waits.
5. **etcd** restarts one voter at a time while every other voter is up and a leader exists, with the etcd leader last (`etcd_restart_allowed`).
6. **Gates:** `wait-nix`, `wait-etcd-quorum`, `wait-leader`, `wait-streaming`.
7. **Reload-only changes** produce `apply-nix` on every node, `reload-member`, at most one `patch-dynamic-config`, and no restarts.
8. **Resume:** the engine runs only steps that have not succeeded or been skipped. A gate that is not met returns "wait" and the next tick (10 s) retries it.

### Operations

Every mutating RPC stores one row in `postgresql_operations` with its steps and returns the id. A cluster has at most one running operation (checked in one transaction); a second request gets `FAILED_PRECONDITION` naming the running one. Only the controller holding the replicated failover lease drives operations (`may_act`, the Slurm reconciler's rule). `CancelPostgresqlOperation` stops before the next step.

An unreachable node makes a step wait. With `force`, steps on unreachable nodes are skipped instead (delete, replace of a dead node).

**Create**

1. Plan with live preflight on every node. Any blocking issue refuses the request before a row is stored.
2. Store the spec (generation 1), claim each node (a node is in at most one cluster), generate the CA and four passwords. Every initial member is an etcd voter. `memoryLimitBytes` defaults to 25 % of the smallest member's RAM from inventory.
3. `apply-nix (etcd-only)` on every voter, then `wait-nix` on each, then `wait-etcd-quorum`. etcd has quorum before any Patroni starts.
4. `apply-nix (full)` on the bootstrap member, then `wait-leader`. Then each other member in turn: `apply-nix (full)`, `wait-streaming`.
5. `ensure-roles`, `ensure-databases` and `ensure-extensions` on the leader, then `verify`.

Member certificates are issued when the member's first apply is built. They are reissued on any later apply once they are within `rotateBeforeDays` of expiry.

**Update.** The diff classifies the change. A reload-only change is applied everywhere, reloaded, and the dynamic config is patched once. A postmaster parameter, `memoryLimitBytes`, a watchdog change or a preload extension adds the rolling restart. Roles and databases that left the spec are dropped only with `force`. `observed_generation` moves when the last step succeeds. A failure leaves the cluster `degraded` with the failed step's message.

**Minor upgrade** (`UpgradePostgresqlCluster` with the running package). A kcore update that moves the nixpkgs pin rebuilds the hosts, but `restartIfChanged = false` keeps the old binaries running. This operation re-applies, rolls etcd (leader last), then rolls PostgreSQL like a restart update, and finally runs `ALTER EXTENSION … UPDATE`.

**Major upgrade** (`postgresql_17` to `postgresql_18`). This has a write-downtime window. Order:

1. `pg-upgrade-check` on the leader (`pg_upgrade --check`). The agent fetches the target binaries with `nix build` of the pinned package, so this runs before anything is paused.
2. `pause`. Stop Patroni on the standbys, then on the leader. While paused, PostgreSQL keeps running when Patroni stops, so `stop-postgres` stops it with `pg_ctl`, standbys first.
3. `apply-nix (upgrade)` on every member: the new package, and the old one kept as `previousPackage` at `/etc/kcore-postgresql/previous`. The data directory is `pgdata/<major>`, so the new major gets a fresh directory on the same filesystem.
4. `pg-upgrade-run` on the old leader. It runs `initdb` with the same options Patroni uses, then `pg_upgrade --clone` (XFS with reflink) or `--link`. `UpgradePostgresqlCluster` refuses `--link` without `force`, because it cannot be rolled back.
5. `set-initialize-key` writes the new system identifier to Patroni's `initialize` key and drops `history`, `status`, `optime/leader` and `leader`. This is the step Spilo's in-place upgrade takes. It must be proven against Patroni 4.1 in the VM test before the first lab use.
6. Start Patroni on the leader, `resume`, `wait-leader`. Then start each standby. Its new `pgdata/<major>` is empty, so Patroni clones it with `pg_basebackup`. Then `wait-streaming`.
7. `ALTER EXTENSION … UPDATE` and `vacuumdb --analyze-in-stages`.
8. The cluster records `previousPackage` until `FinalizePostgresqlUpgrade` (removes the old directories and re-applies without the old package). `RollbackPostgresqlUpgrade` stops everything, re-applies the old package, points the `initialize` key back at the old directory, restarts on it, and removes the abandoned new directories. Writes made after the upgrade are lost; plan says so. Updates and new upgrades are refused until one of the two runs.

**Switchover** (planned). Optional candidate; otherwise the best candidate by invariant 4. **Failover** (manual) requires `force` and a candidate, and is refused while etcd has no quorum. The way out of a lost quorum is to replace the lost voters, not to promote around Patroni.

**Reinit** rebuilds a standby from the leader (`POST /reinitialize`), for example after `pg_rewind` failed. It is refused on the leader.

**Scale.** A spec whose only change is the member list is run as scale, through `UpdatePostgresqlCluster` or `ScalePostgresqlCluster`. Added members are standbys and etcd clients, not voters: existing members are re-applied and reloaded (pg_hba, firewall), then the new node is applied and must reach `streaming`. Removing a member switches over first if it leads, stops Patroni, applies `disable`, re-applies the rest, and releases the volumes unless `retainData`. Removing an etcd voter this way is refused.

**Replace member** moves a member and its etcd vote to a new node. If the old member leads, switch over first. Stop Patroni and etcd on it, run `etcdctl member remove` on a healthy voter, then `disable` the old node. Run `etcdctl member add` for the new one, apply it with `join`, `wait-etcd-quorum`, re-apply the others, `wait-streaming`, and release the old volumes. With `force`, steps on the unreachable old node are skipped.

**Delete.** Stop Patroni on the standbys, then on the leader; stop etcd everywhere; apply `disable`; release volumes unless `retainData`; drop the rows.

**Maintenance** is Patroni pause and resume. Automatic failover is off while it is on, and phase shows `degraded`.

### Health and events

Each engine tick polls `GetPostgresqlClusterStatus` on every member. The engine merges each node's own answer with the `/cluster` view of the leader and stores per-member health. It then updates the leader, timeline, version and pause on the cluster row.

- `ready`: a leader, every standby streaming, etcd fully up, sync count met, nothing paused or pending restart.
- `degraded`: any of those not true while a leader and etcd quorum exist.
- `failed`: no running leader, or etcd quorum lost.

A leader change records a `postgresql_cluster_events` row: `failover` when no operation is running (also the `postgresql.failover` webhook), `switchover` when one is.

### SQLite (schema 72)

- `postgresql_clusters(name PK, generation, observed_generation, phase, message, spec_json, previous_package, leader_node_id, timeline, observed_version, paused, active_operation_id, created_at, updated_at)`
- `postgresql_cluster_members(node_id PK, cluster_name FK, etcd_voter, applied_generation, health_json, updated_at)`. `node_id` as primary key enforces one cluster per node.
- `postgresql_cluster_secrets(cluster_name PK FK, superuser_password, replication_password, rewind_password, restapi_password, ca_cert_pem, ca_key_pem, role_passwords_json)`, stored the way Ceph dashboard credentials and Slurm munge keys are
- `postgresql_member_certs(cluster_name, node_id, cert_pem, key_pem, not_after_unix)`
- `postgresql_operations(id PK, cluster_name, kind, state, target_generation, steps_json, context_json, message, force, created_at, updated_at)`
- `postgresql_cluster_events(id PK, cluster_name, kind, old_leader, new_leader, timeline, message, created_at)`

`schema_down.rs` version 72 drops them. Older readers never select these tables, so `MIN_COMPATIBLE_SCHEMA_READER` does not move.

## API

Controller RPCs (`service Controller`): `PlanPostgresqlCluster`, `CreatePostgresqlCluster`, `UpdatePostgresqlCluster`, `GetPostgresqlCluster`, `ListPostgresqlClusters`, `DeletePostgresqlCluster`, `ScalePostgresqlCluster`, `ReplacePostgresqlMember`, `ReinitPostgresqlMember`, `UpgradePostgresqlCluster`, `FinalizePostgresqlUpgrade`, `RollbackPostgresqlUpgrade`, `SwitchoverPostgresqlCluster`, `FailoverPostgresqlCluster`, `SetPostgresqlMaintenance`, `GetPostgresqlClusterHealth`, `ListPostgresqlClusterEvents`, `Get/List/CancelPostgresqlOperation`, `GetPostgresqlClusterCredentials`. Reads need `read-only`; everything else needs `cluster-admin` (`rbac_matrix.rs`). Credentials reads are audited.

Node RPCs (`service NodeAdmin`): `CheckPostgresqlClusterPreflight`, `ApplyPostgresqlClusterConfig`, `GetPostgresqlClusterStatus`, `RunPostgresqlAdmin`, plus the existing `GetNixApplyStatus`. Only controller certificates may call them, except status, which `kctl` may also read.

Rules the proto cannot express:

- **Immutable:** `ports`, `tls.disabled`, a member's address, interface, witness flag and volumes. Plan returns `replace`. `role` is bootstrap-only and ignored after create.
- **Upgrade, not update:** `package` changes go through `UpgradePostgresqlCluster`.
- **Scale, not mixed:** a member-list change cannot be combined with other changes, or with a package change.
- **`confirm`:** every mutating RPC needs `confirm: true`. Plan is the dry run.
- **`force`:** skip unreachable nodes, drop roles and databases, manual failover, a major upgrade with `--link`.

Single-node `Postgresql` and clusters exclude each other: `CreatePostgresql` refuses a cluster member and its scheduler skips them; cluster plan refuses a node with a single-node database.

## Operator surface

- `kctl pg-cluster plan -f orders.yaml` prints the action, changed fields, each parameter's reload or restart class, per-node preflight, the RPO line and the numbered steps.
- `kctl pg-cluster apply -f orders.yaml --confirm [--force]` creates or updates; `kctl apply -f` routes `kind: PostgresqlCluster` here.
- `kctl pg-cluster list | get | health | events | credentials | operations <name>`, `operation <id>`, `cancel <id>`.
- `kctl pg-cluster switchover orders [--to node-106] --confirm`
- `kctl pg-cluster failover orders --to node-108 --force --confirm`
- `kctl pg-cluster maintenance orders on|off --confirm`
- `kctl pg-cluster reinit orders node-106 --confirm`
- `kctl pg-cluster scale orders --add member.yaml | --remove node-110 --confirm`
- `kctl pg-cluster replace orders --old node-106 -f new-member.yaml [--force] --confirm`
- `kctl pg-cluster upgrade orders [--package postgresql_18] [--force] --confirm`, `finalize-upgrade orders`, `rollback-upgrade orders`
- `kctl pg-cluster delete orders [--force] [--retain-data] --confirm`

The example manifest is `examples/postgresql-cluster/orders.yaml`.

## Built and not built

Built and tested on macOS (Rust tests, Nix evaluation):

- `modules/kcore-postgresql-cluster.nix`, imported by `kcore-host.nix`, seeded empty by the installer and listed in `hostFiles`. The `postgresql-cluster-module-eval` flake check evaluates the golden member, witness and disabled fragments; the full NixOS toplevel of each also evaluates.
- `crates/kcore-postgresql`: spec, validation, diff, planner, gates, Nix renderer with golden files, SQL and Patroni helpers, with unit and property tests.
- Node agent RPCs, controller schema 72, PKI, engine with fake-agent scenarios (create order, leader-last roll, an unplanned failover mid-roll, reload-only update, one operation at a time, unreachable nodes with and without force, failover event, major upgrade order), gRPC handlers, RBAC rows, webhook, and `kctl pg-cluster`.

Built in this revision (beyond the list above):

- **NixOS VM test** (`tests/postgresql-cluster-module.nix`, `.github/workflows/postgresql-cluster-vm.yml`): three machines, etcd quorum, Patroni leader, write through the VIP, kill the leader, rejoin, switchover. Major-upgrade `set-initialize-key` coverage is still thin until the 16→17 step is added to the script.
- **ClusterUpdate hook**: before activate/reboot, the reconciler switches the Patroni leader off the node when needed and refuses a reboot that would lose etcd quorum (`safe_to_take_down`).
- MCP kind `postgresql-cluster` and the dashboard card.
- Lab example for the three Dells: `examples/postgresql-cluster/dell-lab.yaml`.

Not built yet:

- **Backups** (pgBackRest repo, schedules, `Backup/Restore/ListPostgresqlBackups`). The design above stays the plan, but there are no RPCs for it yet, so a major upgrade takes no automatic backup. Take one yourself first.
- TLA+ model and trace check, soak script.
- `postgres_exporter` scrape targets; a periodic certificate rotation pass (today a certificate is reissued at the member's next apply once it is due).
- Adding an etcd voter outside Replace; more than one cluster per node; a reader VIP.

## Tests

Each test lives next to the code it covers. This list is the exit criterion of the phases.

### Static checks

`cargo fmt --check`, `cargo clippy --all-targets -- --deny warnings`, `cargo audit`, `statix`, `deadnix`, `nixfmt --check`, `nix flake show`.

### Unit tests

- `spec.rs`
  - Defaults.
  - Errors: even etcd voters; two members without a witness; no promotable member; mixed datacenter labels; a datacenter majority; duplicate names; owned parameters; `wal_level` lowered; `postgresql` (unversioned) package; `loopWait + 2*retryTimeout > ttl`; names outside the grammar; an unknown extension; the same volume name for data and WAL; a VIP outside a member's subnet.
  - Diff classification: `unchanged`, `update` reload, `update` restart, `upgrade-minor`, `upgrade-major`, `scale`, `replace`, plus `changed_fields`.
  - A failover (leader change in live state) does not produce a diff.
- `rollout.rs`
  - Step order for 1, 2+witness, 3 and 5 members, `nofailover` members, each replication mode, minor, major and Patroni-only upgrades.
  - Switchover is never to a `nofailover` member or an un-rolled standby.
  - The old primary rolls last.
  - Resume yields only the remaining steps.
  - A leader that moved mid-operation re-plans without restarting the new leader.
- `nixgen.rs`: escaping in every string field, the disabled fragment, the witness fragment, no secret in output.
- `ops.rs`
  - SQL builders with names containing `'`, `"`, `;`, `--`.
  - Patroni request builders (`switchover` JSON with `leader` and `candidate`).
  - Parsers for recorded `/cluster` and `/patroni` bodies: leader, sync and quorum standby, streaming, `pending_restart`, paused, a member with `lag: "unknown"`.
  - `etcdctl endpoint status -w json` parser.
  - `pgbackrest info` JSON parser.
- `postgresql_cluster_db.rs`: CRUD, cascade, the unique node index, up/down migration round-trip.
- `kctl`: camelCase and snake_case manifests, unknown-field rejection, plan and health output from canned responses.

### Property tests (proptest)

- For any valid topology and op kind, `plan_rollout` satisfies invariants 1 to 8. Every member restarts exactly once, the primary-at-plan-time restarts after a switchover, and the plan is finite.
- For any string, the rendered fragment parses with `nix-instantiate --parse` (only when `nix` is on `PATH`).
- `quote_ident` and `quote_literal` round-trip, and no output closes the quote early.
- `from_proto(to_proto(spec)) == spec`. `diff(a, a) == unchanged`. Any immutable-field change is `replace`.

### Kani

`quote_ident`, `quote_literal` and `validate_pg_name` move into `kcore-sanitize`, with bounded harnesses next to the ClickHouse ones.

### Golden and contract tests

`tests/fixtures/postgresql-cluster/` holds manifests and rendered fragments: `three-members.yaml` → `.nix`, `two-plus-witness.nix`, `disabled.nix`. A Rust snapshot test compares them byte for byte. The Nix eval check imports the same files.

### Nix evaluation check (`.#checks.x86_64-linux.postgresql-cluster-module-eval`)

`tests/postgresql-cluster-module-eval.nix` asserts that:

- `patroni` and `kcore-pg-etcd-orders` exist with `restartIfChanged = false`, `RequiresMountsFor` and `MemoryMax`
- the Patroni YAML has `etcd3` over https, `postgresql.listen` including `127.0.0.1`, `pg_hba` with only `hostssl`/`local` lines and no `trust`, `bootstrap.dcs.synchronous_mode = quorum`, and `data-checksums`
- `services.postgresql.enable` is false, and the firewall is only on `enp6s0` with iptables rules naming member addresses
- `/etc` text contains no secret and no SCRAM verifier
- the disabled fragment has no units, and the witness has etcd only
- assertions fire on an even peer count and a missing `advertiseAddress`

### NixOS VM test (`make test-postgresql-cluster-vm`)

`tests/postgresql-cluster-module.nix`: three machines, static CA and certs from the test derivation, and a VIP on the test VLAN. The script:

1. waits for etcd quorum and a Patroni leader; two standbys stream; the sync standby is reported
2. writes 1 000 rows through the VIP and reads 1 000 on each standby
3. crashes the leader's VM; a new leader comes up within `ttl + loop_wait`; the VIP answers on the new leader; inserts resume; no acknowledged row is missing
4. restarts the old leader; it rejoins as a streaming standby through `pg_rewind`
5. switchover back while a writer runs; writer errors are bounded to the switchover window
6. a reload-only parameter change does not restart PostgreSQL (`pg_postmaster_start_time()` unchanged); a postmaster parameter change rolls with the leader last
7. major upgrade `postgresql_16` → `postgresql_17` with `--clone` on XFS; rows survive; standbys reinitialise; rollback before finalize returns 16

It runs as its own path-filtered job (`.github/workflows/postgresql-cluster-vm.yml`, copied from `clickhouse-vm.yml`).

### Controller integration tests with a fake node agent

In-process `PostgresqlNodes` fake with scripted state: preflight failures, apply progression, `/cluster` bodies, lag that drains, a leader that moves on its own, a node that never answers. Scenarios:

- plan with a failing node persists nothing
- create orders etcd → bootstrap primary → standbys, and ends `ready`
- reload-only update never restarts and sends exactly one `PATCH /config`
- a rolling update restarts standbys one at a time and the old primary last after a switchover; a standby that never catches up fails at `stepTimeoutSeconds` and leaves `degraded`
- an unplanned failover during a rolling update: the engine re-reads the leader, skips the switchover if the leader is already rolled, and never restarts the new leader first
- a minor upgrade with only Patroni changed pauses, restarts Patroni and resumes without a switchover
- a major upgrade runs check, backup, pause, pg_upgrade, the initialize key, resume and reinit in order; a failing `pg_upgrade --check` stops before any stop
- switchover refuses a `nofailover` or lagging candidate
- manual failover refuses without `force` and with etcd quorum lost
- scale-in of the primary switches over first
- a controller restart mid-operation resumes at the first non-succeeded step; a second controller with an unexpired claim skips
- a health-loop leader change with no operation records one `failover` event and one webhook

### gRPC contract tests

`build.rs` compiles both protos. `buf breaking` against `main`. Encode/decode of every request with every field set. kctl manifest → proto → manifest round-trip.

### Formal model (`specs/tla/PostgresqlRollout.tla`)

Three members, three etcd voters, a leader key with term, quorum sync with count 1, controller steps (restart, switchover) and Patroni actions (leader-key expiry, promotion, demotion) interleaved.

- Safety:
  - at most one member is writable and holds the leader key in any state
  - a member promotes only with the key
  - no acknowledged commit is lost across any sequence of failovers in `quorum` and `sync` mode
  - the planner never takes a second member down
- Liveness: with fair scheduling and at most one failure, every operation terminates.

`scripts/test-postgresql-rollout-trace.sh` checks planner traces against the model's action labels, as the ClickHouse trace check does.

### Soak (`scripts/soak-postgresql-cluster.sh`, nightly or manual)

A writer commits numbered rows continuously while the controller runs an update, a minor upgrade, a switchover and a member replacement, and the test kills the leader twice. It passes when every acknowledged row id is present exactly once and every member ends streaming on the same timeline.

### Security tests

- Injection strings in cluster, role, database, extension and parameter names are rejected.
- Paths outside `/var/lib/kcore-postgresql/` are refused.
- The eval check asserts firewall scope, no `trust` in `pg_hba` and no secrets in `/etc`.
- `GetPostgresqlClusterCredentials` requires the operator role and writes an audit row.
- The Patroni REST API refuses unauthenticated `POST`s (VM test).

### Where each test runs

| Test type | Location | Gate |
|---|---|---|
| Static checks | repo | `rust-checks`, `nix-checks` |
| Unit, property, golden, fake-agent, contract | `crates/*` | `rust-checks` |
| Kani | `kcore-sanitize` | `make kani` |
| Nix eval | `tests/postgresql-cluster-module-eval.nix` | `nix-checks` |
| NixOS VM | `tests/postgresql-cluster-module.nix` | own job, path-filtered |
| TLA+ and trace drift | `specs/tla/`, `scripts/` | `formal-checks` |
| Soak | `scripts/soak-postgresql-cluster.sh` | nightly / manual |
| Lab e2e | garage lab | manual, after the questions |

## Phases

The VM test, ClusterUpdate hook, MCP kind, dashboard card, and Dell lab example are in this revision (see [Built and not built](#built-and-not-built)). What remains, in order:

1. **VM test depth**: add the 16→17 major-upgrade path (and its `set-initialize-key` sequence) to `tests/postgresql-cluster-module.nix`.
2. **Backups**: pgBackRest module, RPCs, schedules, restore into a new cluster, and a full backup as the first step of a major upgrade.
3. **Clients polish**: metrics targets; a periodic certificate rotation pass.
4. **Formal model and soak.**
5. **Lab apply** on the three Dells with `examples/postgresql-cluster/dell-lab.yaml`. Exit: create, write via VIP, read on both standbys, power off the primary host, rejoin, switchover, minor upgrade, replace member.
6. **Later.**
   - etcd voter add and remove outside Replace
   - reader VIP or HAProxy
   - logical-replication blue/green major upgrade
   - more than one cluster per node (needs an in-tree instanced Patroni module)
   - standby cluster in a second kcore installation (Patroni `standby_cluster`)
   - PgBouncer

## Risks

- **A hypervisor reboot is a failover.** `nixos-rebuild switch` never restarts the database (`restartIfChanged = false`), but a `ClusterUpdate` reboot does. The ClusterUpdate hook switches the Patroni leader off the node first and refuses a reboot that would lose etcd quorum.
- **No watchdog by default.** A hung Patroni primary can keep its PostgreSQL accepting connections after a standby promotes. Synchronous quorum mode keeps those writes from being acknowledged, and the VIP check moves clients. Under `async` this window can lose writes, and plan says so.
- **etcd quorum loss stops failover and stops writes.** With 3 voters, losing 2 nodes leaves the survivor read-only. This is by design. The way out is Replace, not a bypass.
- **Memory.** PostgreSQL on a hypervisor competes with guests. `MemoryMax` and the derived `shared_buffers` keep it bounded. Preflight warns.
- **Major upgrade downtime** is proportional to catalog size with `--clone`/`--link`, not data size. Standby reinit is proportional to data size and runs after writes resume, with reduced redundancy until it finishes. Plan prints both.
- **Upstream NixOS Patroni module.** If its shape changes, the fallback is an in-tree copy pinned with the package, as with Ceph.
- **Pin moves.** A versioned attribute (`postgresql_17`) only changes minor on a pin bump. The release notes name the PostgreSQL, Patroni and etcd versions so an operator can hold a `ClusterUpdate`.

## Lab apply answers (three Dells)

| Question | Answer |
|---|---|
| Which three nodes, bootstrap primary? | `dell-1` (primary), `dell-2`, `dell-3`. Controller host stays out. |
| Advertise address / iface | From the Kea reservations in the network provisioner catalog (`kcore-provisioning status`). Example placeholders in `dell-lab.yaml`: `.103` / `.113` / `.123` on `enp6s0`. |
| VIP / VRRP id | `192.168.40.160/24`, `virtualRouterId: 61` (not the Kubernetes API VIP). |
| RPO | `quorum` (default). |
| Volumes | One data LV per node on `vg_kcore` (`pg-orders`, 100 GiB, xfs). WAL and etcd stay on the data volume for the first lab run. |
| Backup target | None yet (backups not built). Take a manual snapshot or dump before a major upgrade. |
| Client networks | `192.168.40.0/24`. |
| Rebuild while guests run | Yes for the first lab apply; ClusterUpdate will switch over before a reboot. Confirm guest impact with the operator. |

Manifest: `examples/postgresql-cluster/dell-lab.yaml`.
