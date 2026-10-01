export type ResourceKind = {
  id: string;
  yamlKind: string;
  title: string;
  summary: string;
  /** How kctl realises the resource. `apply` is the declarative upsert. */
  transport: "apply" | "create-flags" | "update-cluster";
  mutable: string[];
  immutable: string[];
  getArgs: (name?: string) => string[];
  deleteArgs: (name: string) => string[];
};

export const KINDS: ResourceKind[] = [
  {
    id: "cluster",
    yamlKind: "Cluster",
    title: "Cluster context",
    summary:
      "Creates the local mTLS context and certificates for a controller. This is the first step before any other resource.",
    transport: "apply",
    mutable: [],
    immutable: ["controller"],
    getArgs: () => ["version"],
    deleteArgs: () => {
      throw new Error("cluster certificates are local files; remove the context from ~/.kcore/config by hand");
    },
  },
  {
    id: "vm",
    yamlKind: "VM",
    title: "Virtual machine",
    summary: "Declarative VM. Re-applying updates cpu, memory, and desired power state. Disk, NIC, image, and SSH key changes require a replacement.",
    transport: "apply",
    mutable: ["cpu", "memoryBytes", "desiredState"],
    immutable: [
      "network",
      "extraNetworks",
      "imageUrl",
      "imageSha256",
      "imagePath",
      "imageFormat",
      "storageBackend",
      "storageSizeBytes",
      "targetNode",
      "sshKeys",
      "sshPublicKeys",
      "cloudInitUserData",
    ],
    getArgs: (name) => (name ? ["get", "vms", name] : ["get", "vms"]),
    deleteArgs: (name) => ["delete", "vm", name],
  },
  {
    id: "network",
    yamlKind: "Network",
    title: "Network",
    summary: "NAT, bridge, or VXLAN network. Every field is immutable in v1, so a change means delete and create.",
    transport: "apply",
    mutable: [],
    immutable: ["type", "externalIp", "gatewayIp", "internalNetmask", "vlanId", "targetNode"],
    getArgs: () => ["get", "networks"],
    deleteArgs: (name) => ["delete", "network", name],
  },
  {
    id: "ssh-key",
    yamlKind: "SshKey",
    title: "SSH key",
    summary: "Named SSH public key the controller can inject into a VM.",
    transport: "apply",
    mutable: [],
    immutable: ["publicKey"],
    getArgs: (name) => (name ? ["ssh-key", "get", name] : ["ssh-key", "list"]),
    deleteArgs: (name) => ["ssh-key", "delete", name],
  },
  {
    id: "container",
    yamlKind: "Container",
    title: "Container",
    summary: "OCI container workload. desiredState is mutable. Image, command, network, env, and ports are immutable.",
    transport: "apply",
    mutable: ["desiredState"],
    immutable: ["image", "command", "network", "env", "ports"],
    getArgs: (name) => (name ? ["get", "containers", name] : ["get", "containers"]),
    deleteArgs: (name) => ["delete", "container", name],
  },
  {
    id: "volume",
    yamlKind: "Volume",
    title: "Volume",
    summary: "Detached Ceph data volume. Attach only to a stopped VM.",
    transport: "create-flags",
    mutable: [],
    immutable: ["sizeBytes", "encrypt", "fromSnapshot"],
    getArgs: (name) => (name ? ["get", "volume", name] : ["get", "volumes"]),
    deleteArgs: (name) => ["delete", "volume", name],
  },
  {
    id: "volume-snapshot",
    yamlKind: "VolumeSnapshot",
    title: "Volume snapshot",
    summary: "Crash-consistent snapshot of a volume.",
    transport: "create-flags",
    mutable: [],
    immutable: ["volume"],
    getArgs: () => ["get", "snapshots"],
    deleteArgs: (name) => ["delete", "volume-snapshot", name],
  },
  {
    id: "snapshot-policy",
    yamlKind: "SnapshotPolicy",
    title: "Snapshot policy",
    summary: "Scheduled snapshots and retention for a VM or a single volume.",
    transport: "create-flags",
    mutable: ["schedule", "keep", "disabled"],
    immutable: ["vm", "volume"],
    getArgs: () => ["get", "snapshot-policies"],
    deleteArgs: (name) => ["delete", "snapshot-policy", name],
  },
  {
    id: "security-group",
    yamlKind: "SecurityGroup",
    title: "Security group",
    summary: "Firewall rules and attachments. Re-applying reconciles the attachment set.",
    transport: "apply",
    mutable: ["description", "rules", "attachments"],
    immutable: ["name"],
    getArgs: (name) => (name ? ["security-group", "get", name] : ["security-group", "list"]),
    deleteArgs: (name) => ["security-group", "delete", name],
  },
  {
    id: "disk-layout",
    yamlKind: "DiskLayout",
    title: "Disk layout",
    summary: "Day-2 disk layout reconciled by the controller. Plan with kctl diff before apply. Deleting the resource leaves the node disks alone.",
    transport: "apply",
    mutable: ["layoutNix", "diskLayout"],
    immutable: ["nodeId"],
    getArgs: (name) => (name ? ["describe", "disk-layout", name] : ["get", "disk-layouts"]),
    deleteArgs: (name) => ["delete", "disk-layout", name],
  },
  {
    id: "ceph-cluster",
    yamlKind: "CephCluster",
    title: "Ceph cluster",
    summary: "Ceph cluster spec. publicNetwork and clusterNetwork are required.",
    transport: "apply",
    mutable: ["size", "minSize"],
    immutable: ["publicNetwork", "clusterNetwork", "fsid"],
    getArgs: (name) => (name ? ["describe", "ceph-cluster", name] : ["get", "ceph-cluster"]),
    deleteArgs: (name) => ["delete", "ceph-cluster", name],
  },
  {
    id: "shared-filesystem",
    yamlKind: "SharedFilesystem",
    title: "Shared filesystem",
    summary: "CephFS filesystem. Requires an existing Ceph cluster.",
    transport: "apply",
    mutable: ["quotaBytes", "clients"],
    immutable: ["cephCluster"],
    getArgs: (name) =>
      name ? ["describe", "shared-filesystem", name] : ["get", "shared-filesystems"],
    deleteArgs: (name) => ["delete", "shared-filesystem", name],
  },
  {
    id: "object-store",
    yamlKind: "ObjectStore",
    title: "Object store",
    summary: "RGW object store on a Ceph cluster. members is the list of node ids that run the gateway.",
    transport: "apply",
    mutable: ["port", "tls", "members"],
    immutable: ["cephCluster"],
    getArgs: (name) => (name ? ["describe", "object-store", name] : ["get", "object-stores"]),
    deleteArgs: (name) => ["delete", "object-store", name],
  },
  {
    id: "cluster-update",
    yamlKind: "ClusterUpdate",
    title: "Cluster update",
    summary:
      "Rolling NixOS update. Plan resolves nodes and blockers. Apply creates the update. Destroy cancels a non-terminal update and keeps history.",
    transport: "update-cluster",
    mutable: [],
    immutable: ["version", "flakeRef", "flakeRev"],
    getArgs: (name) => (name ? ["update", "cluster", "get", name] : ["update", "cluster", "list"]),
    deleteArgs: (name) => ["update", "cluster", "cancel", name],
  },
];

const BY_ID = new Map(KINDS.map((kind) => [kind.id, kind]));
const BY_YAML = new Map(KINDS.map((kind) => [kind.yamlKind.toLowerCase(), kind]));

export function findKind(value: string | undefined | null): ResourceKind | undefined {
  if (!value) return undefined;
  const key = value.trim().toLowerCase().replaceAll("_", "-");
  return BY_ID.get(key) ?? BY_YAML.get(key.replaceAll("-", ""));
}

export function kindIds(): string[] {
  return KINDS.map((kind) => kind.id);
}
