import type { ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Bars, Gauge, Meter } from "./charts";
import type { Column } from "./columns";
import { cephCapacity, cephHealth, cephMembers, cephPools, cephTone, percent, severityTone } from "./ceph";
import { formatBytes, formatDate, formatNumber, isObjectArray, isRecord, pretty, sentence, text, toneColor, toneFor, type Tone } from "./format";
import type { Row } from "./rows";
import { DataTable, Pill, type DetailView } from "./views";

function record(value: unknown): Row {
  return isRecord(value) ? value : {};
}

function Tag({ value, tone }: { value: string; tone: Tone }) {
  return <span className={`pill ${tone}`}>{value}</span>;
}

function Item({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function Value({ value, unset = "—" }: { value: unknown; unset?: string }) {
  if (value === null || value === undefined || value === "" || (Array.isArray(value) && value.length === 0)) return <span className="muted">{unset}</span>;
  if (typeof value === "boolean") return <span className={value ? "yes" : "no"}>{value ? "Yes" : "No"}</span>;
  if (typeof value === "number") return <>{formatNumber(value)}</>;
  if (Array.isArray(value)) return <>{value.map((item) => text(item)).filter(Boolean).join(", ")}</>;
  return <>{text(value)}</>;
}

function When({ value }: { value: unknown }) {
  const raw = text(value);
  if (!raw) return <span className="muted">—</span>;
  return (
    <time dateTime={raw} title={raw}>
      {formatDate(raw)}
    </time>
  );
}

function ResourceLink({ id, q, children }: { id: string; q: string; children: ReactNode }) {
  return (
    <Link to="/r/$id" params={{ id }} search={{ q }}>
      {children}
    </Link>
  );
}

const col = (key: string, label: string, kind: Column["kind"] = "text", extra: Partial<Column> = {}): Column => ({ key, label, kind, ...extra });

const POOL_COLUMNS: Column[] = [
  col("name", "Pool"),
  col("managed", "Source"),
  col("application", "App"),
  col("pool_type", "Type"),
  col("replicas", "Size/min"),
  col("pg_num", "PGs"),
  col("stored_bytes", "Stored", "bytes"),
  col("used", "Used"),
  col("max_avail_bytes", "Max avail", "bytes"),
  col("objects", "Objects", "number"),
  col("crush_rule", "CRUSH rule"),
  col("compression_mode", "Compression"),
  col("quota_bytes", "Quota", "bytes"),
  col("erasure_profile", "EC profile"),
];

const MEMBER_COLUMNS: Column[] = [
  col("node_id", "Node", "text", { link: "nodes" }),
  col("role", "Role"),
  col("state", "State", "status", { unset: "—" }),
  col("mon_addr", "Mon address"),
  col("cluster_addr", "Cluster address"),
  col("osd_devices", "OSD devices", "list"),
  col("weight", "Weight", "number", { unset: "default" }),
  col("device_class", "Class", "text", { unset: "detect" }),
  col("rack", "Rack", "text", { unset: "—" }),
  col("room", "Room", "text", { unset: "—" }),
  col("public_iface", "Public iface"),
  col("cluster_iface", "Cluster iface"),
];

function shown(columns: Column[], data: Row[]): string[] {
  return columns.filter((column) => column.unset || data.some((row) => row[column.key] !== null && row[column.key] !== undefined && row[column.key] !== "")).map((column) => column.key);
}

function hourOf(value: unknown): string {
  return typeof value === "number" && value >= 0 && value <= 23 ? `${String(value).padStart(2, "0")}:00` : "";
}

function CephClusterDetail({ detail }: { detail: Row }) {
  const spec = record(detail.spec);
  const status = record(detail.status);
  const recovery = record(spec.recovery);
  const health = cephHealth(status);
  const capacity = cephCapacity(detail, health);
  const pools = cephPools(spec, detail.capacity);
  const members = cephMembers(spec, status);
  const name = text(detail.name);
  const phase = text(status.phase) || text(detail.phase) || "unknown";
  const operation = text(status.operation_message);
  const filesystems = Array.isArray(detail.filesystems) ? (detail.filesystems as string[]) : null;
  const stores = Array.isArray(detail.object_stores) ? (detail.object_stores as string[]) : null;
  const scrub = [hourOf(recovery.scrub_begin_hour), hourOf(recovery.scrub_end_hour)];
  const nearfull = typeof recovery.nearfull_ratio_bp === "number" && recovery.nearfull_ratio_bp > 0 ? `${recovery.nearfull_ratio_bp / 100}%` : "";
  const generation = typeof detail.generation === "number" ? detail.generation : 0;
  const observed = typeof status.observed_generation === "number" ? status.observed_generation : 0;

  return (
    <div className="stack">
      <div className={`banner ${toneFor(phase)}`}>
        <Pill value={phase} />
        <strong>{name}</strong>
        {health.status ? <Tag value={health.status} tone={cephTone(health.status)} /> : null}
        {health.flags.map((flag) => (
          <Tag key={flag} value={flag} tone="warn" />
        ))}
      </div>
      {text(status.health_message) ? <p className="muted">{sentence(text(status.health_message))}</p> : null}
      {operation ? <p className="banner warn">{sentence(operation)}</p> : null}
      {generation > observed && observed > 0 ? <p className="muted">Spec generation {generation} is not reconciled yet (observed {observed}).</p> : null}

      <section className="card stack">
        <h3>Cluster</h3>
        <dl className="facts">
          <Item name="FSID">
            <Value value={spec.fsid} unset="assigned at bootstrap" />
          </Item>
          <Item name="Public network">
            <Value value={spec.public_network} />
          </Item>
          <Item name="Cluster network">
            <Value value={spec.cluster_network} unset="public network" />
          </Item>
          <Item name="Replicas (size/min)">
            <Value value={spec.size || spec.min_size ? `${spec.size || "default"}/${spec.min_size || "default"}` : ""} unset="default" />
          </Item>
          <Item name="Encrypted OSDs">
            <Value value={Boolean(spec.encrypt_osds)} />
          </Item>
          <Item name="Messenger mode">
            <Value value={spec.ms_mode} unset="msgr2, secure not forced" />
          </Item>
          <Item name="RBD cache policy">
            <Value value={spec.cache_policy} unset="writeback" />
          </Item>
          <Item name="RBD image features">
            <Value value={spec.image_features} unset="kcore default" />
          </Item>
          <Item name="CRUSH failure domain">
            <Value value={spec.failure_domain} unset="host" />
          </Item>
          <Item name="Stretch">
            <Value value={Boolean(spec.stretch)} />
          </Item>
          <Item name="RBD mirroring">
            <Value
              value={
                record(spec.mirroring).enabled
                  ? `${text(record(spec.mirroring).mode) || "pool"} → ${text(record(spec.mirroring).peer_cluster) || "?"}`
                  : ""
              }
              unset="off"
            />
          </Item>
          <Item name="Max backfills">
            <Value value={recovery.max_backfills || ""} unset="Ceph default" />
          </Item>
          <Item name="Recovery max active">
            <Value value={recovery.recovery_max_active || ""} unset="Ceph default" />
          </Item>
          <Item name="Scrub window">
            <Value value={scrub[0] || scrub[1] ? `${scrub[0] || "any"} to ${scrub[1] || "any"}` : ""} unset="any time" />
          </Item>
          <Item name="Nearfull ratio">
            <Value value={nearfull} unset="Ceph default" />
          </Item>
          <Item name="Generation">
            <Value value={generation ? `${generation} (observed ${observed})` : ""} />
          </Item>
          <Item name="Phase changed">
            <When value={status.last_transition_at} />
          </Item>
        </dl>
      </section>

      <section className="card stack">
        <h3>Capacity</h3>
        {capacity && capacity.total > 0 ? (
          <div className="chart-layout">
            <Gauge id={`ceph-${name.replace(/[^A-Za-z0-9_-]/g, "")}`} percent={percent(capacity.used, capacity.total)} caption={`${formatBytes(capacity.used)} used`} />
            <dl className="facts">
              <Item name="Total">{formatBytes(capacity.total)}</Item>
              <Item name="Used">{formatBytes(capacity.used)}</Item>
              <Item name="Available">{formatBytes(capacity.avail)}</Item>
              <Item name="From">{capacity.source}</Item>
            </dl>
          </div>
        ) : null}
        {capacity?.classes.length ? (
          <div className="stack">
            <h3>Device classes</h3>
            {capacity.classes.map((item) => (
              <div key={item.name} className="stack">
                <Meter label={item.name} percent={percent(item.used, item.total)} />
                <span className="muted">
                  {formatBytes(item.used)} used of {formatBytes(item.total)}, {formatBytes(item.avail)} free
                </span>
              </div>
            ))}
          </div>
        ) : null}
        {capacity?.note ? <p className="muted">{sentence(capacity.note)}</p> : null}
        {!capacity ? <p className="muted">No capacity reported yet.</p> : null}
      </section>

      <section className="stack">
        <h3>Pools</h3>
        <DataTable rows={pools} columns={POOL_COLUMNS} shown={shown(POOL_COLUMNS, pools)} />
      </section>

      <section className="card stack">
        <h3>Health</h3>
        {health.source === "none" ? (
          <p className="muted">No status from Ceph yet.</p>
        ) : (
          <>
            <dl className="facts">
              <Item name="Status">{health.status ? <Tag value={health.status} tone={cephTone(health.status)} /> : <span className="muted">—</span>}</Item>
              <Item name="Mon quorum">
                {health.quorum.length ? `${health.quorum.join(", ")} (${health.quorum.length}/${health.monCount || health.quorum.length})` : <span className="muted">none</span>}
              </Item>
              <Item name="OSDs">{health.osdTotal ? `${health.osdUp} up, ${health.osdIn} in, of ${health.osdTotal}` : <span className="muted">none</span>}</Item>
              <Item name="Placement groups">{formatNumber(health.pgsTotal)}</Item>
              <Item name="PGs not clean">{formatNumber(health.pgsNotClean)}</Item>
              <Item name="PGs backfilling">{formatNumber(health.backfill)}</Item>
              <Item name="Flags">{health.flags.length ? health.flags.join(", ") : <span className="muted">none</span>}</Item>
            </dl>
            {health.pgStates.length ? (
              <Bars
                label="Placement groups by state"
                items={health.pgStates.map((item) => ({
                  label: item.state,
                  value: item.count,
                  color: toneColor(item.state === "active+clean" ? "ok" : /down|incomplete|stale|inconsistent/.test(item.state) ? "bad" : "warn"),
                }))}
              />
            ) : null}
            {health.checks.length ? (
              <ul className="alerts">
                {health.checks.map((check) => (
                  <li key={check.type}>
                    <Tag value={check.severity || "check"} tone={severityTone(check.severity)} /> <code>{check.type}</code> {check.summary}
                  </li>
                ))}
              </ul>
            ) : (
              <p className="muted">No health checks raised.</p>
            )}
            {health.source === "raw" ? <p className="muted">Parsed from the stored ceph status; the controller sent no typed health.</p> : null}
          </>
        )}
        {health.raw ? (
          <details className="advanced">
            <summary>Raw ceph status</summary>
            <pre className="code">{typeof health.raw === "string" ? health.raw : pretty(health.raw)}</pre>
          </details>
        ) : null}
      </section>

      <section className="stack">
        <h3>Members</h3>
        {members.length ? <DataTable rows={members} columns={MEMBER_COLUMNS} shown={shown(MEMBER_COLUMNS, members)} /> : <p className="muted">No members in the spec.</p>}
      </section>

      <section className="stack">
        <h3>Uses this cluster</h3>
        <div className="quick">
          <ResourceLink id="volumes" q="ceph">
            Ceph volumes
          </ResourceLink>
          <ResourceLink id="filesystems" q={name}>
            Shared filesystems{filesystems ? ` (${filesystems.length})` : ""}
          </ResourceLink>
          <ResourceLink id="object-stores" q={name}>
            Object stores{stores ? ` (${stores.length})` : ""}
          </ResourceLink>
        </div>
        {filesystems?.length || stores?.length ? <p className="muted">{[...(filesystems ?? []), ...(stores ?? [])].join(", ")}</p> : null}
      </section>
    </div>
  );
}

const CLUSTER_MEMBER_COLUMNS: Column[] = [col("node_id", "Node", "text", { link: "nodes" }), col("role", "Role"), col("mon_addr", "Mon address"), col("public_iface", "Public iface")];

/** Shared filesystems and object stores: phase, health, members, and what the reconciler said last. */
function CephServiceDetail({ detail, kind }: { detail: Row; kind: "filesystem" | "object-store" }) {
  const spec = record(detail.spec);
  const status = record(detail.status);
  const cluster = text(spec.ceph_cluster);
  const phase = text(status.phase) || text(detail.phase) || "unknown";
  const message = text(status.health_message);
  const generation = typeof detail.generation === "number" ? detail.generation : 0;
  const observed = typeof status.observed_generation === "number" ? status.observed_generation : 0;
  const declared = Array.isArray(spec.members) ? (spec.members as unknown[]).map((item) => text(item)).filter(Boolean) : [];
  const clusterMembers = isObjectArray(detail.cluster_members)
    ? detail.cluster_members.map((node) => ({ node_id: node.node_id, role: node.role || "full", mon_addr: node.mon_addr || null, public_iface: node.public_iface || null }))
    : [];
  const clients = isObjectArray(spec.clients) ? spec.clients.map((client) => ({ name: client.name, paths: client.paths })) : [];

  return (
    <div className="stack">
      <div className={`banner ${toneFor(phase)}`}>
        <Pill value={phase} />
        <strong>{text(detail.name)}</strong>
        {text(detail.cluster_phase) ? <span className="muted">Ceph cluster {text(detail.cluster_phase)}</span> : null}
      </div>
      {generation > observed && observed > 0 ? <p className="muted">Spec generation {generation} is not reconciled yet (observed {observed}).</p> : null}
      <section className="card stack">
        <h3>Last reconciler message</h3>
        {message ? <p>{sentence(message)}</p> : <p className="muted">None yet.</p>}
        <span className="muted">
          Phase changed <When value={status.last_transition_at} />
        </span>
      </section>
      <section className="card stack">
        <h3>Spec</h3>
        <dl className="facts">
          <Item name="Ceph cluster">{cluster ? <ResourceLink id="ceph" q={cluster}>{cluster}</ResourceLink> : <span className="muted">—</span>}</Item>
          {kind === "filesystem" ? (
            <>
              <Item name="CephFS name">
                <Value value={spec.fs_name} unset={text(detail.name)} />
              </Item>
              <Item name="Quota">{typeof spec.quota_bytes === "number" && spec.quota_bytes > 0 ? formatBytes(spec.quota_bytes) : <span className="muted">none</span>}</Item>
            </>
          ) : (
            <>
              <Item name="Port">
                <Value value={spec.port || ""} unset="default" />
              </Item>
              <Item name="TLS">
                <Value value={Boolean(spec.tls)} />
              </Item>
              <Item name="RGW id">
                <Value value={spec.rgw_id} unset={text(detail.name)} />
              </Item>
            </>
          )}
          <Item name="Generation">
            <Value value={generation ? `${generation} (observed ${observed})` : ""} />
          </Item>
        </dl>
      </section>
      {kind === "object-store" ? (
        <section className="card stack">
          <h3>Members</h3>
          {declared.length ? <p>{declared.join(", ")}</p> : <p className="muted">None named; the store runs on the Ceph cluster members below.</p>}
        </section>
      ) : null}
      {clients.length ? (
        <section className="stack">
          <h3>Clients</h3>
          <DataTable rows={clients} columns={[col("name", "Client"), col("paths", "Paths", "list")]} />
        </section>
      ) : null}
      <section className="stack">
        <h3>Ceph cluster members</h3>
        {clusterMembers.length ? (
          <DataTable rows={clusterMembers} columns={CLUSTER_MEMBER_COLUMNS} shown={shown(CLUSTER_MEMBER_COLUMNS, clusterMembers)} />
        ) : (
          <p className="muted">{text(detail.cluster_error) ? sentence(text(detail.cluster_error)) : "Loads with the full record."}</p>
        )}
      </section>
    </div>
  );
}

export const DETAIL_VIEWS: Record<string, DetailView> = {
  ceph: (detail) => <CephClusterDetail detail={detail} />,
  filesystems: (detail) => <CephServiceDetail detail={detail} kind="filesystem" />,
  "object-stores": (detail) => <CephServiceDetail detail={detail} kind="object-store" />,
};
