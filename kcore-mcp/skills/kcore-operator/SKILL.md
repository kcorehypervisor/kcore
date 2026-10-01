---
name: kcore-operator
description: Operate a kcore cluster from the editor. Use when the user wants to create a cluster context, VM, network, volume, Ceph resource, or day-2 change, or asks what Terraform would do on kcore.
---

# kcore operator

kcore apply is a declarative upsert. Creating an existing resource updates mutable fields and rejects immutable ones.

## Steps

1. Call `kcore_advise` with the user's intent and any answers already given.
2. Ask every blocking question. Wait. Ask recommended questions too, unless the user said to use defaults.
3. Call `kcore_plan`. Show the action (`create`, `update`, `unchanged`, or `replace`) and the warnings.
4. Apply, delete, migrate, drain, or install a node only after a clear yes. Pass `confirm: true`.
5. Node install also needs `spec.acknowledge` set to `wipe the selected disks` after the user has confirmed the device names.

## Connection

The server shells out to `kctl` and uses `~/.kcore/config`. Pass `connection.controller` or `connection.operator` when the user names a different context.
