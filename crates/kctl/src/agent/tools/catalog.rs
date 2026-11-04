//! The resources the agent can plan and apply, and which of their fields can change in place.

pub struct ResourceKind {
    pub id: &'static str,
    pub yaml_kind: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
    pub mutable: &'static [&'static str],
    pub immutable: &'static [&'static str],
}

pub static KINDS: &[ResourceKind] = &[
    ResourceKind {
        id: "cluster",
        yaml_kind: "Cluster",
        title: "Cluster context",
        summary: "Creates a cluster CA, a controller certificate, and a client certificate, then saves the context. Later calls dial the controller with that material.",
        mutable: &[],
        immutable: &["controller"],
    },
    ResourceKind {
        id: "vm",
        yaml_kind: "VM",
        title: "Virtual machine",
        summary: "Declarative VM. Re-applying updates cpu, memory, and desired power state. Disk, NIC, image, and SSH key changes require a replacement.",
        mutable: &["cpu", "memoryBytes", "desiredState"],
        immutable: &[
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
    ResourceKind {
        id: "network",
        yaml_kind: "Network",
        title: "Network",
        summary: "NAT, bridge, or VXLAN network. Every field is immutable in v1, so a change means delete and create.",
        mutable: &[],
        immutable: &["type", "externalIp", "gatewayIp", "internalNetmask", "vlanId", "targetNode"],
    },
    ResourceKind {
        id: "ssh-key",
        yaml_kind: "SshKey",
        title: "SSH key",
        summary: "Named SSH public key the controller can inject into a VM.",
        mutable: &[],
        immutable: &["publicKey"],
    },
    ResourceKind {
        id: "postgresql",
        yaml_kind: "Postgresql",
        title: "PostgreSQL database",
        summary: "One database from the NixOS postgresql package (services.postgresql). v1 is a single database per node, reached through the local Unix socket. port can change; package, database, and targetNode cannot.",
        mutable: &["port"],
        immutable: &["database", "package", "targetNode"],
    },
    ResourceKind {
        id: "container",
        yaml_kind: "Container",
        title: "Container",
        summary: "OCI container workload. desiredState is mutable. Image, command, network, env, and ports are immutable.",
        mutable: &["desiredState"],
        immutable: &["image", "command", "network", "env", "ports"],
    },
    ResourceKind {
        id: "volume",
        yaml_kind: "Volume",
        title: "Volume",
        summary: "Detached Ceph data volume. Attach only to a stopped VM.",
        mutable: &[],
        immutable: &["sizeBytes", "encrypt", "fromSnapshot"],
    },
    ResourceKind {
        id: "volume-snapshot",
        yaml_kind: "VolumeSnapshot",
        title: "Volume snapshot",
        summary: "Crash-consistent snapshot of a volume.",
        mutable: &[],
        immutable: &["volume"],
    },
    ResourceKind {
        id: "snapshot-policy",
        yaml_kind: "SnapshotPolicy",
        title: "Snapshot policy",
        summary: "Scheduled snapshots and retention for a VM or a single volume.",
        mutable: &["schedule", "keep", "disabled"],
        immutable: &["vm", "volume"],
    },
    ResourceKind {
        id: "security-group",
        yaml_kind: "SecurityGroup",
        title: "Security group",
        summary: "Firewall rules and attachments. Re-applying reconciles the attachment set.",
        mutable: &["description", "rules", "attachments"],
        immutable: &["name"],
    },
    ResourceKind {
        id: "disk-layout",
        yaml_kind: "DiskLayout",
        title: "Disk layout",
        summary: "Day-2 disk layout reconciled by the controller. Plan runs ClassifyDiskLayout before apply. Deleting the resource leaves the node disks alone.",
        mutable: &["layoutNix", "diskLayout"],
        immutable: &["nodeId"],
    },
    ResourceKind {
        id: "ceph-cluster",
        yaml_kind: "CephCluster",
        title: "Ceph cluster",
        summary: "Ceph cluster spec. publicNetwork and clusterNetwork are required.",
        mutable: &["size", "minSize"],
        immutable: &["publicNetwork", "clusterNetwork", "fsid"],
    },
    ResourceKind {
        id: "shared-filesystem",
        yaml_kind: "SharedFilesystem",
        title: "Shared filesystem",
        summary: "CephFS filesystem. Requires an existing Ceph cluster.",
        mutable: &["quotaBytes", "clients"],
        immutable: &["cephCluster"],
    },
    ResourceKind {
        id: "object-store",
        yaml_kind: "ObjectStore",
        title: "Object store",
        summary: "RGW object store on a Ceph cluster. members is the list of node ids that run the gateway.",
        mutable: &["port", "tls", "members"],
        immutable: &["cephCluster"],
    },
    ResourceKind {
        id: "image",
        yaml_kind: "Image",
        title: "Node image",
        summary: "Boot image cached on a node. Read lists ListImages on each ready node. Images are not created through the controller.",
        mutable: &[],
        immutable: &["name", "path"],
    },
    ResourceKind {
        id: "cluster-update",
        yaml_kind: "ClusterUpdate",
        title: "Cluster update",
        summary: "Rolling NixOS update. Plan resolves nodes and blockers. Apply creates the update. Destroy cancels a non-terminal update and keeps history.",
        mutable: &[],
        immutable: &["version", "flakeRef", "flakeRev"],
    },
];

pub fn find_kind(value: Option<&str>) -> Option<&'static ResourceKind> {
    let key = value?.trim().to_lowercase().replace('_', "-");
    if key.is_empty() {
        return None;
    }
    KINDS.iter().find(|kind| kind.id == key).or_else(|| {
        let squashed = key.replace('-', "");
        KINDS
            .iter()
            .find(|kind| kind.yaml_kind.to_lowercase() == squashed)
    })
}

pub fn kind_ids() -> Vec<&'static str> {
    KINDS.iter().map(|kind| kind.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_resolve_by_id_and_by_manifest_kind() {
        assert_eq!(find_kind(Some("vm")).unwrap().id, "vm");
        assert_eq!(find_kind(Some("VM")).unwrap().id, "vm");
        assert_eq!(find_kind(Some("SshKey")).unwrap().id, "ssh-key");
        assert_eq!(find_kind(Some("ssh_key")).unwrap().id, "ssh-key");
        assert_eq!(
            find_kind(Some("ClusterUpdate")).unwrap().id,
            "cluster-update"
        );
        assert!(find_kind(Some("nope")).is_none());
        assert!(find_kind(None).is_none());
    }
}
