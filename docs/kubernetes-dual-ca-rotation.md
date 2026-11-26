# Kubernetes dual-CA rotation

The controller shows `CA expiry: cluster CA expires in N days; plan a dual-CA rotation` when fewer than 365 days remain on the cluster CA. `kctl kubernetes rotate-ca` replaces the CA without a hard cutover. You can also run it earlier, for scheduled maintenance or after a suspected key leak.

Leaf renewals (`kctl kubernetes rotate-certs`) re-sign under the current CA and do not replace it. They are refused while a CA rotation is in progress, and the automatic renewal waits. The REISSUE step renews every leaf anyway.

## How it works

Every node gets two kinds of CA certificate:

- **Trust bundle** (`/etc/kubernetes/pki/ca.crt`, `etcd/ca.crt`). The API server's `--client-ca-file`, the kubelet's `clientCAFile`, the controller manager's `--root-ca-file`, and etcd's trusted CA files all read it. During a rotation it holds the old CA followed by the new one.
- **Signing certificate** (`/etc/kubernetes/pki/ca-signing.crt`). Only `--cluster-signing-cert-file` reads it. It is always the single CA that matches `ca.key` on disk.

Private keys never go into the Nix store. The node agent writes the active `ca.key` and `etcd/ca.key` under `/var/lib/kcore/kubernetes/<cluster>/pki`. The retiring CA's key stays in controller storage.

The service account keys do not change. Existing tokens keep working.

## Steps

Each step changes the stored PKI and queues one roll. The orchestrator rebuilds and restarts one node at a time, control planes first, and waits for etcd between control planes. Steps never run on their own. The 30-second loop only shows a `CA expiry` notice naming the next step.

Every step is refused unless the cluster is `ready`, the API server answers, etcd is healthy on every control plane, no scale or leaf renewal is running, the previous step's roll has finished, and every node already runs the configuration the controller renders.

1. **BEGIN.** Takes an etcd snapshot (trigger `pre-ca-rotation`), generates a new Kubernetes CA and, by default, a new etcd CA. The old CAs are kept as the retiring ones. Leaves are not touched. Every node rolls onto the old+new trust bundle and still presents certificates from the old CA. The admin kubeconfig's `certificate-authority-data` becomes the bundle.
2. **REISSUE.** Issues every leaf again under the new CA: API server, API server kubelet and etcd clients, etcd server and peer, admin, and each kubelet's client certificate. Every node rolls again with both CAs still trusted.
3. **RETIRE.** Allowed only when every stored leaf verifies against the new CA. Removes the old CA from the trust bundle and from controller storage, sets the admin kubeconfig's CA data to the new CA alone, and rolls once more.

```bash
kctl kubernetes rotate-ca <cluster> --step begin --yes
kctl kubernetes certs <cluster>          # rotation: trust-distributed; wait for "rolling" to clear
kctl kubernetes rotate-ca <cluster> --step reissue --yes
kctl kubernetes certs <cluster>          # rotation: leaves-reissued
kctl kubernetes rotate-ca <cluster> --step retire --yes
kctl kubernetes certs <cluster>          # rotation: idle
```

Options for `--step begin` only:

- `--kubernetes-only` keeps the etcd CA. `--include-etcd` is the default and replaces both.
- `--skip-backup` goes ahead without the etcd snapshot. Without it, a failed snapshot stops BEGIN.

`kctl kubernetes certs` shows the CA expiry (the retiring CA while both are trusted), the rotation phase, and any step whose roll has not finished. The cluster status message shows progress as `CA rotation <step>: rebuilding <node>`.

If a roll stops (a rebuild fails, or etcd loses a member), the step stays queued and the next pass rolls every node again. Fix the cause and let it finish. A step whose roll has not finished blocks the next step.

## Before you start

- Every node must run a node agent that ships the `caSigningCrt` option in `modules/kcore-kubernetes.nix`. An older module fails to evaluate the rendered configuration during BEGIN.
- The etcd data guard (`kcore-etcd-ca.sha256`) records a hash for each certificate in the etcd trust bundle. etcd data is kept as long as one CA overlaps with what was recorded, so the bundle and the final retire do not move the data aside.
- Plan for three full rolls of every node.

## Clients

Each roll rewrites the admin kubeconfig on the control planes (`/etc/kubernetes/admin.conf`) and every kubelet kubeconfig. Copies taken off a node before BEGIN only trust the old CA; after REISSUE the API server presents a certificate from the new one, so copy the admin kubeconfig again. After RETIRE, copy it once more to drop the old CA from it.

Update any external webhook, OIDC provider, or tool that pinned the old CA or its fingerprint before RETIRE.

## Rollback

The safe place to stop is after BEGIN. Leaves are still under the old CA and every node trusts both, so the cluster runs as before. It can stay there; continue with REISSUE when ready.

There is no abort RPC. Going back from BEGIN to the old CA alone means restoring the cluster's `kubernetes_pki` row from a controller database backup taken before BEGIN, then letting the drift reconcile roll the nodes onto the single old CA. Prefer stopping over going back.

After REISSUE, do not restore the old PKI. Leaves are under the new CA, and dropping it from trust breaks every node. Finish with RETIRE, or wait for a cut-short roll to finish.

If the API stops trusting clients mid-roll, restore the etcd snapshot BEGIN took (`kctl kubernetes restore <cluster> --snapshot <id> --yes`) together with the matching PKI and Nix generation.
