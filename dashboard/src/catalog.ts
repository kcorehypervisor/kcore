import { admin, auth, controller } from "./rpc";
import { flatten, lift, rowOf, rowsOf, enumValue, type Row } from "./rows";
import type { MessageInitShape as Init } from "@bufbuild/protobuf";
import {
  CephClusterNodeSpecSchema,
  CephClusterSchema,
  ClusterUpdateSpecSchema,
  ClusterUpdateStrategyType,
  DiskLayoutSchema,
  DiskSchema,
  KubernetesClusterSpecSchema,
  KubernetesNodeSpecSchema,
  NicSchema,
  SecurityGroupRuleSchema,
  VmSpecSchema,
  OperatorRoleKindSchema,
  RevocationReason,
  SecurityGroupTargetKind,
  StorageBackendType,
  VmDesiredState,
  WorkloadDesiredState,
  WorkloadKind,
} from "./gen/controller_pb";
import {
  base64ToBytes,
  bytes,
  bytesToBase64,
  first,
  flag,
  fromRow,
  FormError,
  i64,
  ident,
  int,
  ints,
  labels,
  positiveBytes,
  records,
  required,
  strings,
  text,
  type Values,
} from "./forms";

export type FieldKind = "text" | "number" | "bool" | "json" | "secret" | "multiline" | "file" | "select";

export type Field = {
  name: string;
  label: string;
  kind: FieldKind;
  required: boolean;
  placeholder?: string;
  /** Allowed values for a select field. */
  options?: string[];
};

/** A result the UI must keep on screen: keys, tokens, and downloads. */
export type Reveal = { reveal: Record<string, string>; note?: string };
export type Download = { download: { filename: string; data: Uint8Array; type: string } };

export type Action = {
  name: string;
  label: string;
  confirm: boolean;
  fields: Field[];
  /** Load the stored spec into the form before showing it. */
  preload?: boolean;
  /** Opens the in-browser console instead of calling an RPC. */
  console?: boolean;
  run: (v: Values) => Promise<unknown>;
};

export type Detail = { detail: unknown; spec?: Values };

export type Resource = {
  id: string;
  title: string;
  /** Singular, lower case unless a proper noun: "Create SSH key". */
  noun: string;
  group: string;
  terraform: string[];
  /** The kctl command that lists this resource, without the leading `kctl`. */
  kctl: string;
  idField: string;
  fields: Field[];
  actions: Action[];
  filters?: Field[];
  /** Plan or check actions offered inside the create form. */
  checks?: string[];
  refreshMs?: number;
  /** One record, such as cluster health, shown as a summary board. */
  single?: boolean;
  list?: (filters: Values) => Promise<Row[]>;
  create?: (v: Values) => Promise<unknown>;
  inspect?: (key: string) => Promise<Detail>;
};

const pick = (name: string, label: string, options: string[], required = false): Field => ({ name, label, kind: "select", required, options });
const ROLES = ["read-only", "backup-operator", "vm-admin", "cluster-admin"];
const f = (name: string, label: string, required = false, placeholder?: string): Field => ({ name, label, kind: "text", required, placeholder });
const size = (name: string, label: string, required = false): Field => ({ name, label, kind: "text", required, placeholder: "4 GiB" });
const num = (name: string, label: string, required = false, placeholder?: string): Field => ({ name, label, kind: "number", required, placeholder });
const bool = (name: string, label: string): Field => ({ name, label, kind: "bool", required: false });
const list = (name: string, label: string, required = false): Field => ({ name, label, kind: "json", required });
const secret = (name: string, label: string, required = true): Field => ({ name, label, kind: "secret", required });
const long = (name: string, label: string, required = false): Field => ({ name, label, kind: "multiline", required });
const file = (name: string, label: string): Field => ({ name, label, kind: "file", required: true });

function act(name: string, label: string, confirm: boolean, fields: Field[], run: (v: Values) => Promise<unknown>, extra: Partial<Action> = {}): Action {
  return { name, label, confirm, fields, run, ...extra };
}

function ok(resp: { success?: boolean; message?: string }, extra: Record<string, unknown> = {}) {
  if (resp.success === false) throw new Error(resp.message || "The controller refused the request.");
  return { ok: true, ...(resp.message ? { message: resp.message } : {}), ...extra };
}


const specField = (label: string): Field => ({ name: "spec", label, kind: "multiline", required: true, placeholder: '{"hosts": ["shop.example.com"], ...}' });

/** The spec the form carries: JSON, checked here so the error shows before a round trip. */
function specJson(v: Values, allowEmpty = false): string {
  const raw = text(v, "spec");
  if (!raw && allowEmpty) return "{}";
  if (!raw) throw new FormError("Spec is required");
  try {
    const parsed: unknown = JSON.parse(raw);
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("not an object");
    return JSON.stringify(parsed);
  } catch {
    throw new FormError("Spec must be a JSON object");
  }
}

/** The stored spec of a gateway, route, or policy row, as an object. */
function parsedSpec(row: Row): unknown {
  try {
    return JSON.parse(String(row.spec_json ?? "{}"));
  } catch {
    return row.spec_json;
  }
}

function specObject(row: Row): Record<string, unknown> {
  const spec = parsedSpec(row);
  return spec && typeof spec === "object" && !Array.isArray(spec) ? (spec as Record<string, unknown>) : {};
}

function ingressGatewayRow(row: Row): Row {
  const status = (row.status ?? {}) as Row;
  const nodes = (Array.isArray(status.nodes) ? status.nodes : []) as Row[];
  return {
    ...flatten(row),
    nodes: nodes.map((n) => n.node_id),
    ready_nodes: `${nodes.filter((n) => n.healthy).length}/${nodes.length}`,
    vip_holder: nodes.find((n) => n.vip_holder)?.node_id ?? null,
  };
}

function ingressRouteRow(row: Row): Row {
  const spec = specObject(row);
  const waf = (spec.waf ?? {}) as Row;
  return {
    ...flatten(row),
    kind: spec.kind ?? "http",
    hosts: spec.hosts ?? [],
    waf_mode: waf.mode ?? "policy default",
    managed_by: row.owner || null,
  };
}

function wafPolicyRow(row: Row): Row {
  const spec = specObject(row);
  const crs = (spec.crs ?? {}) as Row;
  return { ...row, engine: spec.engine ?? "detect", paranoia_level: crs.paranoiaLevel ?? 1 };
}

function backend(name: string): StorageBackendType {
  switch (name.toLowerCase()) {
    case "filesystem":
    case "file":
      return StorageBackendType.FILESYSTEM;
    case "lvm":
      return StorageBackendType.LVM;
    case "zfs":
      return StorageBackendType.ZFS;
    case "ceph":
      return StorageBackendType.CEPH;
    default:
      throw new FormError("storage_backend must be filesystem, file, lvm, zfs, or ceph");
  }
}

function role(v: Values, key = "role"): number {
  const value = enumValue(OperatorRoleKindSchema, required(v, key));
  if (!value) throw new FormError(`${key} must be read-only, backup-operator, vm-admin, or cluster-admin`);
  return value;
}

function optionalRole(v: Values, key: string): number {
  return text(v, key) ? role(v, key) : 0;
}

function nics(v: Values): Init<typeof NicSchema>[] {
  const items = records(v, "nics")
    .map((item) => ({ network: text(item, "network"), model: text(item, "model"), macAddress: text(item, "mac_address") }))
    .filter((nic) => nic.network);
  if (items.length || !text(v, "network")) return items;
  return [{ network: text(v, "network"), model: text(v, "nic_model"), macAddress: text(v, "mac_address") }];
}

function disks(v: Values): Init<typeof DiskSchema>[] {
  return records(v, "disks")
    .map((item) => ({ name: text(item, "name"), backendHandle: text(item, "backend_handle"), bus: text(item, "bus"), device: text(item, "device") }))
    .filter((disk) => disk.name || disk.backendHandle);
}

function vmSpec(v: Values) {
  const spec: Init<typeof VmSpecSchema> & { storageBackend: string; memoryBytes: bigint; storageSizeBytes: bigint; cpu: number } = {
    name: required(v, "name"),
    cpu: int(v, "cpu"),
    memoryBytes: bytes(v, "memory_bytes"),
    storageBackend: text(v, "storage_backend"),
    storageSizeBytes: bytes(v, "storage_size_bytes"),
    desiredState: ["stopped", "stop"].includes(text(v, "desired_state")) ? VmDesiredState.STOPPED : VmDesiredState.RUNNING,
    disks: disks(v),
    nics: nics(v),
    pciDevices: strings(v, "pci_devices"),
    gpus: strings(v, "gpus"),
    labels: labels(v),
  };
  if (spec.cpu <= 0) throw new FormError("CPUs must be greater than 0");
  if (spec.memoryBytes <= 0n) throw new FormError("Memory must be greater than 0");
  if (spec.storageSizeBytes <= 0n) throw new FormError("Disk size must be greater than 0");
  backend(spec.storageBackend);
  return spec;
}

function image(v: Values) {
  const out = {
    imageUrl: text(v, "image_url"),
    imageSha256: text(v, "image_sha256"),
    imagePath: text(v, "image_path"),
    imageFormat: text(v, "image_format"),
  };
  if (!out.imagePath && !out.imageUrl) throw new FormError("Set an image URL and its SHA-256, or an image path on the node.");
  if (out.imagePath && !out.imageFormat) throw new FormError("image_format is required with image_path");
  if (out.imageUrl && !out.imageSha256) throw new FormError("image_sha256 is required with image_url");
  return out;
}

function rules(v: Values): Init<typeof SecurityGroupRuleSchema>[] {
  return records(v, "rules").map((item) => ({
    protocol: text(item, "protocol"),
    hostPort: int(item, "host_port"),
    targetPort: int(item, "target_port"),
    sourceCidr: text(item, "source_cidr"),
    targetVm: text(item, "target_vm"),
    enableDnat: flag(item, "enable_dnat"),
  }));
}

function targetKind(v: Values): SecurityGroupTargetKind {
  const kind = required(v, "target_kind").toLowerCase();
  if (kind === "vm") return SecurityGroupTargetKind.VM;
  if (kind === "network") return SecurityGroupTargetKind.NETWORK;
  throw new FormError(`target_kind must be vm or network, got ${kind}`);
}

function cephNodes(v: Values): (Init<typeof CephClusterNodeSpecSchema> & { nodeId: string })[] {
  return records(v, "nodes")
    .map((item) => ({
      nodeId: text(item, "node_id"),
      osdDevice: text(item, "osd_device"),
      osdDevices: strings(item, "osd_devices"),
      monAddr: text(item, "mon_addr"),
      clusterAddr: text(item, "cluster_addr"),
      publicIface: text(item, "public_iface"),
      clusterIface: text(item, "cluster_iface"),
    }))
    .filter((node) => node.nodeId);
}

function cephCluster(v: Values): Init<typeof CephClusterSchema> {
  const nodes = cephNodes(v);
  if (!nodes.length) throw new FormError("nodes is required");
  return {
    name: required(v, "name"),
    spec: {
      fsid: text(v, "fsid"),
      publicNetwork: required(v, "public_network"),
      clusterNetwork: text(v, "cluster_network"),
      size: int(v, "size"),
      minSize: int(v, "min_size"),
      forceWipe: flag(v, "force_wipe"),
      encryptOsds: flag(v, "encrypt_osds"),
      nodes,
    },
  };
}

function cephForm(row: Row): Values {
  const spec = (row.spec ?? {}) as Row;
  const nodes = Array.isArray(spec.nodes) ? (spec.nodes as Row[]) : [];
  return {
    name: row.name,
    fsid: spec.fsid,
    public_network: spec.public_network,
    cluster_network: spec.cluster_network,
    size: spec.size,
    min_size: spec.min_size,
    encrypt_osds: spec.encrypt_osds,
    nodes: nodes.map((node) => ({ ...node, osd_devices: Array.isArray(node.osd_devices) ? node.osd_devices.join(",") : "" })),
  };
}

function k8sNode(item: Values, defaultRole: string) {
  const nodeId = text(item, "node_id");
  if (!nodeId) throw new FormError("node_id is required on each Kubernetes node");
  const node: Init<typeof KubernetesNodeSpecSchema> & { initialCluster: boolean } = {
    nodeId,
    role: text(item, "role") || defaultRole,
    advertiseAddress: text(item, "advertise_address"),
    iface: text(item, "iface"),
    initialCluster: flag(item, "initial_cluster"),
    joinExisting: flag(item, "join_existing"),
  };
  if (!node.advertiseAddress || !node.iface) throw new FormError(`${nodeId} needs advertise_address and iface`);
  return node;
}

function k8sSpec(v: Values): Init<typeof KubernetesClusterSpecSchema> {
  const control = records(v, "control_plane_nodes").map((item) => k8sNode(item, "control-plane"));
  if (!control.length) throw new FormError("control_plane_nodes is required; a blank form would replace the cluster spec");
  if (!control.some((node) => node.initialCluster)) control[0].initialCluster = true;
  const ha = flag(v, "ha_enabled") || Boolean(text(v, "vip_address"));
  if (ha && (!text(v, "vip_address") || !text(v, "vip_interface"))) {
    throw new FormError("high availability needs vip_address and vip_interface");
  }
  return {
    clusterCidr: required(v, "cluster_cidr"),
    serviceCidr: required(v, "service_cidr"),
    clusterDns: text(v, "cluster_dns"),
    apiServerEndpoint: required(v, "api_server_endpoint"),
    cni: text(v, "cni") || "cilium",
    workerNodes: records(v, "worker_nodes").map((item) => k8sNode(item, "worker")),
    controlPlaneNodes: control,
    scheduleOnControlPlane: flag(v, "schedule_on_control_plane"),
    highAvailability: ha
      ? { enabled: true, vipAddress: text(v, "vip_address"), vipInterface: text(v, "vip_interface"), method: text(v, "method") || "keepalived" }
      : undefined,
  };
}

function k8sForm(row: Row): Values {
  const spec = (row.spec ?? {}) as Row;
  const ha = (spec.high_availability ?? {}) as Row;
  return {
    name: row.name,
    cluster_cidr: spec.cluster_cidr,
    service_cidr: spec.service_cidr,
    cluster_dns: spec.cluster_dns,
    api_server_endpoint: spec.api_server_endpoint,
    cni: spec.cni,
    schedule_on_control_plane: spec.schedule_on_control_plane,
    control_plane_nodes: spec.control_plane_nodes ?? [],
    worker_nodes: spec.worker_nodes ?? [],
    ha_enabled: ha.enabled,
    vip_address: ha.vip_address,
    vip_interface: ha.vip_interface,
    method: ha.method,
  };
}

function postedK8s(v: Values): boolean {
  return !fromRow(v) && (Boolean(text(v, "cluster_cidr")) || records(v, "control_plane_nodes").length > 0);
}

function updateSpec(v: Values): Init<typeof ClusterUpdateSpecSchema> {
  return {
    name: required(v, "name"),
    target: { version: text(v, "version"), flakeRef: text(v, "flake_ref"), flakeRev: text(v, "flake_rev") },
    selector: { allNodes: flag(v, "all_nodes"), nodeIds: strings(v, "node_ids") },
    strategy: { type: ClusterUpdateStrategyType.CLUSTER_UPDATE_STRATEGY_ONE_AT_A_TIME, maxUnavailable: 1 },
    updateKind: text(v, "update_kind") || "kcore",
    drainVms: flag(v, "drain_vms"),
  };
}

function revocationReason(v: Values): RevocationReason {
  switch (text(v, "reason")) {
    case "keyCompromise":
    case "key-compromise":
      return RevocationReason.KEY_COMPROMISE;
    case "cACompromise":
    case "ca-compromise":
      return RevocationReason.CA_COMPROMISE;
    case "affiliationChanged":
      return RevocationReason.AFFILIATION_CHANGED;
    case "superseded":
      return RevocationReason.SUPERSEDED;
    case "cessationOfOperation":
      return RevocationReason.CESSATION_OF_OPERATION;
    case "certificateHold":
      return RevocationReason.CERTIFICATE_HOLD;
    default:
      return RevocationReason.UNSPECIFIED;
  }
}

function workloadKind(resource: string, v: Values): WorkloadKind {
  return resource === "containers" || text(v, "kind") === "container" ? WorkloadKind.CONTAINER : WorkloadKind.VM;
}

async function createWorkload(v: Values, container: boolean) {
  if (container) {
    const resp = await controller.createWorkload({
      kind: WorkloadKind.CONTAINER,
      targetNode: text(v, "target_node"),
      containerSpec: {
        name: required(v, "name"),
        image: required(v, "image"),
        network: text(v, "network"),
        command: strings(v, "command"),
        ports: strings(v, "ports"),
        storageBackend: text(v, "storage_backend"),
        storageSizeBytes: bytes(v, "storage_size_bytes"),
        mountTarget: text(v, "mount_target"),
        desiredState: text(v, "desired_state") === "stopped" ? WorkloadDesiredState.STOPPED : WorkloadDesiredState.RUNNING,
      },
    });
    return { ok: true, id: resp.workloadId, node: resp.nodeId };
  }
  const spec = vmSpec(v);
  const resp = await controller.createWorkload({
    kind: WorkloadKind.VM,
    targetNode: text(v, "target_node"),
    vmSpec: spec,
    storageBackend: backend(spec.storageBackend),
    storageSizeBytes: spec.storageSizeBytes,
    sshKeyNames: strings(v, "ssh_key_names"),
    ...image(v),
  });
  return { ok: true, id: resp.workloadId, node: resp.nodeId };
}

function workloadActions(resource: string): Action[] {
  const set = (running: boolean) => async (v: Values) => {
    await controller.setWorkloadDesiredState({
      kind: workloadKind(resource, v),
      workloadId: first(v, "id", "name"),
      desiredState: running ? WorkloadDesiredState.RUNNING : WorkloadDesiredState.STOPPED,
      targetNode: text(v, "target_node"),
    });
    return { ok: true };
  };
  return [
    act("start", "Start", false, [], set(true)),
    act("stop", "Stop", false, [], set(false)),
    act("delete", "Delete", true, [], async (v) => {
      await controller.deleteWorkload({ kind: workloadKind(resource, v), workloadId: first(v, "id", "name"), targetNode: text(v, "target_node") });
      return { ok: true };
    }),
  ];
}

async function workloadRows(): Promise<Row[]> {
  const resp = await controller.listWorkloads({});
  return [...rowsOf(resp, "vms").map((row) => ({ ...row, kind: "vm" })), ...rowsOf(resp, "containers").map((row) => ({ ...row, kind: "container" }))];
}

function nodeRow(row: Row): Row {
  const capacity = (row.capacity ?? {}) as Row;
  const usage = (row.usage ?? {}) as Row;
  return {
    ...row,
    cpu_cores: capacity.cpu_cores,
    memory_bytes: capacity.memory_bytes,
    cpu_cores_used: usage.cpu_cores_used,
    memory_bytes_used: usage.memory_bytes_used,
  };
}

const vmFields: Field[] = [
  f("name", "Name", true),
  num("cpu", "CPUs", true),
  size("memory_bytes", "Memory", true),
  pick("storage_backend", "Storage backend", ["filesystem", "lvm", "zfs", "ceph"], true),
  size("storage_size_bytes", "Disk size", true),
  f("image_url", "Image URL"),
  f("image_sha256", "Image SHA-256"),
  f("image_path", "Image path"),
  pick("image_format", "Image format", ["raw", "qcow2"]),
  f("target_node", "Target node"),
  f("target_dc", "Target datacenter"),
  f("network", "Network"),
  list("nics", "Network cards"),
  list("disks", "Extra disks"),
  f("ssh_key_names", "SSH key names", false, "comma separated"),
  long("cloud_init_user_data", "Cloud-init user data"),
  f("anti_affinity", "Anti-affinity group"),
  f("labels", "Labels", false, "key=value, comma separated"),
  pick("desired_state", "Desired state", ["running", "stopped"]),
];

const k8sFields: Field[] = [
  f("cluster_cidr", "Pod CIDR", true),
  f("service_cidr", "Service CIDR", true),
  f("cluster_dns", "Cluster DNS"),
  f("api_server_endpoint", "API endpoint", true),
  f("cni", "CNI", false, "cilium"),
  list("control_plane_nodes", "Control plane", true),
  list("worker_nodes", "Workers"),
  bool("ha_enabled", "High availability"),
  f("vip_address", "VIP address"),
  f("vip_interface", "VIP interface"),
  f("method", "HA method", false, "keepalived"),
  bool("schedule_on_control_plane", "Schedule on control plane"),
];

export const RESOURCES: Resource[] = [
  {
    id: "vms",
    noun: "virtual machine",
    title: "Virtual machines",
    group: "Compute",
    terraform: ["kcore_vm"],
    kctl: "get vms",
    idField: "id",
    fields: vmFields,
    list: async () => rowsOf(await controller.listVms({}), "vms"),
    create: async (v) => {
      const spec = vmSpec(v);
      const resp = await controller.createVm({
        spec,
        targetNode: text(v, "target_node"),
        targetDc: text(v, "target_dc"),
        cloudInitUserData: text(v, "cloud_init_user_data"),
        sshKeyNames: strings(v, "ssh_key_names"),
        storageBackend: backend(spec.storageBackend),
        storageSizeBytes: spec.storageSizeBytes,
        nodeLabels: strings(v, "node_labels"),
        antiAffinity: text(v, "anti_affinity"),
        ...image(v),
      });
      return { ok: true, id: resp.vmId, node: resp.nodeId };
    },
    inspect: async (key) => {
      const vm = await controller.getVm({ vmId: key });
      const detail: Row = rowOf(vm);
      try {
        detail.migration = rowOf(await controller.getLiveMigrateReceiveStatus({ vmId: key }));
      } catch (error) {
        detail.migration_error = (error as Error).message;
      }
      try {
        detail.operations = rowsOf(await controller.listVmOperations({ vmId: key, includeFinished: true }), "operations");
      } catch {
        /* older controllers do not filter by VM */
      }
      return { detail };
    },
    actions: [
      act("start", "Start", false, [], async (v) => {
        await controller.setVmDesiredState({ vmId: required(v, "id"), targetNode: text(v, "target_node"), desiredState: VmDesiredState.RUNNING });
        return { ok: true };
      }),
      act("stop", "Stop", false, [], async (v) => {
        await controller.setVmDesiredState({ vmId: required(v, "id"), targetNode: text(v, "target_node"), desiredState: VmDesiredState.STOPPED });
        return { ok: true };
      }),
      act("console", "Console", false, [], async () => ({ ok: true }), { console: true }),
      act("update", "Resize", false, [num("cpu", "CPUs"), size("memory_bytes", "Memory"), f("labels", "Labels", false, "key=value")], async (v) =>
        ok(await controller.updateVm({ vmId: required(v, "id"), targetNode: text(v, "target_node"), cpu: int(v, "cpu"), memoryBytes: bytes(v, "memory_bytes"), labels: labels(v) })),
      ),
      act("migrate", "Migrate", true, [f("target_node", "Target node", true), bool("allow_cold_fallback", "Allow cold fallback"), f("gpus", "GPUs on target")], async (v) => {
        const resp = await controller.migrateVm({ vmId: required(v, "id"), targetNode: required(v, "target_node"), allowColdFallback: flag(v, "allow_cold_fallback"), gpus: strings(v, "gpus") });
        return ok(resp, { mode: resp.mode, operation: resp.operationId });
      }),
      act("migrate-status", "Migration status", false, [f("node_id", "Node")], async (v) => rowOf(await controller.getLiveMigrateReceiveStatus({ vmId: required(v, "id"), nodeId: text(v, "node_id") }))),
      act("reset-migrate", "Reset stranded migration", true, [f("node_id", "Node", true), bool("force", "Force clear")], async (v) => {
        if (!flag(v, "force")) throw new FormError("force must be true to clear a stranded receive session");
        return rowOf(await controller.resetLiveMigrateReceive({ vmId: required(v, "id"), nodeId: required(v, "node_id"), force: true }));
      }),
      act("freeze", "Freeze filesystems", true, [bool("freeze", "Freeze (clear to thaw)"), num("timeout_ms", "Timeout ms")], async (v) =>
        ok(await controller.freezeVmFilesystems({ vmName: first(v, "name", "id"), freeze: flag(v, "freeze"), timeoutMs: int(v, "timeout_ms") })),
      ),
      act("delete", "Delete", true, [bool("delete_data_volumes", "Delete data volumes")], async (v) => {
        await controller.deleteVm({ vmId: required(v, "id"), targetNode: text(v, "target_node"), deleteDataVolumes: flag(v, "delete_data_volumes") });
        return { ok: true };
      }),
    ],
  },
  {
    id: "workloads",
    noun: "workload",
    title: "Workloads",
    group: "Compute",
    terraform: ["kcore_workload"],
    kctl: "workload list",
    idField: "id",
    fields: [
      pick("kind", "Kind", ["vm", "container"], true),
      f("name", "Name", true),
      f("image", "Container image"),
      f("network", "Network"),
      num("cpu", "CPUs"),
      size("memory_bytes", "Memory"),
      f("storage_backend", "Storage backend"),
      size("storage_size_bytes", "Disk size"),
      f("image_url", "Image URL"),
      f("image_sha256", "Image SHA-256"),
      f("image_path", "Image path"),
      f("image_format", "Image format"),
      f("target_node", "Target node"),
      f("command", "Command", false, "comma separated"),
      f("ports", "Ports", false, "8080:80, comma separated"),
    ],
    list: workloadRows,
    create: (v) => createWorkload(v, text(v, "kind") === "container"),
    actions: workloadActions("workloads"),
  },
  {
    id: "containers",
    noun: "container",
    title: "Containers",
    group: "Compute",
    terraform: [],
    kctl: "get containers",
    idField: "id",
    fields: [
      f("name", "Name", true),
      f("image", "Image", true),
      f("network", "Network"),
      f("command", "Command", false, "comma separated"),
      f("ports", "Ports", false, "8080:80, comma separated"),
      f("target_node", "Target node"),
      f("storage_backend", "Storage backend"),
      size("storage_size_bytes", "Disk size"),
      f("mount_target", "Mount target"),
    ],
    list: async () => rowsOf(await controller.listWorkloads({}), "containers"),
    create: (v) => createWorkload(v, true),
    inspect: async (key) => ({ detail: rowOf(await controller.getWorkload({ kind: WorkloadKind.CONTAINER, workloadId: key })) }),
    actions: workloadActions("containers"),
  },
  {
    id: "operations",
    noun: "operation",
    title: "Operations",
    group: "Compute",
    terraform: [],
    kctl: "get operations",
    idField: "id",
    fields: [],
    refreshMs: 5_000,
    filters: [bool("include_finished", "Include finished")],
    list: async (q) => rowsOf(await controller.listVmOperations({ includeFinished: flag(q, "include_finished") }), "operations"),
    inspect: async (key) => {
      const resp = await controller.getVmOperation({ id: key });
      const detail = rowOf(resp);
      const op = (detail.operation ?? {}) as Row;
      if (typeof op.detail_json === "string" && op.detail_json) {
        try {
          op.detail = JSON.parse(op.detail_json);
        } catch {
          /* keep the raw text */
        }
      }
      return { detail: op };
    },
    actions: [act("cancel", "Cancel", true, [], async (v) => ok(await controller.cancelVmOperation({ id: required(v, "id") })))],
  },
  {
    id: "networks",
    noun: "network",
    title: "Networks",
    group: "Network",
    terraform: ["kcore_network"],
    kctl: "get networks",
    idField: "name",
    fields: [
      f("name", "Name", true),
      pick("network_type", "Type", ["nat", "bridge", "vxlan"]),
      f("external_ip", "External IP"),
      f("gateway_ip", "Gateway"),
      f("internal_netmask", "Netmask"),
      f("target_node", "Target node"),
      num("vlan_id", "VLAN id"),
      bool("enable_outbound_nat", "Outbound NAT"),
      f("ipv6_prefix", "IPv6 prefix"),
      f("ipv6_gateway", "IPv6 gateway"),
      bool("east_west_firewall", "East-west firewall"),
      f("allowed_tcp_ports", "TCP ports", false, "comma separated"),
      f("allowed_udp_ports", "UDP ports", false, "comma separated"),
    ],
    list: async () => rowsOf(await controller.listNetworks({}), "networks"),
    create: async (v) => {
      const resp = await controller.createNetwork({
        name: required(v, "name"),
        externalIp: text(v, "external_ip"),
        gatewayIp: text(v, "gateway_ip"),
        internalNetmask: text(v, "internal_netmask"),
        targetNode: text(v, "target_node"),
        allowedTcpPorts: ints(v, "allowed_tcp_ports"),
        allowedUdpPorts: ints(v, "allowed_udp_ports"),
        vlanId: int(v, "vlan_id"),
        networkType: text(v, "network_type"),
        enableOutboundNat: flag(v, "enable_outbound_nat"),
        ipv6Prefix: text(v, "ipv6_prefix"),
        ipv6Gateway: text(v, "ipv6_gateway"),
        eastWestFirewall: flag(v, "east_west_firewall"),
      });
      return ok(resp, { node: resp.nodeId });
    },
    inspect: async (key) => {
      const rows = rowsOf(await controller.listNetworks({}), "networks");
      const found = rows.find((row) => row.name === key);
      if (!found) throw new Error(`network ${key} not found`);
      return { detail: found };
    },
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteNetwork({ name: ident(v), targetNode: first(v, "target_node", "node_id") });
        return { ok: true };
      }),
    ],
  },
  {
    id: "network",
    noun: "host networking",
    single: true,
    title: "Host networking",
    group: "Network",
    terraform: [],
    kctl: "",
    idField: "default_gateway_interface",
    fields: [],
    list: async () => [rowOf(await controller.getNetworkOverview({}))],
    actions: [],
  },
  {
    id: "security-groups",
    noun: "security group",
    title: "Security groups",
    group: "Network",
    terraform: ["kcore_security_group", "kcore_security_group_attachment"],
    kctl: "security-group list",
    idField: "name",
    fields: [f("name", "Name", true), f("description", "Description"), list("rules", "Rules")],
    list: async () => rowsOf(await controller.listSecurityGroups({}), "security_groups"),
    create: async (v) =>
      ok(await controller.createSecurityGroup({ securityGroup: { name: required(v, "name"), description: text(v, "description"), rules: rules(v) } })),
    inspect: async (key) => {
      const resp = rowOf(await controller.getSecurityGroup({ name: key }));
      const group = (resp.security_group ?? {}) as Row;
      return { detail: { ...group, attachments: resp.attachments }, spec: { name: group.name, description: group.description, rules: group.rules } };
    },
    actions: [
      act("attach", "Attach", false, [pick("target_kind", "Target kind", ["vm", "network"], true), f("target_id", "Target id", true), f("target_node", "Target node")], async (v) => {
        await controller.attachSecurityGroup({ securityGroup: ident(v), targetKind: targetKind(v), targetId: required(v, "target_id"), targetNode: text(v, "target_node") });
        return { ok: true };
      }),
      act("detach", "Detach", true, [pick("target_kind", "Target kind", ["vm", "network"], true), f("target_id", "Target id", true), f("target_node", "Target node")], async (v) => {
        await controller.detachSecurityGroup({ securityGroup: ident(v), targetKind: targetKind(v), targetId: required(v, "target_id"), targetNode: text(v, "target_node") });
        return { ok: true };
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteSecurityGroup({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "volumes",
    noun: "volume",
    title: "Volumes",
    group: "Storage",
    terraform: ["kcore_volume"],
    kctl: "get volumes",
    idField: "name",
    fields: [
      f("name", "Name", true),
      size("size_bytes", "Size", true),
      f("storage_class", "Storage class"),
      f("vm", "Attach to VM"),
      f("node", "Node"),
      f("volume_name", "LV or zvol name"),
      bool("encrypt", "Encrypt"),
      f("from_snapshot", "Clone from snapshot"),
    ],
    list: async () => rowsOf(await controller.listVolumes({}), "volumes"),
    create: async (v) => {
      const sizeBytes = bytes(v, "size_bytes");
      if (sizeBytes <= 0n && !text(v, "from_snapshot")) throw new FormError("size_bytes must be greater than 0");
      return ok(
        await controller.createVolume({
          name: required(v, "name"),
          sizeBytes,
          storageClass: text(v, "storage_class"),
          vm: text(v, "vm"),
          guestFormatJson: text(v, "guest_format_json"),
          fromSnapshot: text(v, "from_snapshot"),
          encrypt: flag(v, "encrypt"),
          node: text(v, "node"),
          volumeName: text(v, "volume_name"),
        }),
      );
    },
    inspect: async (key) => {
      const resp = rowOf(await controller.getVolume({ name: key }));
      const detail = (resp.volume ?? resp) as Row;
      try {
        detail.snapshots = rowsOf(await controller.listVolumeSnapshots({ volume: key }), "snapshots");
      } catch {
        /* snapshots need Ceph */
      }
      return { detail };
    },
    actions: [
      act("attach", "Attach", false, [f("vm", "VM", true)], async (v) => ok(await controller.attachVolume({ name: ident(v), vm: required(v, "vm") }))),
      act("detach", "Detach", true, [], async (v) => {
        await controller.detachVolume({ name: ident(v) });
        return { ok: true };
      }),
      act("resize", "Resize", true, [size("size_bytes", "Size", true), bool("grow_filesystem", "Grow filesystem"), bool("allow_shrink", "Allow shrink")], async (v) =>
        ok(await controller.resizeVolume({ name: ident(v), sizeBytes: positiveBytes(v, "size_bytes"), growFilesystem: flag(v, "grow_filesystem"), allowShrink: flag(v, "allow_shrink") })),
      ),
      act("snapshot", "Snapshot", false, [f("snapshot", "Snapshot name"), pick("consistency", "Consistency", ["crash", "filesystem"])], async (v) =>
        ok(await controller.createVolumeSnapshot({ volume: ident(v), name: text(v, "snapshot"), consistency: text(v, "consistency") })),
      ),
      act("restore", "Restore", true, [f("snapshot", "Snapshot", true)], async (v) => ok(await controller.restoreVolume({ volume: ident(v), snapshot: required(v, "snapshot") }))),
      act("flatten", "Flatten", true, [], async (v) => {
        await controller.flattenVolume({ name: ident(v) });
        return { ok: true };
      }),
      act("format", "Format", true, [pick("fs_type", "Filesystem", ["ext4", "xfs"], true), bool("force", "Replace existing")], async (v) =>
        ok(await controller.formatVolume({ name: ident(v), fsType: required(v, "fs_type"), force: flag(v, "force") })),
      ),
      act("mount", "Mount", false, [f("mount_point", "Mount point", true), f("fs_type", "Filesystem"), bool("persistent", "Persistent")], async (v) =>
        ok(await controller.mountVolume({ name: ident(v), mountPoint: required(v, "mount_point"), fsType: text(v, "fs_type"), persistent: flag(v, "persistent") })),
      ),
      act("unmount", "Unmount", true, [bool("persistent", "Remove unit")], async (v) => ok(await controller.unmountVolume({ name: ident(v), persistent: flag(v, "persistent") }))),
      act("encrypt", "Encrypt", true, [bool("destroy_data", "Destroy existing data")], async (v) => {
        if (!flag(v, "destroy_data")) throw new FormError("destroy_data must be true");
        return ok(await controller.encryptVolume({ name: ident(v), destroyData: true }));
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteVolume({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "snapshots",
    noun: "volume snapshot",
    title: "Volume snapshots",
    group: "Storage",
    terraform: ["kcore_volume_snapshot"],
    kctl: "get snapshots",
    idField: "name",
    fields: [f("volume", "Volume", true), f("name", "Snapshot name"), pick("consistency", "Consistency", ["crash", "filesystem"])],
    filters: [f("volume", "Volume")],
    list: async (q) => rowsOf(await controller.listVolumeSnapshots({ volume: text(q, "volume") }), "snapshots"),
    create: async (v) => ok(await controller.createVolumeSnapshot({ volume: required(v, "volume"), name: text(v, "name"), consistency: text(v, "consistency") })),
    actions: [
      act("restore", "Restore", true, [], async (v) => ok(await controller.restoreVolume({ volume: required(v, "volume_name"), snapshot: required(v, "name") }))),
      act("expose", "Expose for backup", true, [num("lease_seconds", "Lease seconds")], async (v) =>
        rowOf(await controller.exposeVolumeSnapshot({ volumeName: required(v, "volume_name"), snapshotName: required(v, "name"), leaseSeconds: int(v, "lease_seconds") })),
      ),
      act("unexpose", "End exposure", true, [f("lease_id", "Lease id", true)], async (v) => {
        await controller.unexposeVolumeSnapshot({ leaseId: required(v, "lease_id") });
        return { ok: true };
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteVolumeSnapshot({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "snapshot-policies",
    noun: "snapshot policy",
    title: "Snapshot policies",
    group: "Storage",
    terraform: ["kcore_snapshot_policy"],
    kctl: "get snapshot-policies",
    idField: "name",
    fields: [
      f("name", "Name", true),
      f("selector_vm", "VM selector"),
      f("selector_volume", "Volume selector"),
      f("schedule", "Schedule", true, "hourly, daily, or a cron line"),
      num("keep", "Keep", true),
      bool("enabled", "Enabled"),
    ],
    list: async () => rowsOf(await controller.listSnapshotPolicies({}), "policies"),
    create: async (v) => {
      const keep = int(v, "keep");
      if (keep <= 0) throw new FormError("keep must be greater than 0");
      return ok(
        await controller.createSnapshotPolicy({
          policy: {
            name: required(v, "name"),
            selectorVm: text(v, "selector_vm"),
            selectorVolume: text(v, "selector_volume"),
            schedule: required(v, "schedule"),
            keep,
            enabled: v.enabled === undefined || flag(v, "enabled"),
          },
        }),
      );
    },
    inspect: async (key) => ({ detail: rowOf(await controller.getSnapshotPolicy({ name: key })).policy }),
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteSnapshotPolicy({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "disk-layouts",
    noun: "disk layout",
    title: "Disk layouts",
    group: "Storage",
    terraform: ["kcore_disk_layout"],
    kctl: "get disk-layouts",
    idField: "name",
    fields: [f("name", "Name", true), f("node_id", "Node", true), long("layout_nix", "Layout Nix", true), bool("evacuate", "Evacuate VMs")],
    checks: ["classify"],
    list: async () => rowsOf(await controller.listDiskLayouts({}), "disk_layouts").map(lift("disk_layout", "status")),
    create: async (v) =>
      ok(await controller.createDiskLayout({ diskLayout: { name: required(v, "name"), nodeId: required(v, "node_id"), layoutNix: required(v, "layout_nix"), evacuate: flag(v, "evacuate") } })),
    inspect: async (key) => {
      const resp = rowOf(await controller.getDiskLayout({ name: key }));
      const layout = (resp.disk_layout ?? {}) as Row;
      return { detail: resp, spec: { name: layout.name, node_id: layout.node_id, layout_nix: layout.layout_nix, evacuate: layout.evacuate } };
    },
    actions: [
      act("classify", "Check", false, [], async (v) => {
        let layout: Init<typeof DiskLayoutSchema> | undefined;
        if (!fromRow(v) && text(v, "layout_nix")) {
          layout = { name: text(v, "name"), nodeId: required(v, "node_id"), layoutNix: required(v, "layout_nix"), evacuate: flag(v, "evacuate") };
        } else {
          const name = ident(v);
          if (!name) throw new FormError("name or layout_nix is required");
          layout = (await controller.getDiskLayout({ name })).diskLayout;
          if (!layout) throw new Error("disk layout not found");
        }
        return rowOf(await controller.classifyDiskLayout({ diskLayout: layout }));
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteDiskLayout({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "storage",
    noun: "host storage",
    single: true,
    title: "Host storage",
    group: "Storage",
    terraform: [],
    kctl: "",
    idField: "approved_nodes",
    fields: [],
    list: async () => [rowOf(await controller.getStorageOverview({}))],
    actions: [],
  },
  {
    id: "storage-classes",
    noun: "storage class",
    title: "Storage classes",
    group: "Storage",
    terraform: [],
    kctl: "get storage-class",
    idField: "name",
    fields: [],
    list: async () => {
      const nodes = rowsOf(await controller.listNodes({}), "nodes");
      const counts = new Map<string, number>();
      for (const node of nodes) {
        const name = String(node.storage_backend || "unspecified");
        counts.set(name, (counts.get(name) ?? 0) + 1);
      }
      return [...counts.entries()].map(([name, count]) => ({ name, nodes: count }));
    },
    actions: [],
  },
  {
    id: "ceph",
    noun: "Ceph cluster",
    title: "Ceph clusters",
    group: "Storage",
    terraform: ["kcore_ceph_cluster"],
    kctl: "get ceph-clusters",
    idField: "name",
    fields: [
      f("name", "Name", true),
      f("public_network", "Public network", true),
      f("cluster_network", "Cluster network"),
      f("fsid", "FSID"),
      num("size", "Size"),
      num("min_size", "Min size"),
      list("nodes", "Nodes and OSD devices", true),
      bool("encrypt_osds", "Encrypt OSDs"),
      bool("force_wipe", "Wipe OSD devices"),
    ],
    checks: ["plan"],
    list: async () => rowsOf(await controller.listCephClusters({}), "ceph_clusters").map(flatten),
    create: async (v) => ok(await controller.createCephCluster({ cephCluster: cephCluster(v) })),
    inspect: async (key) => {
      const cluster = rowOf(await controller.getCephCluster({ name: key }));
      const row = (cluster.ceph_cluster ?? {}) as Row;
      const detail: Row = { ...row };
      try {
        detail.capacity = rowOf(await controller.getCephClusterCapacity({ name: key }));
      } catch (error) {
        detail.capacity_error = (error as Error).message;
      }
      return { detail, spec: cephForm(row) };
    },
    actions: [
      act("plan", "Plan", false, [], async (v) => {
        const cluster = fromRow(v) ? (await controller.getCephCluster({ name: ident(v) })).cephCluster : cephCluster(v);
        return rowOf(await controller.planCephCluster({ cephCluster: cluster }));
      }),
      act("credentials", "Set dashboard login", true, [f("url", "URL"), f("username", "Username", true), secret("password", "Password")], async (v) => {
        await controller.setCephDashboardCredentials({ name: ident(v), url: text(v, "url"), username: required(v, "username"), password: required(v, "password") });
        return { ok: true };
      }),
      act("show-credentials", "Show dashboard login", true, [], async (v): Promise<Reveal> => {
        const resp = await controller.getCephDashboardCredentials({ name: ident(v) });
        return { reveal: { url: resp.url, username: resp.username, password: resp.password } };
      }),
      act("delete", "Delete", true, [bool("force", "Force")], async (v) => {
        await controller.deleteCephCluster({ name: ident(v), force: flag(v, "force") });
        return { ok: true };
      }),
    ],
  },
  {
    id: "filesystems",
    noun: "shared filesystem",
    title: "Shared filesystems",
    group: "Storage",
    terraform: ["kcore_shared_filesystem"],
    kctl: "get shared-filesystems",
    idField: "name",
    fields: [f("name", "Name", true), f("ceph_cluster", "Ceph cluster", true), size("quota_bytes", "Quota"), f("fs_name", "FS name")],
    list: async () => rowsOf(await controller.listSharedFilesystems({}), "shared_filesystems").map(flatten),
    create: async (v) =>
      ok(
        await controller.createSharedFilesystem({
          sharedFilesystem: { name: required(v, "name"), spec: { cephCluster: required(v, "ceph_cluster"), quotaBytes: bytes(v, "quota_bytes"), fsName: text(v, "fs_name") } },
        }),
      ),
    inspect: async (key) => ({ detail: rowOf(await controller.getSharedFilesystem({ name: key })).shared_filesystem }),
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteSharedFilesystem({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "ingress-gateways",
    noun: "ingress gateway",
    title: "Ingress gateways",
    group: "Ingress",
    terraform: [],
    kctl: "get ingress-gateways",
    idField: "name",
    refreshMs: 10000,
    fields: [f("name", "Name", true), specField("Spec (JSON)")],
    list: async () => rowsOf(await controller.listIngressGateways({}), "gateways").map(ingressGatewayRow),
    create: async (v) => {
      const resp = await controller.createIngressGateway({ name: required(v, "name"), specJson: specJson(v) });
      return ok(resp, { action: resp.action });
    },
    inspect: async (key) => {
      const gateway = rowOf(await controller.getIngressGateway({ name: key })).gateway as Row;
      return { detail: { ...gateway, spec_json: parsedSpec(gateway) } };
    },
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteIngressGateway({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "ingress-routes",
    noun: "ingress route",
    title: "Ingress routes",
    group: "Ingress",
    terraform: [],
    kctl: "get ingress-routes",
    idField: "name",
    refreshMs: 10000,
    fields: [f("name", "Name", true), specField("Spec (JSON)")],
    filters: [f("gateway", "Gateway")],
    list: async (filters) => rowsOf(await controller.listIngressRoutes({ gateway: text(filters, "gateway") }), "routes").map(ingressRouteRow),
    create: async (v) => {
      const resp = await controller.createIngressRoute({ name: required(v, "name"), specJson: specJson(v) });
      return ok(resp, { action: resp.action });
    },
    // The route page: certificate, upstream health and what the WAF saw lately.
    inspect: async (key) => {
      const route = rowOf(await controller.getIngressRoute({ name: key })).route as Row;
      const since = BigInt(Math.floor(Date.now() / 1000) - 24 * 3600);
      const events = rowsOf(await controller.listWafEvents({ route: key, sinceUnix: since, limit: 50 }), "events");
      return { detail: { ...route, spec_json: parsedSpec(route), waf_events_24h: events } };
    },
    actions: [
      act(
        "waf-mode",
        "Set WAF mode",
        true,
        [pick("mode", "Mode", ["detect", "block", "off"], true)],
        async (v) => ok(await controller.setIngressRouteWafMode({ name: ident(v), mode: required(v, "mode") })),
      ),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteIngressRoute({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "waf-policies",
    noun: "WAF policy",
    title: "WAF policies",
    group: "Ingress",
    terraform: [],
    kctl: "get waf-policies",
    idField: "name",
    fields: [f("name", "Name", true), specField("Spec (JSON)")],
    list: async () => rowsOf(await controller.listWafPolicies({}), "policies").map(wafPolicyRow),
    create: async (v) => {
      const resp = await controller.createWafPolicy({ name: required(v, "name"), specJson: specJson(v, true) });
      return ok(resp, { action: resp.action });
    },
    inspect: async (key) => {
      const policy = rowOf(await controller.getWafPolicy({ name: key })).policy as Row;
      return { detail: { ...policy, spec_json: parsedSpec(policy) } };
    },
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteWafPolicy({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "waf-events",
    noun: "WAF event",
    title: "WAF events",
    group: "Ingress",
    terraform: [],
    kctl: "get waf-events",
    idField: "id",
    refreshMs: 10000,
    fields: [],
    filters: [f("route", "Route"), f("rule_id", "Rule ID"), f("since_hours", "Since (hours)")],
    list: async (filters) => {
      const hours = text(filters, "since_hours") ? Number(text(filters, "since_hours")) : 24;
      if (!Number.isFinite(hours) || hours <= 0) throw new FormError("Since must be a positive number of hours");
      const rule = text(filters, "rule_id") ? Number(text(filters, "rule_id")) : 0;
      if (!Number.isInteger(rule) || rule < 0) throw new FormError("Rule ID must be a whole number");
      return rowsOf(
        await controller.listWafEvents({
          route: text(filters, "route"),
          ruleId: rule,
          sinceUnix: BigInt(Math.floor(Date.now() / 1000 - hours * 3600)),
          limit: 200,
        }),
        "events",
      );
    },
    actions: [],
  },
  {
    id: "object-stores",
    noun: "object store",
    title: "Object stores",
    group: "Storage",
    terraform: ["kcore_object_store"],
    kctl: "get object-stores",
    idField: "name",
    fields: [f("name", "Name", true), f("ceph_cluster", "Ceph cluster", true), f("members", "Members", false, "node ids, comma separated"), num("port", "Port"), bool("tls", "TLS"), f("rgw_id", "RGW id")],
    list: async () => rowsOf(await controller.listObjectStores({}), "object_stores").map(flatten),
    create: async (v) =>
      ok(
        await controller.createObjectStore({
          objectStore: {
            name: required(v, "name"),
            spec: { cephCluster: required(v, "ceph_cluster"), members: strings(v, "members"), port: int(v, "port"), tls: flag(v, "tls"), rgwId: text(v, "rgw_id") },
          },
        }),
      ),
    inspect: async (key) => ({ detail: rowOf(await controller.getObjectStore({ name: key })).object_store }),
    actions: [
      act("add-user", "Add user", false, [f("user", "User name", true)], async (v): Promise<Reveal> => {
        const resp = await controller.createObjectUser({ objectUser: { name: required(v, "user"), spec: { store: ident(v) } } });
        if (!resp.success) throw new Error("The controller refused the request.");
        return { reveal: { access_key: resp.objectUser?.accessKey ?? "", secret: resp.secret }, note: "The secret is shown once." };
      }),
      act("remove-user", "Remove user", true, [f("user", "User name", true)], async (v) => {
        await controller.deleteObjectUser({ name: required(v, "user"), store: ident(v) });
        return { ok: true };
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteObjectStore({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "nodes",
    noun: "node",
    title: "Nodes",
    group: "Cluster",
    terraform: ["kcore_node"],
    kctl: "get nodes",
    idField: "node_id",
    fields: [],
    list: async () => rowsOf(await controller.listNodes({}), "nodes").map(nodeRow),
    inspect: async (key) => {
      const resp = rowOf(await controller.getNode({ nodeId: key }));
      const detail = nodeRow((resp.node ?? {}) as Row);
      try {
        detail.devices = rowsOf(await controller.listPciDevices({ nodeId: key }), "gpus");
      } catch (error) {
        detail.devices_error = (error as Error).message;
      }
      try {
        detail.ha_events = rowsOf(await controller.listHaEvents({ nodeId: key, limit: 50 }), "events");
      } catch {
        /* HA history is optional */
      }
      return { detail };
    },
    actions: [
      act("approve", "Approve", false, [], async (v) => ({ ok: true, message: (await controller.approveNode({ nodeId: first(v, "node_id", "id") })).message })),
      act("reject", "Reject", true, [], async (v) => ({ ok: true, message: (await controller.rejectNode({ nodeId: first(v, "node_id", "id") })).message })),
      act("cordon", "Cordon", true, [], async (v) => ({ ok: true, message: (await controller.cordonNode({ nodeId: first(v, "node_id", "id") })).message })),
      act("uncordon", "Uncordon", false, [], async (v) => ({ ok: true, message: (await controller.uncordonNode({ nodeId: first(v, "node_id", "id") })).message })),
      act("drain", "Drain", true, [f("target_node", "Target node")], async (v) => {
        const resp = await controller.drainNode({ nodeId: first(v, "node_id", "id"), targetNode: text(v, "target_node") });
        return ok(resp, { migrated: resp.vmsMigrated });
      }),
      act("renew-cert", "Renew certificate", true, [], async (v): Promise<Reveal> => {
        const resp = await controller.renewNodeCert({ nodeId: first(v, "node_id", "id") });
        ok(resp);
        return { reveal: { cert_pem: resp.certPem, key_pem: resp.keyPem }, note: "Install the key on the node. It is shown once." };
      }),
      act("delete", "Delete", true, [], async (v) => ({ ok: true, message: (await controller.deleteNode({ nodeId: first(v, "node_id", "id") })).message })),
    ],
  },
  {
    id: "kubernetes",
    noun: "Kubernetes cluster",
    title: "Kubernetes",
    group: "Cluster",
    terraform: ["kcore_kubernetes_cluster"],
    kctl: "kubernetes list",
    idField: "name",
    fields: [f("name", "Name", true), ...k8sFields],
    checks: ["plan"],
    list: async () => rowsOf(await controller.listKubernetesClusters({}), "clusters").map(flatten),
    create: async (v) => {
      const resp = await controller.createKubernetesCluster({ name: required(v, "name"), spec: k8sSpec(v), confirm: true });
      return { ok: true, name: resp.cluster?.name ?? "" };
    },
    inspect: async (key) => {
      const resp = rowOf(await controller.getKubernetesCluster({ name: key }));
      const cluster = (resp.cluster ?? {}) as Row;
      const detail: Row = { ...cluster };
      try {
        detail.health = rowOf(await controller.getKubernetesClusterHealth({ name: key }));
      } catch (error) {
        detail.health_error = (error as Error).message;
      }
      return { detail, spec: k8sForm(cluster) };
    },
    actions: [
      act("plan", "Plan", false, [], async (v) => {
        const name = ident(v);
        const spec = postedK8s(v) ? k8sSpec(v) : (await controller.getKubernetesCluster({ name })).cluster?.spec;
        if (!spec) throw new Error(`kubernetes cluster ${name} has no spec`);
        return rowOf(await controller.planKubernetesCluster({ name, spec }));
      }),
      act(
        "update",
        "Update",
        true,
        k8sFields,
        async (v) => {
          const resp = await controller.updateKubernetesCluster({ name: required(v, "name"), spec: k8sSpec(v), confirm: true });
          return { ok: true, name: resp.cluster?.name ?? "" };
        },
        { preload: true },
      ),
      act(
        "scale",
        "Scale",
        true,
        [f("add_node_id", "Add node"), f("role", "Role", false, "worker"), f("advertise_address", "Advertise address"), f("iface", "Interface"), f("remove_node_ids", "Remove node ids", false, "comma separated")],
        async (v) => {
          const add = text(v, "add_node_id");
          await controller.scaleKubernetesCluster({
            name: ident(v),
            confirm: true,
            addNodes: add ? [{ nodeId: add, role: text(v, "role") || "worker", advertiseAddress: text(v, "advertise_address"), iface: text(v, "iface") }] : [],
            removeNodeIds: strings(v, "remove_node_ids"),
          });
          return { ok: true };
        },
      ),
      act("delete", "Delete", true, [bool("force", "Force")], async (v) => {
        const resp = await controller.deleteKubernetesCluster({ name: ident(v), confirm: true, force: flag(v, "force") });
        return { ok: true, message: resp.message };
      }),
    ],
  },
  {
    id: "cluster-updates",
    noun: "cluster update",
    title: "Cluster updates",
    group: "Cluster",
    terraform: ["kcore_cluster_update"],
    kctl: "update cluster list",
    idField: "name",
    fields: [
      f("name", "Name", true),
      f("version", "Version"),
      f("flake_ref", "Flake ref", false, "github:kcorehypervisor/kcore"),
      f("flake_rev", "Flake revision", false, "40-character commit"),
      f("update_kind", "Update kind", false, "kcore"),
      bool("all_nodes", "All nodes"),
      f("node_ids", "Node ids", false, "comma separated"),
      bool("drain_vms", "Drain VMs first"),
    ],
    checks: ["plan"],
    list: async () =>
      rowsOf(await controller.listClusterUpdates({}), "cluster_updates").map((row) => {
        const flat = flatten(row);
        const target = ((row.spec as Row | null)?.target ?? {}) as Row;
        return { ...flat, version: target.version, flake_ref: target.flake_ref, flake_rev: target.flake_rev };
      }),
    create: async (v) => ok(await controller.createClusterUpdate({ spec: updateSpec(v) })),
    inspect: async (key) => ({ detail: rowOf(await controller.getClusterUpdate({ name: key })) }),
    actions: [
      act("plan", "Plan", false, [], async (v) => {
        const spec = fromRow(v) ? (await controller.getClusterUpdate({ name: ident(v) })).clusterUpdate?.spec : updateSpec(v);
        if (!spec) throw new Error(`cluster update ${ident(v)} has no spec`);
        return rowOf(await controller.planClusterUpdate({ spec }));
      }),
      act("approve", "Approve", true, [], async (v) => {
        await controller.approveClusterUpdate({ name: ident(v) });
        return { ok: true };
      }),
      act("reboot", "Reboot node", true, [f("node_id", "Node", true)], async (v) => ok(await controller.rebootClusterUpdateNode({ name: ident(v), nodeId: required(v, "node_id") }))),
      act("rollback", "Roll back", true, [], async (v) => {
        await controller.rollbackClusterUpdate({ name: ident(v) });
        return { ok: true };
      }),
      act("cancel", "Cancel", true, [], async (v) => {
        await controller.cancelClusterUpdate({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "cluster",
    noun: "cluster",
    single: true,
    title: "Cluster",
    group: "Cluster",
    terraform: ["kcore_cluster"],
    kctl: "get cluster-health",
    idField: "status",
    fields: [],
    list: async () => [rowOf(await controller.getClusterHealth({}))],
    actions: [
      act("backup", "Download backup", false, [], async (): Promise<Download> => {
        const resp = await controller.backupCluster({});
        const stamp = new Date().toISOString().replace(/[:.]/g, "-");
        return { download: { filename: `kcore-controller-schema${resp.schemaVersion}-${stamp}.sqlite`, data: resp.sqlite, type: "application/vnd.sqlite3" } };
      }),
      act("restore", "Restore backup", true, [file("sqlite", "SQLite backup")], async (v) => {
        const data = text(v, "sqlite");
        if (!data) throw new FormError("choose a backup file");
        return ok(await controller.restoreCluster({ sqlite: base64ToBytes(data), confirm: true }));
      }),
      act("apply-nix", "Apply Nix config", true, [long("configuration_nix", "configuration.nix", true), bool("rebuild", "Rebuild")], async (v) =>
        ok(await admin.applyNixConfig({ configurationNix: required(v, "configuration_nix"), rebuild: flag(v, "rebuild") })),
      ),
    ],
  },
  {
    id: "ha",
    noun: "HA event",
    title: "High availability",
    group: "Cluster",
    terraform: [],
    kctl: "get ha-events",
    idField: "id",
    fields: [],
    filters: [f("node_id", "Node"), f("vm_id", "VM")],
    list: async (q) => rowsOf(await controller.listHaEvents({ nodeId: text(q, "node_id"), vmId: text(q, "vm_id"), limit: 200 }), "events"),
    actions: [],
  },
  {
    id: "conflicts",
    noun: "conflict",
    title: "Replication conflicts",
    group: "Cluster",
    terraform: [],
    kctl: "get conflicts",
    idField: "id",
    fields: [],
    list: async () => rowsOf(await admin.listReplicationConflicts({}), "conflicts"),
    actions: [
      act("resolve", "Resolve", true, [], async (v) => {
        const id = i64(v, "id");
        if (id === 0n) throw new FormError("id is required");
        await admin.resolveReplicationConflict({ id });
        return { ok: true };
      }),
    ],
  },
  {
    id: "replication",
    noun: "replication",
    single: true,
    title: "Replication",
    group: "Cluster",
    terraform: [],
    kctl: "get replication-status",
    idField: "outbox_head_event_id",
    fields: [],
    list: async () => [rowOf(await admin.getReplicationStatus({}))],
    actions: [],
  },
  {
    id: "operators",
    noun: "operator",
    title: "Operators",
    group: "Identity",
    terraform: ["kcore_operator"],
    kctl: "operator list",
    idField: "name",
    fields: [f("name", "Name", true)],
    list: async () => rowsOf(await controller.listOperators({}), "operators"),
    create: async (v) => {
      const resp = await controller.createOperator({ name: required(v, "name") });
      return { ok: true, name: resp.operator?.name ?? "", next: "Set a password so the operator can sign in to the dashboard." };
    },
    inspect: async (key) => {
      const detail: Row = rowOf(await controller.getOperator({ name: key }));
      try {
        detail.api_tokens = rowsOf(await auth.listApiTokens({ operator: key }), "tokens");
      } catch {
        /* cluster-admin only */
      }
      return { detail };
    },
    actions: [
      act("grant", "Grant role", false, [pick("role", "Role", ROLES, true)], async (v) => {
        await controller.grantOperatorRole({ operatorName: ident(v), role: role(v) });
        return { ok: true };
      }),
      act("revoke", "Revoke role", true, [pick("role", "Role", ROLES, true)], async (v) => {
        await controller.revokeOperatorRole({ operatorName: ident(v), role: role(v) });
        return { ok: true };
      }),
      act("set-password", "Set password", true, [secret("password", "New password")], async (v) => {
        const resp = await auth.setOperatorPassword({ name: ident(v), password: required(v, "password") });
        return { ok: true, sessions_revoked: resp.sessionsRevoked };
      }),
      act("reset-2fa", "Reset two-factor", true, [], async (v) => {
        await auth.resetOperatorSecondFactor({ name: ident(v) });
        return { ok: true, next: "The operator enrolls a new authenticator at the next sign-in." };
      }),
      act("issue-cert", "Issue kctl certificate", true, [], async (v): Promise<Reveal> => {
        const resp = await controller.issueOperatorCert({ operatorName: ident(v) });
        ok(resp);
        return { reveal: { cert_pem: resp.certPem, key_pem: resp.keyPem }, note: "The private key is shown once." };
      }),
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteOperator({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "api-tokens",
    noun: "API token",
    title: "API tokens",
    group: "Identity",
    terraform: [],
    kctl: "",
    idField: "token_id",
    fields: [f("name", "Name", true), num("expires_in_days", "Expires in days", false, "90"), pick("max_role", "Role cap", ROLES)],
    filters: [f("operator", "Operator")],
    list: async (q) => rowsOf(await auth.listApiTokens({ operator: text(q, "operator") }), "tokens"),
    create: async (v): Promise<Reveal> => {
      const resp = await auth.createApiToken({ name: required(v, "name"), expiresInDays: int(v, "expires_in_days"), maxRole: optionalRole(v, "max_role") });
      return { reveal: { token: resp.token }, note: "Copy it now: it is shown only once. Clients send it in the authorization header as Bearer <token>, and it does not ask for two-factor." };
    },
    actions: [
      act("revoke", "Revoke", true, [], async (v) => {
        await auth.revokeApiToken({ tokenId: required(v, "token_id") });
        return { ok: true };
      }),
    ],
  },
  {
    id: "sessions",
    noun: "session",
    title: "My sessions",
    group: "Identity",
    terraform: [],
    kctl: "",
    idField: "token_id",
    fields: [],
    list: async () => rowsOf(await auth.listSessions({}), "sessions"),
    actions: [
      act("revoke", "Sign out", true, [], async (v) => {
        await auth.revokeSession({ tokenId: required(v, "token_id") });
        return { ok: true };
      }),
    ],
  },
  {
    id: "ssh-keys",
    noun: "SSH key",
    title: "SSH keys",
    group: "Identity",
    terraform: ["kcore_ssh_key"],
    kctl: "ssh-key list",
    idField: "name",
    fields: [f("name", "Name", true), long("public_key", "Public key", true)],
    list: async () => rowsOf(await controller.listSshKeys({}), "keys"),
    create: async (v) => ok(await controller.createSshKey({ name: required(v, "name"), publicKey: required(v, "public_key") })),
    inspect: async (key) => ({ detail: rowOf(await controller.getSshKey({ name: key })).key }),
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deleteSshKey({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "certificates",
    noun: "certificate",
    title: "Certificates",
    group: "Identity",
    terraform: [],
    kctl: "get certificates",
    idField: "serial_hex",
    fields: [],
    list: async () => rowsOf(await controller.listCertificates({}), "certificates"),
    actions: [
      act("revoke", "Revoke", true, [f("reason", "Reason", false, "keyCompromise, superseded, …")], async (v) =>
        ok(await controller.revokeCertificate({ serialHex: first(v, "serial_hex", "id"), subjectCn: text(v, "subject_cn"), nodeId: text(v, "node_id"), reason: revocationReason(v) })),
      ),
      act("rotate", "Rotate node certs", true, [f("node_id", "Node"), bool("all_nodes", "All nodes")], async (v) =>
        ok(await controller.rotateNodeCerts({ nodeId: text(v, "node_id"), allNodes: flag(v, "all_nodes") })),
      ),
      act("sign", "Sign CSR", true, [f("node_id", "Node", true), long("csr_pem", "CSR PEM", true)], async (v): Promise<Reveal> => {
        const resp = await controller.signNodeCsr({ nodeId: required(v, "node_id"), csrPem: required(v, "csr_pem") });
        ok(resp);
        return { reveal: { cert_chain_pem: resp.certChainPem } };
      }),
    ],
  },
  {
    id: "crl",
    noun: "revocation list",
    single: true,
    title: "Revocation list",
    group: "Identity",
    terraform: [],
    kctl: "get crl",
    idField: "crl_number",
    fields: [],
    list: async () => [rowOf(await controller.getCrl({}))],
    actions: [],
  },
  {
    id: "pki",
    noun: "pki",
    single: true,
    title: "PKI",
    group: "Identity",
    terraform: [],
    kctl: "get pki-status",
    idField: "revocation_fail_mode",
    fields: [],
    list: async () => [rowOf(await controller.getPkiStatus({}))],
    actions: [
      act("bootstrap", "Issue bootstrap cert", true, [f("node_id", "Node", true), f("node_host", "Host")], async (v): Promise<Reveal> => {
        const resp = await controller.issueNodeBootstrapCert({ nodeId: required(v, "node_id"), nodeHost: text(v, "node_host") });
        ok(resp);
        return { reveal: { cert_pem: resp.certPem, key_pem: resp.keyPem }, note: "The private key is shown once." };
      }),
      act("rotate-sub-ca", "Rotate sub-CA", true, [long("sub_ca_cert_pem", "Sub-CA certificate", true), secret("sub_ca_key_pem", "Sub-CA key")], async (v) =>
        ok(await controller.rotateSubCa({ subCaCertPem: required(v, "sub_ca_cert_pem"), subCaKeyPem: required(v, "sub_ca_key_pem") })),
      ),
      act("reload-tls", "Reload TLS", true, [long("cert_pem", "Certificate", true), secret("key_pem", "Key")], async (v) =>
        ok(await controller.reloadTls({ certPem: required(v, "cert_pem"), keyPem: required(v, "key_pem") })),
      ),
    ],
  },
  {
    id: "postgresql",
    noun: "PostgreSQL instance",
    title: "PostgreSQL",
    group: "Services",
    terraform: [],
    kctl: "get postgresql",
    idField: "name",
    fields: [f("name", "Name", true), f("database", "Database"), f("package", "Package"), num("port", "Port"), f("target_node", "Target node")],
    list: async () => rowsOf(await controller.listPostgresqls({}), "postgresqls"),
    create: async (v) => {
      const resp = await controller.createPostgresql({ name: required(v, "name"), database: text(v, "database"), package: text(v, "package"), port: int(v, "port"), targetNode: text(v, "target_node") });
      return ok(resp, { node: resp.nodeId });
    },
    inspect: async (key) => ({ detail: rowOf(await controller.getPostgresql({ name: key })).postgresql }),
    actions: [
      act("delete", "Delete", true, [], async (v) => {
        await controller.deletePostgresql({ name: ident(v) });
        return { ok: true };
      }),
    ],
  },
  {
    id: "gpus",
    noun: "device",
    title: "GPUs and devices",
    group: "Observe",
    terraform: [],
    kctl: "gpu list",
    idField: "name",
    fields: [],
    filters: [f("node_id", "Node"), bool("all_devices", "All PCI devices")],
    list: async (q) => {
      const req = { nodeId: text(q, "node_id") };
      const resp = flag(q, "all_devices") ? await controller.listPciDevices(req) : await controller.listGpus(req);
      return rowsOf(resp, "gpus");
    },
    actions: [],
  },
  {
    id: "health",
    noun: "health",
    single: true,
    title: "Health",
    group: "Observe",
    terraform: [],
    kctl: "get cluster-health",
    idField: "status",
    fields: [],
    list: async () => {
      const row = rowOf(await controller.getClusterHealth({}));
      try {
        row.ha = rowOf(await controller.getHaStatus({}));
      } catch {
        /* HA is optional */
      }
      return [row];
    },
    actions: [],
  },
  {
    id: "compliance",
    noun: "compliance",
    single: true,
    title: "Compliance",
    group: "Observe",
    terraform: [],
    kctl: "get compliance-report",
    idField: "controller_version",
    fields: [],
    list: async () => [rowOf(await controller.getComplianceReport({}))],
    actions: [],
  },
  {
    id: "audit",
    noun: "audit event",
    title: "Audit",
    group: "Observe",
    terraform: [],
    kctl: "get audit-events",
    idField: "id",
    fields: [],
    filters: [f("since", "Since", false, "2026-10-01T00:00:00Z"), f("action", "Action")],
    list: async (q) => rowsOf(await controller.listAuditEvents({ limit: 200, since: text(q, "since"), action: text(q, "action") }), "events"),
    actions: [],
  },
  {
    id: "crypto",
    noun: "crypto",
    single: true,
    title: "Crypto",
    group: "Observe",
    terraform: [],
    kctl: "get crypto-config",
    idField: "library",
    fields: [],
    list: async () => [rowOf(await controller.getCryptoConfig({}))],
    actions: [],
  },
  {
    id: "sbom",
    noun: "SBOM document",
    title: "SBOM",
    group: "Observe",
    terraform: [],
    kctl: "get sbom",
    idField: "kind",
    fields: [],
    list: async () => {
      const rows: Row[] = [];
      for (const kind of ["crates", "iso-closure"]) {
        try {
          const row = rowOf(await controller.exportSbom({ kind }));
          delete row.document;
          rows.push({ kind, ...row });
        } catch (error) {
          rows.push({ kind, error: (error as Error).message });
        }
      }
      return rows;
    },
    actions: [
      act("download", "Download", false, [], async (v): Promise<Download> => {
        const resp = await controller.exportSbom({ kind: required(v, "kind") });
        return { download: { filename: resp.filename || `kcore-${text(v, "kind")}.cdx.json`, data: resp.document, type: "application/json" } };
      }),
    ],
  },
];

export function findResource(id: string): Resource | undefined {
  return RESOURCES.find((resource) => resource.id === id);
}

export function isReveal(value: unknown): value is Reveal {
  return Boolean(value) && typeof value === "object" && "reveal" in (value as object);
}

export function isDownload(value: unknown): value is Download {
  return Boolean(value) && typeof value === "object" && "download" in (value as object);
}

export function fileToBase64(file: File): Promise<string> {
  return file.arrayBuffer().then((buffer) => bytesToBase64(new Uint8Array(buffer)));
}
