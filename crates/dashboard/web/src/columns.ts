import type { Row } from "./api";
import { isObjectArray, label, looksLikeDate } from "./format";

export type Kind = "text" | "status" | "bool" | "number" | "bytes" | "date" | "id" | "nested" | "list";

export type Column = {
  key: string;
  label: string;
  kind: Kind;
  hidden?: boolean;
  link?: string;
};

function col(key: string, kind: Kind = "text", extra: Partial<Column> = {}): Column {
  return { key, label: extra.label ?? label(key), kind, hidden: extra.hidden, link: extra.link };
}

const PRESETS: Record<string, Column[]> = {
  vms: [
    col("name"),
    col("state", "status"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("cpu", "number", { label: "CPUs" }),
    col("memory"),
    col("storage_backend", "text", { label: "Backend" }),
    col("ha", "status", { label: "HA" }),
    col("id", "id", { hidden: true }),
  ],
  workloads: [
    col("name"),
    col("kind"),
    col("state", "status"),
    col("image"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("cpu", "number", { label: "CPUs" }),
    col("memory"),
    col("id", "id", { hidden: true }),
  ],
  containers: [
    col("name"),
    col("state", "status"),
    col("image"),
    col("status", "status", { label: "Detail" }),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("id", "id", { hidden: true }),
  ],
  operations: [
    col("kind"),
    col("phase", "status"),
    col("vm", "text", { label: "VM", link: "vms" }),
    col("source_node", "text", { label: "Source" }),
    col("target_node", "text", { label: "Target" }),
    col("started_at", "date", { label: "Started" }),
    col("cancel_requested", "bool", { label: "Cancel requested", hidden: true }),
    col("finished_at", "date", { label: "Finished", hidden: true }),
    col("detail", "text", { hidden: true }),
    col("id", "id", { hidden: true }),
  ],
  networks: [
    col("name"),
    col("network_type", "text", { label: "Type" }),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("external_ip", "text", { label: "External IP" }),
    col("gateway_ip", "text", { label: "Gateway" }),
    col("vlan_id", "number", { label: "VLAN" }),
    col("enable_outbound_nat", "bool", { label: "Outbound NAT" }),
    col("east_west_firewall", "bool", { label: "East-west firewall", hidden: true }),
    col("forwarded_ports", "text", { hidden: true }),
    col("internal_netmask", "text", { hidden: true }),
  ],
  "security-groups": [
    col("name"),
    col("description"),
    col("rules", "nested"),
  ],
  volumes: [
    col("name"),
    col("role"),
    col("attach_state", "status", { label: "Attached" }),
    col("size"),
    col("vm", "text", { label: "VM", link: "vms" }),
    col("backend"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("encrypted", "bool"),
    col("pool", "text", { hidden: true }),
  ],
  snapshots: [
    col("name"),
    col("volume_name", "text", { label: "Volume", link: "volumes" }),
    col("size"),
    col("protected", "bool"),
    col("consistency"),
    col("created", "date"),
    col("id", "id", { hidden: true }),
  ],
  "snapshot-policies": [
    col("name"),
    col("enabled", "bool"),
    col("schedule"),
    col("keep", "number"),
    col("applies_to", "text", { label: "Applies to" }),
    col("last_run", "date", { label: "Last run" }),
    col("last_message", "text", { hidden: true }),
    col("selector_vm", "text", { hidden: true }),
    col("selector_volume", "text", { hidden: true }),
  ],
  "disk-layouts": [
    col("name"),
    col("phase", "status"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("generation", "number"),
    col("observed_generation", "number", { label: "Observed" }),
    col("evacuate", "bool"),
    col("message", "text", { hidden: true }),
  ],
  "storage-classes": [col("name"), col("blurb"), col("nodes", "number")],
  ceph: [
    col("name"),
    col("phase", "status"),
    col("health_message", "text", { label: "Health" }),
    col("size", "number"),
    col("min_size", "number", { label: "Min size" }),
    col("public_network"),
    col("encrypt_osds", "bool", { label: "Encrypted OSDs" }),
    col("members", "text", { hidden: true }),
    col("osd_devices", "text", { hidden: true }),
    col("cluster_network", "text", { hidden: true }),
  ],
  filesystems: [
    col("name"),
    col("phase", "status"),
    col("ceph_cluster", "text", { label: "Ceph", link: "ceph" }),
    col("quota"),
    col("clients", "number"),
    col("health_message", "text", { hidden: true }),
  ],
  "object-stores": [
    col("name"),
    col("phase", "status"),
    col("ceph_cluster", "text", { label: "Ceph", link: "ceph" }),
    col("port", "number"),
    col("tls", "bool", { label: "TLS" }),
    col("members"),
    col("health_message", "text", { hidden: true }),
  ],
  nodes: [
    col("hostname"),
    col("status", "status"),
    col("approval", "status"),
    col("address"),
    col("cpu", "text", { label: "CPU" }),
    col("memory"),
    col("cpu_used", "text", { label: "CPU used" }),
    col("memory_used"),
    col("heartbeat", "date", { hidden: true }),
    col("backend", "text", { hidden: true }),
    col("dc", "text", { label: "Datacenter", hidden: true }),
    col("luks", "text", { label: "LUKS", hidden: true }),
    col("cert", "text", { hidden: true }),
    col("labels", "text", { hidden: true }),
    col("vxlan", "text", { label: "VXLAN", hidden: true }),
    col("id", "id", { hidden: true }),
  ],
  kubernetes: [
    col("name"),
    col("phase", "status"),
    col("cni", "text", { label: "CNI" }),
    col("api_server_endpoint", "text", { label: "API" }),
    col("control_planes", "number"),
    col("ready_control_planes", "number", { label: "Ready control planes" }),
    col("workers", "number"),
    col("ready_workers", "number", { label: "Ready workers" }),
    col("message", "text", { hidden: true }),
  ],
  "cluster-updates": [
    col("name"),
    col("phase", "status"),
    col("approval", "status"),
    col("version"),
    col("update_kind", "text", { label: "Kind" }),
    col("generation", "number"),
  ],
  conflicts: [
    col("resource_key", "text", { label: "Resource" }),
    col("reason"),
    col("incumbent_controller_id", "text", { label: "Incumbent" }),
    col("challenger_controller_id", "text", { label: "Challenger" }),
    col("id", "id", { hidden: true }),
  ],
  operators: [
    col("name"),
    col("roles", "list"),
    col("cert_serial", "id", { label: "Certificate", hidden: true }),
  ],
  "ssh-keys": [col("name"), col("created", "date")],
  certificates: [
    col("subject"),
    col("status", "status"),
    col("kind"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("expires", "date"),
    col("serial", "id", { hidden: true }),
    col("issuer", "text", { hidden: true }),
    col("reason", "text", { hidden: true }),
  ],
  postgresql: [
    col("name"),
    col("engine"),
    col("database"),
    col("port", "number"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("package", "text", { hidden: true }),
  ],
  gpus: [
    col("name"),
    col("model"),
    col("assignable", "bool"),
    col("assigned_vm", "text", { label: "VM", link: "vms" }),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("address"),
    col("family", "text", { hidden: true }),
    col("role", "text", { hidden: true }),
    col("kind", "text", { hidden: true }),
  ],
  audit: [
    col("created_at", "date", { label: "When" }),
    col("actor"),
    col("action"),
    col("resource"),
    col("detail", "text", { hidden: true }),
    col("id", "id", { hidden: true }),
  ],
  sbom: [
    col("title"),
    col("kind"),
    col("signed", "bool"),
    col("format"),
    col("filename"),
    col("signing_scheme", "text", { hidden: true }),
    col("spec_version", "text", { hidden: true }),
    col("generator", "text", { hidden: true }),
    col("source", "text", { hidden: true }),
    col("error", "text", { hidden: true }),
  ],
};

function statusKey(key: string): boolean {
  return key === "approval" || key.endsWith("_state") || key.endsWith("_status") || key.endsWith("_phase") || key === "state" || key === "status" || key === "phase";
}

function infer(key: string, rows: Row[]): Column {
  let sample: unknown;
  for (const row of rows) {
    const value = row[key];
    if (value == null || value === "") continue;
    if (Array.isArray(value) && value.length === 0) continue;
    sample = value;
    break;
  }
  let kind: Kind = "text";
  if (isObjectArray(sample)) kind = "nested";
  else if (Array.isArray(sample)) kind = "list";
  else if (typeof sample === "boolean" || key === "encrypted" || key === "tls" || key === "signed" || key === "enabled") kind = "bool";
  else if (statusKey(key)) kind = "status";
  else if (key === "id" || key === "serial") kind = "id";
  else if (typeof sample === "string" && looksLikeDate(sample)) kind = "date";
  else if (typeof sample === "number" && key.endsWith("_bytes")) kind = "bytes";
  else if (typeof sample === "number") kind = "number";
  return col(key, kind);
}

function importance(column: Column): number {
  const order = ["name", "hostname", "title", "subject", "state", "status", "phase", "kind", "node_id"];
  const index = order.indexOf(column.key);
  if (column.kind === "nested") return 40;
  return index === -1 ? 20 : index;
}

export function columnsFor(id: string, rows: Row[]): Column[] {
  const present = new Set<string>();
  for (const row of rows) {
    for (const key of Object.keys(row)) present.add(key);
  }
  const preset = PRESETS[id];
  if (preset) {
    const known = preset.filter((column) => present.has(column.key));
    if (known.length >= 3) {
      const extras = [...present]
        .filter((key) => !preset.some((column) => column.key === key))
        .map((key) => {
          const column = infer(key, rows);
          return { ...column, hidden: column.kind !== "nested" };
        });
      return [...known, ...extras];
    }
  }
  const inferred = [...present].map((key) => infer(key, rows)).sort((a, b) => importance(a) - importance(b) || a.label.localeCompare(b.label));
  return inferred.map((column, index) => ({
    ...column,
    hidden: column.kind === "nested" ? false : index >= 7,
  }));
}
