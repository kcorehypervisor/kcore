import { stringify } from "yaml";
import type { ResourceKind } from "./catalog.js";
import {
  InputError,
  assertAddress,
  assertArg,
  assertDisk,
  assertHttpsUrl,
  assertName,
  assertSha256,
  assertSize,
  assertSshPublicKey,
} from "./names.js";

export type Rendered = {
  yaml?: string;
  args: string[];
  fileArgIndex?: number;
};

function doc(kind: string, name: string, spec: Record<string, unknown>): string {
  return stringify({
    kind,
    metadata: { name },
    spec,
  });
}

function opt(spec: Record<string, unknown>, key: string): string | undefined {
  const value = spec[key];
  if (typeof value !== "string") return undefined;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function boolish(value: unknown): boolean {
  return value === true || value === "true" || value === "yes";
}

const PASSTHROUGH = new Set([
  "security-group",
  "disk-layout",
  "ceph-cluster",
  "shared-filesystem",
  "object-store",
  "cluster-update",
]);

export function renderApply(kind: ResourceKind, spec: Record<string, unknown>, manifest?: string): Rendered {
  if (PASSTHROUGH.has(kind.id)) {
    if (!manifest || !manifest.trim()) {
      throw new InputError(`${kind.id} apply needs the manifest YAML. Ask for it, then pass manifest.`);
    }
    const args = kind.id === "cluster-update" ? ["update", "cluster", "apply", "-f", ""] : ["apply", "-f", ""];
    return { yaml: manifest, args, fileArgIndex: args.indexOf("-f") + 1 };
  }

  switch (kind.id) {
    case "cluster":
      return yamlApply(
        doc("Cluster", assertName(opt(spec, "name") ?? "default", "context name"), {
          controller: assertAddress(required(spec, "controller", "controller"), "controller"),
          ...(opt(spec, "certsDir") ? { certsDir: assertArg(opt(spec, "certsDir")!, "certsDir") } : {}),
          ...(boolish(spec.force) ? { force: true } : {}),
        }),
      );
    case "vm":
      return yamlApply(renderVm(spec));
    case "network":
      return yamlApply(renderNetwork(spec));
    case "ssh-key":
      return yamlApply(
        doc("SshKey", assertName(required(spec, "name", "name"), "name"), {
          publicKey: assertSshPublicKey(required(spec, "publicKey", "public key")),
        }),
      );
    case "container":
      return yamlApply(renderContainer(spec));
    case "volume":
      return { args: volumeArgs(spec) };
    case "volume-snapshot":
      return { args: snapshotArgs(spec) };
    case "snapshot-policy":
      return { args: policyArgs(spec) };
    default:
      throw new InputError(`unsupported kind ${kind.id}`);
  }
}

export function renderPlanCommand(kind: ResourceKind): string[] {
  if (kind.id === "disk-layout") return ["diff", "-f", ""];
  if (kind.id === "cluster-update") return ["update", "cluster", "plan", "-f", ""];
  return ["apply", "-f", "", "--dry-run"];
}

function yamlApply(yaml: string): Rendered {
  return { yaml, args: ["apply", "-f", ""], fileArgIndex: 2 };
}

function required(spec: Record<string, unknown>, key: string, label: string): string {
  const value = spec[key];
  if (typeof value === "number") return String(value);
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new InputError(`${label} is required`);
  }
  return value.trim();
}

function renderVm(spec: Record<string, unknown>): string {
  const name = assertName(required(spec, "name", "VM name"), "VM name");
  const network = assertName(required(spec, "network", "network"), "network");
  const backend = required(spec, "storageBackend", "storage backend");
  if (!["filesystem", "lvm", "zfs", "ceph"].includes(backend)) {
    throw new InputError("storage backend must be filesystem, lvm, zfs, or ceph");
  }
  const size = assertSize(required(spec, "storageSizeBytes", "storage size"), "storage size");
  const disk: Record<string, string> = {};
  if (opt(spec, "imageUrl")) {
    disk.image = assertHttpsUrl(opt(spec, "imageUrl")!);
    disk.sha256 = assertSha256(required(spec, "imageSha256", "image SHA256"));
    if (opt(spec, "imageFormat")) disk.format = assertArg(opt(spec, "imageFormat")!, "image format");
  } else if (opt(spec, "imagePath")) {
    disk.path = assertArg(opt(spec, "imagePath")!, "image path");
    const format = required(spec, "imageFormat", "image format");
    if (format !== "qcow2" && format !== "raw") throw new InputError("image format must be qcow2 or raw");
    disk.format = format;
  } else {
    throw new InputError("an https image URL or a node-local image path is required");
  }

  const nics = [{ network }];
  const extra = spec.extraNetworks;
  if (Array.isArray(extra)) {
    for (const item of extra) {
      if (typeof item === "string" && item.trim()) nics.push({ network: assertName(item, "extra network") });
    }
  }

  const body: Record<string, unknown> = {
    storageBackend: backend,
    storageSizeBytes: /^\d+$/.test(size) ? Number(size) : size,
    nics,
    disks: [disk],
  };
  if (opt(spec, "cpu")) body.cpu = Number(assertArg(opt(spec, "cpu")!, "cpu"));
  const memory = opt(spec, "memoryBytes") ?? opt(spec, "memory");
  if (memory) body.memoryBytes = /^\d+$/.test(memory) ? Number(memory) : assertSize(memory, "memory");
  if (opt(spec, "desiredState")) {
    const state = opt(spec, "desiredState")!;
    if (state !== "running" && state !== "stopped") {
      throw new InputError("desired state must be running or stopped");
    }
    body.desiredState = state;
  }
  if (opt(spec, "targetNode")) body.targetNode = assertArg(opt(spec, "targetNode")!, "target node");
  if (opt(spec, "dc")) body.dc = assertName(opt(spec, "dc")!, "datacenter");
  if (Array.isArray(spec.sshKeys)) {
    body.sshKeys = spec.sshKeys.filter((item) => typeof item === "string" && item.trim()).map((item) => assertName(String(item), "ssh key name"));
  }
  if (opt(spec, "cloudInitUserData")) body.cloudInitUserData = opt(spec, "cloudInitUserData");
  if (opt(spec, "password")) {
    throw new InputError(
      "Use an SSH public key. Password login stays off unless the operator writes cloudInitUserData and sets compliant to false.",
    );
  }
  if (Array.isArray(spec.sshPublicKeys) && spec.sshPublicKeys.length > 0 && !body.cloudInitUserData) {
    const keys = spec.sshPublicKeys.filter((item) => typeof item === "string").map((item) => assertSshPublicKey(String(item)));
    const username = assertName(opt(spec, "username") ?? "kcore", "username");
    body.cloudInitUserData = cloudInit(name, username, keys);
  }
  if (Array.isArray(spec.nodeLabels)) body.nodeLabels = spec.nodeLabels;
  if (opt(spec, "antiAffinity")) body.antiAffinity = assertName(opt(spec, "antiAffinity")!, "anti-affinity");
  return doc("VM", name, body);
}

function cloudInit(hostname: string, username: string, keys: string[]): string {
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
  ];
  for (const key of keys) lines.push(`      - "${key.replaceAll("\\", "\\\\").replaceAll('"', '\\"')}"`);
  lines.push("ssh_pwauth: false");
  return lines.join("\n") + "\n";
}

function renderNetwork(spec: Record<string, unknown>): string {
  const name = assertName(required(spec, "name", "network name"), "network name");
  const type = required(spec, "type", "network type");
  if (!["nat", "bridge", "vxlan"].includes(type)) {
    throw new InputError("network type must be nat, bridge, or vxlan");
  }
  const body: Record<string, unknown> = {
    type,
    externalIp: assertArg(required(spec, "externalIp", "external IP"), "external IP"),
    gatewayIp: assertArg(required(spec, "gatewayIp", "gateway IP"), "gateway IP"),
  };
  if (opt(spec, "internalNetmask")) body.internalNetmask = assertArg(opt(spec, "internalNetmask")!, "netmask");
  if (opt(spec, "targetNode")) body.targetNode = assertArg(opt(spec, "targetNode")!, "target node");
  if (opt(spec, "vlanId")) body.vlanId = Number(opt(spec, "vlanId"));
  if (boolish(spec.noOutboundNat)) body.enableOutboundNat = false;
  if (opt(spec, "ipv6Prefix")) body.ipv6Prefix = assertArg(opt(spec, "ipv6Prefix")!, "ipv6 prefix");
  if (opt(spec, "ipv6Gateway")) body.ipv6Gateway = assertArg(opt(spec, "ipv6Gateway")!, "ipv6 gateway");
  if (boolish(spec.eastWestFirewall)) body.eastWestFirewall = true;
  return doc("Network", name, body);
}

function renderContainer(spec: Record<string, unknown>): string {
  const name = assertName(required(spec, "name", "container name"), "container name");
  const body: Record<string, unknown> = {
    image: assertArg(required(spec, "image", "image"), "image"),
  };
  if (opt(spec, "network")) body.network = assertName(opt(spec, "network")!, "network");
  if (Array.isArray(spec.ports)) body.ports = spec.ports;
  if (spec.env && typeof spec.env === "object") body.env = spec.env;
  if (Array.isArray(spec.command)) body.command = spec.command;
  if (opt(spec, "desiredState")) body.desiredState = opt(spec, "desiredState");
  return doc("Container", name, body);
}

function volumeArgs(spec: Record<string, unknown>): string[] {
  const name = assertName(required(spec, "name", "volume name"), "volume name");
  const size = assertSize(required(spec, "sizeBytes", "size"), "size");
  const args = ["create", "volume", name, "--size-bytes", size];
  if (opt(spec, "vm")) args.push("--vm", assertName(opt(spec, "vm")!, "vm"));
  if (opt(spec, "fromSnapshot")) args.push("--from-snapshot", assertName(opt(spec, "fromSnapshot")!, "snapshot"));
  if (boolish(spec.encrypt)) args.push("--encrypt");
  return args;
}

function snapshotArgs(spec: Record<string, unknown>): string[] {
  const volume = assertName(required(spec, "volume", "volume"), "volume");
  const name = assertName(required(spec, "name", "snapshot name"), "snapshot name");
  return ["create", "volume-snapshot", volume, "--name", name];
}

function policyArgs(spec: Record<string, unknown>): string[] {
  const name = assertName(required(spec, "name", "policy name"), "policy name");
  const args = ["create", "snapshot-policy", name];
  if (opt(spec, "vm")) args.push("--vm", assertName(opt(spec, "vm")!, "vm"));
  if (opt(spec, "volume")) args.push("--volume", assertName(opt(spec, "volume")!, "volume"));
  if (!opt(spec, "vm") && !opt(spec, "volume")) {
    throw new InputError("a snapshot policy needs a vm or a volume");
  }
  args.push("--schedule", assertArg(opt(spec, "schedule") ?? "@daily", "schedule"));
  args.push("--keep", assertArg(String(spec.keep ?? "7"), "keep"));
  if (boolish(spec.disabled)) args.push("--disabled");
  return args;
}

export function installNodeArgs(spec: Record<string, unknown>): { args: string[]; node: string } {
  const node = assertAddress(required(spec, "node", "node address"), "node address");
  const args = [
    "node",
    "install",
    "--os-disk",
    assertDisk(required(spec, "osDisk", "OS disk"), "OS disk"),
    "--join-controller",
    assertAddress(required(spec, "joinController", "join controller"), "join controller"),
  ];
  const dataDisks = spec.dataDisks ?? spec.dataDisk;
  if (Array.isArray(dataDisks)) {
    for (const disk of dataDisks) {
      if (typeof disk === "string" && disk.trim()) args.push("--data-disk", assertDisk(disk, "data disk"));
    }
  } else if (typeof dataDisks === "string" && dataDisks.trim()) {
    args.push("--data-disk", assertDisk(dataDisks, "data disk"));
  }
  if (opt(spec, "storageBackend")) {
    args.push("--storage-backend", assertArg(opt(spec, "storageBackend")!, "storage backend"));
  }
  if (opt(spec, "dcId")) args.push("--dc-id", assertName(opt(spec, "dcId")!, "datacenter"));
  if (boolish(spec.disableVxlan)) args.push("--disable-vxlan");
  return { args, node };
}

export function operationArgs(action: string, spec: Record<string, unknown>): string[] {
  switch (action) {
    case "set-vm-state": {
      const name = assertName(required(spec, "name", "VM name"), "VM name");
      const state = required(spec, "state", "state");
      if (state !== "running" && state !== "stopped") throw new InputError("state must be running or stopped");
      const args = ["set", "vm", name, "--state", state];
      if (opt(spec, "targetNode")) args.push("--target-node", assertArg(opt(spec, "targetNode")!, "target node"));
      return args;
    }
    case "migrate-vm": {
      const name = assertName(required(spec, "name", "VM name"), "VM name");
      const args = ["migrate", "vm", name, "--target-node", assertArg(required(spec, "targetNode", "target node"), "target node")];
      if (boolish(spec.allowColdFallback)) args.push("--allow-cold-fallback");
      return args;
    }
    case "drain-node": {
      const args = ["drain", "node", assertName(required(spec, "nodeId", "node id"), "node id")];
      if (opt(spec, "targetNode")) args.push("--target-node", assertArg(opt(spec, "targetNode")!, "target node"));
      return args;
    }
    case "cordon-node":
      return ["node", "cordon", assertName(required(spec, "nodeId", "node id"), "node id")];
    case "uncordon-node":
      return ["node", "uncordon", assertName(required(spec, "nodeId", "node id"), "node id")];
    case "approve-node":
      return ["node", "approve", assertName(required(spec, "nodeId", "node id"), "node id")];
    case "reject-node":
      return ["node", "reject", assertName(required(spec, "nodeId", "node id"), "node id")];
    case "approve-update":
      return ["update", "cluster", "approve", assertName(required(spec, "name", "update name"), "update name")];
    case "cancel-update":
      return ["update", "cluster", "cancel", assertName(required(spec, "name", "update name"), "update name")];
    case "rollback-update":
      return ["update", "cluster", "rollback", assertName(required(spec, "name", "update name"), "update name")];
    default:
      throw new InputError(
        "action must be set-vm-state, migrate-vm, drain-node, cordon-node, uncordon-node, approve-node, reject-node, approve-update, cancel-update, rollback-update, or install-node",
      );
  }
}
