# Day-2 Filesystem Operations (Technical)

This document is a technical runbook for day-2 filesystem and disk-layout changes on installed kcore nodes.

It complements:

- `docs/storage.md` (backend model and command surface)
- `docs/node-install-bootstrap-flow.md` (install-time flow)
- `docs/kctl-commands-and-workflows.md` (operator command reference)

## Scope

This document covers **post-install** operations for storage backends used by kcore:

- `filesystem` (ext4 mounts generated via disko)
- `lvm` (PV/VG managed by disko; LVs created by node-agent)
- `zfs` (pool created by disko; zvols/datasets used by node-agent)

It focuses on:

- ownership and safety controls
- idempotent execution model
- operational sequencing
- validation and rollback strategies

## Ownership model and safety gates

kcore enforces explicit ownership for day-2 disk changes via:

- `/etc/kcore/disko-management-mode`

Valid values:

- `installer-only` (default after install)
- `controller-managed`

### Rules

1. Install-time always leaves nodes in `installer-only`.
2. `kctl node apply-disko --apply` is rejected in `installer-only`.
3. Validation-only (`kctl node apply-disko -f ...` without `--apply`) remains available for syntax/eval checks.
4. Promotion to `controller-managed` is a deliberate operator action.

This prevents accidental partitioning operations in normal day-2 workflows.

## RPC and CLI execution path

Day-2 disko operations flow through:

1. `kctl` sends `ApplyDiskoLayoutRequest` to `NodeAdmin.ApplyDiskoLayout`.
2. Node-agent validates input contains `disko.devices`.
3. Node-agent executes bounded command via `timeout`:
   - validate path: `nix-instantiate --parse <file>`
   - apply path: `disko --mode format,mount <file>`
4. Node-agent returns response containing:
   - `success`
   - `message`
   - `mode` (current ownership mode)

Timeout behavior:

- `timeout_seconds <= 0` defaults to `300`
- timeout is capped server-side (`3600`)

## Filesystem backend invariants

For the `filesystem` backend, current generated layouts use ext4 partitions mounted under:

- `/var/lib/kcore/volumes`
- `/var/lib/kcore/volumesN` for additional disks

### Invariants

- existing mount points must remain stable unless migration is planned
- new day-2 data disks should introduce new mount points, not replace active mounts in-place
- VM image cache path (`/var/lib/kcore/images`) must remain accessible
- node-agent process must retain write access to volume roots

## LVM backend invariants

For `lvm`, disko creates PVs and VG; node-agent creates/deletes LVs dynamically.

### Invariants

- VG name in runtime config must match on-disk VG
- PV additions must not remove or rename existing PVs backing active LVs
- thin-pool/data LV topology changes require dedicated maintenance planning

### Practical checks

- `vgs`, `pvs`, `lvs -a -o +devices`
- verify `node-agent.yaml` LVM names before and after apply

## ZFS backend invariants

For `zfs`, disko creates pool; node-agent consumes pool for zvol/dataset operations.

### Invariants

- pool name must remain stable (`tank0` or configured value)
- do not import pool under alternate name
- avoid day-2 operations that modify ashift/feature compatibility without maintenance window

### Practical checks

- `zpool status`
- `zpool list`
- verify pool name in runtime config remains unchanged

## Safe day-2 workflow (recommended)

### 0) Preflight

- capture current state:
  - `lsblk -f`
  - `findmnt -R /var/lib/kcore`
  - backend-specific health (`vgs/pvs/lvs` or `zpool status`)
- ensure target disks are correct devices
- verify maintenance window and backup posture

### 1) Confirm ownership mode

```bash
cat /etc/kcore/disko-management-mode
```

If still `installer-only`, promote intentionally:

```bash
echo controller-managed | sudo tee /etc/kcore/disko-management-mode
```

### 2) Validate disko layout only

```bash
kcore-kctl --node <node:9091> node apply-disko -f ./day2-disko.nix
```

Do not proceed until validation returns success.

### 3) Apply with bounded timeout

```bash
kcore-kctl --node <node:9091> node apply-disko \
  -f ./day2-disko.nix \
  --apply \
  --timeout-seconds 600
```

### 4) Reconcile Nix/system view

Apply node Nix configuration so mount declarations and services are consistent:

```bash
kcore-kctl --node <node:9091> node apply-nix -f ./node-config.nix
```

### 5) Post-apply verification

- `findmnt -R /var/lib/kcore`
- verify backend health
- verify node-agent active and volume operations still succeed

## Idempotence and repeatability expectations

Day-2 operations should be authored so re-running does not introduce drift:

- avoid random/non-deterministic naming in disko definitions
- keep stable identifiers (VG/pool names, mountpoint paths)
- prefer additive changes over in-place destructive replacement

If a change is not naturally idempotent, treat it as a one-time migration step and document a guard condition.

## Failure handling

### Validation failure

- fix layout file
- rerun validation
- no disk mutation expected

### Apply failure

- inspect returned stderr in RPC response
- verify partial state with `lsblk/findmnt` and backend tools
- do not retry blindly; first confirm whether partial partitioning occurred

### Timeout

- command was bounded; inspect actual system state before re-attempt
- increase timeout only when operation is known to be long-running and safe

## Concurrency and maintenance considerations

- avoid concurrent day-2 disko apply operations on the same node
- avoid VM placement churn during partitioning operations
- for topology-changing operations (VG/pool structural edits), prefer temporary workload quiesce

## Example: additive filesystem disk

Minimal additive pattern for `filesystem` backend:

1. Add new disk entry producing `/var/lib/kcore/volumes1`.
2. Validate only.
3. Apply.
4. Reconcile Nix config and verify mount.

This keeps existing `volumes` mount untouched and reduces blast radius.

## What is intentionally not covered

- repartitioning OS/root disk in-place on production nodes
- automatic migration of existing volumes between backends
- backend conversion (`filesystem -> lvm -> zfs`) without explicit migration tooling

Those are maintenance/reinstall-class workflows and should be handled with dedicated migration procedures.
