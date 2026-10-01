import { readFileSync } from "node:fs";
import type { ResourceKind } from "./catalog.js";
import {
  InputError,
  assertAddress,
  assertArg,
  assertDisk,
  assertHttpsUrl,
  assertName,
  assertSha256,
  assertSshPublicKey,
} from "./names.js";

export type RpcCall = {
  target: "controller" | "node";
  method: string;
  request: Record<string, unknown>;
  /** Set when target is the node-agent. */
  address?: string;
};

const BACKEND: Record<string, string> = {
  filesystem: "STORAGE_BACKEND_TYPE_FILESYSTEM",
  lvm: "STORAGE_BACKEND_TYPE_LVM",
  zfs: "STORAGE_BACKEND_TYPE_ZFS",
  ceph: "STORAGE_BACKEND_TYPE_CEPH",
};

export function buildApply(kind: ResourceKind, spec: Record<string, unknown>): RpcCall[] {
  switch (kind.id) {
    case "cluster":
      throw new InputError("cluster creation writes local certificates and is handled before any controller RPC");
    case "vm":
      return [{ target: "controller", method: "createVm", request: vmRequest(spec) }];
    case "network":
      return [{ target: "controller", method: "createNetwork", request: networkRequest(spec) }];
    case "ssh-key":
      return [{ target: "controller", method: "createSshKey", request: sshKeyRequest(spec) }];
    case "container":
      return [{ target: "controller", method: "createWorkload", request: containerRequest(spec) }];
    case "volume":
      return [{ target: "controller", method: "createVolume", request: volumeRequest(spec) }];
    case "volume-snapshot":
      return [{ target: "controller", method: "createVolumeSnapshot", request: snapshotRequest(spec) }];
    case "snapshot-policy":
      return [{ target: "controller", method: "createSnapshotPolicy", request: { policy: policyMessage(spec) } }];
    case "security-group":
      return securityGroupCalls(spec);
    case "disk-layout":
      return [{ target: "controller", method: "createDiskLayout", request: { diskLayout: diskLayoutMessage(spec) } }];
    case "ceph-cluster":
      return [{ target: "controller", method: "createCephCluster", request: { cephCluster: cephMessage(spec) } }];
    case "shared-filesystem":
      return [{ target: "controller", method: "createSharedFilesystem", request: { sharedFilesystem: filesystemMessage(spec) } }];
    case "object-store":
      return [{ target: "controller", method: "createObjectStore", request: { objectStore: objectStoreMessage(spec) } }];
    case "cluster-update":
      return [{ target: "controller", method: "createClusterUpdate", request: { spec: clusterUpdateMessage(spec) } }];
    default:
      throw new InputError(`unsupported kind ${kind.id}`);
  }
}

export function buildRead(kind: ResourceKind, name?: string): RpcCall {
  const named = name?.trim() ? assertName(name, "name") : "";
  switch (kind.id) {
    case "cluster":
      return { target: "controller", method: "getClusterHealth", request: {} };
    case "vm":
      return named
        ? { target: "controller", method: "getVm", request: { vmId: named } }
        : { target: "controller", method: "listVms", request: {} };
    case "network":
      return { target: "controller", method: "listNetworks", request: {} };
    case "ssh-key":
      return named
        ? { target: "controller", method: "getSshKey", request: { name: named } }
        : { target: "controller", method: "listSshKeys", request: {} };
    case "container":
      return named
        ? { target: "controller", method: "getWorkload", request: { kind: "WORKLOAD_KIND_CONTAINER", workloadId: named } }
        : { target: "controller", method: "listWorkloads", request: { kind: "WORKLOAD_KIND_CONTAINER" } };
    case "volume":
      return named
        ? { target: "controller", method: "getVolume", request: { name: named } }
        : { target: "controller", method: "listVolumes", request: {} };
    case "volume-snapshot":
      return { target: "controller", method: "listVolumeSnapshots", request: named ? { volume: named } : {} };
    case "snapshot-policy":
      return named
        ? { target: "controller", method: "getSnapshotPolicy", request: { name: named } }
        : { target: "controller", method: "listSnapshotPolicies", request: {} };
    case "security-group":
      return named
        ? { target: "controller", method: "getSecurityGroup", request: { name: named } }
        : { target: "controller", method: "listSecurityGroups", request: {} };
    case "disk-layout":
      return named
        ? { target: "controller", method: "getDiskLayout", request: { name: named } }
        : { target: "controller", method: "listDiskLayouts", request: {} };
    case "ceph-cluster":
      return named
        ? { target: "controller", method: "getCephCluster", request: { name: named } }
        : { target: "controller", method: "listCephClusters", request: {} };
    case "shared-filesystem":
      return named
        ? { target: "controller", method: "getSharedFilesystem", request: { name: named } }
        : { target: "controller", method: "listSharedFilesystems", request: {} };
    case "object-store":
      return named
        ? { target: "controller", method: "getObjectStore", request: { name: named } }
        : { target: "controller", method: "listObjectStores", request: {} };
    case "cluster-update":
      return named
        ? { target: "controller", method: "getClusterUpdate", request: { name: named } }
        : { target: "controller", method: "listClusterUpdates", request: {} };
    default:
      throw new InputError(`unsupported kind ${kind.id}`);
  }
}

export function buildDelete(kind: ResourceKind, name: string): RpcCall {
  const id = assertName(name, "name");
  switch (kind.id) {
    case "cluster":
      throw new InputError("This server does not delete local certificates. Remove the context from ~/.kcore/config by hand.");
    case "vm":
      return { target: "controller", method: "deleteVm", request: { vmId: id } };
    case "network":
      return { target: "controller", method: "deleteNetwork", request: { name: id } };
    case "ssh-key":
      return { target: "controller", method: "deleteSshKey", request: { name: id } };
    case "container":
      return { target: "controller", method: "deleteWorkload", request: { kind: "WORKLOAD_KIND_CONTAINER", workloadId: id } };
    case "volume":
      return { target: "controller", method: "deleteVolume", request: { name: id } };
    case "volume-snapshot":
      return { target: "controller", method: "deleteVolumeSnapshot", request: { name: id } };
    case "snapshot-policy":
      return { target: "controller", method: "deleteSnapshotPolicy", request: { name: id } };
    case "security-group":
      return { target: "controller", method: "deleteSecurityGroup", request: { name: id } };
    case "disk-layout":
      return { target: "controller", method: "deleteDiskLayout", request: { name: id } };
    case "ceph-cluster":
      return { target: "controller", method: "deleteCephCluster", request: { name: id } };
    case "shared-filesystem":
      return { target: "controller", method: "deleteSharedFilesystem", request: { name: id } };
    case "object-store":
      return { target: "controller", method: "deleteObjectStore", request: { name: id } };
    case "cluster-update":
      return { target: "controller", method: "cancelClusterUpdate", request: { name: id } };
    default:
      throw new InputError(`unsupported kind ${kind.id}`);
  }
}

export function buildPlanCall(kind: ResourceKind, spec: Record<string, unknown>, name?: string): RpcCall {
  if (kind.id === "disk-layout") {
    return { target: "controller", method: "classifyDiskLayout", request: { diskLayout: diskLayoutMessage(spec) } };
  }
  if (kind.id === "cluster-update") {
    return { target: "controller", method: "planClusterUpdate", request: { spec: clusterUpdateMessage(spec) } };
  }
  return buildRead(kind, name ?? (typeof spec.name === "string" ? spec.name : undefined));
}

export function buildOperation(action: string, spec: Record<string, unknown>): RpcCall {
  switch (action) {
    case "set-vm-state": {
      const state = required(spec, "state", "state");
      if (state !== "running" && state !== "stopped") throw new InputError("state must be running or stopped");
      return {
        target: "controller",
        method: "setVmDesiredState",
        request: {
          vmId: assertName(required(spec, "name", "VM name"), "VM name"),
          desiredState: state === "running" ? "VM_DESIRED_STATE_RUNNING" : "VM_DESIRED_STATE_STOPPED",
          targetNode: opt(spec, "targetNode") ?? "",
        },
      };
    }
    case "migrate-vm":
      return {
        target: "controller",
        method: "migrateVm",
        request: {
          vmId: assertName(required(spec, "name", "VM name"), "VM name"),
          targetNode: assertArg(required(spec, "targetNode", "target node"), "target node"),
          allowColdFallback: boolish(spec.allowColdFallback),
        },
      };
    case "drain-node":
      return {
        target: "controller",
        method: "drainNode",
        request: {
          nodeId: assertName(required(spec, "nodeId", "node id"), "node id"),
          targetNode: opt(spec, "targetNode") ?? "",
        },
      };
    case "cordon-node":
      return { target: "controller", method: "cordonNode", request: { nodeId: assertName(required(spec, "nodeId", "node id"), "node id") } };
    case "uncordon-node":
      return { target: "controller", method: "uncordonNode", request: { nodeId: assertName(required(spec, "nodeId", "node id"), "node id") } };
    case "approve-node":
      return { target: "controller", method: "approveNode", request: { nodeId: assertName(required(spec, "nodeId", "node id"), "node id") } };
    case "reject-node":
      return { target: "controller", method: "rejectNode", request: { nodeId: assertName(required(spec, "nodeId", "node id"), "node id") } };
    case "approve-update":
      return { target: "controller", method: "approveClusterUpdate", request: { name: assertName(required(spec, "name", "update name"), "update name") } };
    case "cancel-update":
      return { target: "controller", method: "cancelClusterUpdate", request: { name: assertName(required(spec, "name", "update name"), "update name") } };
    case "rollback-update":
      return { target: "controller", method: "rollbackClusterUpdate", request: { name: assertName(required(spec, "name", "update name"), "update name") } };
    case "install-node":
      return installCall(spec);
    default:
      throw new InputError(
        "action must be set-vm-state, migrate-vm, drain-node, cordon-node, uncordon-node, approve-node, reject-node, approve-update, cancel-update, rollback-update, or install-node",
      );
  }
}

export function bootstrapCertRequest(spec: Record<string, unknown>): { nodeId: string; nodeHost: string; address: string } {
  const address = assertAddress(required(spec, "node", "node address"), "node address");
  const nodeHost = hostOf(address);
  const nodeId = opt(spec, "nodeId") ? assertName(opt(spec, "nodeId")!, "node id") : `kcore-node-${nodeHost.replaceAll(".", "-")}`;
  return { nodeId, nodeHost, address };
}

function installCall(spec: Record<string, unknown>): RpcCall {
  const boot = bootstrapCertRequest(spec);
  const backend = opt(spec, "storageBackend");
  return {
    target: "node",
    method: "installToDisk",
    address: boot.address,
    request: {
      osDisk: assertDisk(required(spec, "osDisk", "OS disk"), "OS disk"),
      dataDisks: disks(spec),
      controller: assertAddress(required(spec, "joinController", "join controller"), "join controller"),
      controllers: [assertAddress(required(spec, "joinController", "join controller"), "join controller")],
      storageBackend: backend ? backendEnum(backend) : "STORAGE_BACKEND_TYPE_UNSPECIFIED",
      disableVxlan: boolish(spec.disableVxlan),
      dcId: opt(spec, "dcId") ? assertName(opt(spec, "dcId")!, "datacenter") : "",
      nodeId: boot.nodeId,
      hostname: opt(spec, "hostname") ?? "",
    },
  };
}

function vmRequest(spec: Record<string, unknown>): Record<string, unknown> {
  const name = assertName(required(spec, "name", "VM name"), "VM name");
  const network = assertName(required(spec, "network", "network"), "network");
  const backend = backendEnum(required(spec, "storageBackend", "storage backend"));
  const nics = [{ network, model: "virtio", macAddress: "" }];
  if (Array.isArray(spec.extraNetworks)) {
    for (const item of spec.extraNetworks) {
      if (typeof item === "string" && item.trim()) nics.push({ network: assertName(item, "extra network"), model: "virtio", macAddress: "" });
    }
  }
  const image = imageFields(spec);
  const memory = opt(spec, "memoryBytes") ?? opt(spec, "memory");
  return {
    targetNode: opt(spec, "targetNode") ?? "",
    targetDc: opt(spec, "dc") ?? "",
    storageBackend: backend,
    storageSizeBytes: sizeBytes(required(spec, "storageSizeBytes", "storage size"), "storage size"),
    imageUrl: image.imageUrl,
    imageSha256: image.imageSha256,
    imagePath: image.imagePath,
    imageFormat: image.imageFormat,
    cloudInitUserData: cloudInit(name, spec),
    sshKeyNames: stringList(spec.sshKeys).map((item) => assertName(item, "ssh key name")),
    nodeLabels: stringList(spec.nodeLabels),
    antiAffinity: opt(spec, "antiAffinity") ? assertName(opt(spec, "antiAffinity")!, "anti-affinity") : "",
    spec: {
      name,
      cpu: opt(spec, "cpu") ? Number(assertArg(opt(spec, "cpu")!, "cpu")) : 2,
      memoryBytes: memory ? sizeBytes(memory, "memory") : sizeBytes("2G", "memory"),
      nics,
      disks: [],
      desiredState: desired(spec.desiredState),
    },
  };
}

function imageFields(spec: Record<string, unknown>): { imageUrl: string; imageSha256: string; imagePath: string; imageFormat: string } {
  if (opt(spec, "imageUrl")) {
    return {
      imageUrl: assertHttpsUrl(opt(spec, "imageUrl")!),
      imageSha256: assertSha256(required(spec, "imageSha256", "image SHA256")),
      imagePath: "",
      imageFormat: opt(spec, "imageFormat") ?? "",
    };
  }
  if (opt(spec, "imagePath")) {
    const format = required(spec, "imageFormat", "image format");
    if (format !== "qcow2" && format !== "raw") throw new InputError("image format must be qcow2 or raw");
    return { imageUrl: "", imageSha256: "", imagePath: assertArg(opt(spec, "imagePath")!, "image path"), imageFormat: format };
  }
  throw new InputError("an https image URL or a node-local image path is required");
}

function cloudInit(hostname: string, spec: Record<string, unknown>): string {
  if (opt(spec, "password")) {
    throw new InputError("Use an SSH public key. Password login stays off unless cloudInitUserData is set and compliant is false.");
  }
  if (opt(spec, "cloudInitUserData")) return opt(spec, "cloudInitUserData")!;
  const keys = stringList(spec.sshPublicKeys).map((item) => assertSshPublicKey(item));
  if (keys.length === 0) return "";
  const username = assertName(opt(spec, "username") ?? "kcore", "username");
  const lines = [
    "#cloud-config",
    `hostname: ${hostname}`,
    "users:",
    "  - default",
    `  - name: ${username}`,
    "    gecos: kcore VM user",
    "    groups: [sudo]",
    "    shell: /bin/bash",
    "    lock_passwd: true",
    "    ssh_authorized_keys:",
    ...keys.map((key) => `      - "${key.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`),
    "ssh_pwauth: false",
  ];
  return lines.join("\n") + "\n";
}

function networkRequest(spec: Record<string, unknown>): Record<string, unknown> {
  const type = required(spec, "type", "network type");
  if (!["nat", "bridge", "vxlan"].includes(type)) throw new InputError("network type must be nat, bridge, or vxlan");
  return {
    name: assertName(required(spec, "name", "network name"), "network name"),
    externalIp: assertArg(required(spec, "externalIp", "external IP"), "external IP"),
    gatewayIp: assertArg(required(spec, "gatewayIp", "gateway IP"), "gateway IP"),
    internalNetmask: opt(spec, "internalNetmask") ?? "255.255.255.0",
    targetNode: opt(spec, "targetNode") ?? "",
    networkType: type,
    vlanId: opt(spec, "vlanId") ? Number(opt(spec, "vlanId")) : 0,
    enableOutboundNat: boolish(spec.noOutboundNat) ? false : type !== "bridge",
    ipv6Prefix: opt(spec, "ipv6Prefix") ?? "",
    ipv6Gateway: opt(spec, "ipv6Gateway") ?? "",
    eastWestFirewall: boolish(spec.eastWestFirewall),
  };
}

function sshKeyRequest(spec: Record<string, unknown>): Record<string, unknown> {
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    publicKey: assertSshPublicKey(required(spec, "publicKey", "public key")),
  };
}

function containerRequest(spec: Record<string, unknown>): Record<string, unknown> {
  const env: Record<string, string> = {};
  if (spec.env && typeof spec.env === "object" && !Array.isArray(spec.env)) {
    for (const [key, value] of Object.entries(spec.env as Record<string, unknown>)) env[key] = String(value);
  }
  return {
    kind: "WORKLOAD_KIND_CONTAINER",
    containerSpec: {
      name: assertName(required(spec, "name", "container name"), "container name"),
      image: assertArg(required(spec, "image", "image"), "image"),
      network: opt(spec, "network") ? assertName(opt(spec, "network")!, "network") : "",
      command: stringList(spec.command),
      env,
      ports: stringList(spec.ports),
      desiredState: desired(spec.desiredState),
    },
  };
}

function volumeRequest(spec: Record<string, unknown>): Record<string, unknown> {
  return {
    name: assertName(required(spec, "name", "volume name"), "volume name"),
    sizeBytes: sizeBytes(required(spec, "sizeBytes", "size"), "size"),
    storageClass: "ceph",
    vm: opt(spec, "vm") ? assertName(opt(spec, "vm")!, "vm") : "",
    fromSnapshot: opt(spec, "fromSnapshot") ? assertName(opt(spec, "fromSnapshot")!, "snapshot") : "",
    encrypt: boolish(spec.encrypt),
  };
}

function snapshotRequest(spec: Record<string, unknown>): Record<string, unknown> {
  return {
    volume: assertName(required(spec, "volume", "volume"), "volume"),
    name: assertName(required(spec, "name", "snapshot name"), "snapshot name"),
    consistency: "crash",
  };
}

function policyMessage(spec: Record<string, unknown>): Record<string, unknown> {
  if (!opt(spec, "vm") && !opt(spec, "volume")) throw new InputError("a snapshot policy needs a vm or a volume");
  return {
    name: assertName(required(spec, "name", "policy name"), "policy name"),
    selectorVm: opt(spec, "vm") ? assertName(opt(spec, "vm")!, "vm") : "",
    selectorVolume: opt(spec, "volume") ? assertName(opt(spec, "volume")!, "volume") : "",
    schedule: assertArg(opt(spec, "schedule") ?? "@daily", "schedule"),
    keep: Number(opt(spec, "keep") ?? "7"),
    enabled: !boolish(spec.disabled),
  };
}

function securityGroupCalls(spec: Record<string, unknown>): RpcCall[] {
  const name = assertName(required(spec, "name", "name"), "name");
  const calls: RpcCall[] = [
    {
      target: "controller",
      method: "createSecurityGroup",
      request: {
        securityGroup: {
          name,
          description: opt(spec, "description") ?? "",
          rules: rules(spec.rules),
        },
      },
    },
  ];
  if (Array.isArray(spec.attachments)) {
    for (const attachment of spec.attachments) {
      if (!attachment || typeof attachment !== "object") continue;
      const row = attachment as Record<string, unknown>;
      const kind = String(row.kind ?? row.targetKind ?? "");
      calls.push({
        target: "controller",
        method: "attachSecurityGroup",
        request: {
          securityGroup: name,
          targetKind: kind === "network" ? "SECURITY_GROUP_TARGET_KIND_NETWORK" : "SECURITY_GROUP_TARGET_KIND_VM",
          targetId: assertArg(String(row.target ?? row.targetId ?? ""), "attachment target"),
          targetNode: typeof row.node === "string" ? row.node : "",
        },
      });
    }
  }
  return calls;
}

function rules(value: unknown): Record<string, unknown>[] {
  const items = Array.isArray(value) ? value : typeof value === "string" ? [value] : [];
  if (items.length === 0) throw new InputError("a security group needs at least one rule");
  return items.map((item) => {
    if (typeof item === "string") {
      const match = /^(tcp|udp)\/(\d+)(?:\s+from\s+(\S+))?/i.exec(item.trim());
      if (!match) throw new InputError(`rule must look like tcp/443 from 10.0.0.0/8, got ${item}`);
      return { protocol: match[1].toLowerCase(), hostPort: Number(match[2]), sourceCidr: match[3] ?? "" };
    }
    if (!item || typeof item !== "object") throw new InputError("a security group rule must be an object or tcp/port text");
    const row = item as Record<string, unknown>;
    return {
      protocol: String(row.protocol ?? ""),
      hostPort: Number(row.hostPort ?? 0),
      targetPort: Number(row.targetPort ?? 0),
      sourceCidr: String(row.sourceCidr ?? ""),
      targetVm: String(row.targetVm ?? ""),
      enableDnat: boolish(row.enableDnat),
    };
  });
}

function diskLayoutMessage(spec: Record<string, unknown>): Record<string, unknown> {
  const layout = opt(spec, "layoutNix") || readLayoutFile(opt(spec, "layoutNixFile"));
  if (!layout && !spec.diskLayout) throw new InputError("a disk layout needs layoutNix, layoutNixFile, or diskLayout");
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    nodeId: assertName(required(spec, "nodeId", "node id"), "node id"),
    layoutNix: layout || "",
  };
}

function readLayoutFile(path: string | undefined): string {
  if (!path) return "";
  return readFileSync(assertArg(path, "layout file"), "utf8");
}

function cephMessage(spec: Record<string, unknown>): Record<string, unknown> {
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    spec: {
      fsid: opt(spec, "fsid") ?? "",
      publicNetwork: assertArg(required(spec, "publicNetwork", "public network"), "public network"),
      clusterNetwork: assertArg(required(spec, "clusterNetwork", "cluster network"), "cluster network"),
      size: opt(spec, "size") ? Number(opt(spec, "size")) : 0,
      minSize: opt(spec, "minSize") ? Number(opt(spec, "minSize")) : 0,
      forceWipe: boolish(spec.forceWipe),
      encryptOsds: spec.encryptOsds === undefined ? true : boolish(spec.encryptOsds),
      nodes: Array.isArray(spec.nodes) ? spec.nodes : [],
    },
  };
}

function filesystemMessage(spec: Record<string, unknown>): Record<string, unknown> {
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    spec: {
      cephCluster: assertName(required(spec, "cephCluster", "ceph cluster"), "ceph cluster"),
      quotaBytes: sizeBytes(opt(spec, "quotaBytes") ?? "0", "quota"),
      clients: Array.isArray(spec.clients) ? spec.clients : [],
      fsName: opt(spec, "fsName") ?? "",
    },
  };
}

function objectStoreMessage(spec: Record<string, unknown>): Record<string, unknown> {
  const members = stringList(spec.members).map((item) => assertName(item, "member"));
  if (members.length === 0) throw new InputError("an object store needs at least one member node");
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    spec: {
      cephCluster: assertName(required(spec, "cephCluster", "ceph cluster"), "ceph cluster"),
      members,
      port: opt(spec, "port") ? Number(opt(spec, "port")) : 0,
      tls: boolish(spec.tls),
    },
  };
}

function clusterUpdateMessage(spec: Record<string, unknown>): Record<string, unknown> {
  const strategy = opt(spec, "strategy") ?? "one-at-a-time";
  return {
    name: assertName(required(spec, "name", "name"), "name"),
    target: {
      version: assertArg(required(spec, "version", "version"), "version"),
      flakeRef: assertArg(required(spec, "flakeRef", "flake ref"), "flake ref"),
      flakeRev: opt(spec, "flakeRev") ?? "",
    },
    strategy: {
      type: strategy === "one-at-a-time" ? "CLUSTER_UPDATE_STRATEGY_ONE_AT_A_TIME" : "CLUSTER_UPDATE_STRATEGY_UNSPECIFIED",
      maxUnavailable: 1,
    },
    drainVms: spec.drainVms === undefined ? true : boolish(spec.drainVms),
  };
}

function backendEnum(value: string): string {
  const mapped = BACKEND[value.trim().toLowerCase()];
  if (!mapped) throw new InputError("storage backend must be filesystem, lvm, zfs, or ceph");
  return mapped;
}

function desired(value: unknown): string {
  if (value === "running") return "VM_DESIRED_STATE_RUNNING";
  if (value === "stopped") return "VM_DESIRED_STATE_STOPPED";
  if (value === undefined || value === "") return "VM_DESIRED_STATE_UNSPECIFIED";
  throw new InputError("desired state must be running or stopped");
}

function sizeBytes(value: string | number, label: string): string {
  if (typeof value === "number") {
    if (!Number.isFinite(value) || value < 0) throw new InputError(`${label} must be a byte count`);
    return String(Math.trunc(value));
  }
  const text = value.trim();
  if (/^\d+$/.test(text)) return text;
  const match = /^([1-9]\d*)\s*([KMGT])i?B?$/i.exec(text);
  if (!match) throw new InputError(`${label} must be a byte count or a size such as 40G`);
  const unit: Record<string, bigint> = { K: 1024n, M: 1024n ** 2n, G: 1024n ** 3n, T: 1024n ** 4n };
  return (BigInt(match[1]) * unit[match[2].toUpperCase()]).toString();
}

function disks(spec: Record<string, unknown>): string[] {
  const value = spec.dataDisks ?? spec.dataDisk;
  if (Array.isArray(value)) return value.filter((item) => typeof item === "string" && item.trim()).map((item) => assertDisk(String(item), "data disk"));
  if (typeof value === "string" && value.trim()) return [assertDisk(value, "data disk")];
  return [];
}

function hostOf(address: string): string {
  if (address.startsWith("[")) {
    const end = address.indexOf("]");
    return end > 1 ? address.slice(1, end) : address;
  }
  return address.slice(0, address.lastIndexOf(":"));
}

function stringList(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((item) => typeof item === "string" && item.trim()).map((item) => item.trim());
}

function opt(spec: Record<string, unknown>, key: string): string | undefined {
  const value = spec[key];
  if (typeof value === "number") return String(value);
  if (typeof value !== "string") return undefined;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function required(spec: Record<string, unknown>, key: string, label: string): string {
  const value = opt(spec, key);
  if (!value) throw new InputError(`${label} is required`);
  return value;
}

function boolish(value: unknown): boolean {
  return value === true || value === "true" || value === "yes";
}
