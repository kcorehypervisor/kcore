# Kubernetes dual-CA rotation (manual)

The controller surfaces `CA expiry: cluster CA expires in N days; plan a dual-CA rotation` when fewer than 365 days remain on the cluster CA. There is no automated dual-CA flip yet; run this procedure when that notice appears (or earlier for scheduled maintenance).

Leaf renewals (`kctl kubernetes rotate-certs`) re-sign under the **current** CA and do not replace it. Dual-CA is only for replacing the cluster CA (and usually the etcd CA) without a hard cutover.

## Prerequisites

- Cluster phase `ready`, etcd healthy, recent etcd snapshot (`kctl kubernetes backup create`).
- Access to every control-plane and worker node (or wait for the next Nix roll).
- Maintenance window long enough for two full control-plane rolls plus client/kubeconfig redistribution.

## Outline

1. **Snapshot.** Take an etcd backup and confirm it lands on the configured target.
2. **Issue a new CA.** Generate a new kubernetes CA keypair (and etcd CA if you rotate that too). Keep the old CA private key until every leaf under it is gone.
3. **Trust bundle = old + new.** Distribute a concatenated `ca.crt` (old PEM then new PEM) to every apiserver, kubelet, and client that verifies cluster certs. etcd peer/client trust gets the same treatment for the etcd CA. Roll control planes one at a time; wait for etcd healthy / API ready before the next.
4. **Re-issue leaves under the new CA.** Renew apiserver, kubelet, admin, and etcd server/peer/client certs so they chain to the new CA. Workers and control planes can roll; keep the dual bundle until every leaf is new.
5. **Drop the old CA.** After the oldest leaf signed by the old CA has expired (or you have force-rotated every identity), remove the old PEM from the trust bundle and roll once more.
6. **Clients.** Refresh operator kubeconfigs and any external webhook / OIDC trust that pinned the old CA fingerprint.

## Operator commands (today)

```bash
kctl kubernetes backup create <cluster>
kctl kubernetes status <cluster>
kctl kubernetes certs <cluster>
# Leaf-only (not CA replacement):
kctl kubernetes rotate-certs <cluster> --force --yes
```

CA material itself still lives in controller PKI storage and the rendered Nix / `pkiKeyDir` on each node. Until a `rotate-ca` RPC exists, inject the dual bundle and new leaves via a planned configuration roll (apply the updated cluster after updating stored PKI), one control plane at a time.

## Rollback

Restore the previous trust bundle and leaf set from the etcd snapshot and the prior Nix generation if the API stops trusting clients mid-roll. Prefer aborting after step 3 (dual trust, old leaves still valid) over removing the old CA early.

## Tracking

Automated dual-CA (controller-owned new CA, dual-bundle nixgen, leaf reissue, old-CA retirement) is intentionally not implemented; this document is the interim operator runbook.
