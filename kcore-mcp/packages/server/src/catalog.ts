export type ResourceKind = {
  id: string;
  yamlKind: string;
  title: string;
  summary: string;
  mutable: string[];
  immutable: string[];
};

export const KINDS: ResourceKind[] = [
  {
    id: "cluster",
    yamlKind: "Cluster",
    title: "Cluster context",
    summary:
      "Creates a cluster CA, a controller certificate, and a client certificate, then saves the context. Later calls dial the controller with that material.",
    mutable: [],
    immutable: ["controller"],
  },
  {
    id: "vm",
    yamlKind: "VM",
    title: "Virtual machine",
    summary: "Declarative VM. Re-applying updates cpu, memory, and desired power state. Disk, NIC, image, and SSH key changes require a replacement.",
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
  },
  {
    id: "network",
    yamlKind: "Network",
    title: "Network",
    summary: "NAT, bridge, or VXLAN network. Every field is immutable in v1, so a change means delete and create.",
    mutable: [],
    immutable: ["type", "externalIp", "gatewayIp", "internalNetmask", "vlanId", "targetNode"],
  },
  {
    id: "ssh-key",
    yamlKind: "SshKey",
    title: "SSH key",
    summary: "Named SSH public key the controller can inject into a VM.",
    mutable: [],
    immutable: ["publicKey"],
  },
  {
    id: "container",
    yamlKind: "Container",
    title: "Container",
    summary: "OCI container workload. desiredState is mutable. Image, command, network, env, and ports are immutable.",
    mutable: ["desiredState"],
    immutable: ["image", "command", "network", "env", "ports"],
  },
  {
    id: "volume",
    yamlKind: "Volume",
    title: "Volume",
    summary: "Detached Ceph data volume. Attach only to a stopped VM.",
    mutable: [],
    immutable: ["sizeBytes", "encrypt", "fromSnapshot"],
  },
  {
    id: "volume-snapshot",
    yamlKind: "VolumeSnapshot",
    title: "Volume snapshot",
    summary: "Crash-consistent snapshot of a volume.",
    mutable: [],
    immutable: ["volume"],
  },
  {
    id: "snapshot-policy",
    yamlKind: "SnapshotPolicy",
    title: "Snapshot policy",
    summary: "Scheduled snapshots and retention for a VM or a single volume.",
    mutable: ["schedule", "keep", "disabled"],
    immutable: ["vm", "volume"],
  },
  {
    id: "security-group",
    yamlKind: "SecurityGroup",
    title: "Security group",
    summary: "Firewall rules and attachments. Re-applying reconciles the attachment set.",
    mutable: ["description", "rules", "attachments"],
    immutable: ["name"],
  },
  {
    id: "disk-layout",
    yamlKind: "DiskLayout",
    title: "Disk layout",
    summary: "Day-2 disk layout reconciled by the controller. Plan runs ClassifyDiskLayout before apply. Deleting the resource leaves the node disks alone.",
    mutable: ["layoutNix", "diskLayout"],
    immutable: ["nodeId"],
  },
  {
    id: "ceph-cluster",
    yamlKind: "CephCluster",
    title: "Ceph cluster",
    summary: "Ceph cluster spec. publicNetwork and clusterNetwork are required.",
    mutable: ["size", "minSize"],
    immutable: ["publicNetwork", "clusterNetwork", "fsid"],
  },
  {
    id: "shared-filesystem",
    yamlKind: "SharedFilesystem",
    title: "Shared filesystem",
    summary: "CephFS filesystem. Requires an existing Ceph cluster.",
    mutable: ["quotaBytes", "clients"],
    immutable: ["cephCluster"],
  },
  {
    id: "object-store",
    yamlKind: "ObjectStore",
    title: "Object store",
    summary: "RGW object store on a Ceph cluster. members is the list of node ids that run the gateway.",
    mutable: ["port", "tls", "members"],
    immutable: ["cephCluster"],
  },
  {
    id: "cluster-update",
    yamlKind: "ClusterUpdate",
    title: "Cluster update",
    summary:
      "Rolling NixOS update. Plan resolves nodes and blockers. Apply creates the update. Destroy cancels a non-terminal update and keeps history.",
    mutable: [],
    immutable: ["version", "flakeRef", "flakeRev"],
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
