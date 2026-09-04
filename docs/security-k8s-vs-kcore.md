# Security: Kubernetes vs kcore

This document compares the security model of Kubernetes with kcore's current
implementation. The goal is transparency about what kcore does and does not
provide, and to guide future hardening work.

## Trust Model

**Kubernetes** uses a multi-layered trust model designed for multi-tenant
environments running untrusted workloads. Identity is established through
service accounts, RBAC policies, and certificate signing requests (CSRs)
that go through an approval workflow.

**kcore** uses a simpler single-operator model. Trust is rooted in a shared
CA whose private key lives on the operator's machine. Certificates signed
by that CA grant access; the Common Name (CN) in the certificate determines
the role (`kcore-controller`, `kctl`, or `kcore-node-<host>`).

---

## Certificate and Identity Management

| Area | Kubernetes | kcore |
|------|-----------|-------|
| Bootstrap | TLS Bootstrap with short-lived tokens (24h default). Node submits a CSR; the control plane signs it. Token expires automatically. | Operator pushes a CA-signed cert directly to the node during `kctl node install`. No bootstrap token. |
| Cert rotation | kubelet auto-renews its certificate before expiry. Built into the kubelet. | Node-agent generates a fresh keypair and submits a CSR (`SignNodeCsr`); the private key never leaves the node. It checks hourly and rotates inside 30 days of expiry or the last 25% of the lifetime, then reloads its TLS listener in process. Operators can force it with `kctl rotate node-certs`. Controller cert via `kctl rotate certs`, sub-CA via `kctl rotate sub-ca`. |
| Revocation | No CRL or OCSP for client certificates: the remedies are denying CSR renewals, deleting the node object, or rotating the CA. Removing an RBAC binding blocks access immediately, which is what most deployments actually rely on. | Per-certificate revocation: `kctl revoke cert` records an RFC 5280 reason code against the certificate inventory, regenerates the sub-CA-signed CRL, and the OCSP responder answers `revoked` immediately. Enforced by an interceptor ahead of every handler rather than in the handshake. Sub-CA and root CA rotation remain the blunt instruments. |
| Identity granularity | Each component has a distinct service account with RBAC bindings. Least-privilege by default. | Machine CNs (controller / node) plus per-operator certs `kctl:<name>` with flat roles `read-only`, `vm-admin`, `cluster-admin`. |

### How kcore bootstrap works

1. Operator runs `kctl create cluster --controller <ip:port>` on their
   local machine. This generates a self-signed CA, a controller cert
   (with the controller IP as a SAN), and a kctl client cert.
2. When adding a node, `kctl node install --join-controller <ip:port>`
   signs a fresh node certificate using the local CA key and pushes it
   (along with the CA cert) to the node over gRPC.
3. After reboot, the node-agent uses its cert to establish mTLS with the
   controller and self-registers.

The security boundary is the CA private key stored at
`~/.kcore/<context>/ca.key`. Whoever possesses it can add nodes to the
cluster.

---

## Node Lifecycle

| Area | Kubernetes | kcore |
|------|-----------|-------|
| Admission | Node CSR must be approved (automatically or manually). Node object is created only after approval. | New nodes register as `pending` and require operator approval (`kctl node approve`). Rejected nodes cannot participate. |
| Heartbeat | kubelet sends periodic leases. After `node-monitor-grace-period` (40s default) the node is marked `NotReady`. Pods get evicted after `pod-eviction-timeout`. | Heartbeat mechanism exists but there is no automatic VM migration when a node becomes unreachable. |
| Graceful removal | `kubectl drain` cordons the node and evicts pods respecting PodDisruptionBudgets. `kubectl delete node` removes the identity. | `kctl drain node` migrates VMs but there is no cordon (prevent new scheduling without draining). Removing a node from the DB does not invalidate its certificate on its own — `kctl revoke cert --node <id>` is the step that does, and it takes effect on that node's next RPC. |

---

## API Security

| Area | Kubernetes | kcore |
|------|-----------|-------|
| Authorization | RBAC with Roles, ClusterRoles, and bindings. Per-resource, per-verb granularity (e.g., "this service account can only list pods in namespace X"). | Flat operator RBAC on controller management RPCs (`read-only` < `vm-admin` < `cluster-admin`). Node and peer-controller machine identities remain CN-prefix based. |
| Audit logging | Built-in audit log pipeline recording request metadata, response codes, and actor identity. Configurable verbosity levels. | Append-only `audit_events` for mutating controller RPCs and console session opens (`AttachVmConsole`), with actor, action, resource, and timestamp; query via `ListAuditEvents` / `kctl audit`. |
| Admission control | Validating and mutating admission webhooks that can intercept any API request before persistence. | Validation happens inside gRPC handlers. No pluggable admission mechanism. |
| API versioning | Strict versioning (v1, v1beta1) with deprecation policy and conversion webhooks. | Protobuf fields are added in a backward-compatible way but there is no formal version negotiation or compatibility contract. |

---

## Secrets Management

| Area | Kubernetes | kcore |
|------|-----------|-------|
| Secrets storage | Dedicated Secret resource. Supports encryption at rest via KMS providers. Access is RBAC-scoped. | SSH public keys are stored in plaintext in SQLite. No general-purpose secrets management. |
| Workload identity | Automatic service account token mounting with short-lived projected tokens (bound service account tokens). | VMs do not receive identity tokens from the control plane. |

---

## What This Means in Practice

Most of the Kubernetes machinery described above exists because Kubernetes
is designed for **multi-tenant environments** running **untrusted
workloads**. kcore is a **single-operator bare-metal hypervisor**. The
threat model and practical priorities are different.

### Security measures already in place

- **mTLS everywhere**: all gRPC communication (controller to node, kctl to
  controller) requires mutual TLS authentication.
- **CA key stays local**: the CA private key never leaves the operator's
  machine. Only signed certificates are transmitted to nodes.
- **CN-based authorization**: machine peers use certificate Common Names
  (`kctl`, `kcore-node-*`, `kcore-controller-*`); human operators use
  `kctl:<name>` with flat RBAC roles (`read-only`, `vm-admin`,
  `cluster-admin`).
- **Node approval queue**: new nodes register as `pending` and must be
  approved by the operator (`kctl node approve <id>`) before they can
  participate in scheduling or heartbeats.
- **Sub-CA hierarchy**: a sub-CA (intermediate CA) is deployed to the
  controller and issues all leaf certificates. It is replaceable by the
  operator (`kctl rotate sub-ca`) without touching the root CA.
- **Automatic node certificate rotation, keys stay on the node**: the
  node-agent generates its own keypair and submits a CSR (`SignNodeCsr`);
  the private key never crosses the network. It checks hourly and rotates
  inside 30 days of expiry or the last 25% of the certificate lifetime,
  installs the signed chain atomically, and rebuilds its TLS listener inside
  the same process — no `systemctl restart`, no lost in-memory state such as
  live-migration bookkeeping. Established gRPC connections do close, and every
  kcore client already retries. A failed rotation leaves the previous
  certificate serving.
- **Certificate inventory**: every certificate the controller signs is
  recorded with its serial, subject, fingerprint, validity window and status
  (`active` / `rotated` / `revoked`). `kctl get certificates` and
  `kctl get pki-status` query it; it is the single source of truth behind both
  the CRL and the OCSP responder.
- **Certificate revocation (CRL and OCSP)**: `kctl revoke cert
  --serial | --node | --subject` records an RFC 5280 §5.3.1 reason code,
  regenerates the sub-CA-signed CRL, and takes effect on the very next RPC.
  The controller publishes `GET /pki/crl.der`, `GET /pki/crl.pem`,
  `POST /pki/ocsp` and `GET /pki/ocsp/<base64>` on `pki.httpListenAddr`
  (default `0.0.0.0:9092`); node-agents fetch the CRL over their existing
  mTLS gRPC channel and verify its signature before trusting it.
- **Certificate rotation commands**: `kctl rotate node-certs --node <id>`
  (or `--all`) forces node rotation; `kctl rotate certs` re-signs the
  controller certificate; `kctl rotate sub-ca` generates a new sub-CA
  and pushes it to the controller.
- **Certificate expiry visibility**: `kctl get nodes` displays days until
  each node's certificate expires with a `⚠` warning when within 30 days.
- **FIPS-compatible cryptography**: controller, node-agent, and Linux `kctl`
  TLS use `aws-lc-rs` (wrapping AWS-LC, FIPS 140-3 certificate #4816) with
  cipher suites restricted to AES-GCM + ECDHE P-256/P-384. No ChaCha20, no
  X25519. Certificate generation (`rcgen`) also uses `aws-lc-rs` for those
  binaries. macOS `kctl` release binaries use rustls/ring so they can be
  cross-compiled for Intel macOS and Apple Silicon.

### Limits of the revocation implementation

Both of these are constraints of the TLS stack rather than deliberate
omissions, and both are documented in full in
[mTLS bootstrap and authentication](./mtls-bootstrap-and-auth.md).

- **Revocation is enforced above TLS, not inside the handshake.**
  `tonic::transport::ServerTlsConfig` builds its rustls configuration
  internally and accepts no custom `ClientCertVerifier`, so rustls' own CRL
  support is unreachable. A `tonic` interceptor on every service checks the
  presented certificate's serial before any handler runs. The practical
  consequence: a revoked peer *completes* the TLS handshake and is then
  rejected with `PermissionDenied` on its first RPC, so no RPC ever succeeds
  but the connection is not refused at the transport layer. This is close to
  how Kubernetes behaves in practice, where an identity is cut off by RBAC on
  each request rather than at the TLS layer.
- **OCSP stapling is not implemented and is not reachable with this stack.**
  `tonic` exposes neither a rustls `sign::CertifiedKey` (where a server would
  attach stapled bytes) nor a `ServerCertVerifier` (where a client would
  consume them), so no stapled response is produced or consumed during a kcore
  handshake. What exists instead is a full OCSP **responder** on the
  controller, queryable by any external tool, plus a direct OCSP client in the
  node-agent used as a fallback when CRL data has gone stale. Bulk enforcement
  uses the CRL, which is the right shape for it. The kube-apiserver staples no
  OCSP response either. Lifting this needs upstream `tonic` support for
  supplying a rustls config directly, or moving the gRPC transport to `hyper` +
  `tokio-rustls`.

Enforcement is snapshot-based with a bounded staleness window
(`revocation.maxStalenessSecs`). The default is **soft-fail**: once the data is
older than that, peers absent from the last known revocation set are still
accepted and a warning is logged, because a controller outage must not lock
every node out of its own cluster. `hard-fail` inverts that trade and returns
`Unavailable` — an availability problem, not an authorization decision. A
serial already known to be revoked is rejected however stale the data is:
stale data can miss new revocations, it can never invent them.

### Planned improvements

1. **SBOM and signed releases** -- `ExportSbom` / `GetCryptoConfig` and
   signed release artifacts for regulated environments.

### Not planned (Kubernetes-specific complexity)

- **CSR approval flow** -- rotation is CSR-based (the node keypair stays on
  the node), but there is no per-CSR approval queue. `SignNodeCsr` requires an
  existing node certificate on the caller and an `approved` node record, and
  the CN and SANs it issues are derived from the address that node registered
  with rather than from anything the CSR asked for — so there is nothing for
  an operator to adjudicate. An operator approves a *node* once, not each of
  its certificates.
- **Admission webhooks** -- no plugin ecosystem to justify the abstraction.
- **API versioning** -- single codebase with all components deployed
  together; protocol compatibility is maintained through additive protobuf
  changes.
