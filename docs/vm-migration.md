# VM migration (cold and live)

kcore moves VMs between nodes in two ways. Both assume the guest disk is **shared** (today: **Ceph RBD**). Local backends (`filesystem` / `lvm` / `zfs`) keep disks on one host, so they are not live-migratable across nodes.

| Mode | Operator command | Guest downtime | Disk copy | Typical use |
|------|------------------|----------------|-----------|-------------|
| **Cold** | `kctl drain node …` or `kctl migrate vm … --allow-cold-fallback` | Full stop/start (or reassignment then start) | None (shared RBD) | Maintenance, evacuate a node, fallback |
| **Live** | `kctl migrate vm <id> --target-node <node>` | Brief pause during CH cutover | None (shared RBD) | Keep workload running while changing host |

Product docs: [VM migration](https://kcorehypervisor.com/docs/user/vm-migration.html) · SAN overview: [`ceph.md`](./ceph.md).

## Prerequisites (both modes)

- VM `storage_backend = ceph`.
- Source and destination are members of the same `CephCluster`, and that cluster's reconciled status is `healthy`. `MigrateVm` rejects a destination outside a healthy cluster with `FAILED_PRECONDITION`, and `DrainNode` will not pick one as a target.
- RBD image exists in pool `kcore-vms` as `kcore-<vm-id>` (controller `volumes` row).
- Live only: Cloud Hypervisor on both nodes; guest memory configured with `shared=on` (Ceph VM units in `modules/ch-vm/vm-service.nix`); TCP allowed between node data IPs for an ephemeral migration port.

RBAC: `DrainNode` → `cluster-admin`; `MigrateVm` → `vm-admin`.

---

## Cold migration / drain

### What operators run

```bash
# Evacuate every VM off a node (cold reassignment)
kctl drain node <source-node> [--target-node <dest>]

# Single VM: live first, cold if live fails
kctl migrate vm app-1 --target-node node-b --allow-cold-fallback
```

### Control-plane behaviour (`DrainNode` / cold path)

1. Mark source `draining` (drain only).
2. For each VM (or the one VM on cold fallback):
   - Pick a target with capacity and compatible backend (`ceph` membership counts). The target must be `approved` and not itself `draining`/`drained` — that holds for an operator-supplied `--target-node` too, not just for scheduler picks. For `ceph` VMs the target must also belong to a **healthy** `CephCluster` — a node in a degraded cluster may not be able to map the image at all.
   - **Release the shared RBD on the source** (see below). A VM whose image cannot be released is skipped and left where it is.
   - **Reassign ownership**: `UPDATE vms SET node_id` in place. Never delete-and-reinsert — `vm_ssh_keys` and `security_group_vm_attachments` both reference `vms(id) ON DELETE CASCADE`, so a delete silently destroys the VM's SSH keys and security group attachments. The `volumes` row is unchanged: the RBD image stays put.
3. `push_config_to_node` on source (VM disappears from Nix → unit stops / disk unmapped).
4. `push_config_to_node` on destination, **waiting for the apply** (unit appears → map RBD, start CH if `autoStart`).
5. Drain marks source `drained` **only if every VM moved and every config push applied**; otherwise the node stays `draining` and `DrainNode` returns `success = false` with a per-VM error list. Nothing in the per-VM loop aborts the RPC: VMs earlier in the loop have already been reassigned in the database and no config has been pushed yet, so an early return would leave the cluster disagreeing with the database.

There is **no** Cloud Hypervisor migration API on the cold path. The guest is stopped on the source (via systemd/Nix removal) and started fresh on the destination against the same RBD device path once mapped.

### The exclusivity barrier (why a cold move is not just two config pushes)

`ApplyNixConfig` starts `nixos-rebuild` **asynchronously** and returns as soon as the file is written, so the two pushes in steps 3 and 4 race: the destination can map and boot from the shared image while the source VMM is still writing to it. Two writers on one RBD image corrupts the guest filesystem.

Before reassigning a `ceph` VM, the controller therefore calls `FinalizeLiveMigrateSource` on the node that currently owns it, which stops the VM unit and runs `rbd unmap`. `rbd unmap` **cannot succeed while a local VMM still holds the device open**, so a successful call is positive proof that the source has let go — it is the barrier, not just a request.

The node answers with the post-conditions it **observed** (`vmm_stopped`, `rbd_unmapped`), not with the fact that it issued the calls, and the controller checks both.

Failure handling:

- **Source unreachable** (`UNAVAILABLE`): tolerated with a warning. This is the node-failure drain case, where the source VMM died with its node.
- **Any other failure, or a reply with either post-condition false**: hard `FAILED_PRECONDITION`. The VM is left on the source rather than risking a second writer.

`DeleteVm` uses the same barrier before destroying a Ceph VM's RBD image: `rbd rm` cannot remove an image the owning node still has mapped, so deleting first left the image orphaned in the pool with its `volumes` row already gone.

### Waiting for the Nix apply

`ApplyNixConfig` returns as soon as the configuration file is written; the `nixos-rebuild` it starts runs in a transient `kcore-nix-rebuild` unit, because `nixos-rebuild switch` restarts the node agent and no in-process watcher would survive it. That unit records the verdict, and `GetNixApplyStatus` reports it.

Any step that assumes the generated unit already exists therefore polls that verdict first: the live-migration destination push (`FinalizeLiveMigrateDest` runs `systemctl start` on the unit), the cold-move destination push, `CreateVm`, `SetVmDesiredState`, and both legs of `DrainNode`. The wait is bounded (10 minutes) and reports `DEADLINE_EXCEEDED` with the node's last message.

A node agent that predates apply tracking returns an empty `apply_id`, and a node whose `/run` state was discarded (or whose apply was superseded by a newer one) reports `NIX_APPLY_PHASE_UNKNOWN`. Both mean "there is no verdict coming": the controller logs it and proceeds rather than failing an operation that may well have worked.

### Why Ceph makes cold cheap

Without shared storage, “migrate” would mean copy or recreate the disk. With RBD, cold move is a **control-plane + systemd** operation: remap `/dev/rbd/<pool>/<image>` on another host.

---

## Live migration

### What operators run

```bash
kctl migrate vm app-1 --target-node node-b
# optional:
kctl migrate vm app-1 --target-node node-b --allow-cold-fallback
```

Response includes `mode=live` or `mode=cold`.

### End-to-end sequence

Orchestrator: controller `MigrateVm` → node `NodeAdmin` peer RPCs.

```
  Controller                         Source node                    Dest node
      |                                   |                             |
      |  PrepareLiveMigrateReceive        |                             |
      |---------------------------------------------------------------->|
      |                                   |                    map RBD
      |                                   |                    spawn CH (--api-socket only)
      |                                   |                    PUT vm.receive-migration
      |                                   |                    listen tcp:0.0.0.0:<port>
      |  SendLiveMigrate                  |                             |
      |---------------------------------->|                             |
      |                        disable systemd Restart                  |
      |                        PUT vm.send-migration                    |
      |                        tcp:<dest-ip>:<port> -------------------->|
      |                                   |                    receive completes
      |                                   |                    write .live-migrated marker
      |  WaitLiveMigrateReceive           |                             |
      |---------------------------------------------------------------->|
      |  reassign vms.node_id             |                             |
      |  push Nix (src + dest)            |                             |
      |  FinalizeLiveMigrateDest          |                             |
      |---------------------------------------------------------------->|
      |                                   |                    systemctl start adopts PID
      |  FinalizeLiveMigrateSource        |                             |
      |---------------------------------->|                             |
      |                        stop unit / rbd unmap                    |
```

### Node details

**Prepare (destination)** — `crates/node-agent/src/live_migrate.rs`

- `rbd map <pool>/<image>` → `/dev/rbd/<pool>/<image>` (same path the migrated config will open).
- Spawn `cloud-hypervisor --api-socket=/run/kcore/<vm>.sock` with **no** CLI disk/memory (empty receive VMM).
- Record PID in `/run/kcore/<vm>.migrate.pid`.
- Async task: CH `PUT /api/v1/vm.receive-migration` with `receiver_url=tcp:0.0.0.0:<port>`.
- On success, write `/run/kcore/<vm>.live-migrated`.

**Send (source)**

- `systemctl set-property kcore-vm-<vm>.service Restart=no` so CH exit after send is not restarted against the still-mapped RBD.
- `PUT /api/v1/vm.send-migration` with `destination_url=tcp:<dest-host>:<port>` (host taken from the destination node’s gRPC address, port from prepare).

**Handoff (destination systemd)** — `modules/ch-vm/vm-service.nix`

- Ceph VMs: `--memory size=…M,shared=on` (CH live-migrate requirement).
- `stopIfChanged` / `restartIfChanged` keep their NixOS defaults. `switch-to-configuration` only consults them for units that **already existed and changed**; the destination unit is brand new, so it is simply started and adopts the receive process. (Overriding them for Ceph would also have quietly stopped `cpu`/`memory`/`extraArgs` updates from ever taking effect.)
- `ExecStartPre`: if `.live-migrated` exists, skip wiping the API socket and skip cold provision.
- `ExecStart`: if marker present, `tail --pid=<migrate.pid> -f /dev/null` so systemd’s MainPID waits on the already-running CH; else cold-start the full CH CLI.

**Finalize**

- Dest: `systemctl start kcore-vm-<vm>.service` (adopts or no-ops if already started via `wantedBy`).
- Source: stop unit, `rbd unmap`.

**Abort** (send failed before cutover): kill receive CH, remove marker/socket, unmap dest RBD.

### Failure and fallback

- If live fails **before** a successful send, destination receive is aborted; with `--allow-cold-fallback` the controller cold-reassigns and pushes Nix.
- After a **successful send**, cold fallback is **disabled** (would start a second VMM on the same RBD). Recover by finalizing / fixing handoff on the dest.
- After a **successful send**, the source VMM is gone — do not abort the destination process.

### Seeding (cold drain safety)

First boot seeds the RBD with `qemu-img convert` and sets Ceph image-meta `kcore.seeded=1` (plus a local marker). Cold drain/migrate to another node must **not** re-seed; the shared meta flag prevents wiping guest data.

### Networking

- Migration stream uses a TCP port on the destination from the fixed range **18000–18127** (`MIGRATE_PORT_BASE`/`MIGRATE_PORT_COUNT` in `crates/node-agent/src/live_migrate.rs`). The range sits deliberately **below** the kernel's default ephemeral range (32768–60999), so the kernel never hands one of these ports to an unrelated outbound connection while Cloud Hypervisor is starting up. The node-agent holds a listener on the port it picked until Cloud Hypervisor is about to bind it, so nothing else can win the race in between.
- Allow that traffic between Ceph member hosts on 18000–18127 (public/client fabric is fine).

### RBD features

Images are created with **`layering` only**. Exclusive-lock is omitted so source and destination can map the same image during cutover. Changing this without another locking strategy will break live migrate.

---

## Runbook: a stranded receive session

### Symptom

A live migration that used to work now fails immediately, and the error names the destination:

```
migrate failed: status: AlreadyExists, message: "preparing node <dest> to receive VM '<vm>':
live migrate receive already prepared for <vm>"
```

Nothing is actually receiving. What happened is that the destination node still holds the receive session it created in `PrepareLiveMigrateReceive`, because the controller died (or lost the node) between preparing the receive and aborting it. The session outlives the migration it belonged to, and `PrepareLiveMigrateReceive` will keep answering `ALREADY_EXISTS` for that VM on that node until an operator clears it.

The same state can also survive on disk without an in-memory session, if the **node agent** restarted after spawning the receive VMM: the `.migrate.pid` file is then the only remaining handle on an orphaned Cloud Hypervisor that still holds the API socket and the RBD mapping.

### There is no automatic reaping — on purpose

Nothing in kcore reaps a receive session on a timer or on the next prepare. Reaping requires deciding that a session is *provably* dead, and the cost of getting that wrong is asymmetric: clearing a session whose receive VMM is still running kills an in-flight migration and can leave the guest destroyed on both sides. So kcore surfaces the evidence and an operator makes the call.

### 1. Look before acting

```bash
# Every CephCluster member (the only nodes a receive is ever prepared on)
kctl get migrate-session <vm>

# Or one node, e.g. the destination named in the error
kctl get migrate-session <vm> --node <node-id>
```

Read-only: it touches no processes and no files. Per node it reports

| Field | Meaning |
|-------|---------|
| `session tracked` | A prepare ran here and the session is still in memory. This is what returns `ALREADY_EXISTS`. |
| `port` | The migration port held for the session, and whether anything is **listening** on it. |
| `session pid` / `pid file` | The receive-mode Cloud Hypervisor pid, from memory and from `<socket-dir>/<vm>.migrate.pid`. |
| `receive VMM` | Whether that pid is a live process — and whether it really is *this VM's* VMM, checked against its `/proc` cmdline. An alive pid that is not our VMM is a **recycled pid**, and clearing will not signal it. |
| `handoff marker` | `<socket-dir>/<vm>.live-migrated` exists, i.e. the receive already completed. |
| `verdict` | `stranded - nothing is receiving`, or `LIVE - a receive may be in flight; clearing would kill it`. |

The verdict is a summary of the fields above, not a decision. **Do not clear a node whose verdict is `LIVE`** unless you know the migration it belongs to is already lost.

### 2. Clear it

```bash
# Reports what it would clear, and clears nothing
kctl migrate reset-session <vm> --node <node-id>

# Actually clear it
kctl migrate reset-session <vm> --node <node-id> --force
```

Without `--force` the command prints the state above and exits non-zero, so running it by accident — or before reading the state — cannot destroy anything. `--force` is the explicit operator intent, matching the other destructive `kctl` commands.

Clearing runs the same cleanup as the abort that ends a failed migration, so the two cannot drift:

- the in-memory session and its **port reservation** (returned to the 18000–18127 pool),
- the spawned **receive VMM** — from the tracked session, or from the pid file when the agent restarted; a recycled pid is reported and *not* signalled,
- the **marker**, **pid** and **API socket** files under the VM socket directory,
- the destination's **RBD mapping** for the VM's volume.

It is **idempotent**: clearing a node with no session is a success, so a repeated runbook step needs no special casing.

Two guards make it hard to use destructively by mistake:

- The node must be named explicitly with `--node`; there is no "wherever you find one" mode.
- The RPC **refuses the node that currently owns the VM** (`FAILED_PRECONDITION`). A receive session only strands on a node that does not own the VM — reassignment is the last step of a successful migration — and on the owner the same cleanup would stop a VM that is running perfectly well.

Every successful clear is recorded in the audit log as `ResetLiveMigrateReceive`, including whether the session still looked live.

### Retry

Once the session is gone, retry the migration normally:

```bash
kctl migrate vm <vm> --target-node <node-id>
```

### RPCs behind it

| RPC | Service | Role | Notes |
|-----|---------|------|-------|
| `GetLiveMigrateReceiveStatus` | `Controller` | `read-only` | Fans out to the nodes; an unreachable node is reported, not fatal. |
| `ResetLiveMigrateReceive` | `Controller` | `cluster-admin` | Same bar as `DrainNode`, not `MigrateVm`. |
| `GetLiveMigrateReceiveStatus` | `NodeAdmin` | controller cert | Read-only observation on one node. |
| `AbortLiveMigrateReceive` | `NodeAdmin` | controller cert | The shared cleanup; now also reports the state it observed before tearing down. |

---

## Comparison with other stacks

| Stack | Shared disk | Memory path |
|-------|-------------|-------------|
| kcore + Ceph RBD | krbd map on both sides | CH TCP send/receive-migration |
| libvirt + RBD | often librbd in QEMU | QEMU migrate |
| Local LVM/ZFS on kcore | N/A (node-local) | Not supported across nodes |

---

## Source map

| Piece | Location |
|-------|----------|
| `MigrateVm` / `DrainNode` | `crates/controller/src/grpc/controller.rs` |
| Peer RPCs | `proto/node.proto`, `crates/node-agent/src/grpc/admin.rs` |
| CH HTTP client | `crates/node-agent/src/vmm/client.rs` |
| Helpers | `crates/node-agent/src/live_migrate.rs` |
| Unit handoff | `modules/ch-vm/vm-service.nix` |
| Receive-session escape hatch | `crates/kctl/src/commands/migrate_session.rs` |
| CLI | `kctl migrate vm`, `kctl drain node`, `kctl get migrate-session`, `kctl migrate reset-session` |
