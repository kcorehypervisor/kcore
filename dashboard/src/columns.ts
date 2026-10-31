import type { Row } from "./rows";
import { isObjectArray, label, looksLikeDate } from "./format";

export type Kind = "text" | "status" | "bool" | "number" | "bytes" | "date" | "id" | "nested" | "list";

export type Column = {
  key: string;
  label: string;
  kind: Kind;
  hidden?: boolean;
  link?: string;
  /** Shown instead of a dash when the value is unset; such a column stays visible. */
  unset?: string;
};

function col(key: string, kind: Kind = "text", extra: Partial<Column> = {}): Column {
  return { key, label: extra.label ?? label(key), kind, hidden: extra.hidden, link: extra.link, unset: extra.unset };
}

export const PRESETS: Record<string, Column[]> = {
  vms: [
    col("name"),
    col("state", "status"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("cpu", "number", { label: "CPUs" }),
    col("memory_bytes", "bytes", { label: "Memory" }),
    col("storage_backend", "text", { label: "Backend" }),
    col("storage_size_bytes", "bytes", { label: "Disk" }),
    col("ha_protection", "status", { label: "HA" }),
    col("created_at", "date", { label: "Created", hidden: true }),
    col("replica_node", "text", { label: "Replica", hidden: true }),
    col("id", "id", { hidden: true }),
  ],
  workloads: [
    col("name"),
    col("kind"),
    col("state", "status"),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("cpu", "number", { label: "CPUs" }),
    col("memory_bytes", "bytes", { label: "Memory" }),
    col("created_at", "date", { label: "Created", hidden: true }),
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
    col("vm_id", "text", { label: "VM", link: "vms" }),
    col("source_node", "text", { label: "Source" }),
    col("target_node", "text", { label: "Target" }),
    col("started_at", "date", { label: "Started" }),
    col("updated_at", "date", { label: "Updated", hidden: true }),
    col("cancel_requested", "bool", { label: "Cancel requested", hidden: true }),
    col("finished_at", "date", { label: "Finished", hidden: true }),
    col("detail_json", "text", { label: "Detail", hidden: true }),
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
    col("allowed_tcp_ports", "list", { label: "TCP ports", hidden: true }),
    col("allowed_udp_ports", "list", { label: "UDP ports", hidden: true }),
    col("internal_netmask", "text", { hidden: true }),
    col("ipv6_prefix", "text", { label: "IPv6 prefix", hidden: true }),
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
    col("storage_size_bytes", "bytes", { label: "Size" }),
    col("vm_name", "text", { label: "VM", link: "vms" }),
    col("storage_backend", "text", { label: "Backend" }),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("encrypted", "bool"),
    col("mount_point", "text", { label: "Mounted at", hidden: true }),
    col("fs_type", "text", { label: "Filesystem", hidden: true }),
    col("pool", "text", { hidden: true }),
    col("image_format", "text", { label: "Format", hidden: true }),
    col("id", "id", { hidden: true }),
  ],
  snapshots: [
    col("name"),
    col("volume_name", "text", { label: "Volume", link: "volumes" }),
    col("size_bytes", "bytes", { label: "Size" }),
    col("protected", "bool"),
    col("consistency"),
    col("created_at", "date", { label: "Created" }),
    col("id", "id", { hidden: true }),
  ],
  "snapshot-policies": [
    col("name"),
    col("enabled", "bool"),
    col("schedule"),
    col("keep", "number"),
    col("selector_vm", "text", { label: "VMs" }),
    col("selector_volume", "text", { label: "Volumes" }),
    col("last_run_at", "date", { label: "Last run" }),
    col("last_message", "text", { label: "Last result", hidden: true }),
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
  "storage-classes": [col("name"), col("nodes", "number")],
  ceph: [
    col("name"),
    col("phase", "status"),
    col("health_message", "text", { label: "Health" }),
    col("size", "number", { label: "Replicas" }),
    col("min_size", "number", { label: "Min replicas" }),
    col("public_network"),
    col("encrypt_osds", "bool", { label: "Encrypted OSDs" }),
    col("nodes", "list", { hidden: true }),
    col("fsid", "id", { label: "FSID", hidden: true }),
    col("cluster_network", "text", { hidden: true }),
    col("last_transition_at", "date", { label: "Changed", hidden: true }),
  ],
  filesystems: [
    col("name"),
    col("phase", "status"),
    col("ceph_cluster", "text", { label: "Ceph", link: "ceph" }),
    col("quota_bytes", "bytes", { label: "Quota" }),
    col("clients", "list"),
    col("health_message", "text", { label: "Health", hidden: true }),
    col("fs_name", "text", { label: "CephFS name", hidden: true }),
  ],
  "ingress-gateways": [
    col("name"),
    col("phase", "status"),
    col("ready_nodes", "text", { label: "Ready nodes" }),
    col("vip_holder", "text", { label: "VIP holder", link: "nodes" }),
    col("nodes", "list", { hidden: true }),
    col("generation", "number", { hidden: true }),
    col("message", "text", { hidden: true }),
  ],
  "ingress-routes": [
    col("name"),
    col("phase", "status"),
    col("gateway", "text", { link: "ingress-gateways" }),
    col("hosts", "list"),
    col("kind"),
    col("waf_mode", "text", { label: "WAF" }),
    col("managed_by", "text", { label: "Managed by", hidden: true }),
    col("message", "text", { hidden: true }),
  ],
  "waf-policies": [
    col("name"),
    col("engine", "status"),
    col("paranoia_level", "number", { label: "Paranoia" }),
    col("generation", "number", { hidden: true }),
  ],
  "waf-events": [
    col("created_at", "date", { label: "Time" }),
    col("action", "status"),
    col("route", "text", { link: "ingress-routes" }),
    col("rule_id", "number", { label: "Rule" }),
    col("client_ip", "text", { label: "Client" }),
    col("host"),
    col("uri", "text", { label: "URI" }),
    col("severity", "text", { hidden: true }),
    col("node_id", "text", { label: "Node", hidden: true }),
    col("tx_id", "id", { hidden: true }),
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
    col("approval_status", "status", { label: "Approval" }),
    col("address"),
    col("cpu_cores", "number", { label: "Cores" }),
    col("cpu_cores_used", "number", { label: "Cores used" }),
    col("memory_bytes", "bytes", { label: "Memory" }),
    col("memory_bytes_used", "bytes", { label: "Memory used" }),
    col("kcore_version", "text", { label: "Version" }),
    col("last_heartbeat", "date", { label: "Heartbeat", hidden: true }),
    col("storage_backend", "text", { label: "Backend", hidden: true }),
    col("dc_id", "text", { label: "Datacenter", hidden: true }),
    col("luks_method", "text", { label: "LUKS", hidden: true }),
    col("cert_expiry_days", "number", { label: "Cert expires (days)", hidden: true }),
    col("labels", "list", { hidden: true }),
    col("booted_generation", "text", { label: "Booted generation", hidden: true }),
    col("node_id", "id", { label: "ID", hidden: true }),
  ],
  kubernetes: [
    col("name"),
    col("phase", "status"),
    col("cni", "text", { label: "CNI" }),
    col("api_server_endpoint", "text", { label: "API" }),
    col("control_plane_nodes", "list", { label: "Control planes" }),
    col("ready_control_planes", "number", { label: "Ready control planes" }),
    col("worker_nodes", "list", { label: "Workers" }),
    col("ready_workers", "number", { label: "Ready workers" }),
    col("message", "text", { hidden: true }),
    col("last_updated", "date", { label: "Updated", hidden: true }),
  ],
  "cluster-updates": [
    col("name"),
    col("phase", "status"),
    col("approval_status", "status", { label: "Approval" }),
    col("version"),
    col("update_kind", "text", { label: "Kind" }),
    col("flake_rev", "id", { label: "Revision" }),
    col("generation", "number", { hidden: true }),
    col("drain_vms", "bool", { label: "Drain VMs", hidden: true }),
    col("created_at", "date", { label: "Created", hidden: true }),
  ],
  conflicts: [
    col("resource_key", "text", { label: "Resource" }),
    col("reason"),
    col("incumbent_controller_id", "text", { label: "Incumbent" }),
    col("challenger_controller_id", "text", { label: "Challenger" }),
    col("id", "id", { hidden: true }),
  ],
  sessions: [
    col("kind"),
    col("current", "bool", { label: "This browser" }),
    col("created_from", "text", { label: "From" }),
    col("created_at", "date", { label: "Signed in" }),
    col("last_used_at", "date", { label: "Last used" }),
    col("expires_at", "date", { label: "Expires" }),
    col("token_id", "id", { label: "ID", hidden: true }),
  ],
  "api-tokens": [
    col("name"),
    col("operator"),
    col("max_role", "text", { label: "Max role", unset: "Operator's role" }),
    col("created_at", "date", { label: "Created" }),
    col("last_used_at", "date", { label: "Last used", unset: "Never" }),
    col("expires_at", "date", { label: "Expires" }),
    col("created_from", "text", { label: "From", hidden: true }),
    col("token_id", "id", { label: "ID", hidden: true }),
  ],
  operators: [
    col("name"),
    col("roles", "list"),
    col("cert_serial", "id", { label: "Certificate", hidden: true }),
  ],
  "ssh-keys": [col("name"), col("public_key", "id", { label: "Key" }), col("created_at", "date", { label: "Created" })],
  certificates: [
    col("subject_cn", "text", { label: "Subject" }),
    col("status", "status"),
    col("identity_kind", "text", { label: "Kind" }),
    col("node_id", "text", { label: "Node", link: "nodes" }),
    col("not_after", "date", { label: "Expires" }),
    col("days_until_expiry", "number", { label: "Days left" }),
    col("serial_hex", "id", { label: "Serial", hidden: true }),
    col("issuer_cn", "text", { label: "Issuer", hidden: true }),
    col("revocation_reason", "text", { label: "Revocation reason", hidden: true }),
    col("fingerprint_sha256", "id", { label: "Fingerprint", hidden: true }),
  ],
  "slurm-cluster": [
    col("name"),
    col("phase", "status"),
    col("controller_node", "text", { label: "Controller", link: "nodes" }),
    col("compute_count", "number", { label: "Compute" }),
    col("idle_nodes", "number", { label: "Idle" }),
    col("alloc_nodes", "number", { label: "Alloc" }),
    col("down_nodes", "number", { label: "Down" }),
  ],
  postgresql: [
    col("name"),
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
    col("profile", "text", { label: "Profile" }),
    col("profiles", "list", { label: "Profiles" }),
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
    col("kind"),
    col("format"),
    col("filename"),
    col("signature_present", "bool", { label: "Signed" }),
    col("spec_version", "text", { label: "Spec", hidden: true }),
    col("generator", "text", { hidden: true }),
    col("signing_scheme", "text", { hidden: true }),
    col("sha256", "id", { label: "SHA-256", hidden: true }),
    col("source", "text", { hidden: true }),
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

function blank(value: unknown): boolean {
  return value == null || value === "" || (Array.isArray(value) && value.length === 0);
}

export function columnsFor(id: string, rows: Row[]): Column[] {
  const present = new Set<string>();
  for (const row of rows) {
    for (const key of Object.keys(row)) present.add(key);
  }
  const empty = (key: string) => rows.length > 0 && rows.every((row) => blank(row[key]));
  const preset = PRESETS[id];
  if (preset) {
    const known = preset.filter((column) => present.has(column.key)).map((column) => (empty(column.key) && !column.unset ? { ...column, hidden: true } : column));
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
    hidden: empty(column.key) || (column.kind !== "nested" && index >= 7),
  }));
}
