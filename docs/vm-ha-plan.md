# VM High Availability: plan

The first question a vSphere buyer asks is "when a host dies, do its VMs
restart somewhere else without anyone doing anything?" This document is the
plan to make the answer an unqualified yes, and to make the gaps that remain
visible in the product instead of in a footnote.

It is written against the code on the `lvm-zfs-live-migration` branch. The
phases are ordered by what a buyer can verify in a demo, with safety fixes
first. No calendar estimates: each phase names the subsystems it touches and
the gate that closes it.

## Where we are

kcore already has automatic failover. It is narrower than what the word "HA"
means to a vSphere operator, and some of its edges are unsafe enough that we
should not put it on a slide yet.

What runs today (`crates/controller/src/main.rs`, `grpc/controller.rs`):

| Step | Mechanism | Timing |
|------|-----------|--------|
| Node liveness | Node agent heartbeats every 15 s (`registration.rs`); a `ready` node with no heartbeat for 90 s is set `not-ready` by a 30 s polling task | 90–120 s after the last heartbeat |
| Trigger | A second 30 s polling task calls `failover_not_ready_nodes` | up to 30 s more |
| Fencing | `NodeStorage.FenceVolume` through a healthy Ceph member blocklists the lost node's rbd-nbd clients per image | seconds |
| Move | `reassign_vm_node` + `push_config_and_await_apply` on the target, one VM at a time; the lost node is put in `pending_node_config_push` | one `nixos-rebuild switch` per VM |
| Start | The target's `kcore-vm-<name>` unit starts the guest if `autoStart` is set | guest boot |

Lab measurement (`docs/vm-migration.md`): heartbeat deadline at +93 s, fence
at +93.5 s, guest booted on the new node at +113 s. Worst case with the two
polling intervals is about 150 s before the move begins.

What it does not do:

1. **Local disks are stranded.** `lvm`, `zfs`, and `filesystem` VMs stay on
   the dead node (`failover_unreachable_node` skips them with a warning).
   There is no replica to start from. This is the same constraint vSphere HA
   has (shared datastore or vSAN required), but kcore does not say so in the
   UI, the CLI, or the VM spec.
2. **A returning host can corrupt the image.** A lost node that reboots boots
   its old NixOS generation, in which the moved VM is still an `autoStart`
   unit, and starts a second writer with a fresh Ceph client nonce before the
   controller's first config push removes the unit. Listed as P0 in
   `storage-and-mobility-gaps.md`.
3. **An image with no watcher is treated as fenced.** A node partitioned from
   both the controller and the Ceph public network loses its watch after
   30 s; `FenceVolume` then finds nothing to blocklist and the move proceeds.
   If the partition heals, the old rbd-nbd reconnects and writes.
4. **Only `ready` nodes are watched.** `get_stale_nodes` filters on
   `status = 'ready'`, so a node that dies while `cordoned` or `draining` is
   never marked lost and its VMs are never moved.
5. **Restart follows `autoStart`, not what was running.** A VM an operator
   started by hand on a node that then dies is not restarted; a VM that was
   deliberately stopped but has `autoStart` is.
6. **No restart ordering or capacity reservation.** VMs move in DB row order.
   Nothing stops the cluster from being filled to the point where losing the
   largest host leaves no room to restart its VMs; the failure is discovered
   when the restart fails.
7. **No isolation response on the host.** A node that lost the controller
   keeps its guests running with a fenced disk. The guest sees I/O errors
   until the node's next heartbeat brings a config without the unit, which
   may be never if the agent is the thing that died.
8. **One controller runs it.** With replicated controllers
   (`ha-crdt-replication.md`) every controller with `failover.enabled` runs
   the loop over its own replicated view. Two controllers can move the same
   VM to two different targets. If the single controller is down, or was on
   the host that died, nothing moves.
9. **Detection is slow by vSphere standards.** vSphere HA declares a host
   dead in about 15–30 s and begins restarts immediately. Our 90 s deadline
   plus two 30 s polls is 3–5x that before the first fence.
10. **Nothing to show.** There is no HA status per VM, no failover event
    history beyond the controller log and the `vm.migrated` webhook with
    `mode: failover`, and no dashboard surface.

## Target

After this plan, the answer to the buyer is:

> HA is on by default. Every VM on kcore SAN (Ceph) is protected: when a
> host stops heartbeating, the controller fences its storage within the
> configured deadline (default 30 s), restarts its VMs on surviving hosts in
> priority order, and refuses to admit new VMs that would leave the cluster
> unable to absorb one host failure. A host that comes back cannot start a
> stale copy. The failover controller is itself redundant. VMs on local
> disks are marked unprotected in every listing, and `zfs` VMs can opt into
> an asynchronous replica with a stated RPO.

## Feature map against vSphere HA

| vSphere HA | kcore today | Plan |
|------------|-------------|------|
| Host failure detection (~15–30 s) | 90 s deadline, polled at 30 s | Phase 2: 30 s default deadline, 5 s detector, event-driven trigger |
| Fencing / datastore heartbeating (isolation vs. death) | Per-client RBD blocklist; no-watcher hole | Phase 1: address-range blocklist and OSD-state check |
| Host isolation response | none | Phase 3: node agent stops protected guests after isolation timeout |
| Protected on shared datastore only | Ceph only, undocumented | Phase 1: `ha.protected` derived and displayed; Phase 6: ZFS replica |
| Restart powered-on VMs | restarts `autoStart` VMs | Phase 2: restart what was running at last sync |
| Restart priority | none | Phase 2: `restartPriority` on the VM spec |
| Admission control (host failures to tolerate) | none | Phase 4 |
| Primary/secondary election among hosts | one controller, no lease | Phase 5: failover lease among replicated controllers |
| HA events and per-VM status in the UI | log lines | Phase 2: events table, `kctl get ha`, dashboard panel |
| VM monitoring (guest heartbeat restart) | systemd `Restart=` on VMM exit only | Phase 7, optional |
| Proactive HA (hardware health) | none | not planned |

## Design decisions

### HA requires shared storage

HA protection is a property derived from the VM's storage backend, not a
flag the operator sets: `ceph` is protected, `lvm`/`zfs`/`filesystem` are
not (until Phase 6 adds the ZFS replica). Every place that lists a VM shows
it. vSphere operators already accept this rule; what they do not accept is
discovering it after the outage.

### Fence first, always

The move keeps its current precondition: no reassignment until the lost
node's access to every image is provably cut, through a healthy member.
Phase 1 closes the two known holes without changing that contract.

Exclusive-lock on the RBD images is not the fix. `vm-migration.md` documents
why: live migration maps the image on both nodes during cutover. Blocklisting
by address is compatible with that.

### Restart is a controller decision, not a systemd default

The controller records what was running and restarts exactly that set. The
node's unit files stay declarative; the controller decides what to put in
them.

### A node must confirm its placement before starting a guest

Phase 1 adds a gate: `kcore-vm-*` units wait for a target the node agent
reaches only after the controller confirms the node's VM assignment. A node
that cannot reach any controller does not start a protected guest. This
closes the returning-host hole and is the host-side half of fencing.

### One actor moves VMs

With replicated controllers, only the holder of a failover lease runs the
detector and the mover. The lease is a replicated LWW register with a short
expiry. A controller whose replication apply frontier is stale steps down.
This is the same single-writer rule the migration path already follows for
`vm_operations`.

### Local-disk HA is asynchronous and says so

Synchronous replication of local disks (keeping the dm-mirror from
`kcore-volume-mirror` permanently attached over NBD) doubles write latency
and stalls the guest when the replica is unreachable. It is not in this
plan. ZFS incremental `send | recv` to a designated replica node, on a
schedule, with the RPO printed on the VM, is the Proxmox model and is what
Phase 6 delivers.

## Phases

### Phase 1: make the existing failover safe to sell

Everything in this phase is a correctness fix to behaviour that already
exists. The only new knob is the placement-gate timeout.

1. **Placement gate on boot.** `modules/ch-vm/vm-service.nix`: every
   `kcore-vm-*` unit gets `After=` and `Requires=` on a new
   `kcore-placement-confirmed.target`. The node agent reaches it after the
   first heartbeat whose response carries the controller's list of VM ids
   assigned to this node (new `HeartbeatResponse.assigned_vm_ids`, plus
   `assignment_confirmed`). The agent compares that list with the VMs in
   its booted configuration; for any VM the controller no longer assigns
   here it stops and masks the unit before reaching the target. If no
   controller answers within `placementGate.timeoutSecs` (default 300),
   units for non-Ceph VMs start anyway (nobody else can run them); Ceph VM
   units stay held and the agent logs why. Single-node installs with the
   controller on the same host reach the target as soon as the local
   controller is up.
2. **Blocklist the whole address.** `NodeStorage.FenceVolume` adds
   `ceph osd blocklist range add <ip>/32` for the lost node's Ceph public
   address and its gRPC host, in addition to the per-client entries, so a
   watcher that expired before the fence is still refused on reconnect.
   The node's OSD front sessions are on the same address; verify in the
   lab that a returning node's OSDs recover once the controller lifts the
   range entry on the node's first confirmed heartbeat (`ceph osd blocklist
   range rm`). If they do not, fall back to blocklisting the rbd-nbd
   port range only and record the gap.
3. **Isolation vs. death signal.** Before fencing, the mover asks the
   healthy member for `ceph osd tree` state of the lost host's OSDs and
   whether its mon (if any) is in quorum. The result is recorded on the
   failover event as `hostState: dead | isolated | unknown`. Phase 1 does
   not change the decision on it; Phase 3 uses it.
4. **Watch every approved node.** Replace the `status = 'ready'` filter in
   `get_stale_nodes` with a `nodes.lost_at` timestamp: set when the deadline
   passes for any approved node that is not `drained`, cleared on the next
   heartbeat. `status` keeps its current meaning for scheduling; `not-ready`
   is still written for `ready` nodes so existing callers and
   `cluster_health.rs` keep working. The mover acts on `lost_at IS NOT NULL`.
5. **Show protection.** `VmInfo.ha_protected` (derived: `storage_backend ==
   "ceph"`), a `HA` column in `kctl get vms`, the VM detail page in the
   dashboard, and `cluster-health` counting unprotected VMs on lost nodes
   separately from protected ones that could not be moved.
6. **Tests.** NixOS test `tests/placement-gate.nix`: a node boots with a
   configuration that still contains a VM the controller has reassigned;
   the unit must not start before the controller answers and must be masked
   after. Controller unit tests for `lost_at` on cordoned nodes and for the
   range-blocklist call. Extend `specs/tla/ControllerNodeReconcile.tla`
   with the invariant that no VM is in `started` state on two nodes, with
   the placement gate as the node-side action.

Gate: the P0 "rebooted node restarts guests" and "nothing to blocklist" rows
in `storage-and-mobility-gaps.md` are deleted; a cordoned node that dies in
the lab has its Ceph VMs moved.

### Phase 2: fast detection, correct restart set, visible events

1. **Config.** `FailoverConfig` grows:

   ```yaml
   failover:
     enabled: true
     nodeTimeoutSecs: 30        # was a fixed 90
     detectIntervalSecs: 5      # was a fixed 30
   ```

   The heartbeat stays at 15 s; 30 s is two missed beats. Validation
   refuses `nodeTimeoutSecs < 2 * heartbeat interval`. The two polling
   tasks in `main.rs` become one detector that marks `lost_at` and sends
   the node id on a `tokio::sync::mpsc` channel; the mover wakes on the
   channel instead of on a 30 s sleep and still runs a sweep every
   `detectIntervalSecs * 6` for nodes whose earlier attempt failed.
2. **Restart what was running.** `SyncVmState` already stores
   `runtime_state`. The mover restarts a protected VM when its last synced
   state was `running` or `paused`, or when `auto_start` is set; a VM last
   seen `stopped` is reassigned but its unit is rendered with
   `autoStart = false`. The decision is written to the failover event.
3. **Restart priority.** `vms.restart_priority` (`high`, `medium`, `low`;
   default `medium`), set with `kctl create vm --restart-priority`,
   `spec.restartPriority` in manifests, and the Terraform provider's
   `restart_priority` attribute (cut at the same `VERSION`, per the release
   train). The mover sorts by priority then by memory descending, groups
   VMs by chosen target, and pushes each target's configuration once
   (one `nixos-rebuild` per target instead of one per VM). It does not wait
   for a lower tier's guests to boot; ordering is about who gets capacity
   first when it is scarce.
4. **Events.** New table `ha_events` (migration 64): `id`, `node_id`,
   `vm_id`, `phase` (`detected`, `fenced`, `reassigned`, `applied`,
   `failed`, `skipped`), `detail` JSON, `created_at`. Written by the
   detector and mover; replicated as `ha.event` so every controller shows
   the same history. `kctl get ha-events [--node N] [--vm V]`,
   `ControllerAdmin.ListHaEvents`, and a dashboard panel on the cluster
   page. Webhooks: `node.lost`, `vm.failover.started`,
   `vm.failover.completed`, `vm.failover.failed` (keep emitting
   `vm.migrated` with `mode: failover` for existing subscribers). Each
   completed failover writes `failover_duration_seconds` (detect to
   applied) into the event so RTO is measurable from the data, not from
   log timestamps.
5. **Tests.** Unit tests with a mocked clock for the detector (`lost_at`
   at exactly `nodeTimeoutSecs`, no earlier), ordering and grouping in the
   mover, and the restart-set decision table. The existing
   `failover_fences_then_moves_ceph_vms_and_leaves_local_disks` test keeps
   passing.

Gate: lab run with one `high` and one `low` VM on the lost host; fence
within 35 s of the last heartbeat; `kctl get ha-events` shows the full
sequence with durations; the `high` VM is applied first.

### Phase 3: host isolation response

The controller-side fence stops writes. This phase stops the guest too, so
an isolated host does not hold CPU and network for a VM that is already
running elsewhere, and so a guest with a fenced disk does not sit in I/O
error for minutes.

1. **Node agent.** Track the time of the last successful heartbeat to any
   controller. After `isolation.timeoutSecs` (default 60, must exceed the
   controller's `nodeTimeoutSecs`), for every Ceph VM in the booted
   configuration: `vm.power-button`, wait `isolation.graceSecs` (default
   20), then stop the unit (SIGTERM path already in `ExecStop`). Local-disk
   VMs are left running. Record the action in the agent journal and report
   it in the first heartbeat after reconnecting (`HeartbeatRequest
   .isolation_actions`), which the controller writes to `ha_events`.
2. **Policy.** `isolation.response: powerOff | leaveRunning` in
   `node-agent.yaml`, installed from a controller-side default in
   `failover.isolationResponse`. `leaveRunning` is for sites where the
   controller link is known to be flakier than storage; the controller's
   fence still protects the image.
3. **Avoid stopping guests the controller did not move.** The agent only
   powers off VMs whose images it can no longer write (an `EBLOCKLISTED`
   probe via `rbd status`/a zero-length write through the mapped device is
   not safe; use `ceph osd blocklist ls` through the node's own client,
   which still works when only the controller link is down). If the node is
   also cut off from Ceph, the guests already have a dead disk; power them
   off after the timeout regardless.
4. **Tests.** Node-agent unit tests on the state machine with a mocked
   clock and a mocked `rbd`/`ceph` runner. Lab: cut the controller link
   only; the lost host's guests must be off within `timeoutSecs +
   graceSecs` and must not come back when the link heals (placement gate).

Gate: a guest on an isolated host is stopped by the host itself, and the
event trail shows both the controller's fence and the host's response.

### Phase 4: admission control

1. **Config.**

   ```yaml
   failover:
     admissionControl:
       hostFailuresToTolerate: 1     # 0 disables
   ```

2. **Rule.** For each approved, non-drained node, sum the protected VMs'
   `cpu` and `memory_bytes` placed on it. The cluster passes if, for the
   `hostFailuresToTolerate` largest such sums, the remaining nodes' free
   capacity (physical times overcommit, minus heartbeat load, minus the
   protected VMs they already host) covers them. The computation lives in
   `scheduler.rs` next to the existing capacity maths, with its own unit
   tests.
3. **Enforcement.** `CreateVm`, `SetVmDesiredState(running)`, `MigrateVm`,
   and `DrainNode` run the rule on the post-change placement and refuse
   with `FAILED_PRECONDITION` naming the shortfall. `--ignore-admission-
   control` on `kctl` (and the manifest field) overrides; the override is
   audited. `DeleteNode` and cordon do not block, but `cluster-health`
   reports `failoverCapacity: insufficient` with the deficit.
4. **Visibility.** `kctl get cluster-health` and the dashboard show
   "can tolerate N host failure(s)" and the memory/CPU headroom to the
   next threshold. `GetReplicationStatus` is not involved; this is
   per-cluster capacity, not replication health.

Gate: filling a 3-node lab cluster until the fourth `CreateVm` is refused
with a message naming the host whose loss could not be absorbed; after
`kctl delete vm`, the same create succeeds.

### Phase 5: failover survives controller loss

Depends on the replication work in `ha-crdt-replication.md` being
materialising `node.heartbeat` events on peers (it is).

1. **Lease.** Replicated resource `cluster_lease/failover` with
   `holder` (controller id), `expires_at`, `term`. LWW on `term` then
   `holder`. The holder renews every `leaseRenewSecs` (default 5) with
   expiry `3 * leaseRenewSecs`. Any controller whose local clock sees the
   lease expired, and whose replication apply lag for every peer is below
   `nodeTimeoutSecs / 2`, may claim it with `term + 1`. A holder whose apply
   lag exceeds that bound stops moving VMs and lets the lease lapse.
2. **Detector and mover run on the holder only.** The detector reads
   `last_heartbeat` from the replicated node rows, so it does not matter
   which controller a node agent chose to heartbeat to. Non-holders still
   mark `lost_at` locally (it is observational) but never fence or
   reassign.
3. **Idempotence across a lease change.** The mover already records every
   step in `ha_events` and `vm_operations`; a new holder resumes from the
   last `fenced` event rather than re-fencing, and the reassign is an
   `UPDATE … WHERE node_id = <lost>` so a replicated reassign from the
   previous holder is not undone.
4. **Co-located controller.** The common two-controller shape puts a
   controller on a node. When that node dies, its controller dies with it;
   the other controller claims the lease after expiry and moves the VMs,
   including the ones that were on the dead controller's own host. Test
   this exact case.
5. **Tests.** Extend `specs/tla/ControllerReplication.tla` with the lease
   and the invariant "at most one controller is in `moving` for a given
   node in any state". Rust tests for claim/renew/step-down against a
   simulated peer lag. Lab: two controllers, power off the one holding the
   lease, measure the extra latency (expected: lease expiry plus one
   detector interval).

Gate: `kctl get ha` shows the lease holder and term on every controller;
killing the holder's host moves its VMs within `nodeTimeoutSecs + 3 *
leaseRenewSecs + detectIntervalSecs`.

### Phase 6: asynchronous replica for ZFS VMs

1. **Spec.** `spec.replica: { node: <id>, intervalSecs: 300 }` on a `zfs`
   VM (`kctl create vm --replica-node N --replica-interval 5m`). Refused for
   `lvm` and `filesystem`.
2. **Node agent.** `NodeStorage.ReplicateSnapshot`: `zfs snapshot`, then
   an incremental `zfs send -i` streamed to the replica node over the
   migration transport. `kcore-volume-mirror` already exports a backing
   device on TCP 18128–18255 for the dm-mirror; the ZFS stream uses the
   same port range, the same controller-brokered port reservation, and the
   same node identity check the live-migration receive performs. Keep the
   last two snapshots on both sides; `zfs hold` the common base.
3. **Controller.** A `replica_reconciler` task drives the schedule and
   records `last_replica_at` and `replica_lag_secs` on the VM. On failover
   of a `zfs` VM with a current replica, the mover reassigns to the replica
   node, which rolls back to the last common snapshot and starts the guest.
   The event records the RPO actually lost. The VM is `ha_protected =
   true` with `protection: async (rpo 5m)` in listings.
4. **Fencing.** There is no shared image to fence. The placement gate
   (Phase 1) is what prevents the old node from restarting the guest; the
   returning node's copy is destroyed on its first confirmed heartbeat and
   re-seeded from the new primary when the operator re-enables replication.
5. **Tests.** Extend `tests/local-disk-migrate.nix` with a replica cycle
   and a failover start from the replica, checking a write made after the
   last snapshot is absent and a write before it is present.

Gate: a `zfs` VM with a 5-minute replica restarts on its replica node with
data no older than the last snapshot, and the event shows the RPO.

### Phase 7 (optional): VM monitoring

Restart a VM whose guest hangs while the host is healthy. The guest-ops
channel (`GuestFsFreeze`, `GuestDiskProbe`) gives a heartbeat path; a VM
with no guest agent is out of scope, as in vSphere. Policy per VM
(`vmMonitoring: off | restartAfterSecs`). Not needed for the demo answer;
listed so it is not forgotten.

## Operator-facing summary after Phase 5

```bash
kctl get ha                       # lease holder, deadline, admission headroom
kctl get ha-events --node node-3  # what happened and how long it took
kctl get vms                      # HA column: protected | unprotected | async (rpo 5m)
kctl get cluster-health           # failoverCapacity, lost nodes, unmoved VMs
```

`controller.yaml`:

```yaml
failover:
  enabled: true
  nodeTimeoutSecs: 30
  detectIntervalSecs: 5
  isolationResponse: powerOff
  admissionControl:
    hostFailuresToTolerate: 1
```

## Risks and open points

- **Range blocklist versus the node's own OSDs** (Phase 1.2). The lost
  node's OSDs share its public address. Blocklisting the address while the
  node is down is harmless; the question is recovery. If lifting the entry
  on return is not enough, the fallback is per-port blocklisting, and the
  no-watcher hole is only narrowed, not closed. Decide in the lab before
  writing the code.
- **Placement gate on single-node installs.** A node that is its own
  controller must not deadlock on boot: the agent waits for the local
  controller service, which does not depend on any VM. Make the dependency
  explicit in the NixOS module and test it in `tests/kcore-host-eval.nix`.
- **30 s deadline and nixos-rebuild.** Nodes under a heavy
  `nixos-rebuild switch` (cluster updates) can delay the heartbeat task.
  The agent's heartbeat loop must not share a thread with the apply; verify
  under `cluster-updates` and raise the default if false positives appear.
  A false positive is a fence of a live node, which is a guest outage, so
  the default is conservative until that evidence exists.
- **Admission control versus overcommit.** With `memoryOvercommit > 1`,
  "free capacity" after a failure is heartbeat load, which drops when the
  host dies. The rule uses the schedulable ceiling, not live load, for the
  restarted VMs. Document that admission control is a reservation, not a
  guarantee that the guests will not swap.
- **Two controllers, one lease, clocks.** The lease compares `expires_at`
  against local time. Controllers already require NTP for mTLS; state it
  for the lease and alarm on skew above `leaseRenewSecs`.
- **Community edition scope.** Nothing here needs a binary cache or a
  private channel; it ships in the public commit like the rest of the
  failover path.

## Work items by subsystem

| Subsystem | Phase 1 | Phase 2 | Phase 3 | Phase 4 | Phase 5 | Phase 6 |
|-----------|---------|---------|---------|---------|---------|---------|
| `proto/controller.proto` | `HeartbeatResponse.assigned_vm_ids`, `VmInfo.ha_protected` | `restart_priority`, `ListHaEvents` | `isolation_actions` | `ignore_admission_control` | `GetHaStatus` (lease) | `spec.replica` |
| `proto/node.proto` | `FenceVolume` range mode | | | | | `ReplicateSnapshot` |
| `crates/controller` `db.rs` | `nodes.lost_at` (v63) | `vms.restart_priority`, `ha_events` (v64) | | | `cluster_lease` | `vm_replicas` |
| `crates/controller` `main.rs`, `grpc/controller.rs` | stale filter, host-state probe | detector channel, mover ordering, events | | admission rule | lease, holder-only mover | replica reconciler |
| `crates/controller` `scheduler.rs` | | | | `tolerates_host_failures()` | | |
| `crates/node-agent` | placement gate, range blocklist | | isolation state machine | | | zfs send/recv transport |
| `modules/ch-vm` | `kcore-placement-confirmed.target` | `autoStart` from controller decision | | | | |
| `crates/kctl`, `crates/dashboard` | HA column, health counts | `ha-events`, priority flag, panel | | `ignore-admission-control`, headroom | `kctl get ha` | replica flags |
| `specs/tla` | single-writer invariant | | | | lease invariant | |
| `tests/` (NixOS) | `placement-gate.nix` | | | | | replica in `local-disk-migrate.nix` |
| docs | `vm-migration.md`, gaps list | user doc "High availability" | | | `ha-crdt-replication.md` | `storage.md` |

## Relation to other documents

- `vm-migration.md` describes the current failover and fencing; its
  "Automatic failover" section becomes the operator reference once Phases
  1–2 land and this plan is pruned.
- `storage-and-mobility-gaps.md` holds the P0 items Phase 1 closes.
- `ha-crdt-replication.md` is the controller-replication plan Phase 5
  builds on.
- `heartbeat.md` documents the 90 s / 30 s constants Phase 2 replaces.
