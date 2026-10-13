# Plan: Ceph cluster detail dashboard

Goal: clicking a Ceph cluster on the dashboard **Ceph** page opens a detail view
that surfaces as much operator-useful data as the controller already has (parity
with `kctl describe ceph-cluster`), plus capacity statistics and device-class
breakdowns with charts.

Worktree: `/mnt/data/kcore-rust-ceph-dashboard-5e55`
Branch: `cursor/ceph-dashboard-details-5e55` (from `origin/main`)

## Current state (gap)

| Surface | What exists today |
|---------|-------------------|
| Dashboard `/ceph` | Flat table: name, phase, health string, networks, `min/size`, encrypt, members, OSD device list. **No click-through.** |
| Dashboard API | `ListCephClusters` → thin `CephClusterRowDto` only. No `GetCephCluster` client. |
| Controller | `GetCephCluster` / `ListCephClusters` return full `CephCluster` including `spec` (nodes, pools, recovery knobs, FSID, encrypt) and `status.ceph_status_json` (`ceph -s`). |
| Node agent | `GetCephHealth` runs `ceph -s --format json` and returns typed fields (OSD up/in, PGs, `noout` / `norebalance`, mon quorum, summary messages) plus `raw_status`. |
| CLI | `kctl describe ceph-cluster` already prints FSID, networks, size/min, encrypt, recovery, pools, phase/health, typed lines from `ceph_status_json`, and per-node mon/cluster/OSD devices. |
| Capacity / device class | **Not collected.** `ceph -s` has cluster-level `pgmap.bytes_*`; device-class totals need `ceph df --format json` (`stats_by_class`). |

Docs already call this out: *typed Ceph health is Done (CLI); there is no dashboard view*
(`docs/storage-and-mobility-gaps.md`, P2 visibility).

## Product intent

1. **Click cluster → detail page** (`/ceph/:name`).
2. **Show controller-grade detail** — everything useful from `GetCephCluster` +
   parsed `ceph_status_json` / typed health (not a raw JSON dump as the primary UI).
3. **Replication-first** — cluster `size` / `min_size`, every pool’s size/minSize,
   CRUSH rule, application, compression, PG count; live PG state and OSD up/in;
   flags that affect replica placement (`noout`, `norebalance`).
4. **Capacity statistics** — total / used / available at cluster scope, with
   clear % used.
5. **Device class charts** — pie (or donut) of capacity by CRUSH device class
   (`hdd` / `ssd` / `nvme` / …), emphasizing **max size per class** and used vs
   free within each class.

Out of scope for this plan: mutating the cluster from the UI, CephFS/RGW detail
pages (follow-up), controller HA “replication conflicts” (already a separate
concern), live `ceph` streaming.

## Architecture

```
Browser  /ceph/:name
    → Leptos server fn get_ceph_cluster_detail_dto(name)
        → controller GetCephCluster
        → (phase B) controller GetCephCapacity / enriched status
            → node GetCephHealth (+ new df JSON)
    → CephClusterDetailDto (typed, chart-ready)
```

Prefer **typed DTOs** over shipping full `ceph_status_json` to the browser.
Reuse / extract the parse helpers already in `kctl` (`typed_health_lines`) and
`node-agent` (`parse_typed_status`, OSD/PG counters) into a shared place if
practical (`kcore-sanitize` is the wrong crate; prefer a small shared helper in
dashboard mappers first, then promote if CLI needs the same).

## Phased delivery

### Phase A — Detail page from data already on the controller (no proto change)

**Backend (dashboard crate)**

- Add `get_ceph_cluster(cfg, name)` in `controller_client.rs`.
- Add server fn `get_ceph_cluster_detail_dto(name)`.
- Add `CephClusterDetailDto` with at least:
  - Identity: name, generation, fsid, created/updated, phase, health_message
  - Networks: public / cluster
  - Replication: cluster `size` / `min_size`; `encrypt_osds`; recovery knobs
  - Pools: name, size, min_size, pg_num, crush_rule, compression_mode, application
  - Members: node_id, mon_addr, cluster_addr, ifaces, osd devices
  - Live health (parsed from `status.ceph_status_json`):
    - mon quorum
    - osds total / up / in
    - pgs total + by-state breakdown
    - flags (`noout`, `norebalance`, others of interest)
    - health check messages (severity + name + summary)
  - Cluster capacity from `pgmap` when present:
    - `bytes_total`, `bytes_used`, `bytes_avail` (+ human labels)

**UI**

- Make cluster name in `ceph_clusters_table` a link to `/ceph/{name}`.
- New route `CephClusterDetailPage`:
  - Header: name, phase badge, health status
  - Stat strip: total / used / available (+ % used bar or single donut)
  - Section **Replication**: cluster size/min + pools table
  - Section **Health**: quorum, OSDs, PG states, flags, check messages
  - Section **Members**: per-node table (addresses, devices)
  - Section **Spec**: FSID, networks, encrypt, recovery limits
- Keep list page for CephFS / RGW; detail is clusters only in this phase.
- Empty / missing cluster → clear not-found; stale JSON → show what parsed,
  mute missing capacity.

**Tests**

- Mapper unit tests with fixture `ceph -s` JSON (healthy + WARN + nearfull).
- HTTP/integration: detail server fn maps a stubbed `GetCephCluster` response.

**Exit criteria:** dashboard detail ≈ `kctl describe ceph-cluster`, plus a
cluster used/avail summary when `pgmap` bytes exist.

### Phase B — Device-class capacity (proto + agent)

`pgmap` alone cannot answer “how big is the SSD class?”. Add capacity inventory.

**Proto / agent**

- Extend `GetCephHealthResponse` **or** add `GetCephDf`:
  - raw `ceph df --format json` (or structured fields only)
  - Per device class: `class`, `total_bytes`, `used_bytes`, `avail_bytes`
  - Optional per-pool rows: name, stored, objects, %used, max_avail
- Controller: either
  - (B1) fetch df on demand when dashboard calls a new `GetCephClusterCapacity`
    RPC (fresh, no DB), or
  - (B2) store `ceph_df_json` next to `ceph_status_json` in reconciler status
    (same staleness as health; simpler for list enrichment later).

Prefer **B1 on-demand for detail** so charts stay fresh without bloating every
list poll; keep reconciler status as-is unless list page also needs mini-bars.

**Dashboard**

- `CephCapacityDto` / `DeviceClassCapacityDto`.
- Charts (CSS/SVG, no new JS chart library unless one already exists):
  1. **Cluster donut**: used vs available (and optionally reserved).
  2. **Device-class pie**: slice = class `total_bytes` (max size of that class).
  3. Optional stacked bar per class: used | avail.
- Legend with human sizes and % full; warn styling when a class is nearfull
  (from health checks or % threshold).

**Tests**

- Parse fixtures for `ceph df` with `ssd`/`hdd`/`nvme` classes and a single-class
  lab cluster.
- UI mapper tests for pie slice percentages (avoid divide-by-zero).

**Exit criteria:** detail page shows class max size pie + used/avail numbers
without SSHing to a mon.

### Phase C — Polish and cross-links (optional follow-up)

- Related volumes filtered by cluster/pool (`/volumes?q=` or dedicated section).
- Link members to Storage / Nodes pages when those routes exist.
- CephFS / RGW row click → their own detail pages (same pattern).
- List-page sparkline or % used column once capacity is cheap enough.
- Update `docs/storage-and-mobility-gaps.md` row to **Done (dashboard)**.

## UI sketch (detail page)

```
← Ceph                          lab-san          HEALTHY / HEALTH_WARN
FSID …   public …   cluster …   size 3 / min 2   encrypt yes

[ Donut used/avail ]   Total  …   Used  …   Avail  …   42%

Device classes
[ Pie by class max ]   ssd 12 TiB (max)  ·  hdd 40 TiB (max)
  ssd: 5 / 12 TiB used     hdd: 10 / 40 TiB used

Replication
  Cluster default: size=3 minSize=2
  Pools:
    kcore-vms   size=3 min=2  crush=…  app=rbd  pgs=…
    …

Health
  mon quorum: a,b,c (3/3)
  osds: 6 total, 6 up, 6 in
  pgs: 128 (128 active+clean)
  flags: (none | noout | norebalance)
  checks: …

Members
  node-103  mon=…  cluster=…  osd=/dev/disk/by-id/…
  …
```

Match existing dashboard CSS (`.hero`, `.stat`, `.data` tables, `.muted`,
`.empty-note`). Charts should be small SVG/CSS in `dashboard.css`, not a
separate SPA.

## Risks and decisions

| Topic | Recommendation |
|-------|----------------|
| Stale status | Phase A uses reconciler-cached `ceph_status_json` (same as CLI describe). Phase B on-demand df for charts. Document freshness. |
| Large JSON | Never render raw JSON as the main UI; keep optional collapsed “raw status” for debugging. |
| Replication wording | In this page, “replication” means **Ceph pool size/minSize + PG/OSD health**, not controller SQLite peer replication. |
| Multi-cluster | Route is name-keyed; one detail page per `CephCluster` name. |
| Branch base | Implement on this worktree from `main`; do not pile onto `cursor/ceph-p0-safety-64a3`. |

## Suggested implementation order

1. DTO + mapper + `GetCephCluster` server fn + unit fixtures.
2. Route `/ceph/:name` + link from table + sections without charts.
3. Cluster used/avail from `pgmap` + simple donut.
4. Phase B: `ceph df` plumbing + device-class pie.
5. Docs gap table + screenshot/walkthrough if required by release process.

## Non-goals / do not

- Do not wipe disks, install nodes, or create Ceph as part of this UI work.
- Do not add Kani harnesses for JSON parsers; use unit + proptest-style tests if
  fuzzing inputs.
- Do not call `cargo kani` from the agent shell; use `make kani-local` only if
  sanitize proofs are touched (they should not be).
