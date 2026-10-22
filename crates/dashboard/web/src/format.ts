import type { Row } from "./api";

const TITLES: Record<string, string> = {
  network: "Host networking",
  storage: "Host storage",
};

/** Cluster is the same payload as Health, so the menu keeps one of them. */
export const HIDDEN_NAV = new Set(["cluster"]);

export function displayTitle(id: string, fallback: string): string {
  return TITLES[id] ?? fallback;
}

const FULL: Record<string, string> = {
  node_id: "Node",
  vm_id: "VM",
  memory_bytes: "Memory",
  storage_size_bytes: "Disk",
  cpu_used: "CPU used",
  memory_used: "Memory used",
  storage_backend: "Backend",
};

const WORDS: Record<string, string> = {
  id: "ID",
  ip: "IP",
  ha: "HA",
  vm: "VM",
  vms: "VMs",
  cpu: "CPU",
  cpus: "CPUs",
  mtu: "MTU",
  pki: "PKI",
  crl: "CRL",
  sbom: "SBOM",
  fsid: "FSID",
  osd: "OSD",
  lvm: "LVM",
  luks: "LUKS",
  nat: "NAT",
  vxlan: "VXLAN",
  tls: "TLS",
  cni: "CNI",
  api: "API",
  url: "URL",
  mac: "MAC",
};

export function label(key: string): string {
  if (FULL[key]) return FULL[key];
  return key
    .split("_")
    .map((part) => WORDS[part] ?? part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

export function isRecord(value: unknown): value is Row {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

export function isObjectArray(value: unknown): value is Row[] {
  return Array.isArray(value) && value.length > 0 && value.every(isRecord);
}

export type Tone = "ok" | "warn" | "bad" | "neutral";

const OK = /^(running|ready|healthy|active|approved|ok|up|enabled|online|available|true|protected|succeeded|complete|completed)$/i;
const BAD = /^(stopped|failed|error|rejected|down|unhealthy|expired|revoked|critical|false|lost)$/i;
const WARN = /^(paused|pending|degraded|draining|warning|expiring|unknown|creating|bootstrapping|starting|stopping|migrating|unprotected)$/i;

export function toneFor(value: string): Tone {
  if (OK.test(value)) return "ok";
  if (BAD.test(value)) return "bad";
  if (WARN.test(value)) return "warn";
  return "neutral";
}

export function toneColor(tone: Tone): string {
  if (tone === "ok") return "#10b981";
  if (tone === "bad") return "#f87171";
  if (tone === "warn") return "#f59e0b";
  return "#6366f1";
}

export function looksLikeDate(value: string): boolean {
  return /^\d{4}-\d{2}-\d{2}([T\s]\d{2}:\d{2})?/.test(value);
}

/** Relative when recent, otherwise a London wall time. */
export function formatDate(value: string): string {
  const time = Date.parse(value);
  if (Number.isNaN(time)) return value;
  const minutes = Math.round(Math.abs(Date.now() - time) / 60_000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  if (days < 14) return `${days}d ago`;
  return new Intl.DateTimeFormat("en-GB", {
    dateStyle: "medium",
    timeStyle: "short",
    timeZone: "Europe/London",
  }).format(time);
}

export function formatBytes(value: number): string {
  if (!Number.isFinite(value)) return String(value);
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let size = value;
  let index = 0;
  while (size >= 1024 && index < units.length - 1) {
    size /= 1024;
    index += 1;
  }
  const digits = size >= 10 || index === 0 ? 0 : 1;
  return `${size.toFixed(digits)} ${units[index]}`;
}

export function formatNumber(value: number): string {
  return value.toLocaleString("en-GB");
}

export function shortId(value: string): string {
  if (value.length <= 18) return value;
  return `${value.slice(0, 8)}…${value.slice(-4)}`;
}

export function text(value: unknown): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}

export function pretty(value: unknown): string {
  const rendered = JSON.stringify(value, null, 2) ?? "";
  if (rendered.length > 80_000) return `${rendered.slice(0, 80_000)}\n… truncated`;
  return rendered;
}

export function rowTitle(row: Row): string {
  for (const key of ["name", "hostname", "title", "subject", "id"]) {
    const value = text(row[key]);
    if (value) return value;
  }
  return "Details";
}

export function numberOf(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  if (typeof value !== "string") return null;
  const match = value.replace(/,/g, "").match(/-?\d+(\.\d+)?/);
  if (!match) return null;
  const parsed = Number(match[0]);
  return Number.isFinite(parsed) ? parsed : null;
}

/** Percent used when both sides are plain quantities, such as "3" of "8". */
export function usedPercent(used: unknown, total: unknown): number | null {
  if (typeof used === "string" && used.includes("%")) {
    const parsed = numberOf(used);
    return parsed === null ? null : Math.max(0, Math.min(100, parsed));
  }
  const left = numberOf(used);
  const right = numberOf(total);
  if (left === null || right === null || right <= 0) return null;
  return Math.max(0, Math.min(100, (left / right) * 100));
}
