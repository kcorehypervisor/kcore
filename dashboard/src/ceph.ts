import type { Tone } from "./format";
import type { Row } from "./rows";

export type CephCheck = { severity: string; type: string; summary: string };

export type CephHealth = {
  status: string;
  quorum: string[];
  monCount: number;
  osdTotal: number;
  osdUp: number;
  osdIn: number;
  pgsTotal: number;
  pgsNotClean: number;
  backfill: number;
  pgStates: { state: string; count: number }[];
  flags: string[];
  checks: CephCheck[];
  bytes: { total: number; used: number; avail: number } | null;
  /** typed: the controller's CephTypedHealth; raw: parsed here from ceph_status_json. */
  source: "typed" | "raw" | "none";
  raw: unknown;
};

export type CephCapacity = {
  total: number;
  used: number;
  avail: number;
  source: string;
  classes: { name: string; total: number; used: number; avail: number }[];
  note: string;
};

function record(value: unknown): Row {
  return value && typeof value === "object" && !Array.isArray(value) ? (value as Row) : {};
}

function count(value: unknown): number {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value === "string" && value.trim() !== "" && Number.isFinite(Number(value))) return Number(value);
  return 0;
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string" && item !== "") : [];
}

function rows(value: unknown): Row[] {
  return Array.isArray(value) ? value.filter((item): item is Row => Boolean(item) && typeof item === "object" && !Array.isArray(item)) : [];
}

const WATCHED_FLAGS = ["noout", "norebalance", "nobackfill", "norecover", "noscrub", "nodeep-scrub", "pause", "noup", "nodown", "noin"];

/** `noout,norebalance flag(s) set` from the OSDMAP_FLAGS check. */
function flagsFromChecks(checks: CephCheck[]): string[] {
  const out: string[] = [];
  for (const check of checks) {
    const match = /^([\w,-]+) flag\(s\) set/.exec(check.summary);
    if (match) out.push(...match[1].split(",").filter((flag) => WATCHED_FLAGS.includes(flag)));
  }
  return out;
}

function unique(values: string[]): string[] {
  return [...new Set(values.filter(Boolean))];
}

function notClean(total: number, states: { state: string; count: number }[]): number {
  if (!states.length) return 0;
  const clean = states.filter((item) => item.state === "active+clean").reduce((sum, item) => sum + item.count, 0);
  return Math.max(0, total - clean);
}

function parseJson(raw: unknown): unknown {
  if (typeof raw !== "string" || !raw.trim()) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/** `ceph -s --format json`, for a controller that stores the blob but no typed health. */
function fromRaw(parsed: unknown): Omit<CephHealth, "source" | "raw"> | null {
  const root = record(parsed);
  if (!Object.keys(root).length) return null;
  const health = record(root.health);
  const checks = Object.entries(record(health.checks)).map(([type, value]) => {
    const check = record(value);
    return { severity: String(check.severity ?? ""), type, summary: String(record(check.summary).message ?? "") };
  });
  const osdmap = record(record(root.osdmap).osdmap ?? root.osdmap);
  const pgmap = record(root.pgmap);
  const pgStates = rows(pgmap.pgs_by_state).map((item) => ({ state: String(item.state_name ?? ""), count: count(item.count) }));
  const pgsTotal = count(pgmap.num_pgs);
  const total = count(pgmap.bytes_total);
  return {
    status: String(health.status ?? ""),
    quorum: strings(root.quorum_names),
    monCount: count(record(root.monmap).num_mons) || strings(root.quorum_names).length,
    osdTotal: count(osdmap.num_osds),
    osdUp: count(osdmap.num_up_osds),
    osdIn: count(osdmap.num_in_osds),
    pgsTotal,
    pgsNotClean: notClean(pgsTotal, pgStates),
    backfill: pgStates.filter((item) => item.state.includes("backfill")).reduce((sum, item) => sum + item.count, 0),
    pgStates,
    flags: unique(flagsFromChecks(checks)),
    checks,
    bytes: total > 0 ? { total, used: count(pgmap.bytes_used), avail: count(pgmap.bytes_avail) } : null,
  };
}

/** Health from `status`: the typed message when the controller filled it, else the raw `ceph -s` JSON. */
export function cephHealth(statusValue: unknown): CephHealth {
  const status = record(statusValue);
  const raw = parseJson(status.ceph_status_json);
  const typed = record(status.typed_health);
  if (typed.health_status || count(typed.osd_total) > 0 || count(typed.mon_count) > 0) {
    const pgStates = rows(typed.pgs_by_state).map((item) => ({ state: String(item.state ?? ""), count: count(item.count) }));
    const checks = rows(typed.checks).map((item) => ({ severity: String(item.severity ?? ""), type: String(item.type ?? ""), summary: String(item.summary ?? "") }));
    const pgsTotal = count(typed.pgs_total);
    const total = count(typed.bytes_total);
    return {
      status: String(typed.health_status ?? ""),
      quorum: strings(typed.mon_quorum),
      monCount: count(typed.mon_count),
      osdTotal: count(typed.osd_total),
      osdUp: count(typed.osd_up),
      osdIn: count(typed.osd_in),
      pgsTotal,
      pgsNotClean: count(typed.pgs_not_clean) || notClean(pgsTotal, pgStates),
      backfill: count(typed.backfill_pgs),
      pgStates,
      flags: unique([...(typed.noout ? ["noout"] : []), ...(typed.norebalance ? ["norebalance"] : []), ...strings(typed.flags)]),
      checks,
      bytes: total > 0 ? { total, used: count(typed.bytes_used), avail: count(typed.bytes_avail) } : null,
      source: "typed",
      raw,
    };
  }
  const parsed = fromRaw(raw);
  if (parsed) return { ...parsed, source: "raw", raw };
  return {
    status: "",
    quorum: [],
    monCount: 0,
    osdTotal: 0,
    osdUp: 0,
    osdIn: 0,
    pgsTotal: 0,
    pgsNotClean: 0,
    backfill: 0,
    pgStates: [],
    flags: [],
    checks: [],
    bytes: null,
    source: "none",
    raw,
  };
}

/** Capacity from GetCephClusterCapacity (`ceph df`), else the byte counters in `ceph -s`. */
export function cephCapacity(detail: Row, health: CephHealth): CephCapacity | null {
  const capacity = record(detail.capacity);
  const message = String(capacity.message ?? "") || String(detail.capacity_error ?? "");
  if (capacity.ok !== false && count(capacity.total_bytes) > 0) {
    return {
      total: count(capacity.total_bytes),
      used: count(capacity.used_bytes),
      avail: count(capacity.avail_bytes),
      source: capacity.queried_node ? `ceph df on ${String(capacity.queried_node)}` : "ceph df",
      classes: rows(capacity.by_class)
        .map((item) => ({ name: String(item.device_class || "unclassified"), total: count(item.total_bytes), used: count(item.used_bytes), avail: count(item.avail_bytes) }))
        .filter((item) => item.total > 0),
      note: capacity.ok === true ? "" : message,
    };
  }
  if (health.bytes) return { ...health.bytes, source: "ceph -s", classes: [], note: message };
  return message ? { total: 0, used: 0, avail: 0, source: "", classes: [], note: message } : null;
}

export function percent(used: number, total: number): number {
  return total > 0 ? Math.max(0, Math.min(100, (used / total) * 100)) : 0;
}

/**
 * Pools from the spec joined with `ceph df` by name. With no pools in the spec
 * the controller ensures kcore-vms at the cluster size, so that row is shown too.
 */
export function cephPools(specValue: unknown, capacityValue: unknown): Row[] {
  const spec = record(specValue);
  const usage = new Map(rows(record(capacityValue).pools).map((pool) => [String(pool.name), pool]));
  const declared = rows(spec.pools);
  const wanted = declared.length ? declared : [{ name: "kcore-vms", application: "rbd", size: spec.size, min_size: spec.min_size }];
  const out: Row[] = wanted.map((pool) => {
    const used = usage.get(String(pool.name));
    usage.delete(String(pool.name));
    return {
      name: pool.name,
      managed: declared.length ? "spec" : "default",
      application: pool.application || "rbd",
      pool_type: pool.pool_type || "replicated",
      replicas: replicas(pool.size || spec.size, pool.min_size || spec.min_size),
      pg_num: count(pool.pg_num) > 0 ? count(pool.pg_num) : "autoscale",
      crush_rule: pool.crush_rule || null,
      compression_mode: pool.compression_mode || null,
      quota_bytes: count(pool.quota_bytes) > 0 ? count(pool.quota_bytes) : null,
      erasure_profile: pool.erasure_profile || null,
      ...usageColumns(used),
    };
  });
  for (const [name, used] of usage) out.push({ name, managed: "not in spec", ...usageColumns(used) });
  return out;
}

function replicas(size: unknown, minSize: unknown): string | null {
  const left = count(size);
  const right = count(minSize);
  if (!left && !right) return null;
  return `${left || "default"}/${right || "default"}`;
}

function usageColumns(pool: Row | undefined): Row {
  if (!pool) return { stored_bytes: null, objects: null, used: null, max_avail_bytes: null };
  return {
    stored_bytes: count(pool.stored_bytes),
    objects: count(pool.objects),
    used: `${(count(pool.percent_used_bp) / 100).toFixed(1)}%`,
    max_avail_bytes: count(pool.max_avail_bytes),
  };
}

function mentions(message: string, nodeId: string): boolean {
  const escaped = nodeId.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`(^|[^\\w-])${escaped}($|[^\\w-])`).test(message);
}

/** Spec members with their drain or lost state; lost members already dropped from `nodes` stay listed. */
export function cephMembers(specValue: unknown, statusValue: unknown): Row[] {
  const spec = record(specValue);
  const operation = String(record(statusValue).operation_message ?? "");
  const lost = strings(spec.lost_members);
  const busy = (nodeId: string) => (operation && mentions(operation, nodeId) ? (/drain|remov|purg|retir/i.test(operation) ? "draining" : "in operation") : null);
  const members: Row[] = rows(spec.nodes).map((node) => {
    const nodeId = String(node.node_id ?? "");
    return {
      node_id: nodeId,
      role: node.role || "full",
      state: lost.includes(nodeId) ? "lost" : busy(nodeId),
      mon_addr: node.mon_addr || null,
      cluster_addr: node.cluster_addr || null,
      public_iface: node.public_iface || null,
      cluster_iface: node.cluster_iface || null,
      osd_devices: unique([String(node.osd_device ?? ""), ...strings(node.osd_devices)]),
      weight: count(node.weight) > 0 ? count(node.weight) : null,
      device_class: node.device_class || null,
      rack: node.rack || null,
      room: node.room || null,
    };
  });
  for (const nodeId of lost) {
    if (!members.some((member) => member.node_id === nodeId)) members.push({ node_id: nodeId, role: null, state: "lost", osd_devices: [] });
  }
  return members;
}

export function cephTone(status: string): Tone {
  if (status === "HEALTH_OK") return "ok";
  if (status === "HEALTH_WARN") return "warn";
  if (status === "HEALTH_ERR") return "bad";
  return "neutral";
}

export function severityTone(severity: string): Tone {
  return cephTone(severity);
}
