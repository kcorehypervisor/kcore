# Host metrics

Every installed node runs Prometheus `node_exporter` on TCP **9100**. Controller
nodes also run a single-process **VictoriaMetrics** server on **8428** that
scrapes those exporters.

This is operator telemetry (CPU, memory, disk, filesystem, network, load). It
does **not** replace the [node heartbeat](./heartbeat.md): heartbeats remain the
liveness signal and the scheduler still reads `cpu_used` / `memory_used` from
`Heartbeat` / `NodeUsage` (which the agent may still report as zeros).

## Architecture

```text
  node A                 node B                 controller node
  node_exporter:9100     node_exporter:9100     node_exporter:9100
         ^                      ^                       ^
         |                      |                       |
         +---------- VictoriaMetrics :8428 -------------+
                     (file_sd from controller)
```

- **Pull, not push.** Controllers already reach every node on the cluster
  network. Nodes expose metrics; VictoriaMetrics scrapes them.
- **file_sd.** The controller rewrites
  `/etc/kcore/metrics/file_sd/nodes.json` from the SQLite node inventory about
  every 30 seconds (`metrics.refreshIntervalSecs`). New nodes appear without a
  `nixos-rebuild` of the metrics host.
- **HA.** Each controller with VictoriaMetrics scrapes the same nodes and keeps
  its own local TSDB. Query either server. Series are not replicated between
  controllers in v1.

## Ports

| Service | Port | Where | Firewall |
|---------|------|--------|----------|
| `node_exporter` | 9100 | every node | source-restricted (not on world-open `allowedTCPPorts`) |
| VictoriaMetrics | 8428 | controller nodes | open like the dashboard (8080) |
| Controller gRPC | 9090 | controller | unchanged |
| Node agent | 9091 | every node | unchanged |

## Firewall for 9100

`9100` is never added to `networking.firewall.allowedTCPPorts`. Install-time
config allows scrapes from:

1. `127.0.0.1`
2. Host IPs of `--controller` / `--run-controller` endpoints
3. The node's LAN `/24` (so peer controllers on the same subnet can scrape)

Tighten later by editing `kcore.metrics.allowedScrapeSources` in
`/etc/nixos/configuration.nix` (or a day-2 Nix fragment) and rebuilding.

## Query UI

VictoriaMetrics serves PromQL and a built-in UI (`vmui`) at
`http://<controller-ip>:8428/vmui/`.

Example:

```bash
curl -sG 'http://192.168.40.10:8428/api/v1/query' \
  --data-urlencode 'query=node_load1'
```

## NixOS module

[`modules/kcore-metrics.nix`](../modules/kcore-metrics.nix) is imported on every
installed node:

```nix
kcore.metrics = {
  enable = true;
  allowedScrapeSources = [ "127.0.0.1" "192.168.40.10" "192.168.40.0/24" ];
  server.enable = true; # controllers only
};
```

## Controller config

On controller installs, `controller.yaml` includes:

```yaml
metrics:
  enabled: true
  fileSdPath: /etc/kcore/metrics/file_sd/nodes.json
  scrapePort: 9100
  refreshIntervalSecs: 30   # optional; default 30
```

Set `metrics.enabled: false` to stop rewriting the file_sd path (VictoriaMetrics
will keep scraping the last written targets until they are removed).

## Out of scope (v1)

- Grafana (point it at `:8428` later if `vmui` is not enough)
- Guest / VM exporters
- Controller or node-agent process metrics
- Feeding the scheduler from these series
- VictoriaMetrics cluster mode (`vminsert` / `vmselect` / `vmstorage`)
