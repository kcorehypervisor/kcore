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

`container` backends are accepted by the schema but refused at plan time: node
agents do not report container addresses yet. Point an `external` backend at the
container's address instead.

## Certificates

Certificates come from ACME with HTTP-01 and TLS-ALPN-01. Caddy keeps them in
`/var/lib/kcore/ingress/certmagic` on each gateway node. Hostnames are the union
of the `hosts` of the gateway's routes. Wildcard hosts need DNS-01, which is not
available yet.

## High availability

List two nodes and add `highAvailability` with a VIP that is one of the listener
addresses. keepalived holds the VIP on one node (the first listed is preferred),
and the node that holds it is shown in `kctl describe ingress-gateway`. Each
node keeps its own certificates, so after a failover the standby may need to
obtain them; shared certificate storage is not available yet.

## Limits today

- Active/active gateways, wildcard certificates, TCP routes, TLS to backends,
  Kubernetes `Ingress` objects and shared certificate storage are not built yet.
- A gateway binds a public address on a hypervisor host. The service runs as a
  dynamic user with only `CAP_NET_BIND_SERVICE`, and its admin socket is
  reachable by root only.
