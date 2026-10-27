# Ingress: Caddy load balancer with Coraza WAF: plan

Status: largely implemented on the `feat/ingress-gateway` branch; see
"Implementation status" below for what is built, what differs from this plan, and
what is not built. The sections after that are the original design.

The question this answers is "how does traffic from the internet reach a
VM, a container, or a Kubernetes service on kcore, over HTTPS, with a
certificate nobody has to manage, and with a web application firewall in
front of it?" Today the answer is a DNAT rule to a single VM port and a
certificate the guest has to obtain for itself. The plan makes the answer
a declarative `IngressRoute` that the cluster terminates, inspects, and
balances.

Phases are ordered by what an operator can verify in a lab. No calendar
estimates: each phase names the subsystems it touches and the gate that
closes it.

## Implementation status

Operator documentation is in [user/ingress.md](./user/ingress.md).

**Built**

- Phase 0: pinned `caddy.withPlugins` build (`nix build .#caddy-kcore`),
  `modules/kcore-ingress.nix` (hardened unit, firewall, VIP, scrape-source rule
  for metrics), `tests/kcore-ingress.nix` (Pebble) and host-eval coverage.
- Phase 1: migration 67, the three resources and their RPCs, `PlanIngress`,
  the renderer and reconciler, `nixgen::with_ingress`, node agent apply/status/
  secret, the WAF log tailer, `kctl` verbs, VM and external backends.
- Phase 2: Kubernetes Service backends (`loadBalancer`, `nodePort`, `pods`),
  Ingress controller mode through a dedicated `kcore-ingress` ServiceAccount.
- Phase 3: keepalived VIP, holder reporting, `ingress.vip.moved`, and the
  replicated certificate store (see below).
- Phase 4: `kctl ingress waf suggest` and `set-mode`, per-route exclusions,
  custom rules, rate limits (gateway and route), IP allow and deny lists,
  `ingress.waf.*` webhooks, event retention, dashboard section, Caddy metrics in
  the VictoriaMetrics targets.
- Phase 5: layer-4 TCP and SNI-passthrough routes, DNS-01 (Cloudflare, Route 53,
  RFC 2136) and wildcard hosts, `backendTls` with `system` or a PEM bundle, and
  PROXY protocol v2.
- MCP catalog and advise entries.

**Differences from the plan**

- Proto messages carry `spec_json` (the canonical camelCase document) instead of
  mirrored message types; the controller parses it strictly (unknown fields are
  an error) and validates it.
- The admin socket is `/run/kcore-ingress/admin.sock`.
- coraza-caddy is v2.5.0 and caddy-l4 v0.1.0: the newer releases need a Caddy
  newer than the one in nixpkgs.
- `SyncIngressStore` (streaming, agent to agent) became `ExportIngressStore` and
  `ImportIngressStore`, relayed by the controller, because node agents only
  accept calls from the controller. Only a node that holds the VIP, or already
  has every certificate, runs ACME.
- The DNS-01 credentials file is fixed (`/etc/kcore/ingress/dns.env`) and is
  written with `kctl ingress secret set` (`SetIngressSecret`).
- Gateways, routes and policies are not replicated between controllers
  (`ingress.*` replication events were not added), like `SharedFilesystem`.
- Caddy metrics listen on every address; the firewall opens the port only to
  `kcore.metrics.allowedScrapeSources`, so VictoriaMetrics can scrape it.

**Not built**

- `kind: container` backends: node agents do not report container addresses.
- NAT DHCP reservations for referenced VMs, and the gateway joining a `vxlan`
  network it is not a member of. A route to such a VM is refused with the fix.
- `tls.storage: shared-filesystem`: nodes do not mount a `SharedFilesystem`.
- `issuer: kcore-pki` and `backendTls.caBundle: kcore-pki`.
- Gateway API `HTTPRoute`.
- A Terraform provider (none exists in this repository).
- The mirrored-route status does not report the time of the last WAF block.
- A two-node failover test and the lab gates below are not run here. The
  Pebble VM test (`kcore-ingress`) and `caddy validate` of the golden
  configurations (`kcore-ingress-config`) are.

## Where we are

North-south traffic today, by workload:

| Workload | How it is reached from outside | TLS | WAF | Load balancing |
|----------|--------------------------------|-----|-----|----------------|
| VM on a `nat` network | nftables DNAT from `externalIP:hostPort` to one VM (`security-groups.md`), or `allowedTCPPorts` to the bridge gateway (`networking.md`, "DNAT targets the bridge gateway") | in the guest | none | none |
| VM on a `bridge` network | the guest has a LAN address | in the guest | none | none |
| VM on a `vxlan` network | no DNAT; the overlay is reachable only from member hosts | in the guest | none | none |
| Container workload (`WorkloadKind.CONTAINER`) | same as the VM on the same network | in the container | none | none |
| Kubernetes `Service` | Cilium L2 LoadBalancer pool on the management network (`kubernetes-networking-architecture.md`); no ingress controller, no hostname routing | in the pod | none | Cilium per-Service |

What is missing is a layer that owns the public address, terminates TLS
with ACME certificates, applies one security policy, and forwards to any
of those backends. `kubernetes-networking-architecture.md` lists ingress
as "Phase 12+". This document is that phase, widened to every workload
kcore runs.

## Target

After this plan:

> An operator applies an `IngressGateway` naming one or two nodes and a
> public address, then applies `IngressRoute`s that map hostnames and
> paths to VMs, containers, Kubernetes services, or any address. The
> gateway obtains and renews Let's Encrypt certificates, speaks HTTP/1.1,
> HTTP/2, and HTTP/3, runs the OWASP Core Rule Set through Coraza on every
> request, rate-limits, and balances across healthy upstreams. Two
> gateway nodes share a VIP and a certificate store, so a host loss costs
> seconds and no re-issuance. WAF detections are visible in `kctl`, the
> dashboard, and webhooks. A Kubernetes cluster on kcore can set
> `ingressClassName: kcore` and get all of this without installing an
> ingress controller.

## Components

- **Caddy** (`caddyserver/caddy` v2) is the data plane: TLS termination,
  ACME client (CertMagic), HTTP reverse proxy with load-balancing
  policies, active and passive health checks, and a JSON config API that
  reloads without dropping connections.
- **Coraza** (`corazawaf/coraza-caddy/v2`) is the WAF, compiled into the
  Caddy binary as an HTTP handler. It is ModSecurity-compatible and
  embeds the OWASP Core Rule Set v4 (`load_owasp_crs`, `@owasp_crs/*`),
  so no rule files are downloaded or shipped.
- **keepalived** provides the VIP for a two-node gateway, reusing the
  VRRP pattern already in `modules/kcore-kubernetes.nix` (including the
  protocol-112 conntrack bypass).
- **kcore-controller** stores gateways, routes, and WAF policies,
  resolves backends to addresses, renders the Caddy JSON, and pushes it.
- **kcore-node-agent** installs the rendered JSON through Caddy's admin
  socket, reports certificate and upstream health, and ships WAF audit
  events to the controller.
- **Nix** (`modules/kcore-ingress.nix`) owns the binary, the systemd
  unit, the firewall, keepalived, the certificate store mount, and the
  ACME account settings.

## Design decisions

### The gateway runs on the host, not in an appliance VM

`kcore.kubernetes`, `services.postgresql`, and `kcore.metrics.server` are
already host services rendered by `nixgen`. The gateway follows them. On
the host, Caddy can reach every `kbr-<net>` bridge directly: a NAT VM by
its private address with no DNAT, a VXLAN VM through the local overlay
interface, a container through its CNI veth on the same bridge, and a
Kubernetes LoadBalancer or NodePort on the management interface. An
appliance VM would need a NIC on every network it fronts and a DNAT rule
for its own public port.

The cost is attack surface on the host. Mitigations are in "Host
hardening" below: Coraza is in front, the process runs as a dynamic user
with `CAP_NET_BIND_SERVICE` only, the admin API is a root-only Unix
socket, and the firewall opens exactly the listener ports.

### Nix owns the service; the node agent owns the routing table

Route changes are frequent (every Kubernetes `Ingress`, every VM
replacement) and a `nixos-rebuild switch` per change is too slow and
contends with VM pushes. The split mirrors `kcore-firewall-<net>`, which
is reloaded without touching the bridge:

- Nix renders everything that changes rarely: package pin, unit
  hardening, listener addresses, VIP, firewall, storage mount, ACME
  account email, CA directory, DNS-01 provider, metrics exposure.
- The controller renders the Caddy JSON (`apps.http`, `apps.tls`) from
  the gateway, its routes, and the referenced WAF policies. The node agent
  writes it to `/var/lib/kcore/ingress/config.json` and `POST`s it to
  the admin socket (`/load`). Caddy swaps configuration atomically and
  drains old connections.
- On boot, `caddy run --config /var/lib/kcore/ingress/config.json` starts
  from the last applied configuration; the agent's first heartbeat
  carries `ingress_config_hash` and the controller re-pushes on drift.

Both halves are declared in the controller database and replicated, so
the result is still reproducible from the controller state.

### One policy language, two modes

WAF behaviour is a `WafPolicy` resource rendered to Coraza directives.
Every new route starts in `detect` mode (`SecRuleEngine DetectionOnly`)
so the operator sees what CRS would block before it blocks. `block` is a
one-field change on the policy or the route. There is no third
hand-written config path.

### Backends are resolved by the controller, not by DNS tricks

A route names a backend by kind and name; the controller resolves it to
addresses and re-renders when they change. `vxlan` VMs already have a
controller-assigned address. `nat` VMs get a DHCP reservation when they
are first referenced by a route (the mechanism `vm_nics` already uses for
secondary NICs), which takes effect on the guest's next lease renewal;
until then the agent resolves the newest lease, as `CheckVmSshReady`
does. Containers use the CNI result the node already reports.
Kubernetes services use the Cilium LoadBalancer address or a NodePort on
every ready worker. External backends are given as `address:port`.

### Certificates belong to the cluster, not to the node

A gateway with two nodes shares one CertMagic storage, so the pair is one
ACME "cluster": a renewal started on either node is completed by
whichever node receives the challenge, and a failover does not re-issue.
When a Ceph cluster exists, the store is a `SharedFilesystem` (CephFS)
mount. Without Ceph, only the VIP holder issues and the node agent
replicates the store to the standby over the node mTLS channel after each
change.

### Shipped with the Community release, compiled on the node

The Caddy build is `pkgs.caddy.withPlugins { plugins = [...]; hash = ...; }`
pinned in `modules/kcore-ingress-caddy.nix`. It is a fixed-output
derivation that Community nodes compile like the rest of the flake. No
binary cache is introduced (release-train rule). The pin is bumped and the
hash refreshed in the same commit as the `VERSION` change that ships it.

## Resources

Canonical YAML is camelCase. All three kinds are applied with
`kctl apply -f` and planned with `kctl ingress plan -f`.

### IngressGateway

The data plane: where it runs, what it listens on, how it gets
certificates, and its default policy.

```yaml
kind: IngressGateway
metadata:
  name: edge
spec:
  nodes:
    - node-105
    - node-107
  listeners:
    - address: 203.0.113.10         # must be a VIP or an address on the node
      ports: [80, 443]
      http3: true
  highAvailability:
    vipAddress: 203.0.113.10
    vipInterface: eno1
    virtualRouterId: 61
  tls:
    acme:
      email: ops@example.com
      ca: letsencrypt                # letsencrypt | letsencrypt-staging | zerossl | <directory URL>
      challenges: [http-01, tls-alpn-01]
      dns01:
        provider: cloudflare         # cloudflare | route53 | rfc2136
        credentialsFile: /etc/kcore/ingress/dns.env
    minVersion: "1.2"
    storage: shared-filesystem       # shared-filesystem | replicated
    storageFilesystem: ceph-shared   # SharedFilesystem name when shared-filesystem
  defaultWafPolicy: crs-default
  rateLimit:
    requestsPerMinute: 600
    key: client_ip
  kubernetes:
    ingressController: true          # watch Ingress with ingressClassName: kcore
    clusters: [lab-ha]
  accessLog: true
```

Validation refuses a listener address that is neither a VIP nor present
on every named node, a port already bound on a node (the controller's
gRPC-web listener included), `http-01` with no listener on port 80,
`dns01` without a provider, more than two nodes (active/active is out of
scope), and `shared-filesystem` on a cluster without the named
`SharedFilesystem`.

### IngressRoute

A hostname-and-path match to a set of backends.

```yaml
kind: IngressRoute
metadata:
  name: shop
spec:
  gateway: edge
  hosts: [shop.example.com, www.shop.example.com]
  paths:
    - prefix: /
      backends:
        - kind: vm
          name: shop-web-1
          port: 8080
        - kind: vm
          name: shop-web-2
          port: 8080
      loadBalancing:
        policy: least_conn            # round_robin | least_conn | ip_hash | cookie | first
        tryDuration: 5s
      healthCheck:
        path: /healthz
        interval: 10s
        expectStatus: 200
    - prefix: /api
      backends:
        - kind: kubernetes
          cluster: lab-ha
          namespace: shop
          service: api
          port: http
          endpoints: loadBalancer     # loadBalancer | nodePort | pods
    - prefix: /legacy
      backends:
        - kind: container
          name: legacy-php
          port: 80
    - prefix: /payments
      backends:
        - kind: external
          address: 192.168.40.50:8443
      backendTls:
        enabled: true
        serverName: payments.internal
        caBundle: kcore-pki            # kcore-pki | system | <PEM>
  tls:
    issuer: acme                      # acme | kcore-pki | none
  waf:
    policy: crs-default
    mode: block                       # detect | block | off; overrides the policy
    exclusions:
      - ruleId: 942100
        path: /api/search
        arg: q
  headers:
    hsts: true
    forwarded: [X-Forwarded-For, X-Forwarded-Proto, X-Real-IP]
  redirectHttp: true
```

`kind: vm`, `container`, and `kubernetes` are resolved by the controller.
`kind: external` is passed through. A `nat` backend is accepted only when
the gateway's nodes host that network (NAT networks are per host); a
route that needs a NAT VM on a third node is refused with the fix
spelled out (move the VM, use a `vxlan` network, or add an `external`
backend through that node's security-group DNAT). `issuer: kcore-pki`
signs with the cluster sub-CA for intranet hostnames. `tls.issuer: none`
is for `http`-only internal routes and is refused on a public listener.

### WafPolicy

Coraza configuration as data.

```yaml
kind: WafPolicy
metadata:
  name: crs-default
spec:
  engine: detect                      # detect | block
  crs:
    paranoiaLevel: 1
    inboundAnomalyThreshold: 5
    outboundAnomalyThreshold: 4
    allowedMethods: [GET, HEAD, POST, OPTIONS, PUT, PATCH, DELETE]
    allowedContentTypes:
      - application/json
      - application/x-www-form-urlencoded
      - multipart/form-data
  requestBodyLimitBytes: 1048576
  responseBodyInspection: false
  ipAllowList: []
  ipDenyList: []
  customRules: |
    SecRule REQUEST_HEADERS:User-Agent "@pm sqlmap nikto" "id:10001,phase:1,deny,status:403,log,msg:'scanner'"
  audit:
    engine: relevantOnly              # off | relevantOnly | on
    parts: ABCFHZ
```

Rendered to:

```
Include @coraza.conf-recommended
Include @crs-setup.conf.example
SecAction "id:900000,phase:1,pass,nolog,setvar:tx.blocking_paranoia_level=1"
SecAction "id:900110,phase:1,pass,nolog,setvar:tx.inbound_anomaly_score_threshold=5,setvar:tx.outbound_anomaly_score_threshold=4"
Include @owasp_crs/*.conf
SecRequestBodyLimit 1048576
SecRuleEngine DetectionOnly
SecAuditEngine RelevantOnly
SecAuditLogFormat JSON
SecAuditLog /var/log/kcore/ingress/waf.jsonl
<customRules>
```

Route-level `exclusions` become `SecRuleUpdateTargetById` entries placed
after the CRS includes. `customRules` are validated by compiling the
policy with Coraza at `plan` time (the controller links `coraza-caddy`'s
validation through a small Go helper, or runs `caddy validate` on the
rendered JSON on the target node); an invalid rule is refused before
anything is pushed.

## Data path

```
Internet
  │  TCP 80/443, UDP 443
  ▼
VIP 203.0.113.10 (keepalived, active node)
  │
  ▼
caddy  ── apps.http.servers.kcore
  │  1. tls: SNI → certificate (CertMagic, Let's Encrypt or kcore PKI)
  │  2. coraza_waf: policy for the matched host → detect or block
  │  3. rate_limit: per client IP
  │  4. reverse_proxy: policy, health checks, retries
  ▼
upstreams
  ├─ VM on nat     → kbr-<net> → 10.240.x.y:port       (same host, no DNAT)
  ├─ VM on vxlan   → vxlan<VNI>/kbr-<net> → 10.250.x.y  (gateway node joins the overlay)
  ├─ VM on bridge  → eno1[.vlan] → LAN address
  ├─ container     → kbr-<net> → CNI address
  ├─ kubernetes    → management NIC → Cilium LB IP, or NodePort on every ready worker
  └─ external      → address:port as given
```

Caddy JSON for the `shop` route above, abbreviated:

```json
{
  "apps": {
    "http": {
      "servers": {
        "kcore": {
          "listen": ["203.0.113.10:443"],
          "routes": [{
            "match": [{"host": ["shop.example.com", "www.shop.example.com"], "path": ["/*"]}],
            "handle": [
              {"handler": "waf", "load_owasp_crs": true, "directives": "..."},
              {"handler": "rate_limit", "rate_limits": {"client": {"key": "{client_ip}", "window": "1m", "max_events": 600}}},
              {"handler": "reverse_proxy",
               "upstreams": [{"dial": "10.240.10.102:8080"}, {"dial": "10.240.10.103:8080"}],
               "load_balancing": {"selection_policy": {"policy": "least_conn"}, "try_duration": "5s"},
               "health_checks": {"active": {"uri": "/healthz", "interval": "10s", "expect_status": 200}}}
            ]
          }]
        }
      }
    },
    "tls": {
      "automation": {
        "policies": [{
          "subjects": ["shop.example.com", "www.shop.example.com"],
          "issuers": [{"module": "acme", "email": "ops@example.com", "ca": "https://acme-v02.api.letsencrypt.org/directory"}]
        }]
      }
    }
  },
  "storage": {"module": "file_system", "root": "/var/lib/kcore/ingress/certmagic"}
}
```

The gateway node joins every `vxlan` network a route references: the
controller renders that network on the gateway node with no VMs, so the
bridge, the VXLAN interface, and the FDB entries exist and the gateway
IP answers. This is the same rendering path `networking.md` describes
for member nodes.

The east-west filter does not interfere: `networking.md` says it
"accepts established flows, traffic to and from the gateway", and the
proxy's source address on a NAT or VXLAN bridge is the gateway address.

## Kubernetes integration

Two layers, both in `crates/controller/src/kubernetes_admin.rs`, which
already holds the admin kubeconfig for clusters kcore created.

1. **Service backends.** `kind: kubernetes` resolves to the Service's
   `status.loadBalancer.ingress[].ip` (Cilium L2 pool), or to
   `nodePort` on every worker whose node is `ready`, or, when the
   gateway node is itself a node of that cluster, to pod endpoints from
   `EndpointSlices`. The controller watches Services and EndpointSlices
   for referenced backends and re-renders on change.
2. **Ingress controller mode.** With `kubernetes.ingressController: true`
   the controller watches `networking.k8s.io/v1 Ingress` objects whose
   `ingressClassName` is `kcore`, synthesises one `IngressRoute` per
   object (owner-tagged, read-only in `kctl`), and writes
   `status.loadBalancer.ingress[].ip` back with the gateway address.
   Annotations `ingress.kcore.io/waf-policy`, `ingress.kcore.io/waf-mode`,
   and `ingress.kcore.io/rate-limit` map to the route fields. TLS is
   automatic; a `spec.tls[].secretName` is honoured when present
   (`tls.issuer: secret`). The controller registers an `IngressClass`
   named `kcore` in the cluster. Gateway API `HTTPRoute` follows the same
   path once the Ingress path is proven.

Pod networks do not need to be routable from the gateway for the default
`loadBalancer` mode, so a gateway can front a cluster it is not a member
of.

## Containers and VMs

`WorkloadKind.CONTAINER` and VM backends share one resolver: look up the
workload, require it to be on a network the gateway node hosts (or a
`vxlan` network, which the gateway joins), and take its address:

| Network | Address source | Stability |
|---------|----------------|-----------|
| `vxlan` | `vms.vm_ip` / container CNI result | stable |
| `nat` | DHCP reservation created on first reference; newest lease until the guest renews | stable after one renewal |
| `bridge` | upstream DHCP; agent reads the guest address from the lease it observes, or the route declares `address` | operator-declared recommended |

A VM migration, failover, or recreate changes the node and possibly the
address; the controller's existing `vm.migrated` / `vm.state.changed`
paths trigger a re-render, and a backend on a node that can no longer be
reached is marked `unhealthy` in the route status rather than silently
dropped.

## TLS and ACME

- Default issuer is Let's Encrypt production; `kctl ingress plan` and
  the NixOS test use staging. `zerossl` and a custom directory URL are
  accepted.
- HTTP-01 and TLS-ALPN-01 by default. DNS-01 requires a provider plugin
  compiled in (`caddy-dns/cloudflare`, `caddy-dns/route53`,
  `caddy-dns/rfc2136` in the default build) and a credentials file the
  agent writes with mode 0600 outside the Nix store. Wildcard hosts are
  refused without DNS-01.
- On-demand TLS is off. Hostnames are the union of `spec.hosts` across
  routes; a hostname on two routes of one gateway is refused.
- `issuer: kcore-pki` has the controller sign a server certificate with
  the cluster sub-CA (`pki/`), rotated by `cert_rotation_reconciler`.
  Clients that trust the kcore CA can reach intranet hostnames with no
  public CA.
- Backend TLS: `backendTls.caBundle: kcore-pki` for guests enrolled in
  the kcore PKI, `system` for public CAs, or a PEM. There is no
  `insecureSkipVerify`; a backend with a self-signed certificate is
  given its PEM.
- Minimum TLS 1.2, Caddy's default cipher order. `crypto_profile.rs`
  policies that restrict ciphers (compliance report) are rendered into
  `apps.tls.cipher_suites` when set.

## WAF operations

- Coraza audit log is JSON at `/var/log/kcore/ingress/waf.jsonl`
  (`SecAuditLogFormat JSON`). The node agent tails it, batches events, and
  reports them in a new node RPC `ReportWafEvents`. The controller stores
  them in `waf_events` (bounded by `ingress.wafEventRetentionDays`,
  default 14) and exposes `ListWafEvents` with filters on gateway, route,
  rule id, client, and time.
- `kctl get waf-events --route shop --since 1h`, a dashboard panel on the
  route page (top rules, top clients, detect-vs-block counts), and
  webhooks `ingress.waf.blocked` and `ingress.waf.detected`, aggregated
  per route per minute to avoid flooding subscribers.
- Tuning loop: start in `detect`, read events, add `exclusions` by rule
  id and path, flip to `block`. `kctl ingress waf suggest --route shop`
  prints the exclusions that would have cleared the last 24 h of
  anomalies above threshold, as YAML to paste.
- The CRS version is the one embedded in the pinned `coraza-caddy`; it
  changes only with a `VERSION` bump, so a rule-set change is a cluster
  update, not a surprise.

## Observability

- Caddy metrics (`admin.metrics`) are exposed on `127.0.0.1:2020` and
  added to the controller's `file_sd` targets through
  `metrics_targets.rs`, scraped by VictoriaMetrics alongside
  `node_exporter`. Upstream health, request rate, TLS handshakes, and
  per-route latency come for free.
- Access logs are JSON to journald under the unit, with the client IP,
  host, route, upstream, status, and the Coraza transaction id
  (`tx_id_req_header` is not set; Caddy's request id is logged by both).
- Route status (`kctl get ingress-route shop`): certificate issuer,
  expiry, per-upstream health, last WAF block, config hash applied on
  each node.
- Gateway status (`kctl get ingress-gateway edge`): VIP holder, nodes
  and their applied hash, certificate store mode, ACME account.

## High availability

- Two nodes, one VIP, keepalived VRRP on `vipInterface`, priorities
  100/50, health check on the Caddy admin socket's `/config/` endpoint
  so a stopped Caddy releases the VIP. The module reuses
  `keepalivedNotrack` from `kcore-kubernetes.nix` (moved to a shared
  helper) so the masquerade does not rewrite VRRP adverts.
- Both nodes run the identical configuration; the standby answers
  nothing until it holds the VIP.
- Certificate store: `shared-filesystem` mounts the named CephFS
  `SharedFilesystem` at `/var/lib/kcore/ingress/certmagic` on both nodes.
  CertMagic's file-system storage uses lock files with heartbeats and the
  distributed solver, so either node may renew and the active node
  answers the challenge. `replicated` keeps a local directory, renders
  `apps.tls.automation.policies[].issuers` only on the VIP holder, and
  the agent syncs the directory to the standby after each write (inotify
  on the holder, `NodeAdmin.SyncIngressStore` on the standby, over node
  mTLS). The standby does not attempt issuance, so no challenge goes
  unanswered.
- Failover does not re-issue: the standby already has the certificates.
  Expected VIP move under 5 s, as with the Kubernetes API VIP.
- Active/active (ECMP, BGP, anycast) is out of scope; a second DC gets
  its own gateway and DNS does the rest.

## Host hardening

`modules/kcore-ingress.nix` sets on `kcore-ingress.service`:

- `DynamicUser=true`, `AmbientCapabilities=CAP_NET_BIND_SERVICE`,
  `CapabilityBoundingSet=CAP_NET_BIND_SERVICE`, `NoNewPrivileges=true`
- `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`,
  `PrivateDevices=true`, `ProtectKernelTunables=true`,
  `ProtectControlGroups=true`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`,
  `RestrictNamespaces=true`, `LockPersonality=true`,
  `MemoryDenyWriteExecute=true`, `SystemCallFilter=@system-service`
- `StateDirectory=kcore/ingress`, `LogsDirectory=kcore/ingress`
- Admin API on `unix//run/kcore-ingress/admin.sock` with mode 0600, owned
  by root; never on TCP. The node agent is the only client.
- Firewall: the listener ports on the listener addresses only
  (`networking.firewall.extraInputRules` keyed on destination address),
  UDP 443 when `http3` is on. `2020` (metrics) stays on loopback.
- `Restart=on-failure`, `LimitNOFILE=1048576`.
- Caddy's `servers.kcore.trusted_proxies` is empty; `X-Forwarded-*` from
  clients is stripped and rewritten.

## API and data model

### `proto/controller.proto`

```proto
rpc PlanIngressGateway(PlanIngressGatewayRequest) returns (PlanIngressGatewayResponse);
rpc CreateIngressGateway(CreateIngressGatewayRequest) returns (CreateIngressGatewayResponse);  // upsert, ApplyAction
rpc GetIngressGateway(GetIngressGatewayRequest) returns (GetIngressGatewayResponse);
rpc ListIngressGateways(ListIngressGatewaysRequest) returns (ListIngressGatewaysResponse);
rpc DeleteIngressGateway(DeleteIngressGatewayRequest) returns (DeleteIngressGatewayResponse);

rpc CreateIngressRoute(CreateIngressRouteRequest) returns (CreateIngressRouteResponse);        // upsert
rpc GetIngressRoute(GetIngressRouteRequest) returns (GetIngressRouteResponse);
rpc ListIngressRoutes(ListIngressRoutesRequest) returns (ListIngressRoutesResponse);
rpc DeleteIngressRoute(DeleteIngressRouteRequest) returns (DeleteIngressRouteResponse);

rpc CreateWafPolicy(CreateWafPolicyRequest) returns (CreateWafPolicyResponse);                // upsert, validates rules
rpc GetWafPolicy(GetWafPolicyRequest) returns (GetWafPolicyResponse);
rpc ListWafPolicies(ListWafPoliciesRequest) returns (ListWafPoliciesResponse);
rpc DeleteWafPolicy(DeleteWafPolicyRequest) returns (DeleteWafPolicyResponse);

rpc ListWafEvents(ListWafEventsRequest) returns (ListWafEventsResponse);
rpc ReportWafEvents(ReportWafEventsRequest) returns (ReportWafEventsResponse);               // node -> controller
```

`HeartbeatRequest` gains `ingress_config_hash` and `ingress_vip_holder`.
Create RPCs are idempotent upserts returning `ApplyAction` and
`changed_fields`, like `CreatePostgresql`.

### `proto/node.proto`

```proto
rpc ApplyIngressConfig(ApplyIngressConfigRequest) returns (ApplyIngressConfigResponse);      // JSON + hash; POST /load
rpc GetIngressStatus(GetIngressStatusRequest) returns (GetIngressStatusResponse);            // certs, upstreams, hash
rpc WriteIngressSecret(WriteIngressSecretRequest) returns (WriteIngressSecretResponse);      // dns.env, 0600
rpc SyncIngressStore(stream IngressStoreChunk) returns (SyncIngressStoreResponse);            // replicated mode
```

### Controller DB (`crates/controller/src/db.rs`, migration 67)

- `ingress_gateways(name PK, spec_json, status_json, created_at, updated_at)`
- `ingress_routes(name PK, gateway FK, spec_json, owner, status_json, created_at, updated_at)`
  with `owner` empty for operator routes and `k8s:<cluster>/<ns>/<ingress>` for synthesised ones
- `waf_policies(name PK, spec_json, rendered_directives, created_at, updated_at)`
- `waf_events(id, gateway, route, node_id, rule_id, severity, client_ip, host, uri, action, tx_id, raw_json, created_at)`
  indexed on `(route, created_at)` and `(rule_id, created_at)`
- `ingress_backend_reservations(vm_id, network, node_id, ip)` for NAT VMs
  referenced by routes

Specs are stored as JSON because they are nested; the columns that are
filtered on are lifted. Replication events `ingress.gateway`,
`ingress.route`, `waf.policy` follow the existing LWW resource pattern;
`waf_events` are appended locally and replicated as `waf.event`.

### Rendering (`crates/controller/src/ingress_render.rs`)

Pure function: `(gateway, routes, policies, resolved backends) → CaddyJson`,
unit-tested with golden files under `tests/fixtures/ingress/`. A second
pure function renders a `WafPolicy` to directives. `nixgen.rs` grows
`with_ingress(config, Option<&IngressNix>)` that appends the
`kcore.ingress = { ... }` block for the node, in the style of
`with_postgresql`.

### Reconciler (`crates/controller/src/ingress_reconciler.rs`)

Wakes on gateway, route, policy, VM, container, Kubernetes Service, and
node events; recomputes the config per gateway node; pushes when the hash
differs from the node's reported hash; records per-node status. One
reconciler per controller, gated by the failover lease when controllers
are replicated (same rule as `vm-ha-plan.md` Phase 5).

### RBAC

`IngressGateway` and `WafPolicy`: `cluster-admin`. `IngressRoute`:
`vm-admin`. Reads: `read-only`. All writes go through the existing audit
log.

## Nix

`modules/kcore-ingress-caddy.nix`:

```nix
{ pkgs }:
pkgs.caddy.withPlugins {
  plugins = [
    "github.com/corazawaf/coraza-caddy/v2@v2.5.0"
    "github.com/mholt/caddy-ratelimit@v0.1.0"
    "github.com/mholt/caddy-l4@v0.1.0"
    "github.com/caddy-dns/cloudflare@v0.2.4"
    "github.com/caddy-dns/route53@v1.6.2"
    "github.com/caddy-dns/rfc2136@v1.0.0"
  ];
  hash = "<refresh when the pins change>";  # real value lives in the module
}
```

`modules/kcore-ingress.nix` options under `kcore.ingress`: `enable`,
`package`, `listenAddresses`, `http3`, `vip` (`address`, `interface`,
`virtualRouterId`, `priority`), `acme` (`email`, `ca`, `dnsProvider`,
`credentialsFile`), `storage` (`mode`, `sharedFilesystemMount`),
`metricsPort`, `wafAuditLogPath`. The module is added to `flake.nix`'s
source filter and `tests/kcore-host-eval.nix` next to `kcore-kubernetes`.
`tests/kcore-ingress.nix` is a NixOS VM test: a gateway with a Pebble
ACME server, one backend, and a `WafPolicy` in `block` mode; it asserts a
certificate is obtained, `GET /` is 200, and `GET /?q=<script>` is 403.

`statix` and `deadnix` run in `nix-checks`; use `inherit (cfg) name;`
rather than aliases, per the static-checks rule.

## Operator surface

```bash
kctl ingress plan -f edge.yaml            # validates, shows rendered Caddy JSON diff, uses LE staging
kctl apply -f edge.yaml                   # IngressGateway | IngressRoute | WafPolicy
kctl get ingress-gateways
kctl get ingress-gateway edge             # nodes, VIP holder, applied hash, certs
kctl get ingress-routes [--gateway edge]
kctl get ingress-route shop               # hosts, upstream health, cert expiry, WAF mode
kctl get waf-policies
kctl get waf-events --route shop --since 1h [--rule 942100]
kctl ingress waf suggest --route shop     # exclusions from recent anomalies
kctl ingress waf set-mode --route shop block
kctl delete ingress-route shop
```

Dashboard: an "Ingress" section with gateway and route lists, a route
page showing certificate, upstreams, and the WAF event panel, and a
detect/block toggle. MCP: `kcore_catalog` gains the three kinds;
`kcore_advise` asks for the public address, ACME email, DNS provider when
a wildcard is requested, backend kinds, and whether to start in `detect`.
Terraform: `kcore_ingress_gateway`, `kcore_ingress_route`,
`kcore_waf_policy`, cut at the same `VERSION`.

## Phases

### Phase 0: package and module

1. `modules/kcore-ingress-caddy.nix` with pinned plugins and hash;
   `nix build .#caddy-kcore` works offline from the FOD.
2. `modules/kcore-ingress.nix` with the unit, hardening, firewall, and
   admin socket; `kcore.ingress.enable = false` by default.
3. `tests/kcore-ingress.nix` with Pebble; `tests/kcore-host-eval.nix`
   extended.

Gate: `nix-checks` green with the module evaluated; the VM test obtains a
certificate from Pebble and blocks an XSS probe.

### Phase 1: one gateway, HTTPS, VM and container and external backends

1. Proto, DB migration 67, RBAC rows, replication events.
2. `ingress_render.rs` and `ingress_reconciler.rs`; `nixgen::with_ingress`.
3. Node agent: `ApplyIngressConfig`, `GetIngressStatus`,
   `WriteIngressSecret`, heartbeat hash, WAF log tailer and
   `ReportWafEvents`.
4. Backend resolver for `vm` (`vxlan`, `nat` with reservation, `bridge`
   with declared address), `container`, `external`; VXLAN join on the
   gateway node.
5. `kctl ingress plan`, `kctl apply` for the three kinds, `kctl get`
   verbs, `kctl get waf-events`.
6. Docs: operator page under `docs/user/` (snapshotted by the release
   train), this plan pruned to the parts that are still future.

Gate: in the lab, a route to two NAT VMs on the gateway node obtains a
Let's Encrypt staging certificate, balances between them, and a request
with a CRS-matching payload appears in `kctl get waf-events` in `detect`
mode and is refused with 403 after `set-mode block`.

### Phase 2: Kubernetes

1. `kind: kubernetes` backends with `loadBalancer`, `nodePort`, and
   `pods` modes; Service and EndpointSlice watches.
2. Ingress controller mode: `IngressClass kcore`, Ingress watch,
   synthesised routes, status write-back, annotations.
3. Dashboard: Ingress section, route page.

Gate: `kubectl apply` of an Ingress with `ingressClassName: kcore` on a
kcore Kubernetes cluster serves over HTTPS at the gateway address within
one reconcile interval, and `kubectl get ingress` shows that address.

### Phase 3: two-node gateway

1. keepalived in the module, shared `keepalivedNotrack` helper.
2. `shared-filesystem` store on CephFS; `replicated` store with
   `SyncIngressStore` and issuer gating on the VIP holder.
3. Gateway status reports the holder; webhook `ingress.vip.moved`.
4. `tests/kcore-ingress-ha.nix`: two nodes, kill the holder, assert the
   VIP moves and the same certificate serial is served.

Gate: lab failover under 5 s, no re-issuance, `kctl get ingress-gateway`
shows the new holder.

### Phase 4: WAF operations

1. `kctl ingress waf suggest`, route-level exclusions, custom rules with
   plan-time validation.
2. Rate limiting, IP allow and deny lists.
3. WAF panel in the dashboard; `ingress.waf.*` webhooks; VictoriaMetrics
   targets for Caddy metrics.
4. Retention job for `waf_events`.

Gate: a route tuned from `detect` to `block` with `suggest` output
serves a known-good traffic replay with zero false positives at PL1.

### Phase 5: L4, wildcards, backend TLS

1. `IngressRoute.spec.kind: tcp` through `caddy-l4`: SNI passthrough and
   raw TCP (Postgres, SSH bastions, game servers) with the same backend
   resolver; no WAF on L4 routes, stated in the plan output.
2. DNS-01 providers and wildcard hosts.
3. `backendTls` with `kcore-pki` and PEM bundles; PROXY protocol v2 to
   backends that ask for it.
4. Gateway API `HTTPRoute` in ingress controller mode.

Gate: a wildcard certificate via DNS-01, a TCP route to a Postgres VM, and
a re-encrypted route to a kcore-PKI-enrolled guest all pass the lab
checks.

## Work items by subsystem

| Subsystem | Phase 0 | Phase 1 | Phase 2 | Phase 3 | Phase 4 | Phase 5 |
|-----------|---------|---------|---------|---------|---------|---------|
| `modules/` | `kcore-ingress.nix`, `kcore-ingress-caddy.nix` | | | keepalived, store mount | | l4 app |
| `tests/` (NixOS) | `kcore-ingress.nix`, host eval | | | `kcore-ingress-ha.nix` | | |
| `proto/controller.proto` | | gateway, route, policy, WAF events RPCs; heartbeat hash | | VIP holder | | |
| `proto/node.proto` | | `ApplyIngressConfig`, `GetIngressStatus`, `WriteIngressSecret`, `ReportWafEvents` | | `SyncIngressStore` | | |
| `crates/controller` `db.rs` | | migration 67 | `owner` on routes | | retention | |
| `crates/controller` render, reconciler | | `ingress_render.rs`, `ingress_reconciler.rs`, resolver, `nixgen::with_ingress` | k8s resolver, Ingress watch | holder-gated issuers | suggest, limits | l4, DNS-01, backend TLS |
| `crates/controller` `kubernetes_admin.rs` | | | Service, EndpointSlice, Ingress, IngressClass | | | HTTPRoute |
| `crates/node-agent` | | apply, status, secret, WAF tailer | | store sync, VIP report | | |
| `crates/kctl` | | plan, apply, get, waf-events | | | `waf suggest`, `set-mode` | |
| `dashboard/` | | | Ingress section | holder badge | WAF panel | |
| `kcore-mcp/` | | catalog, advise | | | | |
| `terraform-provider-kcore` | | three resources | | | | |
| docs | | `docs/user/ingress.md` | | | | |

## Risks and open points

- **Public address on the host.** The gateway binds a public address on
  a hypervisor. The hardening list is the mitigation; the alternative
  (appliance VM) is kept as a documented option if a site's policy
  forbids host exposure. Decide per site, not in the code.
- **NAT backends are per host.** A route to NAT VMs spread over several
  nodes cannot be served by one gateway without DNAT hops. The plan
  refuses it and recommends `vxlan`. If this is common in practice, add a
  controller-managed security-group DNAT as an `external` backend in a
  later phase.
- **Fixed-output hash churn.** Every plugin pin bump needs a new hash and
  a build with network. Bundle pin bumps with `VERSION` changes and make
  `nix build .#caddy-kcore` part of `docs/release.md`.
- **Coraza throughput.** CRS at PL1 costs CPU per request; Caddy's
  `rate_limit` is in front of nothing else. Measure requests per second
  on a lab node and publish the number; add `responseBodyInspection:
  false` as the default so the expensive outbound phase is opt-in.
- **Let's Encrypt rate limits.** Staging in `plan` and in tests; the
  controller refuses to render more than 50 distinct hostnames per
  gateway per week to production without `--acknowledge-ca-limits`.
- **Shared store without Ceph.** `replicated` mode has a window after a
  renewal on the holder and before the sync completes; a failover in
  that window serves the previous certificate, which is still valid for
  at least 30 days under default renewal timing. Acceptable; stated in
  the user doc.
- **Kubernetes write-back ownership.** Writing `Ingress.status` from
  outside the cluster is what any external ingress controller does, but
  it needs a dedicated ServiceAccount, not the admin kubeconfig. Create
  `kcore-ingress` with a Role limited to `ingresses/status` and
  `ingressclasses` when ingress controller mode is turned on.
- **Controller replication.** Two controllers must not both push config
  to the same gateway. The reconciler runs on the failover lease holder
  only; without replication there is one controller and no lease.
- **Community edition scope.** Nothing here needs a binary cache or a
  private channel; the Caddy build compiles on the node like every other
  package.

## Relation to other documents

- `networking.md`: the NAT, bridge, and VXLAN plumbing the gateway
  reaches into, and the "DNAT targets the bridge gateway" limitation
  this plan removes for HTTP.
- `security-groups.md`: per-VM DNAT remains for non-HTTP ports and for
  sites that do not want a proxy.
- `kubernetes-networking-architecture.md`: section 4 lists ingress as
  future work; Phase 2 here is that work, with kcore as the controller.
- `support-for-containerd.md`: container backends use the CNI address
  that plan has the node report.
- `vm-ha-plan.md`: the failover lease the ingress reconciler shares.
- `cephfs-rgw.md`: the `SharedFilesystem` the certificate store mounts.
- `host-metrics.md`: where Caddy metrics land.
- `release.md`: the pin-and-hash step added for the Caddy build.
