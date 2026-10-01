import { parse as parseYaml } from "yaml";
import { findKind, kindIds, type ResourceKind } from "./catalog.js";
import { isBlank } from "./names.js";

export type Question = {
  id: string;
  prompt: string;
  why: string;
  required: boolean;
  choices?: string[];
  example?: string;
};

export type Advice = {
  kind: string | null;
  ready: boolean;
  blocking: Question[];
  recommended: Question[];
  warnings: string[];
  askTheUser: string;
  nextTool: "kcore_advise" | "kcore_plan";
};

export type Desired = {
  kind: ResourceKind;
  name?: string;
  spec: Record<string, unknown>;
  manifest?: string;
};

const INTENT: Array<[RegExp, string]> = [
  [/\bvirtual machines?\b|\bvms?\b/i, "vm"],
  [/\bnetworks?\b|\bvxlan\b|\bbridge\b/i, "network"],
  [/\bssh keys?\b/i, "ssh-key"],
  [/\bpostgres(?:ql)?\b/i, "postgresql"],
  [/\bcontainers?\b/i, "container"],
  [/\bvolumes?\b/i, "volume"],
  [/\bsnapshot polic/i, "snapshot-policy"],
  [/\bsnapshots?\b/i, "volume-snapshot"],
  [/\bsecurity groups?\b|\bfirewall\b/i, "security-group"],
  [/\bdisk layouts?\b|\bdisko\b/i, "disk-layout"],
  [/\bceph\b/i, "ceph-cluster"],
  [/\bcephfs\b|\bshared filesystems?\b/i, "shared-filesystem"],
  [/\bobject stores?\b|\brgw\b|\bs3\b/i, "object-store"],
  [/\bcluster updates?\b|\brolling update\b/i, "cluster-update"],
  [/\bclusters?\b|\bcontext\b|\bbootstrap\b/i, "cluster"],
];

export function inferKind(intent: string | undefined): string | undefined {
  if (!intent) return undefined;
  const hits = new Set<string>();
  for (const [pattern, id] of INTENT) {
    if (pattern.test(intent)) hits.add(id);
  }
  if (hits.size !== 1) return undefined;
  return [...hits][0];
}

function str(spec: Record<string, unknown>, key: string): string {
  const value = spec[key];
  return typeof value === "string" ? value.trim() : "";
}

function question(
  id: string,
  prompt: string,
  why: string,
  required: boolean,
  extra?: Pick<Question, "choices" | "example">,
): Question {
  return { id, prompt, why, required, ...extra };
}

function missing(spec: Record<string, unknown>, key: string): boolean {
  return isBlank(spec[key]);
}

export function questionsFor(kind: ResourceKind, spec: Record<string, unknown>): Question[] {
  switch (kind.id) {
    case "cluster":
      return [
        question(
          "controller",
          "What is the controller address (host:port)?",
          "The MCP generates a CA and a client certificate for this address, then dials it over mTLS.",
          missing(spec, "controller"),
          { example: "10.0.0.10:9090" },
        ),
        question(
          "name",
          "What should the local context be called?",
          "The context name selects this cluster in ~/.kcore/config. 'default' is fine for a single cluster.",
          false,
          { example: "default" },
        ),
        question(
          "force",
          "Overwrite existing certificates for this context?",
          "Overwriting replaces the trust root. Existing controllers and nodes that use the current certificates will need new material.",
          false,
          { choices: ["no", "yes"] },
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "vm":
      return vmQuestions(spec);
    case "network":
      return [
        question("name", "What should the network be called?", "VMs attach to this name.", missing(spec, "name"), {
          example: "frontend",
        }),
        question(
          "type",
          "Which network type: nat, bridge, or vxlan?",
          "nat hides guests behind one address. bridge puts them on a physical LAN. vxlan is the cross-host overlay.",
          missing(spec, "type"),
          { choices: ["nat", "bridge", "vxlan"] },
        ),
        question(
          "externalIp",
          "Which external IP should this network use?",
          "NAT and DNAT use this address. Bridge networks use an address on the LAN.",
          missing(spec, "externalIp"),
          { example: "203.0.113.10" },
        ),
        question(
          "gatewayIp",
          "Which gateway IP should the bridge use?",
          "This is the address guests use as their default gateway.",
          missing(spec, "gatewayIp"),
          { example: "10.240.10.1" },
        ),
        question(
          "internalNetmask",
          "What is the internal netmask?",
          "Defaults to 255.255.255.0 when you accept defaults.",
          false,
          { example: "255.255.255.0" },
        ),
        question(
          "eastWestFirewall",
          "Drop VM-to-VM traffic on this bridge?",
          "Leaves the gateway and DHCP reachable and drops the rest unless a security group allows it.",
          false,
          { choices: ["no", "yes"] },
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "ssh-key":
      return [
        question("name", "What should this SSH key be called?", "VMs refer to the key by this name.", missing(spec, "name")),
        question(
          "publicKey",
          "Paste the SSH public key.",
          "The controller stores the public key and can inject it into cloud-init. The private key stays with the operator.",
          missing(spec, "publicKey"),
          { example: "ssh-ed25519 AAAA... operator@host" },
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "postgresql":
      return [
        question(
          "name",
          "What should this PostgreSQL instance be called?",
          "The name identifies the resource. It is also the database name when you leave database empty and the name is a PostgreSQL identifier.",
          missing(spec, "name"),
          { example: "app" },
        ),
        question(
          "database",
          "What is the single database name?",
          "NixOS ensureDatabases creates this database. Use letters, digits, and underscores. Defaults to the instance name.",
          false,
          { example: "app" },
        ),
        question(
          "package",
          "Which NixOS PostgreSQL package should it use?",
          "postgresql tracks the nixpkgs default. Pin postgresql_16 when the major version must stay put. This choice is immutable.",
          false,
          { choices: ["postgresql", "postgresql_14", "postgresql_15", "postgresql_16", "postgresql_17"] },
        ),
        question(
          "port",
          "Which port should PostgreSQL use?",
          "Defaults to 5432. v1 still serves clients on the local Unix socket.",
          false,
          { example: "5432" },
        ),
        question(
          "targetNode",
          "Pin it to a node, or let the scheduler choose a free one?",
          "v1 runs one PostgreSQL database on a node. targetNode is immutable.",
          false,
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "container":
      return [
        question("name", "What should the container be called?", "This is the workload name.", missing(spec, "name")),
        question(
          "image",
          "Which OCI image should it run?",
          "Example: nginx:alpine. The image reference is immutable after create.",
          missing(spec, "image"),
          { example: "nginx:alpine" },
        ),
        question(
          "network",
          "Which kcore network should it join?",
          "Optional. Leave empty only if the container should use the runtime default.",
          false,
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "volume":
      return [
        question("name", "What should the volume be called?", "The name is how VMs and snapshots refer to it.", missing(spec, "name")),
        question(
          "sizeBytes",
          "How large should the volume be, in bytes?",
          "Ceph data volume size. This size is fixed at create.",
          missing(spec, "sizeBytes"),
          { example: "10737418240" },
        ),
        question(
          "encrypt",
          "Encrypt the volume with host-side LUKS?",
          "Encryption at rest wraps the data key with the cluster master key. Say yes for data volumes.",
          false,
          { choices: ["yes", "no"] },
        ),
        question(
          "vm",
          "Attach it to a stopped VM now, or leave it detached?",
          "Attach only works while that VM is stopped.",
          false,
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    case "volume-snapshot":
      return [
        question("volume", "Which volume should be snapshotted?", "Name or id of an existing volume.", missing(spec, "volume")),
        question("name", "What should the snapshot be called?", "A name makes later restore unambiguous.", missing(spec, "name")),
      ].filter((item) => item.required);
    case "snapshot-policy":
      return [
        question("name", "What should the snapshot policy be called?", "Policies are reconciled by name.", missing(spec, "name")),
        question(
          "target",
          "Should this policy cover a VM or a single volume?",
          "Provide vm or volume. One selector is enough.",
          missing(spec, "vm") && missing(spec, "volume"),
          { choices: ["vm", "volume"] },
        ),
        question(
          "schedule",
          "How often should it snapshot?",
          "@hourly, @daily, or every:<seconds>.",
          missing(spec, "schedule"),
          { choices: ["@hourly", "@daily"], example: "@daily" },
        ),
        question(
          "keep",
          "How many snapshots should each volume retain?",
          "Older snapshots beyond this count are removed.",
          missing(spec, "keep"),
          { example: "7" },
        ),
      ].filter((item) => item.required);
    case "security-group":
      return [
        question("name", "What should the security group be called?", "Attachments refer to this name.", missing(spec, "name")),
        question(
          "rules",
          "Which ports and protocols should it allow?",
          "Each rule needs protocol and hostPort. sourceCidr limits who can connect.",
          missing(spec, "rules"),
          { example: "tcp/443 from 0.0.0.0/0" },
        ),
      ].filter((item) => item.required);
    case "disk-layout":
      return [
        question("name", "What should the disk layout be called?", "The controller stores the layout under this name.", missing(spec, "name")),
        question("nodeId", "Which node id should receive this layout?", "The reconciler pushes the layout only to that node.", missing(spec, "nodeId")),
        question(
          "layout",
          "Paste the layout: layoutNix, layoutNixFile, or diskLayout.",
          "Exactly one of those three. Run plan first so the classifier can refuse a disk that still backs a VM.",
          missing(spec, "layoutNix") && missing(spec, "layoutNixFile") && missing(spec, "diskLayout"),
        ),
      ].filter((item) => item.required);
    case "ceph-cluster":
      return [
        question("name", "What should the Ceph cluster be called?", "Volumes and filesystems refer to this name.", missing(spec, "name")),
        question(
          "publicNetwork",
          "What is the Ceph public network CIDR?",
          "Clients and monitors use this network.",
          missing(spec, "publicNetwork"),
          { example: "10.0.0.0/24" },
        ),
        question(
          "clusterNetwork",
          "What is the Ceph cluster network CIDR?",
          "Replication traffic uses this network. It can match the public network on a small cluster.",
          missing(spec, "clusterNetwork"),
          { example: "10.0.0.0/24" },
        ),
      ].filter((item) => item.required);
    case "shared-filesystem":
      return [
        question("name", "What should the shared filesystem be called?", "Clients mount it by this name.", missing(spec, "name")),
        question(
          "cephCluster",
          "Which Ceph cluster should back it?",
          "The filesystem is created inside that cluster.",
          missing(spec, "cephCluster"),
        ),
        question(
          "quotaBytes",
          "What quota, in bytes, should the filesystem have?",
          "0 means no quota. A positive quota caps the filesystem.",
          missing(spec, "quotaBytes"),
          { example: "0" },
        ),
      ].filter((item) => item.required);
    case "object-store":
      return [
        question("name", "What should the object store be called?", "The RGW service is reconciled under this name.", missing(spec, "name")),
        question("cephCluster", "Which Ceph cluster should back it?", "RGW stores buckets in that cluster.", missing(spec, "cephCluster")),
        question(
          "members",
          "Which node ids should run the gateway?",
          "At least one node id.",
          missing(spec, "members"),
          { example: "node-a" },
        ),
      ].filter((item) => item.required);
    case "cluster-update":
      return [
        question("name", "What should the cluster update be called?", "Approve, cancel, and rollback use this name.", missing(spec, "name")),
        question("version", "Which kcore version is the target?", "The rolling update converges nodes to this version.", missing(spec, "version"), {
          example: "0.3.0",
        }),
        question(
          "flakeRef",
          "Which Nix flake ref should nodes build?",
          "Plan resolves this ref before any node reboots.",
          missing(spec, "flakeRef"),
        ),
        question(
          "strategy",
          "One node at a time, or a wider batch?",
          "one-at-a-time keeps a quorum while a node reboots.",
          missing(spec, "strategy"),
          { choices: ["one-at-a-time"], example: "one-at-a-time" },
        ),
      ].filter((item) => item.required || missing(spec, item.id));
    default:
      return [];
  }
}

function vmQuestions(spec: Record<string, unknown>): Question[] {
  const hasUrl = !missing(spec, "imageUrl");
  const hasPath = !missing(spec, "imagePath");
  const hasKeyName = !missing(spec, "sshKeys");
  const hasPublicKey = !missing(spec, "sshPublicKeys");
  const hasCloudInit = !missing(spec, "cloudInitUserData");
  const password = str(spec, "password");
  const compliant = spec.compliant !== false && spec.compliant !== "false";
  const authed = hasKeyName || hasPublicKey || hasCloudInit || (password.length > 0 && !compliant);

  const items: Question[] = [
    question("name", "What should the VM be called?", "The name is the resource id you will start, stop, and migrate.", missing(spec, "name"), {
      example: "web-01",
    }),
    question(
      "image",
      "Which boot image should it use: an https URL plus SHA256, or a qcow2/raw path already on the node?",
      "kcore checks the SHA256 for URL images. ISO uploads are unsupported.",
      !hasUrl && !hasPath,
      { example: "https://cloud.debian.org/.../debian-12-genericcloud-amd64.qcow2" },
    ),
  ];
  if (hasUrl && missing(spec, "imageSha256")) {
    items.push(
      question(
        "imageSha256",
        "What is the SHA256 of that image URL?",
        "The controller refuses a URL image without a 64-character checksum.",
        true,
      ),
    );
  }
  if (hasPath && missing(spec, "imageFormat")) {
    items.push(
      question("imageFormat", "Is the node-local image raw or qcow2?", "The node boots the path with this format.", true, {
        choices: ["qcow2", "raw"],
      }),
    );
  }
  items.push(
    question(
      "network",
      "Which network should the first NIC join?",
      "Create the network first if it does not exist yet.",
      missing(spec, "network"),
      { example: "default" },
    ),
    question(
      "storageBackend",
      "Which storage backend: filesystem, lvm, zfs, or ceph?",
      "The backend is fixed for the life of the VM. ceph is what live migration uses.",
      missing(spec, "storageBackend"),
      { choices: ["filesystem", "lvm", "zfs", "ceph"] },
    ),
    question(
      "storageSizeBytes",
      "How large should the VM disk be?",
      "Bytes, or a size such as 40G. The size is fixed after create.",
      missing(spec, "storageSizeBytes"),
      { example: "40G" },
    ),
    question(
      "auth",
      "Which SSH public key should log in, or which stored ssh-key name should the VM use?",
      "Key login is the compliant default. Password login requires an explicit acknowledgement that it is non-compliant.",
      !authed,
      { example: "ssh-ed25519 AAAA... operator@host" },
    ),
    question("cpu", "How many vCPUs?", "Mutable later. 2 is a reasonable lab default if you accept defaults.", false, {
      example: "2",
    }),
    question("memory", "How much memory?", "Mutable later. A size such as 4G.", false, { example: "4G" }),
    question(
      "desiredState",
      "Should the VM be running or stopped after apply?",
      "This is the desired power state. Omit it to leave an existing VM's power state alone.",
      false,
      { choices: ["running", "stopped"] },
    ),
    question(
      "placement",
      "Pin it to a node or datacenter, or let the scheduler choose?",
      "targetNode is immutable. Leaving it empty lets the controller place the VM.",
      false,
    ),
  );
  return items.filter((item) => {
    if (item.required) return true;
    if (item.id === "cpu") return missing(spec, "cpu");
    if (item.id === "memory") return missing(spec, "memory") && missing(spec, "memoryBytes");
    if (item.id === "desiredState") return missing(spec, "desiredState");
    if (item.id === "placement") return missing(spec, "targetNode") && missing(spec, "dc");
    return true;
  });
}

export function warningsFor(kind: ResourceKind, spec: Record<string, unknown>): string[] {
  const warnings: string[] = [];
  if (kind.id === "vm") {
    const password = str(spec, "password");
    const compliant = spec.compliant !== false && spec.compliant !== "false";
    if (password && compliant) {
      warnings.push(
        "Password login is non-compliant. Apply will be refused until compliant is false and the operator has accepted that risk.",
      );
    }
    if (password && !compliant) {
      warnings.push("This VM will allow password login. Prefer an SSH key for anything beyond a lab.");
    }
    if (str(spec, "storageBackend") === "ceph") {
      warnings.push("Ceph-backed VMs can live-migrate. Confirm the Ceph cluster is healthy before relying on that.");
    }
  }
  if (kind.id === "cluster" && (spec.force === true || spec.force === "yes")) {
    warnings.push("force replaces the local trust root for this context.");
  }
  if (kind.id === "disk-layout") {
    warnings.push("Plan this layout before apply. The node refuses a disk that still backs a VM, an LVM PV, or a ZFS pool member.");
  }
  if (kind.id === "postgresql") {
    warnings.push(
      "v1 is one database per node, from the NixOS postgresql package, on the local Unix socket. Deleting the resource removes it from the node config and leaves /var/lib/postgresql on disk.",
    );
  }
  if (kind.id === "cluster-update") {
    warnings.push("A cluster update can reboot nodes. Plan it, read the blockers, and apply only after the operator agrees.");
  }
  if (kind.immutable.length > 0) {
    warnings.push(
      `Immutable after create: ${kind.immutable.join(", ")}. Changing those later means delete and create, which the operator must agree to.`,
    );
  }
  return warnings;
}

export function normalizeDesired(input: {
  kind?: string;
  intent?: string;
  spec?: Record<string, unknown>;
  manifest?: string;
}): { desired?: Desired; kindError?: string } {
  let doc: Record<string, unknown> | undefined;
  if (input.manifest && input.manifest.trim()) {
    const parsed = parseYaml(input.manifest);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return { kindError: "manifest must be a YAML mapping" };
    }
    doc = parsed as Record<string, unknown>;
  }

  const yamlKind = typeof doc?.kind === "string" ? doc.kind : undefined;
  const kind = findKind(input.kind) ?? findKind(yamlKind) ?? findKind(inferKind(input.intent));
  if (!kind) {
    return {
      kindError: `Which resource? One of: ${kindIds().join(", ")}.`,
    };
  }

  const metadata =
    doc?.metadata && typeof doc.metadata === "object" && !Array.isArray(doc.metadata)
      ? (doc.metadata as Record<string, unknown>)
      : {};
  const manifestSpec =
    doc?.spec && typeof doc.spec === "object" && !Array.isArray(doc.spec)
      ? (doc.spec as Record<string, unknown>)
      : {};
  const spec: Record<string, unknown> = {
    ...manifestSpec,
    ...(input.spec ?? {}),
  };
  if (typeof metadata.name === "string" && isBlank(spec.name)) spec.name = metadata.name;
  if (typeof doc?.metadata === "object" && spec.name === undefined && typeof metadata.name === "string") {
    spec.name = metadata.name;
  }

  flattenManifest(kind.id, spec);
  const name = typeof spec.name === "string" ? spec.name : undefined;
  return {
    desired: {
      kind,
      name,
      spec,
      manifest: input.manifest,
    },
  };
}

function flattenManifest(kindId: string, spec: Record<string, unknown>): void {
  if (kindId !== "vm") return;
  const disks = spec.disks;
  if (Array.isArray(disks) && disks[0] && typeof disks[0] === "object") {
    const disk = disks[0] as Record<string, unknown>;
    if (typeof disk.image === "string" && isBlank(spec.imageUrl)) spec.imageUrl = disk.image;
    if (typeof disk.sha256 === "string" && isBlank(spec.imageSha256)) spec.imageSha256 = disk.sha256;
    if (typeof disk.path === "string" && isBlank(spec.imagePath)) spec.imagePath = disk.path;
    if (typeof disk.format === "string" && isBlank(spec.imageFormat)) spec.imageFormat = disk.format;
  }
  const nics = spec.nics;
  if (Array.isArray(nics)) {
    const networks = nics
      .map((nic) => (nic && typeof nic === "object" ? (nic as Record<string, unknown>).network : undefined))
      .filter((network): network is string => typeof network === "string" && network.trim().length > 0);
    if (networks[0] && isBlank(spec.network)) spec.network = networks[0];
    if (networks.length > 1 && isBlank(spec.extraNetworks)) spec.extraNetworks = networks.slice(1);
  }
}

export function advise(input: {
  kind?: string;
  intent?: string;
  spec?: Record<string, unknown>;
  manifest?: string;
  acceptDefaults?: boolean;
}): Advice {
  const normalized = normalizeDesired(input);
  if (!normalized.desired) {
    const prompt = normalized.kindError ?? "Which kcore resource should this be?";
    const blocking = [
      question("kind", prompt, "Each resource has its own required questions and immutable fields.", true, {
        choices: kindIds(),
      }),
    ];
    return {
      kind: null,
      ready: false,
      blocking,
      recommended: [],
      warnings: [],
      askTheUser: formatAsk(blocking, []),
      nextTool: "kcore_advise",
    };
  }

  const { kind, spec } = normalized.desired;
  const all = questionsFor(kind, spec);
  const blocking = all.filter((item) => item.required);
  const recommended = input.acceptDefaults ? [] : all.filter((item) => !item.required);
  const ready = blocking.length === 0;
  return {
    kind: kind.id,
    ready,
    blocking,
    recommended,
    warnings: warningsFor(kind, spec),
    askTheUser: formatAsk(blocking, recommended),
    nextTool: ready ? "kcore_plan" : "kcore_advise",
  };
}

function formatAsk(blocking: Question[], recommended: Question[]): string {
  const lines: string[] = [];
  if (blocking.length === 0 && recommended.length === 0) {
    return "The required answers are present. Show the plan and wait for an explicit yes before apply.";
  }
  if (blocking.length > 0) {
    lines.push("Ask the operator these questions and wait for the answers before planning:");
    for (const item of blocking) lines.push(`- ${item.prompt} (${item.why})`);
  }
  if (recommended.length > 0) {
    lines.push("Also ask these, unless the operator already said to use defaults:");
    for (const item of recommended) lines.push(`- ${item.prompt} (${item.why})`);
  }
  return lines.join("\n");
}

export function mergeAnswers(
  spec: Record<string, unknown>,
  answers: Record<string, unknown>,
): Record<string, unknown> {
  const next = { ...spec };
  for (const [key, value] of Object.entries(answers)) {
    if (key === "kind" || key === "image" || key === "auth" || key === "placement" || key === "target" || key === "layout") {
      continue;
    }
    if (isBlank(value)) continue;
    next[key] = value;
  }
  const image = answers.image;
  if (typeof image === "string" && image.trim().startsWith("https://") && isBlank(next.imageUrl)) {
    next.imageUrl = image.trim();
  }
  if (typeof image === "string" && image.trim().startsWith("/") && isBlank(next.imagePath)) {
    next.imagePath = image.trim();
  }
  const auth = answers.auth;
  if (typeof auth === "string" && auth.trim().startsWith("ssh-") && isBlank(next.sshPublicKeys)) {
    next.sshPublicKeys = [auth.trim()];
  } else if (typeof auth === "string" && auth.trim() && isBlank(next.sshKeys) && !auth.trim().startsWith("ssh-")) {
    next.sshKeys = [auth.trim()];
  }
  const placement = answers.placement;
  if (typeof placement === "string" && placement.includes(":") && isBlank(next.targetNode)) {
    next.targetNode = placement.trim();
  }
  const memory = answers.memory;
  if (typeof memory === "string" && memory.trim() && isBlank(next.memoryBytes)) {
    next.memoryBytes = memory.trim();
  }
  if (answers.target === "vm" && typeof answers.vm === "string") next.vm = answers.vm;
  if (answers.target === "volume" && typeof answers.volume === "string") next.volume = answers.volume;
  return next;
}
