# VM Scheduler

The scheduler decides which node should host a new VM when the user doesn't
explicitly specify a `--target-node`.

Source: `crates/controller/src/scheduler.rs`

## How it works

When `CreateVm` is called without a `target_node`, the controller loads every
registered node from the database and passes the list to the scheduler.

The scheduler does two things:

1. **Filters** — only nodes with `status == "ready"` and `approval_status == "approved"` are considered. Required labels, the datacenter, and anti-affinity remove more candidates before ranking.
2. **Ranks** — among the eligible nodes it picks the one with the **most free
   resources**, using `(free_memory, free_cpu)` as a composite sort key.
   Memory is the primary factor because it's usually the bottleneck for VMs.

### Capacity-aware placement

Free capacity is heartbeat load subtracted from a schedulable ceiling. The
ceiling is physical capacity times the overcommit ratio from `controller.yaml`
(`scheduler.cpuOvercommit` and `scheduler.memoryOvercommit`, both default
`1.0`):

```
free_memory = memory_bytes * memoryOvercommit - memory_used
free_cpu    = cpu_cores * cpuOvercommit - cpu_used
```

- **`memory_bytes` / `cpu_cores`** — total node capacity, reported once during
  `RegisterNode`.
- **`memory_used` / `cpu_used`** — live load, updated on every heartbeat
  (see [docs/heartbeat.md](heartbeat.md)).

A node is eligible only when both free values cover the requested VM. If no
node can fit, the RPC returns `UNAVAILABLE`.

## Algorithm: most-free-first, with placement constraints

The default strategy is still **spread scheduling**. New VMs land on the ready,
approved node with the most free memory, then free CPU. Free capacity is live
heartbeat load subtracted from the schedulable ceiling:

```
schedulable = physical * overcommit
free        = schedulable - heartbeat_used
```

`controller.yaml` sets the ratios. Both default to `1.0`, which is physical
capacity. A value above 1 admits another VM while the node is quiet even if
the sum of VM sizes would pass the physical core count. A loaded node stays
ineligible because the heartbeat load is what counts as used.

```yaml
scheduler:
  cpuOvercommit: 1.0
  memoryOvercommit: 1.0
```

Each ratio must be from 1.0 through 16.0.

### Label affinity

`kctl create vm web --label dc=dc-a --label rack=rack-1` places the VM only on
a node that has every listed label. Labels are the `key=value` strings stored
at node registration. `--target-node` still has to carry those labels; the
controller does not substitute a different node for a label mismatch.

### Anti-affinity

`kctl create vm web --anti-affinity web` refuses a node that already hosts a
VM in the group `web`. The group is stored with the VM and is kept when the
controller drains a node, so an evacuate does not put both members on the
same host. A manifest can set `spec.antiAffinity` and `spec.nodeLabels`.

## Functions

### `select_node(nodes) -> Option<&NodeRow>`

Returns the ready node with the most free resources. Used when no specific VM
size is known.

### `select_node_for_vm(nodes, requested_cpu, requested_memory) -> Option<&NodeRow>`

Returns the ready node that can fit the requested VM at the default overcommit
of 1.0 and has the most remaining capacity after placement.

### `select_with_constraints(...)`

Same ranking, plus datacenter, required labels, anti-affinity, and the
configured overcommit. `CreateVm` uses this.

## Edge cases

| Situation | Behaviour |
|-----------|-----------|
| No nodes registered | Returns `None` → `UNAVAILABLE` |
| All nodes `status != "ready"` | Returns `None` → `UNAVAILABLE` |
| No node has enough capacity under the overcommit ceiling | `UNAVAILABLE` |
| No node has every required label | `UNAVAILABLE` |
| Every fitting node already hosts the anti-affinity group | `UNAVAILABLE` |
| Explicit node is missing a required label | `FAILED_PRECONDITION` |
| Single node with capacity | That node is selected |
| Tie (equal free resources) | Deterministic but arbitrary (depends on DB row order) |

