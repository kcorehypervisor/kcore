# Ingress gateway

An ingress gateway terminates HTTPS for hostnames you name, gets and renews
Let's Encrypt certificates, runs the OWASP Core Rule Set through Coraza on every
request, and balances across healthy backends. It runs on one or two kcore
nodes as Caddy.

You describe it with three resources:

| Resource | What it holds |
|----------|---------------|
| `WafPolicy` | CRS paranoia level, thresholds, IP lists, custom rules, detect or block |
| `IngressGateway` | The nodes, the public address, ACME settings, the default policy |
| `IngressRoute` | Hostnames and paths mapped to backends, load balancing, health checks |

Design notes and the phases not yet built are in
[ingress-waf-load-balancer-plan.md](../ingress-waf-load-balancer-plan.md).
Example manifests are in [examples/ingress/](../../examples/ingress/).

## Set one up

1. Create a policy. It starts in `detect`, so nothing is blocked yet.

   ```bash
   kctl apply -f examples/ingress/waf-policy.yaml
   ```

2. Create the gateway. Its listener address must exist on the node, and port
   80 must be reachable from the internet for `http-01` validation. Use
   `ca: letsencrypt-staging` until the setup works.

   ```bash
   kctl ingress plan -f examples/ingress/gateway.yaml
   kctl apply -f examples/ingress/gateway.yaml
   ```

   Applying a gateway pushes the node's NixOS configuration, which enables the
   `kcore-ingress` service. Expect a rebuild. Until the service is up the
   gateway reports `applying`.

3. Create a route.

   ```bash
   kctl ingress plan -f examples/ingress/route.yaml --show-config
   kctl apply -f examples/ingress/route.yaml
   kctl get ingress-routes
   ```

4. Check it.

   ```bash
   kctl describe ingress-gateway edge     # per node: applied hash, certificates
   kctl describe ingress-route shop       # phase, certificate, upstream health
   ```

## `kctl ingress plan`

`plan` validates the documents against the live cluster without storing them.
It overlays the files you pass on what is stored, resolves the backends, and
renders the configuration for the gateway:

```bash
kctl ingress plan -f gateway.yaml -f route.yaml [--gateway edge] [--show-config]
```

It prints errors (the command fails), warnings, and for each gateway node
whether the configuration would change. `--show-config` adds the Caddy JSON.

Common errors, and what they mean:

- `VM web is on NAT network lab of node-2, which is not a gateway node`: a NAT
  network exists on one host only. Move the VM, put it on a `vxlan` network that
  every gateway node joins, or add an `external` backend.
- `VM web has no recorded address yet`: the controller has not learned the VM's
  address. Use a static address or wait for the lease to be recorded.
- `WafPolicy crs does not exist`: create the policy first.
- `host x is used by routes a and b`: a hostname belongs to one route.

## Tune the WAF: detect first, then block

New routes inherit the policy's engine, which is `detect` by default. In
`detect` mode Coraza logs what it would block and lets the request through.

```bash
kctl get waf-events --route shop --since 1h
kctl get waf-events --route shop --rule 942100
kctl ingress waf suggest --route shop --since 24h
```

`suggest` groups recent events by rule, path and argument and prints the
`exclusions` that would have cleared them. Review each one: an exclusion turns
that rule off for that path. Add the ones you accept to the route's `spec.waf`
and apply it, then switch the route to blocking:

```bash
kctl ingress waf set-mode --route shop block      # detect | block | off
```

Events are kept for 14 days.

## Backends

| `kind` | Resolved to |
|--------|-------------|
| `vm` | The VM's recorded address and the given port |
| `external` | `address:port` exactly as written |
| `kubernetes` | A Service of a kcore Kubernetes cluster (see below) |

`container` backends are accepted by the schema but refused at plan time: node
agents do not report container addresses yet. Point an `external` backend at the
container's address instead.

## Kubernetes

A route can name a Service of a kcore Kubernetes cluster:

```yaml
backends:
  - kind: kubernetes
    cluster: lab-ha
    namespace: shop
    service: api
    port: http                 # Service port, by name or number
    endpoints: loadBalancer    # loadBalancer | nodePort | pods
```

`loadBalancer` uses the Service's external address, `nodePort` the node port on
every ready worker, and `pods` the ready addresses from its EndpointSlices (pod
IPs must be routable from the gateway). The controller resolves the Service on
every reconcile, so a changed address is picked up without touching the route.

### Ingress objects

With `kubernetes.ingressController: true` on the gateway, the gateway also
serves `Ingress` objects whose `ingressClassName` is `kcore`:

```yaml
spec:
  kubernetes:
    ingressController: true
    clusters: [lab-ha]
```

The controller creates the `kcore` IngressClass, mirrors each Ingress rule into
a read-only route (`kctl get ingress-routes` shows `owner=k8s:<cluster>/<ns>/<ingress>`),
removes the route when the Ingress goes, and writes the gateway address into
`status.loadBalancer`. Annotations:

| Annotation | Route field |
|------------|-------------|
| `ingress.kcore.io/waf-policy` | `waf.policy` |
| `ingress.kcore.io/waf-mode` | `waf.mode` (`detect`, `block`, `off`) |
| `ingress.kcore.io/rate-limit` | requests per minute per client |

Certificates come from ACME for the hosts of the rules; `spec.tls` is ignored.
A mirrored route with a problem (a Service with no address yet, a host that an
operator route already serves) is left out and shown as a warning in
`kctl ingress plan`; it never blocks the other routes.

The controller reaches the cluster as a dedicated `kcore-ingress` ServiceAccount,
not the admin kubeconfig. Its ClusterRole reads Ingresses, Services,
EndpointSlices and Nodes, and writes only `ingresses/status` and the
IngressClass. Its token lives for an hour and is held in memory.

## Certificates

Certificates come from ACME. HTTP-01 and TLS-ALPN-01 are the defaults. Caddy
keeps the certificates in `/var/lib/kcore/ingress/certmagic` on each gateway
node. Hostnames are the union of the `hosts` of the gateway's HTTP routes.

### Wildcards and DNS-01

A wildcard host such as `*.apps.example.com` needs a DNS-01 provider on the
gateway:

```yaml
spec:
  tls:
    acme:
      email: ops@example.com
      dns01:
        provider: cloudflare        # cloudflare | route53 | rfc2136
```

Give each gateway node the provider's credentials as an environment file. The
file is written with mode 0600 outside the Nix store and never logged:

```bash
printf 'CLOUDFLARE_API_TOKEN=%s\n' "$TOKEN" | kctl ingress secret set --gateway edge -f -
```

Variables read by each provider: `CLOUDFLARE_API_TOKEN`; the standard `AWS_*`
variables for route53; `RFC2136_SERVER`, `RFC2136_KEY_NAME`, `RFC2136_KEY_ALG`
and `RFC2136_KEY` for rfc2136. Wildcard names use DNS-01 and the other names
keep using the challenges the gateway lists. List only `dns-01` in
`challenges` to use DNS-01 for every name.

## TCP routes and TLS passthrough

`kind: tcp` forwards raw TCP to its backends, for Postgres, SSH bastions and
similar. Add `hosts` to match by SNI and pass TLS through untouched. The route
needs a `port` that one of the gateway's listeners carries (not 80 or 443), and
the firewall opens it on exactly those addresses. The WAF does not inspect TCP
routes and `plan` says so. See [route-tcp.yaml](../../examples/ingress/route-tcp.yaml).

## TLS to backends and the PROXY protocol

`backendTls` on a path re-encrypts to the backend and always verifies it:
`caBundle: system` uses the public CAs, or give a PEM bundle for a private CA.
There is no option to skip verification. Set `serverName` to the name in the
backend's certificate. `proxyProtocol: true` sends the PROXY protocol v2 header
to the backend, on a path or on a TCP route.

## High availability

List two nodes and add `highAvailability` with a VIP that is one of the listener
addresses. keepalived holds the VIP on one node (the first listed is preferred),
and the node that holds it is shown in `kctl describe ingress-gateway`. Each
node keeps its own certificates, so after a failover the standby may need to
obtain them; shared certificate storage is not available yet.

## Limits today

- Active/active gateways and shared certificate storage are not built yet.
  Gateway API `HTTPRoute` is not served; only `Ingress`. `backendTls` with the cluster CA (`kcore-pki`) is
  not available either: give the PEM.
- A gateway binds a public address on a hypervisor host. The service runs as a
  dynamic user with only `CAP_NET_BIND_SERVICE`, and its admin socket is
  reachable by root only.
