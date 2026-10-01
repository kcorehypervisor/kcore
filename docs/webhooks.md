# Webhooks and events

The controller can POST JSON to operator-configured HTTP endpoints when
something an external system would alert on actually changes. Delivery is
asynchronous: a slow or failing receiver does not fail the RPC that observed
the event.

## Events

| Type | When it fires |
|------|----------------|
| `node.heartbeat.missed` | A node in `ready` passes the 90-second heartbeat deadline and is marked `not-ready`. The next miss fires again only after a later heartbeat has marked it `ready`. |
| `cert.expiry.warning` | An active certificate is inside `certRotation.warnBeforeDays` (default 45), or a node with no inventory row reports `certExpiryDays` inside that window. Emitted once per certificate until it leaves the window. A renewed certificate emits again. |
| `vm.state.changed` | A node reports a VM runtime state that differs from the value stored for that VM. The node agent sends this report on each successful heartbeat, from the Cloud Hypervisor API sockets in `vmSocketDir`. |

Each controller delivers events it observes itself. Set `replication.controllerId`
when several controllers post to the same URL; the field is copied onto every
body so a receiver can tell the copies apart. With no replication section the
controller uses `listenAddr`.

## Configuration

Add a `webhooks` section to `controller.yaml`. All fields except `name` and
`url` are optional.

```yaml
webhooks:
  enabled: true          # false keeps the endpoints but delivers nothing
  timeoutSecs: 5         # 1-30, per attempt
  maxRetries: 3          # 0-10 extra attempts after the first
  endpoints:
    - name: pager
      url: https://hooks.example/kcore
      secret: replace-me
      events:            # omit or leave empty to receive every event
        - node.heartbeat.missed
        - cert.expiry.warning
        - vm.state.changed
      # caFile: /etc/kcore/certs/webhook-ca.crt
```

`secret` is the HMAC key. It is never written to the controller log. The file
that holds it should be readable only by the controller user, the same way the
TLS key is.

`caFile` is a PEM bundle added to the HTTPS trust store for that endpoint.
Public roots are always loaded as well. HTTP endpoints are accepted for a lab
receiver on the cluster network. Redirects are not followed: `url` must be the
final address.

A non-2xx response in the 400 range (except 408 and 429) is not retried. 408,
429, 5xx, timeouts, and connection errors are retried with a short backoff.
Endpoints for one event are delivered concurrently. A full delivery queue (256
events) drops the newest event and logs a warning.

## Body

```json
{
  "id": "5b1c1c0e-7e2a-4f1b-9c0d-6a0e0c2b9f11",
  "type": "vm.state.changed",
  "occurredAt": "2026-09-29T21:49:00Z",
  "controllerId": "kcore-controller-203.0.113.10",
  "resource": "vm/web-1",
  "data": {
    "vmId": "vm-1",
    "name": "web-1",
    "nodeId": "node-a",
    "previousState": "stopped",
    "state": "running"
  }
}
```

`node.heartbeat.missed` uses `resource` `node/{id}` and `data` fields
`nodeId`, `hostname`, `address`, `lastHeartbeat`, `timeoutSeconds`.

`cert.expiry.warning` uses `resource` `cert/{serial}` (or `node/{id}` when the
only signal is the heartbeat day count) and `data` fields `serial`, `subject`,
`nodeId`, `notAfter`, `daysUntilExpiry`, `expired`. Heartbeat-only warnings
also set `source` to `node-reported`.

## Request

```
POST {url} HTTP/1.1
Content-Type: application/json
User-Agent: kcore-controller
X-Kcore-Event: vm.state.changed
X-Kcore-Delivery: 5b1c1c0e-7e2a-4f1b-9c0d-6a0e0c2b9f11
X-Kcore-Signature: sha256=<hex>
```

`X-Kcore-Delivery` is the event `id`. Retries send the same body and the same
id. `X-Kcore-Signature` is present only when `secret` is set. It is
HMAC-SHA256 of the raw request body, hex-encoded, prefixed with `sha256=`.
Verify the bytes you received. Do not parse the JSON and serialize it again
before checking the signature.

The controller opens an outbound connection to each endpoint. Allow that path
in the host firewall. A 2xx response completes delivery; the response body is
ignored.
