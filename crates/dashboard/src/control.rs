//! JSON control plane for the dashboard. The React app is driven by `/catalog`
//! and calls the same controller RPCs as `kctl` and the Terraform provider.
//!
//! Prost messages gain fields over time. Filling them from `Default` keeps a
//! new proto field from breaking every constructor.
#![allow(clippy::field_reassign_with_default)]

use std::collections::HashMap;

use anyhow::Context;
#[cfg(dashboard_spa)]
use axum::body::Body;
use axum::extract::{Json, Path, Query};
use axum::http::StatusCode;
#[cfg(dashboard_spa)]
use axum::http::{header, Uri};
#[cfg(dashboard_spa)]
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{json, Value};

use crate::controller_client::controller_proto::{self, controller_client::ControllerClient};
use crate::{controller_client, format, mappers};
use tonic::transport::Channel;

#[cfg(test)]
const TERRAFORM: &[&str] = &[
    "kcore_cluster",
    "kcore_node",
    "kcore_vm",
    "kcore_network",
    "kcore_security_group",
    "kcore_security_group_attachment",
    "kcore_volume",
    "kcore_volume_snapshot",
    "kcore_snapshot_policy",
    "kcore_disk_layout",
    "kcore_ceph_cluster",
    "kcore_shared_filesystem",
    "kcore_object_store",
    "kcore_object_user",
    "kcore_ssh_key",
    "kcore_operator",
    "kcore_cluster_update",
    "kcore_workload",
    "kcore_kubernetes_cluster",
];

#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    label: &'static str,
    kind: &'static str,
    required: bool,
}

#[derive(Clone, Copy)]
struct Act {
    name: &'static str,
    label: &'static str,
    confirm: bool,
    /// Load the stored spec into the form before submit. An empty form must
    /// not replace a Kubernetes cluster.
    preload: bool,
    fields: &'static [Field],
}

#[derive(Clone, Copy)]
struct Res {
    id: &'static str,
    title: &'static str,
    group: &'static str,
    terraform: &'static [&'static str],
    kctl: &'static str,
    id_field: &'static str,
    list: bool,
    creatable: bool,
    fields: &'static [Field],
    actions: &'static [Act],
}

const NONE: &[Field] = &[];

fn text_field(name: &'static str, label: &'static str, required: bool) -> Field {
    Field {
        name,
        label,
        kind: "text",
        required,
    }
}

fn num_field(name: &'static str, label: &'static str, required: bool) -> Field {
    Field {
        name,
        label,
        kind: "number",
        required,
    }
}

fn bool_field(name: &'static str, label: &'static str) -> Field {
    Field {
        name,
        label,
        kind: "bool",
        required: false,
    }
}

fn json_field(name: &'static str, label: &'static str, required: bool) -> Field {
    Field {
        name,
        label,
        kind: "json",
        required,
    }
}

fn act(name: &'static str, label: &'static str, confirm: bool, fields: &'static [Field]) -> Act {
    Act {
        name,
        label,
        confirm,
        preload: false,
        fields,
    }
}

fn act_load(
    name: &'static str,
    label: &'static str,
    confirm: bool,
    fields: &'static [Field],
) -> Act {
    let mut action = act(name, label, confirm, fields);
    action.preload = true;
    action
}

fn catalog_resources() -> Vec<Res> {
    let vm_fields = vec![
        text_field("name", "Name", true),
        num_field("cpu", "CPUs", true),
        num_field("memory_bytes", "Memory bytes", true),
        text_field("storage_backend", "Storage backend", true),
        num_field("storage_size_bytes", "Disk bytes", true),
        text_field("image_url", "Image URL", false),
        text_field("image_sha256", "Image SHA256", false),
        text_field("image_path", "Image path", false),
        text_field("image_format", "Image format", false),
        text_field("target_node", "Target node", false),
        text_field("target_dc", "Target datacenter", false),
        text_field("network", "Network", false),
        json_field("nics", "Network cards", false),
        json_field("disks", "Extra disks", false),
        text_field("ssh_key_names", "SSH key names", false),
        text_field("cloud_init_user_data", "Cloud-init user data", false),
        text_field("anti_affinity", "Anti-affinity group", false),
        text_field("desired_state", "Desired state", false),
    ];
    // The catalog is static JSON. Owned field lists are leaked once so the
    // descriptors can be `'static` for the process lifetime.
    fn leak(fields: Vec<Field>) -> &'static [Field] {
        Box::leak(fields.into_boxed_slice())
    }
    let vm_fields = leak(vm_fields);
    let migrate_fields = leak(vec![text_field("target_node", "Target node", true)]);
    let vm_update = leak(vec![
        num_field("cpu", "CPUs", false),
        num_field("memory_bytes", "Memory bytes", false),
    ]);
    vec![
        Res {
            id: "vms",
            title: "Virtual machines",
            group: "Compute",
            terraform: &["kcore_vm"],
            kctl: "vm",
            id_field: "id",
            list: true,
            creatable: true,
            fields: vm_fields,
            actions: leak_acts(vec![
                act("start", "Start", false, NONE),
                act("stop", "Stop", false, NONE),
                act(
                    "delete",
                    "Delete",
                    true,
                    leak(vec![bool_field(
                        "delete_data_volumes",
                        "Delete data volumes",
                    )]),
                ),
                act("migrate", "Migrate", true, migrate_fields),
                act(
                    "migrate-status",
                    "Migration status",
                    false,
                    leak(vec![text_field("node_id", "Node", false)]),
                ),
                act(
                    "reset-migrate",
                    "Reset stranded migration",
                    true,
                    leak(vec![
                        text_field("node_id", "Node", true),
                        bool_field("force", "Force clear"),
                    ]),
                ),
                act("update", "Resize", false, vm_update),
                act("console", "Console", false, NONE),
            ]),
        },
        Res {
            id: "workloads",
            title: "Workloads",
            group: "Compute",
            terraform: &["kcore_workload"],
            kctl: "workload",
            id_field: "id",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("kind", "Kind (vm or container)", true),
                text_field("name", "Name", true),
                text_field("image", "Container image", false),
                text_field("network", "Network", false),
                num_field("cpu", "CPUs", false),
                num_field("memory_bytes", "Memory bytes", false),
                text_field("storage_backend", "Storage backend", false),
                num_field("storage_size_bytes", "Disk bytes", false),
                text_field("image_url", "Image URL", false),
                text_field("image_sha256", "Image SHA256", false),
                text_field("image_path", "Image path", false),
                text_field("image_format", "Image format", false),
                text_field("target_node", "Target node", false),
                text_field("command", "Command", false),
            ]),
            actions: leak_acts(vec![
                act("start", "Start", false, NONE),
                act("stop", "Stop", false, NONE),
                act("delete", "Delete", true, NONE),
            ]),
        },
        Res {
            id: "containers",
            title: "Containers",
            group: "Compute",
            terraform: &[],
            kctl: "container",
            id_field: "id",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("image", "Image", true),
                text_field("network", "Network", false),
                text_field("command", "Command", false),
                text_field("target_node", "Target node", false),
                text_field("storage_backend", "Storage backend", false),
                num_field("storage_size_bytes", "Disk bytes", false),
                text_field("mount_target", "Mount target", false),
            ]),
            actions: leak_acts(vec![
                act("start", "Start", false, NONE),
                act("stop", "Stop", false, NONE),
                act("delete", "Delete", true, NONE),
            ]),
        },
        Res {
            id: "operations",
            title: "Operations",
            group: "Compute",
            terraform: &[],
            kctl: "operation",
            id_field: "id",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![act("cancel", "Cancel", true, NONE)]),
        },
        Res {
            id: "networks",
            title: "Networks",
            group: "Network",
            terraform: &["kcore_network"],
            kctl: "network",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("external_ip", "External IP", false),
                text_field("gateway_ip", "Gateway", false),
                text_field("internal_netmask", "Netmask", false),
                text_field("target_node", "Target node", false),
                text_field("network_type", "Type (nat, bridge, vxlan)", false),
                num_field("vlan_id", "VLAN id", false),
                bool_field("enable_outbound_nat", "Outbound NAT"),
                text_field("ipv6_prefix", "IPv6 prefix", false),
                text_field("ipv6_gateway", "IPv6 gateway", false),
                bool_field("east_west_firewall", "East-west firewall"),
                text_field("allowed_tcp_ports", "TCP ports", false),
                text_field("allowed_udp_ports", "UDP ports", false),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "network",
            title: "Network overview",
            group: "Network",
            terraform: &[],
            kctl: "network",
            id_field: "name",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "security-groups",
            title: "Security groups",
            group: "Network",
            terraform: &["kcore_security_group", "kcore_security_group_attachment"],
            kctl: "security_group",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("description", "Description", false),
                json_field("rules", "Rules", false),
            ]),
            actions: leak_acts(vec![
                act("delete", "Delete", true, NONE),
                act(
                    "attach",
                    "Attach",
                    false,
                    leak(vec![
                        text_field("target_kind", "Target kind (vm or network)", true),
                        text_field("target_id", "Target id", true),
                        text_field("target_node", "Target node", false),
                    ]),
                ),
                act(
                    "detach",
                    "Detach",
                    true,
                    leak(vec![
                        text_field("target_kind", "Target kind (vm or network)", true),
                        text_field("target_id", "Target id", true),
                        text_field("target_node", "Target node", false),
                    ]),
                ),
            ]),
        },
        Res {
            id: "volumes",
            title: "Volumes",
            group: "Storage",
            terraform: &["kcore_volume"],
            kctl: "volume",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                num_field("size_bytes", "Size bytes", true),
                text_field("storage_class", "Storage class", false),
                text_field("vm", "Attach to VM", false),
                text_field("node", "Node", false),
                text_field("volume_name", "LV or zvol name", false),
                bool_field("encrypt", "Encrypt"),
                text_field("from_snapshot", "Clone from snapshot", false),
            ]),
            actions: leak_acts(vec![
                act("delete", "Delete", true, NONE),
                act(
                    "attach",
                    "Attach",
                    false,
                    leak(vec![text_field("vm", "VM", true)]),
                ),
                act("detach", "Detach", true, NONE),
                act(
                    "resize",
                    "Resize",
                    true,
                    leak(vec![
                        num_field("size_bytes", "Size bytes", true),
                        bool_field("grow_filesystem", "Grow filesystem"),
                        bool_field("allow_shrink", "Allow shrink"),
                    ]),
                ),
                act(
                    "snapshot",
                    "Snapshot",
                    false,
                    leak(vec![text_field("snapshot", "Snapshot name", false)]),
                ),
                act(
                    "restore",
                    "Restore",
                    true,
                    leak(vec![text_field("snapshot", "Snapshot", true)]),
                ),
                act("flatten", "Flatten", true, NONE),
                act(
                    "format",
                    "Format",
                    true,
                    leak(vec![
                        text_field("fs_type", "Filesystem", true),
                        bool_field("force", "Replace existing"),
                    ]),
                ),
                act(
                    "mount",
                    "Mount",
                    false,
                    leak(vec![
                        text_field("mount_point", "Mount point", true),
                        text_field("fs_type", "Filesystem", false),
                        bool_field("persistent", "Persistent"),
                    ]),
                ),
                act(
                    "unmount",
                    "Unmount",
                    true,
                    leak(vec![bool_field("persistent", "Remove unit")]),
                ),
                act(
                    "encrypt",
                    "Encrypt",
                    true,
                    leak(vec![bool_field("destroy_data", "Destroy existing data")]),
                ),
            ]),
        },
        Res {
            id: "snapshots",
            title: "Volume snapshots",
            group: "Storage",
            terraform: &["kcore_volume_snapshot"],
            kctl: "volume",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("volume", "Volume", true),
                text_field("name", "Snapshot name", false),
                text_field("consistency", "Consistency", false),
            ]),
            actions: leak_acts(vec![
                act("delete", "Delete", true, NONE),
                act(
                    "restore",
                    "Restore",
                    true,
                    leak(vec![text_field("volume", "Volume", true)]),
                ),
            ]),
        },
        Res {
            id: "snapshot-policies",
            title: "Snapshot policies",
            group: "Storage",
            terraform: &["kcore_snapshot_policy"],
            kctl: "volume",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("selector_vm", "VM selector", false),
                text_field("selector_volume", "Volume selector", false),
                text_field("schedule", "Schedule", true),
                num_field("keep", "Keep", true),
                bool_field("enabled", "Enabled"),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "disk-layouts",
            title: "Disk layouts",
            group: "Storage",
            terraform: &["kcore_disk_layout"],
            kctl: "disk_layout",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("node_id", "Node", true),
                text_field("layout_nix", "Layout Nix", true),
                bool_field("evacuate", "Evacuate VMs"),
                bool_field("confirm", "Confirm"),
            ]),
            actions: leak_acts(vec![
                act("classify", "Check", false, NONE),
                act("delete", "Delete", true, NONE),
            ]),
        },
        Res {
            id: "storage",
            title: "Storage overview",
            group: "Storage",
            terraform: &[],
            kctl: "storage",
            id_field: "name",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "storage-classes",
            title: "Storage classes",
            group: "Storage",
            terraform: &[],
            kctl: "storage_class",
            id_field: "name",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "ceph",
            title: "Ceph clusters",
            group: "Storage",
            terraform: &["kcore_ceph_cluster"],
            kctl: "ceph_cluster",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("public_network", "Public network", true),
                text_field("cluster_network", "Cluster network", false),
                text_field("fsid", "FSID", false),
                num_field("size", "Size", false),
                num_field("min_size", "Min size", false),
                json_field("nodes", "Nodes and OSD devices", true),
                bool_field("encrypt_osds", "Encrypt OSDs"),
                bool_field("confirm", "Confirm"),
            ]),
            actions: leak_acts(vec![
                act(
                    "delete",
                    "Delete",
                    true,
                    leak(vec![bool_field("force", "Force")]),
                ),
                act("plan", "Plan", false, NONE),
                act(
                    "credentials",
                    "Set dashboard login",
                    true,
                    leak(vec![
                        text_field("url", "URL", false),
                        text_field("username", "Username", true),
                        text_field("password", "Password", true),
                    ]),
                ),
                act("show-credentials", "Show dashboard login", true, NONE),
            ]),
        },
        Res {
            id: "filesystems",
            title: "Shared filesystems",
            group: "Storage",
            terraform: &["kcore_shared_filesystem"],
            kctl: "shared_filesystem",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("ceph_cluster", "Ceph cluster", true),
                num_field("quota_bytes", "Quota bytes", false),
                text_field("fs_name", "FS name", false),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "object-stores",
            title: "Object stores",
            group: "Storage",
            terraform: &["kcore_object_store"],
            kctl: "object_store",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("ceph_cluster", "Ceph cluster", true),
                text_field("members", "Members", false),
                num_field("port", "Port", false),
                bool_field("tls", "TLS"),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "object-users",
            title: "Object users",
            group: "Storage",
            terraform: &["kcore_object_user"],
            kctl: "object_store",
            id_field: "name",
            list: false,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("store", "Store", true),
            ]),
            actions: leak_acts(vec![act(
                "delete",
                "Delete",
                true,
                leak(vec![
                    text_field("name", "Name", true),
                    text_field("store", "Store", true),
                ]),
            )]),
        },
        Res {
            id: "nodes",
            title: "Nodes",
            group: "Cluster",
            terraform: &["kcore_node"],
            kctl: "node",
            id_field: "id",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![
                act("approve", "Approve", false, NONE),
                act("reject", "Reject", true, NONE),
                act("cordon", "Cordon", true, NONE),
                act("uncordon", "Uncordon", false, NONE),
                act(
                    "drain",
                    "Drain",
                    true,
                    leak(vec![text_field("target_node", "Target node", false)]),
                ),
                act("delete", "Delete", true, NONE),
            ]),
        },
        Res {
            id: "kubernetes",
            title: "Kubernetes",
            group: "Cluster",
            terraform: &["kcore_kubernetes_cluster"],
            kctl: "kubernetes",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("cluster_cidr", "Pod CIDR", true),
                text_field("service_cidr", "Service CIDR", true),
                text_field("cluster_dns", "Cluster DNS", false),
                text_field("api_server_endpoint", "API endpoint", true),
                text_field("cni", "CNI", false),
                json_field("control_plane_nodes", "Control plane", true),
                json_field("worker_nodes", "Workers", false),
                bool_field("ha_enabled", "High availability"),
                text_field("vip_address", "VIP address", false),
                text_field("vip_interface", "VIP interface", false),
                text_field("method", "HA method", false),
                bool_field("schedule_on_control_plane", "Schedule on control plane"),
                bool_field("confirm", "Confirm"),
            ]),
            actions: leak_acts(vec![
                act(
                    "delete",
                    "Delete",
                    true,
                    leak(vec![bool_field("force", "Force")]),
                ),
                act_load(
                    "update",
                    "Update",
                    true,
                    leak(vec![
                        text_field("cluster_cidr", "Pod CIDR", true),
                        text_field("service_cidr", "Service CIDR", true),
                        text_field("cluster_dns", "Cluster DNS", false),
                        text_field("api_server_endpoint", "API endpoint", true),
                        text_field("cni", "CNI", false),
                        json_field("control_plane_nodes", "Control plane", true),
                        json_field("worker_nodes", "Workers", false),
                        bool_field("ha_enabled", "High availability"),
                        text_field("vip_address", "VIP address", false),
                        text_field("vip_interface", "VIP interface", false),
                        text_field("method", "HA method", false),
                        bool_field("schedule_on_control_plane", "Schedule on control plane"),
                        bool_field("confirm", "Confirm"),
                    ]),
                ),
                act(
                    "scale",
                    "Scale",
                    true,
                    leak(vec![
                        text_field("add_node_id", "Add node", false),
                        text_field("role", "Role", false),
                        text_field("advertise_address", "Advertise address", false),
                        text_field("iface", "Interface", false),
                        text_field("remove_node_ids", "Remove node ids", false),
                    ]),
                ),
                act("plan", "Plan", false, NONE),
            ]),
        },
        Res {
            id: "cluster-updates",
            title: "Cluster updates",
            group: "Cluster",
            terraform: &["kcore_cluster_update"],
            kctl: "cluster_update",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("version", "Version", false),
                text_field("flake_ref", "Flake ref", false),
                text_field("flake_rev", "Flake revision", false),
                text_field("update_kind", "Update kind", false),
                bool_field("all_nodes", "All nodes"),
                text_field("node_ids", "Node ids", false),
                bool_field("confirm", "Confirm"),
            ]),
            actions: leak_acts(vec![
                act("plan", "Plan", false, NONE),
                act("approve", "Approve", true, NONE),
                act("cancel", "Cancel", true, NONE),
                act("rollback", "Rollback", true, NONE),
                act(
                    "reboot",
                    "Reboot node",
                    true,
                    leak(vec![text_field("node_id", "Node", true)]),
                ),
            ]),
        },
        Res {
            id: "cluster",
            title: "Cluster",
            group: "Cluster",
            terraform: &["kcore_cluster"],
            kctl: "cluster",
            id_field: "status",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![
                act("backup", "Backup", false, NONE),
                act(
                    "restore",
                    "Restore",
                    true,
                    leak(vec![text_field("sqlite_base64", "SQLite base64", true)]),
                ),
                act(
                    "apply-nix",
                    "Apply Nix config",
                    true,
                    leak(vec![
                        text_field("configuration_nix", "configuration.nix", true),
                        bool_field("rebuild", "Rebuild"),
                    ]),
                ),
            ]),
        },
        Res {
            id: "conflicts",
            title: "Replication conflicts",
            group: "Cluster",
            terraform: &[],
            kctl: "conflict",
            id_field: "id",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![act("resolve", "Resolve", true, NONE)]),
        },
        Res {
            id: "replication",
            title: "Replication",
            group: "Cluster",
            terraform: &[],
            kctl: "cluster",
            id_field: "status",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "operators",
            title: "Operators",
            group: "Identity",
            terraform: &["kcore_operator"],
            kctl: "operator",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![text_field("name", "Name", true)]),
            actions: leak_acts(vec![
                act("delete", "Delete", true, NONE),
                act(
                    "grant",
                    "Grant role",
                    false,
                    leak(vec![text_field("role", "Role", true)]),
                ),
                act(
                    "revoke",
                    "Revoke role",
                    true,
                    leak(vec![text_field("role", "Role", true)]),
                ),
                act("issue-cert", "Issue certificate", true, NONE),
            ]),
        },
        Res {
            id: "ssh-keys",
            title: "SSH keys",
            group: "Identity",
            terraform: &["kcore_ssh_key"],
            kctl: "ssh_key",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("public_key", "Public key", true),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "certificates",
            title: "Certificates",
            group: "Identity",
            terraform: &[],
            kctl: "certs",
            id_field: "serial",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![
                act(
                    "revoke",
                    "Revoke",
                    true,
                    leak(vec![text_field("reason", "Reason", false)]),
                ),
                act("renew", "Renew", true, NONE),
                act(
                    "rotate",
                    "Rotate",
                    true,
                    leak(vec![
                        text_field("node_id", "Node", false),
                        bool_field("all_nodes", "All nodes"),
                    ]),
                ),
                act(
                    "sign",
                    "Sign CSR",
                    true,
                    leak(vec![
                        text_field("node_id", "Node", true),
                        text_field("csr_pem", "CSR PEM", true),
                    ]),
                ),
            ]),
        },
        Res {
            id: "crl",
            title: "Revocation list",
            group: "Identity",
            terraform: &[],
            kctl: "pki",
            id_field: "revoked",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "pki",
            title: "PKI",
            group: "Identity",
            terraform: &[],
            kctl: "pki",
            id_field: "revocation_fail_mode",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![
                act(
                    "bootstrap",
                    "Issue bootstrap cert",
                    true,
                    leak(vec![
                        text_field("node_id", "Node", true),
                        text_field("node_host", "Host", false),
                    ]),
                ),
                act(
                    "rotate-sub-ca",
                    "Rotate sub-CA",
                    true,
                    leak(vec![
                        text_field("sub_ca_cert_pem", "Sub-CA certificate", true),
                        text_field("sub_ca_key_pem", "Sub-CA key", true),
                    ]),
                ),
                act(
                    "reload-tls",
                    "Reload TLS",
                    true,
                    leak(vec![
                        text_field("cert_pem", "Certificate", true),
                        text_field("key_pem", "Key", true),
                    ]),
                ),
            ]),
        },
        Res {
            id: "postgresql",
            title: "PostgreSQL",
            group: "Services",
            terraform: &[],
            kctl: "postgresql",
            id_field: "name",
            list: true,
            creatable: true,
            fields: leak(vec![
                text_field("name", "Name", true),
                text_field("database", "Database", false),
                text_field("package", "Package", false),
                num_field("port", "Port", false),
                text_field("target_node", "Target node", false),
            ]),
            actions: leak_acts(vec![act("delete", "Delete", true, NONE)]),
        },
        Res {
            id: "gpus",
            title: "GPUs and devices",
            group: "Observe",
            terraform: &[],
            kctl: "gpu",
            id_field: "name",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "health",
            title: "Health",
            group: "Observe",
            terraform: &[],
            kctl: "cluster",
            id_field: "status",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "compliance",
            title: "Compliance",
            group: "Observe",
            terraform: &[],
            kctl: "compliance",
            id_field: "controller_version",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "audit",
            title: "Audit",
            group: "Observe",
            terraform: &[],
            kctl: "audit",
            id_field: "id",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "crypto",
            title: "Crypto",
            group: "Observe",
            terraform: &[],
            kctl: "compliance",
            id_field: "library",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
        Res {
            id: "sbom",
            title: "SBOM",
            group: "Observe",
            terraform: &[],
            kctl: "compliance",
            id_field: "kind",
            list: true,
            creatable: false,
            fields: NONE,
            actions: leak_acts(vec![]),
        },
    ]
}

fn leak_acts(actions: Vec<Act>) -> &'static [Act] {
    Box::leak(actions.into_boxed_slice())
}

fn resources() -> &'static [Res] {
    static CELL: std::sync::OnceLock<Vec<Res>> = std::sync::OnceLock::new();
    CELL.get_or_init(catalog_resources)
}

fn inspectable(id: &str) -> bool {
    matches!(
        id,
        "vms"
            | "nodes"
            | "volumes"
            | "kubernetes"
            | "ceph"
            | "disk-layouts"
            | "postgresql"
            | "ssh-keys"
            | "security-groups"
            | "networks"
            | "filesystems"
            | "object-stores"
            | "snapshot-policies"
            | "operations"
    )
}

fn res_json(r: &Res) -> Value {
    json!({
        "id": r.id,
        "title": r.title,
        "group": r.group,
        "terraform": r.terraform,
        "kctl": r.kctl,
        "idField": r.id_field,
        "list": r.list,
        "creatable": r.creatable,
        "inspect": inspectable(r.id),
        "fields": r.fields.iter().map(field_json).collect::<Vec<_>>(),
        "actions": r.actions.iter().map(act_json).collect::<Vec<_>>(),
    })
}

fn field_json(f: &Field) -> Value {
    json!({
        "name": f.name,
        "label": f.label,
        "kind": f.kind,
        "required": f.required,
    })
}

fn act_json(a: &Act) -> Value {
    json!({
        "name": a.name,
        "label": a.label,
        "confirm": a.confirm,
        "client": a.name == "console",
        "preload": a.preload,
        "fields": a.fields.iter().map(field_json).collect::<Vec<_>>(),
    })
}

fn catalog_value() -> Value {
    json!({ "resources": resources().iter().map(res_json).collect::<Vec<_>>() })
}

#[derive(Debug)]
enum ApiError {
    Status(StatusCode, String),
    Rpc(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Rpc(err)
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::Status(StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> ApiError {
    ApiError::Status(StatusCode::NOT_FOUND, msg.into())
}

fn into_pair(err: ApiError) -> (StatusCode, String) {
    match err {
        ApiError::Status(code, msg) => (code, msg),
        ApiError::Rpc(err) => (StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

pub fn router() -> Router {
    Router::new()
        .route("/catalog", get(catalog))
        .route("/resources/{id}", get(list_resource).post(create_resource))
        .route("/resources/{id}/items/{key}", get(read_resource))
        .route("/resources/{id}/actions", post(act_resource))
}

async fn catalog() -> Json<Value> {
    Json(catalog_value())
}

#[derive(Debug, Deserialize, Default)]
struct ListQuery {
    #[serde(default)]
    volume: String,
    #[serde(default)]
    since: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    limit: u32,
    #[serde(default)]
    node_id: String,
}

#[derive(Debug, Deserialize)]
struct ActionBody {
    action: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    target_node: String,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    confirm: bool,
    #[serde(default)]
    extra: Value,
}

async fn list_resource(Path(id): Path<String>, Query(q): Query<ListQuery>) -> ApiResult {
    list_rows(&id, &q).await.map_err(into_pair)
}

async fn create_resource(Path(id): Path<String>, Json(body): Json<Value>) -> ApiResult {
    create_one(&id, &body).await.map(Json).map_err(into_pair)
}

async fn read_resource(Path((id, key)): Path<(String, String)>) -> ApiResult {
    read_item(&id, &key).await.map(Json).map_err(into_pair)
}

async fn act_resource(Path(id): Path<String>, Json(body): Json<ActionBody>) -> ApiResult {
    let action = body.action.clone();
    let value = action_value(body);
    run_action(&id, &action, &value)
        .await
        .map(Json)
        .map_err(into_pair)
}

fn action_value(body: ActionBody) -> Value {
    let mut base = match body.extra {
        Value::Object(map) => Value::Object(map),
        _ => Value::Object(serde_json::Map::new()),
    };
    let Some(obj) = base.as_object_mut() else {
        return base;
    };
    obj.insert("action".into(), json!(body.action));
    if !body.id.is_empty() {
        obj.entry("id").or_insert_with(|| json!(body.id.clone()));
        obj.entry("name").or_insert_with(|| json!(body.id));
    }
    if !body.target_node.is_empty() {
        obj.insert("target_node".into(), json!(body.target_node));
    }
    if body.force {
        obj.insert("force".into(), json!(true));
    }
    if body.confirm {
        obj.insert("confirm".into(), json!(true));
    }
    base
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn flag(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

fn num_i64(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn num_i32(v: &Value, key: &str) -> i32 {
    num_i64(v, key) as i32
}

fn required(v: &Value, key: &str) -> Result<String, ApiError> {
    let value = text(v, key);
    if value.is_empty() {
        Err(bad(format!("{key} is required")))
    } else {
        Ok(value)
    }
}

fn confirmed(v: &Value) -> Result<(), ApiError> {
    if flag(v, "confirm") {
        Ok(())
    } else {
        Err(bad("confirm must be true"))
    }
}

fn ident(v: &Value) -> String {
    let id = text(v, "id");
    if id.is_empty() {
        text(v, "name")
    } else {
        id
    }
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn num_list(v: &Value, key: &str) -> Vec<i32> {
    match v.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_i64()
                    .map(|n| n as i32)
                    .or_else(|| item.as_str().and_then(|s| s.parse().ok()))
            })
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect(),
        _ => Vec::new(),
    }
}

fn label_map(v: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(obj) = v.get("labels").and_then(|value| value.as_object()) {
        for (key, value) in obj {
            if let Some(text) = value.as_str() {
                map.insert(key.clone(), text.to_string());
            }
        }
    }
    map
}

fn backend_enum(name: &str) -> Result<i32, ApiError> {
    use controller_proto::StorageBackendType::{Ceph, Filesystem, Lvm, Zfs};
    Ok(match name {
        "filesystem" | "file" => Filesystem as i32,
        "lvm" => Lvm as i32,
        "zfs" => Zfs as i32,
        "ceph" => Ceph as i32,
        _ => {
            return Err(bad(
                "storage_backend must be filesystem, file, lvm, zfs, or ceph",
            ))
        }
    })
}

fn desired_state(v: &Value) -> i32 {
    use controller_proto::VmDesiredState::{Running, Stopped};
    match text(v, "desired_state").as_str() {
        "stopped" | "stop" => Stopped as i32,
        _ => Running as i32,
    }
}

fn workload_desired(running: bool) -> i32 {
    use controller_proto::WorkloadDesiredState::{Running, Stopped};
    if running {
        Running as i32
    } else {
        Stopped as i32
    }
}

fn nics(v: &Value) -> Vec<controller_proto::Nic> {
    if let Some(items) = v.get("nics").and_then(|value| value.as_array()) {
        return items
            .iter()
            .map(|item| {
                let mut nic = controller_proto::Nic::default();
                nic.network = text(item, "network");
                nic.model = text(item, "model");
                nic.mac_address = text(item, "mac_address");
                nic
            })
            .filter(|nic| !nic.network.is_empty())
            .collect();
    }
    let network = text(v, "network");
    if network.is_empty() {
        return Vec::new();
    }
    let mut nic = controller_proto::Nic::default();
    nic.network = network;
    nic.model = text(v, "nic_model");
    nic.mac_address = text(v, "mac_address");
    vec![nic]
}

fn disks(v: &Value) -> Vec<controller_proto::Disk> {
    let Some(items) = v.get("disks").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            let mut disk = controller_proto::Disk::default();
            disk.name = text(item, "name");
            disk.backend_handle = text(item, "backend_handle");
            disk.bus = text(item, "bus");
            disk.device = text(item, "device");
            disk
        })
        .filter(|disk| !disk.name.is_empty() || !disk.backend_handle.is_empty())
        .collect()
}

fn proto_name<E>(value: i32) -> String
where
    E: TryFrom<i32> + std::fmt::Debug,
{
    match E::try_from(value) {
        Ok(parsed) => format!("{parsed:?}"),
        Err(_) => value.to_string(),
    }
}

fn rows_of<T: Serialize>(value: T) -> Result<Vec<Value>, ApiError> {
    let parsed = serde_json::to_value(value).context("json")?;
    Ok(match parsed {
        Value::Array(items) => items,
        other => vec![other],
    })
}

fn vm_rows(vms: Vec<controller_proto::VmInfo>) -> Vec<Value> {
    vms.into_iter()
        .map(|vm| {
            json!({
                "id": vm.id,
                "name": vm.name,
                "kind": "vm",
                "state": format::vm_state_label(vm.state),
                "cpu": vm.cpu,
                "memory": format::memory_mebibytes(vm.memory_bytes),
                "node_id": vm.node_id,
                "storage_backend": format::storage_backend_name(&vm.storage_backend),
            })
        })
        .collect()
}

async fn client() -> Result<ControllerClient<Channel>, ApiError> {
    let channel = controller_client::connect_channel(crate::state::dashboard_config()).await?;
    Ok(ControllerClient::new(channel))
}

async fn admin(
) -> Result<controller_proto::controller_admin_client::ControllerAdminClient<Channel>, ApiError> {
    let channel = controller_client::connect_channel(crate::state::dashboard_config()).await?;
    Ok(controller_proto::controller_admin_client::ControllerAdminClient::new(channel))
}

fn cfg() -> &'static crate::config::DashboardConfig {
    crate::state::dashboard_config()
}

async fn list_rows(id: &str, q: &ListQuery) -> Result<Json<Value>, ApiError> {
    let rows = match id {
        "vms" => vm_rows(controller_client::list_vms(cfg()).await?),
        "networks" => rows_of(mappers::networks_from_proto(
            controller_client::list_networks(cfg()).await?,
        ))?,
        "volumes" => rows_of(mappers::volumes_from_proto(
            controller_client::list_volumes(cfg()).await?,
        ))?,
        "nodes" => rows_of(mappers::nodes_from_proto(
            controller_client::list_nodes(cfg()).await?,
        ))?,
        "ssh-keys" => rows_of(mappers::ssh_keys_from_proto(
            controller_client::list_ssh_keys(cfg()).await?,
        ))?,
        "kubernetes" => rows_of(mappers::kubernetes_clusters_from_proto(
            controller_client::list_kubernetes_clusters(cfg()).await?,
        ))?,
        "ceph" => rows_of(mappers::ceph_clusters_from_proto(
            controller_client::list_ceph_clusters(cfg()).await?,
        ))?,
        "filesystems" => rows_of(mappers::shared_filesystems_from_proto(
            controller_client::list_shared_filesystems(cfg()).await?,
        ))?,
        "object-stores" => rows_of(mappers::object_stores_from_proto(
            controller_client::list_object_stores(cfg()).await?,
        ))?,
        "postgresql" => rows_of(mappers::dbms_from_postgresqls(
            controller_client::list_postgresqls(cfg()).await?,
        ))?,
        "containers" => rows_of(mappers::containers_from_proto(
            controller_client::list_containers(cfg()).await?,
        ))?,
        "disk-layouts" => rows_of(mappers::disk_layouts_from_proto(
            controller_client::list_disk_layouts(cfg()).await?,
        ))?,
        "snapshot-policies" => rows_of(mappers::snapshot_policies_from_proto(
            controller_client::list_snapshot_policies(cfg()).await?,
        ))?,
        "snapshots" => rows_of(mappers::snapshots_from_proto(
            controller_client::list_volume_snapshots(cfg(), &q.volume).await?,
        ))?,
        "audit" => {
            let limit = if q.limit == 0 { 100 } else { q.limit };
            rows_of(mappers::audit_events_from_proto(
                controller_client::list_audit_events(cfg(), limit, &q.since, &q.action).await?,
            ))?
        }
        "operations" => rows_of(mappers::vm_operations_from_proto(
            controller_client::list_vm_operations(cfg(), true).await?,
        ))?,
        "compliance" => rows_of(mappers::compliance_from_proto(
            controller_client::get_compliance(cfg()).await?,
        ))?,
        "health" | "cluster" => rows_of(mappers::cluster_health_from_proto(
            controller_client::get_cluster_health(cfg()).await?,
        ))?,
        "certificates" => rows_of(mappers::certificates_from_proto(
            controller_client::list_certificates(cfg()).await?,
        ))?,
        "pki" => rows_of(mappers::pki_status_from_proto(
            controller_client::get_pki_status(cfg()).await?,
        ))?,
        "crl" => rows_of(mappers::crl_summary_from_proto(
            controller_client::get_crl(cfg()).await?,
        ))?,
        "crypto" => rows_of(mappers::crypto_config_from_proto(
            controller_client::get_crypto_config(cfg()).await?,
        ))?,
        "sbom" => {
            let mut rows = Vec::new();
            for kind in ["crates", "iso-closure"] {
                let result = controller_client::export_sbom(cfg(), kind)
                    .await
                    .map_err(|err| format!("{err:#}"));
                rows.push(mappers::sbom_summary_from_proto(kind, result));
            }
            rows_of(rows)?
        }
        "conflicts" => rows_of(mappers::conflicts_from_proto(
            controller_client::list_replication_conflicts(cfg()).await?,
        ))?,
        "replication" => rows_of(mappers::replication_status_from_proto(
            controller_client::get_replication_status(cfg()).await?,
        ))?,
        "storage" => rows_of(mappers::storage_overview_from_proto(
            controller_client::get_storage_overview(cfg()).await?,
        ))?,
        "network" => rows_of(mappers::network_overview_from_proto(
            controller_client::get_network_overview(cfg()).await?,
        ))?,
        "storage-classes" => {
            let nodes = mappers::nodes_from_proto(controller_client::list_nodes(cfg()).await?);
            rows_of(mappers::storage_classes_from_nodes(&nodes))?
        }
        "object-users" => Vec::new(),
        "security-groups" => list_security_groups().await?,
        "operators" => list_operators().await?,
        "cluster-updates" => list_cluster_updates().await?,
        "workloads" => list_workloads().await?,
        "gpus" => list_gpus(&q.node_id).await?,
        other => {
            return Err(ApiError::Status(
                StatusCode::NOT_FOUND,
                format!("unknown resource {other}"),
            ))
        }
    };
    Ok(Json(json!({ "rows": rows })))
}

async fn list_security_groups() -> Result<Vec<Value>, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .list_security_groups(controller_proto::ListSecurityGroupsRequest {})
        .await
        .context("ListSecurityGroups")?
        .into_inner();
    Ok(resp
        .security_groups
        .into_iter()
        .map(|group| {
            json!({
                "name": group.name,
                "description": group.description,
                "rules": group.rules.len(),
            })
        })
        .collect())
}

async fn list_operators() -> Result<Vec<Value>, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .list_operators(controller_proto::ListOperatorsRequest {})
        .await
        .context("ListOperators")?
        .into_inner();
    Ok(resp
        .operators
        .into_iter()
        .map(|operator| {
            json!({
                "name": operator.name,
                "cert_serial": operator.cert_serial,
                "roles": operator.roles.into_iter().map(proto_name::<controller_proto::OperatorRoleKind>).collect::<Vec<_>>(),
            })
        })
        .collect())
}

async fn list_cluster_updates() -> Result<Vec<Value>, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .list_cluster_updates(controller_proto::ListClusterUpdatesRequest {})
        .await
        .context("ListClusterUpdates")?
        .into_inner();
    Ok(resp
        .cluster_updates
        .into_iter()
        .map(|update| {
            let spec = update.spec.unwrap_or_default();
            let version = spec
                .target
                .as_ref()
                .map(|target| target.version.clone())
                .unwrap_or_default();
            json!({
                "name": spec.name,
                "generation": update.generation,
                "phase": proto_name::<controller_proto::ClusterUpdatePhase>(update.phase),
                "approval": proto_name::<controller_proto::ClusterUpdateApprovalStatus>(update.approval_status),
                "update_kind": spec.update_kind,
                "version": version,
            })
        })
        .collect())
}

async fn list_workloads() -> Result<Vec<Value>, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .list_workloads(controller_proto::ListWorkloadsRequest::default())
        .await
        .context("ListWorkloads")?
        .into_inner();
    let mut rows = vm_rows(resp.vms);
    for container in resp.containers {
        rows.push(json!({
            "id": container.id,
            "name": container.name,
            "kind": "container",
            "image": container.image,
            "state": container.status,
            "node_id": container.node_id,
        }));
    }
    Ok(rows)
}

async fn list_gpus(node_id: &str) -> Result<Vec<Value>, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .list_gpus(controller_proto::ListGpusRequest {
            node_id: node_id.to_string(),
        })
        .await
        .context("ListGpus")?
        .into_inner();
    Ok(resp
        .gpus
        .into_iter()
        .map(|gpu| {
            json!({
                "name": gpu.name,
                "family": gpu.family,
                "model": gpu.model,
                "address": gpu.address,
                "node_id": gpu.node_id,
                "assigned_vm": gpu.assigned_vm,
                "kind": gpu.kind,
                "role": gpu.role,
                "assignable": gpu.assignable,
            })
        })
        .collect())
}

fn wrapped(detail: impl Serialize, spec: Option<Value>) -> Result<Value, ApiError> {
    Ok(json!({
        "detail": serde_json::to_value(detail).context("json")?,
        "spec": spec.unwrap_or(Value::Null),
    }))
}

async fn read_item(id: &str, key: &str) -> Result<Value, ApiError> {
    if key.is_empty() {
        return Err(bad("id is required"));
    }
    let mut rpc = client().await?;
    match id {
        "vms" => {
            let vm = rpc
                .get_vm(controller_proto::GetVmRequest {
                    vm_id: key.to_string(),
                    target_node: String::new(),
                })
                .await
                .context("GetVm")?
                .into_inner();
            let migrate = match rpc
                .get_live_migrate_receive_status(
                    controller_proto::GetLiveMigrateReceiveStatusRequest {
                        vm_id: key.to_string(),
                        node_id: String::new(),
                    },
                )
                .await
            {
                Ok(resp) => Ok(resp.into_inner()),
                Err(err) => Err(err.to_string()),
            };
            wrapped(mappers::vm_detail_from_proto(vm, migrate), None)
        }
        "nodes" => {
            let node = rpc
                .get_node(controller_proto::GetNodeRequest {
                    node_id: key.to_string(),
                })
                .await
                .context("GetNode")?
                .into_inner()
                .node
                .ok_or_else(|| missing(format!("node {key} not found")))?;
            let devices = match rpc
                .list_gpus(controller_proto::ListGpusRequest {
                    node_id: key.to_string(),
                })
                .await
            {
                Ok(resp) => Ok(resp.into_inner().gpus),
                Err(err) => Err(err.to_string()),
            };
            wrapped(mappers::node_detail_from_proto(node, devices), None)
        }
        "volumes" => {
            let volume = rpc
                .get_volume(controller_proto::GetVolumeRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetVolume")?
                .into_inner()
                .volume
                .ok_or_else(|| missing(format!("volume {key} not found")))?;
            wrapped(mappers::volume_detail_from_info(volume, true), None)
        }
        "kubernetes" => {
            let cluster = rpc
                .get_kubernetes_cluster(controller_proto::GetKubernetesClusterRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetKubernetesCluster")?
                .into_inner()
                .cluster
                .ok_or_else(|| missing(format!("kubernetes cluster {key} not found")))?;
            let health = match rpc
                .get_kubernetes_cluster_health(
                    controller_proto::GetKubernetesClusterHealthRequest {
                        name: key.to_string(),
                    },
                )
                .await
            {
                Ok(resp) => Ok(resp.into_inner()),
                Err(err) => Err(err.to_string()),
            };
            let spec = cluster
                .spec
                .as_ref()
                .map(|spec| k8s_form(&cluster.name, spec));
            wrapped(mappers::kubernetes_detail_from_proto(cluster, health), spec)
        }
        "ceph" => {
            let cluster = rpc
                .get_ceph_cluster(controller_proto::GetCephClusterRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetCephCluster")?
                .into_inner()
                .ceph_cluster
                .ok_or_else(|| missing(format!("ceph cluster {key} not found")))?;
            let spec = ceph_form(&cluster);
            let mut detail = serde_json::to_value(mappers::ceph_cluster_detail_from_proto(cluster))
                .context("json")?;
            match rpc
                .get_ceph_cluster_capacity(controller_proto::GetCephClusterCapacityRequest {
                    name: key.to_string(),
                })
                .await
            {
                Ok(resp) => {
                    let cap = resp.into_inner();
                    detail["capacity"] = json!({
                        "ok": cap.ok,
                        "message": cap.message,
                        "queried_node": cap.queried_node,
                        "total_bytes": cap.total_bytes,
                        "used_bytes": cap.used_bytes,
                        "avail_bytes": cap.avail_bytes,
                        "by_class": cap.by_class.iter().map(|class| json!({
                            "device_class": class.device_class,
                            "total_bytes": class.total_bytes,
                            "used_bytes": class.used_bytes,
                            "avail_bytes": class.avail_bytes,
                        })).collect::<Vec<_>>(),
                        "pools": cap.pools.iter().map(|pool| json!({
                            "name": pool.name,
                            "stored_bytes": pool.stored_bytes,
                            "objects": pool.objects,
                            "percent_used_bp": pool.percent_used_bp,
                            "max_avail_bytes": pool.max_avail_bytes,
                        })).collect::<Vec<_>>(),
                    });
                }
                Err(err) => {
                    detail["capacity_error"] = json!(err.to_string());
                }
            }
            Ok(json!({ "detail": detail, "spec": spec }))
        }
        "disk-layouts" => {
            let resp = rpc
                .get_disk_layout(controller_proto::GetDiskLayoutRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetDiskLayout")?
                .into_inner();
            let layout = resp.disk_layout.clone().unwrap_or_default();
            let spec = json!({
                "name": layout.name,
                "node_id": layout.node_id,
                "layout_nix": layout.layout_nix,
                "evacuate": layout.evacuate,
                "confirm": true,
            });
            wrapped(mappers::disk_layout_detail_from_proto(resp), Some(spec))
        }
        "postgresql" => {
            let info = rpc
                .get_postgresql(controller_proto::GetPostgresqlRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetPostgresql")?
                .into_inner()
                .postgresql
                .ok_or_else(|| missing(format!("postgresql {key} not found")))?;
            wrapped(mappers::dbms_detail_from_postgresql(info), None)
        }
        "ssh-keys" => {
            let key_info = rpc
                .get_ssh_key(controller_proto::GetSshKeyRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetSshKey")?
                .into_inner()
                .key
                .ok_or_else(|| missing(format!("ssh key {key} not found")))?;
            wrapped(mappers::ssh_key_detail_from_proto(key_info), None)
        }
        "security-groups" => {
            let resp = rpc
                .get_security_group(controller_proto::GetSecurityGroupRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetSecurityGroup")?
                .into_inner();
            let group = resp
                .security_group
                .ok_or_else(|| missing(format!("security group {key} not found")))?;
            let rules = group
                .rules
                .iter()
                .map(|rule| {
                    json!({
                        "protocol": rule.protocol,
                        "host_port": rule.host_port,
                        "target_port": rule.target_port,
                        "source_cidr": rule.source_cidr,
                        "target_vm": rule.target_vm,
                        "enable_dnat": rule.enable_dnat,
                    })
                })
                .collect::<Vec<_>>();
            let attachments = resp
                .attachments
                .iter()
                .map(|attachment| {
                    json!({
                        "security_group": attachment.security_group,
                        "target_kind": proto_name::<controller_proto::SecurityGroupTargetKind>(attachment.target_kind),
                        "target_id": attachment.target_id,
                        "target_node": attachment.target_node,
                    })
                })
                .collect::<Vec<_>>();
            let spec_rules = rules.clone();
            Ok(json!({
                "detail": {
                    "name": group.name,
                    "description": group.description,
                    "rules": rules,
                    "attachments": attachments,
                },
                "spec": {
                    "name": group.name,
                    "description": group.description,
                    "rules": spec_rules,
                },
            }))
        }
        "networks" => {
            let nets = controller_client::list_networks(cfg()).await?;
            let net = nets
                .into_iter()
                .find(|net| net.name == key)
                .ok_or_else(|| missing(format!("network {key} not found")))?;
            let rows = mappers::networks_from_proto(vec![net]);
            let detail = rows
                .into_iter()
                .next()
                .ok_or_else(|| missing(format!("network {key} not found")))?;
            wrapped(detail, None)
        }
        "filesystems" => {
            let fs = rpc
                .get_shared_filesystem(controller_proto::GetSharedFilesystemRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetSharedFilesystem")?
                .into_inner()
                .shared_filesystem
                .ok_or_else(|| missing(format!("filesystem {key} not found")))?;
            wrapped(mappers::shared_filesystem_detail_from_proto(fs), None)
        }
        "object-stores" => {
            let store = rpc
                .get_object_store(controller_proto::GetObjectStoreRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetObjectStore")?
                .into_inner()
                .object_store
                .ok_or_else(|| missing(format!("object store {key} not found")))?;
            wrapped(mappers::object_store_detail_from_proto(store), None)
        }
        "snapshot-policies" => {
            let policy = rpc
                .get_snapshot_policy(controller_proto::GetSnapshotPolicyRequest {
                    name: key.to_string(),
                })
                .await
                .context("GetSnapshotPolicy")?
                .into_inner()
                .policy
                .ok_or_else(|| missing(format!("snapshot policy {key} not found")))?;
            wrapped(mappers::snapshot_policy_from_proto(policy), None)
        }
        "operations" => {
            let operation = rpc
                .get_vm_operation(controller_proto::GetVmOperationRequest {
                    id: key.to_string(),
                })
                .await
                .context("GetVmOperation")?
                .into_inner()
                .operation
                .ok_or_else(|| missing(format!("operation {key} not found")))?;
            wrapped(mappers::operation_detail_from_proto(operation), None)
        }
        other => Err(missing(format!("no detail for {other}"))),
    }
}

async fn create_one(id: &str, v: &Value) -> Result<Value, ApiError> {
    match id {
        "vms" => create_vm(v).await,
        "networks" => create_network(v).await,
        "volumes" => create_volume(v).await,
        "security-groups" => create_security_group(v).await,
        "ssh-keys" => create_ssh_key(v).await,
        "operators" => create_operator(v).await,
        "kubernetes" => create_kubernetes(v).await,
        "postgresql" => create_postgresql(v).await,
        "snapshots" => create_snapshot(v).await,
        "snapshot-policies" => create_snapshot_policy(v).await,
        "disk-layouts" => create_disk_layout(v).await,
        "cluster-updates" => create_cluster_update(v).await,
        "ceph" => create_ceph(v).await,
        "filesystems" => create_filesystem(v).await,
        "object-stores" => create_object_store(v).await,
        "object-users" => create_object_user(v).await,
        "workloads" => create_workload(v, false).await,
        "containers" => create_workload(v, true).await,
        other => Err(bad(format!("{other} cannot be created from the dashboard"))),
    }
}

async fn run_action(id: &str, action: &str, v: &Value) -> Result<Value, ApiError> {
    if action == "console" {
        return Err(bad("console opens /vms/<id>/console in the browser"));
    }
    match (id, action) {
        ("vms", "start") => set_vm(v, true).await,
        ("vms", "stop") => set_vm(v, false).await,
        ("vms", "delete") => {
            confirmed(v)?;
            delete_vm(v).await
        }
        ("vms", "migrate") => {
            confirmed(v)?;
            migrate_vm(v).await
        }
        ("vms", "migrate-status") => migrate_status(v).await,
        ("vms", "reset-migrate") => reset_migrate(v).await,
        ("vms", "update") => update_vm(v).await,
        ("workloads" | "containers", "start") => set_workload(id, v, true).await,
        ("workloads" | "containers", "stop") => set_workload(id, v, false).await,
        ("workloads" | "containers", "delete") => {
            confirmed(v)?;
            delete_workload(id, v).await
        }
        ("operations", "cancel") => cancel_operation(v).await,
        ("networks", "delete") => {
            confirmed(v)?;
            delete_named(v, "network").await
        }
        ("security-groups", "delete") => {
            confirmed(v)?;
            delete_security_group(v).await
        }
        ("security-groups", "attach") => attach_security_group(v, true).await,
        ("security-groups", "detach") => {
            confirmed(v)?;
            attach_security_group(v, false).await
        }
        ("volumes", "delete") => {
            confirmed(v)?;
            delete_volume(v).await
        }
        ("volumes", "attach") => attach_volume(v).await,
        ("volumes", "detach") => {
            confirmed(v)?;
            detach_volume(v).await
        }
        ("volumes", "resize") => {
            confirmed(v)?;
            resize_volume(v).await
        }
        ("volumes", "snapshot") => create_snapshot(v).await,
        ("volumes", "restore") => {
            confirmed(v)?;
            restore_volume(v).await
        }
        ("volumes", "flatten") => {
            confirmed(v)?;
            flatten_volume(v).await
        }
        ("volumes", "format") => {
            confirmed(v)?;
            format_volume(v).await
        }
        ("volumes", "mount") => mount_volume(v).await,
        ("volumes", "unmount") => {
            confirmed(v)?;
            unmount_volume(v).await
        }
        ("volumes", "encrypt") => encrypt_volume(v).await,
        ("snapshots", "delete") => {
            confirmed(v)?;
            delete_snapshot(v).await
        }
        ("snapshots", "restore") => {
            confirmed(v)?;
            restore_volume(v).await
        }
        ("snapshot-policies", "delete") => {
            confirmed(v)?;
            delete_policy(v).await
        }
        ("disk-layouts", "classify") => classify_disk_layout(v).await,
        ("disk-layouts", "delete") => {
            confirmed(v)?;
            delete_disk_layout(v).await
        }
        ("ceph", "delete") => {
            confirmed(v)?;
            delete_ceph(v).await
        }
        ("ceph", "plan") => plan_ceph(v).await,
        ("ceph", "credentials") => {
            confirmed(v)?;
            set_ceph_credentials(v).await
        }
        ("ceph", "show-credentials") => {
            confirmed(v)?;
            show_ceph_credentials(v).await
        }
        ("filesystems", "delete") => {
            confirmed(v)?;
            delete_filesystem(v).await
        }
        ("object-stores", "delete") => {
            confirmed(v)?;
            delete_object_store(v).await
        }
        ("object-users", "delete") => {
            confirmed(v)?;
            delete_object_user(v).await
        }
        ("nodes", "approve") => node_simple(v, "approve").await,
        ("nodes", "reject") => {
            confirmed(v)?;
            node_simple(v, "reject").await
        }
        ("nodes", "cordon") => {
            confirmed(v)?;
            node_simple(v, "cordon").await
        }
        ("nodes", "uncordon") => node_simple(v, "uncordon").await,
        ("nodes", "drain") => {
            confirmed(v)?;
            drain_node(v).await
        }
        ("nodes", "delete") => {
            confirmed(v)?;
            node_simple(v, "delete").await
        }
        ("kubernetes", "delete") => {
            confirmed(v)?;
            delete_kubernetes(v).await
        }
        ("kubernetes", "update") => update_kubernetes(v).await,
        ("kubernetes", "scale") => {
            confirmed(v)?;
            scale_kubernetes(v).await
        }
        ("kubernetes", "plan") => plan_kubernetes(v).await,
        ("cluster-updates", "plan") => plan_update(v).await,
        ("cluster-updates", "approve") => {
            confirmed(v)?;
            update_simple(v, "approve").await
        }
        ("cluster-updates", "cancel") => {
            confirmed(v)?;
            update_simple(v, "cancel").await
        }
        ("cluster-updates", "rollback") => {
            confirmed(v)?;
            update_simple(v, "rollback").await
        }
        ("cluster-updates", "reboot") => {
            confirmed(v)?;
            reboot_update(v).await
        }
        ("cluster", "backup") => backup_cluster().await,
        ("cluster", "restore") => {
            confirmed(v)?;
            restore_cluster(v).await
        }
        ("cluster", "apply-nix") => {
            confirmed(v)?;
            apply_nix(v).await
        }
        ("conflicts", "resolve") => {
            confirmed(v)?;
            resolve_conflict(v).await
        }
        ("operators", "delete") => {
            confirmed(v)?;
            delete_operator(v).await
        }
        ("operators", "grant") => grant_role(v, true).await,
        ("operators", "revoke") => {
            confirmed(v)?;
            grant_role(v, false).await
        }
        ("operators", "issue-cert") => {
            confirmed(v)?;
            issue_operator_cert(v).await
        }
        ("ssh-keys", "delete") => {
            confirmed(v)?;
            delete_ssh_key(v).await
        }
        ("certificates", "revoke") => {
            confirmed(v)?;
            revoke_cert(v).await
        }
        ("certificates", "renew") => {
            confirmed(v)?;
            renew_cert(v).await
        }
        ("certificates", "rotate") => {
            confirmed(v)?;
            rotate_certs(v).await
        }
        ("certificates", "sign") => {
            confirmed(v)?;
            sign_csr(v).await
        }
        ("pki", "bootstrap") => {
            confirmed(v)?;
            bootstrap_cert(v).await
        }
        ("pki", "rotate-sub-ca") => {
            confirmed(v)?;
            rotate_sub_ca(v).await
        }
        ("pki", "reload-tls") => {
            confirmed(v)?;
            reload_tls(v).await
        }
        ("postgresql", "delete") => {
            confirmed(v)?;
            delete_postgresql(v).await
        }
        _ => Err(bad(format!("unsupported action {action} on {id}"))),
    }
}

fn vm_spec(v: &Value) -> Result<controller_proto::VmSpec, ApiError> {
    let mut spec = controller_proto::VmSpec::default();
    spec.name = required(v, "name")?;
    spec.cpu = num_i32(v, "cpu");
    spec.memory_bytes = num_i64(v, "memory_bytes");
    spec.storage_backend = text(v, "storage_backend");
    spec.storage_size_bytes = num_i64(v, "storage_size_bytes");
    spec.desired_state = desired_state(v);
    spec.disks = disks(v);
    spec.nics = nics(v);
    spec.pci_devices = strings(v, "pci_devices");
    spec.gpus = strings(v, "gpus");
    spec.labels = label_map(v);
    if spec.cpu <= 0 {
        return Err(bad("cpu must be greater than 0"));
    }
    if spec.memory_bytes <= 0 {
        return Err(bad("memory_bytes must be greater than 0"));
    }
    if spec.storage_size_bytes <= 0 {
        return Err(bad("storage_size_bytes must be greater than 0"));
    }
    let _ = backend_enum(&spec.storage_backend)?;
    Ok(spec)
}

fn image_fields(v: &Value, req: &mut controller_proto::CreateVmRequest) -> Result<(), ApiError> {
    req.image_url = text(v, "image_url");
    req.image_sha256 = text(v, "image_sha256");
    req.image_path = text(v, "image_path");
    req.image_format = text(v, "image_format");
    if req.image_path.is_empty() && req.image_url.is_empty() {
        return Err(bad(
            "set image_url and image_sha256, or image_path and image_format",
        ));
    }
    if !req.image_path.is_empty() && req.image_format.is_empty() {
        return Err(bad("image_format is required with image_path"));
    }
    if !req.image_url.is_empty() && req.image_sha256.is_empty() {
        return Err(bad("image_sha256 is required with image_url"));
    }
    Ok(())
}

async fn create_vm(v: &Value) -> Result<Value, ApiError> {
    let spec = vm_spec(v)?;
    let mut req = controller_proto::CreateVmRequest::default();
    req.target_node = text(v, "target_node");
    req.target_dc = text(v, "target_dc");
    req.cloud_init_user_data = text(v, "cloud_init_user_data");
    req.ssh_key_names = strings(v, "ssh_key_names");
    req.storage_backend = backend_enum(&spec.storage_backend)?;
    req.storage_size_bytes = spec.storage_size_bytes;
    req.node_labels = strings(v, "node_labels");
    req.anti_affinity = text(v, "anti_affinity");
    image_fields(v, &mut req)?;
    req.spec = Some(spec);
    let mut rpc = client().await?;
    let resp = rpc.create_vm(req).await.context("CreateVm")?.into_inner();
    Ok(json!({ "ok": true, "id": resp.vm_id, "node": resp.node_id }))
}

async fn set_vm(v: &Value, running: bool) -> Result<Value, ApiError> {
    use controller_proto::VmDesiredState::{Running, Stopped};
    let mut req = controller_proto::SetVmDesiredStateRequest::default();
    req.vm_id = required(v, "id")?;
    req.target_node = text(v, "target_node");
    req.desired_state = if running {
        Running as i32
    } else {
        Stopped as i32
    };
    let mut rpc = client().await?;
    rpc.set_vm_desired_state(req)
        .await
        .context("SetVmDesiredState")?;
    Ok(json!({ "ok": true }))
}

async fn delete_vm(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DeleteVmRequest::default();
    req.vm_id = required(v, "id")?;
    req.target_node = text(v, "target_node");
    req.delete_data_volumes = flag(v, "delete_data_volumes");
    let mut rpc = client().await?;
    rpc.delete_vm(req).await.context("DeleteVm")?;
    Ok(json!({ "ok": true }))
}

async fn migrate_vm(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::MigrateVmRequest::default();
    req.vm_id = required(v, "id")?;
    req.target_node = required(v, "target_node")?;
    req.allow_cold_fallback = flag(v, "allow_cold_fallback");
    req.gpus = strings(v, "gpus");
    let mut rpc = client().await?;
    let resp = rpc.migrate_vm(req).await.context("MigrateVm")?.into_inner();
    Ok(
        json!({ "ok": resp.success, "message": resp.message, "mode": resp.mode, "operation": resp.operation_id }),
    )
}

fn migrate_reset_guard(v: &Value) -> Result<(String, String), ApiError> {
    confirmed(v)?;
    if !flag(v, "force") {
        return Err(bad(
            "force must be true to clear a stranded receive session",
        ));
    }
    Ok((required(v, "id")?, required(v, "node_id")?))
}

async fn migrate_status(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::GetLiveMigrateReceiveStatusRequest::default();
    req.vm_id = required(v, "id")?;
    req.node_id = text(v, "node_id");
    let mut rpc = client().await?;
    let resp = rpc
        .get_live_migrate_receive_status(req)
        .await
        .context("GetLiveMigrateReceiveStatus")?
        .into_inner();
    let nodes = resp
        .nodes
        .iter()
        .map(|node| {
            let state = node.state.clone().unwrap_or_default();
            json!({
                "node_id": node.node_id,
                "address": node.node_address,
                "reachable": node.reachable,
                "error": node.error,
                "has_session": state.has_session,
                "owns_vm": node.owns_vm,
                "summary": state.summary,
                "port": state.port,
                "vmm_alive": state.vmm_alive,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "ok": resp.success,
        "message": resp.message,
        "vm_id": resp.vm_id,
        "vm_name": resp.vm_name,
        "runtime_name": resp.runtime_name,
        "current_node": resp.current_node,
        "nodes": nodes,
    }))
}

async fn reset_migrate(v: &Value) -> Result<Value, ApiError> {
    let (vm_id, node_id) = migrate_reset_guard(v)?;
    let mut req = controller_proto::ResetLiveMigrateReceiveRequest::default();
    req.vm_id = vm_id;
    req.node_id = node_id;
    req.force = true;
    let mut rpc = client().await?;
    let resp = rpc
        .reset_live_migrate_receive(req)
        .await
        .context("ResetLiveMigrateReceive")?
        .into_inner();
    Ok(json!({
        "ok": resp.success,
        "message": resp.message,
        "cleared": resp.cleared,
        "node_id": resp.node_id,
        "runtime_name": resp.runtime_name,
        "summary": resp.observed.map(|state| state.summary).unwrap_or_default(),
    }))
}

async fn update_vm(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::UpdateVmRequest::default();
    req.vm_id = required(v, "id")?;
    req.target_node = text(v, "target_node");
    req.cpu = num_i32(v, "cpu");
    req.memory_bytes = num_i64(v, "memory_bytes");
    req.labels = label_map(v);
    let mut rpc = client().await?;
    let resp = rpc.update_vm(req).await.context("UpdateVm")?.into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn create_network(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::CreateNetworkRequest::default();
    req.name = required(v, "name")?;
    req.external_ip = text(v, "external_ip");
    req.gateway_ip = text(v, "gateway_ip");
    req.internal_netmask = text(v, "internal_netmask");
    req.target_node = text(v, "target_node");
    req.allowed_tcp_ports = num_list(v, "allowed_tcp_ports");
    req.allowed_udp_ports = num_list(v, "allowed_udp_ports");
    req.vlan_id = num_i32(v, "vlan_id");
    req.network_type = text(v, "network_type");
    req.enable_outbound_nat = flag(v, "enable_outbound_nat");
    req.ipv6_prefix = text(v, "ipv6_prefix");
    req.ipv6_gateway = text(v, "ipv6_gateway");
    req.east_west_firewall = flag(v, "east_west_firewall");
    let mut rpc = client().await?;
    let resp = rpc
        .create_network(req)
        .await
        .context("CreateNetwork")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message, "node": resp.node_id }))
}

async fn delete_named(v: &Value, kind: &str) -> Result<Value, ApiError> {
    let name = ident(v);
    if name.is_empty() {
        return Err(bad("name is required"));
    }
    let mut rpc = client().await?;
    match kind {
        "network" => {
            let mut req = controller_proto::DeleteNetworkRequest::default();
            req.name = name;
            req.target_node = text(v, "target_node");
            rpc.delete_network(req).await.context("DeleteNetwork")?;
        }
        _ => return Err(bad("unknown delete")),
    }
    Ok(json!({ "ok": true }))
}

fn one_rule(v: &Value) -> controller_proto::SecurityGroupRule {
    let mut rule = controller_proto::SecurityGroupRule::default();
    rule.protocol = text(v, "protocol");
    rule.host_port = num_i32(v, "host_port");
    rule.target_port = num_i32(v, "target_port");
    rule.source_cidr = text(v, "source_cidr");
    rule.target_vm = text(v, "target_vm");
    rule.enable_dnat = flag(v, "enable_dnat");
    rule
}

fn security_rules(v: &Value) -> Vec<controller_proto::SecurityGroupRule> {
    if let Some(items) = v.get("rules").and_then(|value| value.as_array()) {
        return items.iter().map(one_rule).collect();
    }
    if text(v, "protocol").is_empty() && num_i32(v, "host_port") == 0 {
        return Vec::new();
    }
    vec![one_rule(v)]
}

async fn create_security_group(v: &Value) -> Result<Value, ApiError> {
    let mut group = controller_proto::SecurityGroup::default();
    group.name = required(v, "name")?;
    group.description = text(v, "description");
    group.rules = security_rules(v);
    let mut rpc = client().await?;
    let resp = rpc
        .create_security_group(controller_proto::CreateSecurityGroupRequest {
            security_group: Some(group),
        })
        .await
        .context("CreateSecurityGroup")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

fn target_kind(v: &Value) -> Result<i32, ApiError> {
    use controller_proto::SecurityGroupTargetKind::{Network, Vm};
    Ok(match text(v, "target_kind").to_ascii_lowercase().as_str() {
        "vm" | "1" => Vm as i32,
        "network" | "2" => Network as i32,
        other => {
            return Err(bad(format!(
                "target_kind must be vm or network, got {other}"
            )))
        }
    })
}

async fn attach_security_group(v: &Value, attach: bool) -> Result<Value, ApiError> {
    let kind = target_kind(v)?;
    let target_id = required(v, "target_id")?;
    let group = {
        let name = text(v, "security_group");
        if name.is_empty() {
            ident(v)
        } else {
            name
        }
    };
    if group.is_empty() {
        return Err(bad("security group name is required"));
    }
    let mut rpc = client().await?;
    if attach {
        let mut req = controller_proto::AttachSecurityGroupRequest::default();
        req.security_group = group;
        req.target_kind = kind;
        req.target_id = target_id;
        req.target_node = text(v, "target_node");
        rpc.attach_security_group(req)
            .await
            .context("AttachSecurityGroup")?;
    } else {
        let mut req = controller_proto::DetachSecurityGroupRequest::default();
        req.security_group = group;
        req.target_kind = kind;
        req.target_id = target_id;
        req.target_node = text(v, "target_node");
        rpc.detach_security_group(req)
            .await
            .context("DetachSecurityGroup")?;
    }
    Ok(json!({ "ok": true }))
}

async fn delete_security_group(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_security_group(controller_proto::DeleteSecurityGroupRequest { name: ident(v) })
        .await
        .context("DeleteSecurityGroup")?;
    Ok(json!({ "ok": true }))
}

async fn create_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::CreateVolumeRequest::default();
    req.name = required(v, "name")?;
    req.size_bytes = num_i64(v, "size_bytes");
    if req.size_bytes <= 0 && text(v, "from_snapshot").is_empty() {
        return Err(bad("size_bytes must be greater than 0"));
    }
    req.storage_class = text(v, "storage_class");
    req.vm = text(v, "vm");
    req.guest_format_json = text(v, "guest_format_json");
    req.from_snapshot = text(v, "from_snapshot");
    req.encrypt = flag(v, "encrypt");
    req.node = text(v, "node");
    req.volume_name = text(v, "volume_name");
    let mut rpc = client().await?;
    let resp = rpc
        .create_volume(req)
        .await
        .context("CreateVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn delete_volume(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_volume(controller_proto::DeleteVolumeRequest { name: ident(v) })
        .await
        .context("DeleteVolume")?;
    Ok(json!({ "ok": true }))
}

async fn attach_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::AttachVolumeRequest::default();
    req.name = ident(v);
    req.vm = required(v, "vm")?;
    let mut rpc = client().await?;
    let resp = rpc
        .attach_volume(req)
        .await
        .context("AttachVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn detach_volume(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.detach_volume(controller_proto::DetachVolumeRequest { name: ident(v) })
        .await
        .context("DetachVolume")?;
    Ok(json!({ "ok": true }))
}

async fn resize_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::ResizeVolumeRequest::default();
    req.name = ident(v);
    req.size_bytes = num_i64(v, "size_bytes");
    req.grow_filesystem = flag(v, "grow_filesystem");
    req.allow_shrink = flag(v, "allow_shrink");
    let mut rpc = client().await?;
    let resp = rpc
        .resize_volume(req)
        .await
        .context("ResizeVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn create_snapshot(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::CreateVolumeSnapshotRequest::default();
    req.volume = {
        let volume = text(v, "volume");
        if volume.is_empty() {
            ident(v)
        } else {
            volume
        }
    };
    if req.volume.is_empty() {
        return Err(bad("volume is required"));
    }
    req.name = text(v, "snapshot");
    if req.name.is_empty() && !text(v, "volume").is_empty() {
        req.name = text(v, "name");
    }
    req.consistency = text(v, "consistency");
    let mut rpc = client().await?;
    let resp = rpc
        .create_volume_snapshot(req)
        .await
        .context("CreateVolumeSnapshot")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn delete_snapshot(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_volume_snapshot(controller_proto::DeleteVolumeSnapshotRequest { name: ident(v) })
        .await
        .context("DeleteVolumeSnapshot")?;
    Ok(json!({ "ok": true }))
}

async fn restore_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::RestoreVolumeRequest::default();
    req.volume = {
        let volume = text(v, "volume");
        if volume.is_empty() {
            ident(v)
        } else {
            volume
        }
    };
    req.snapshot = {
        let snap = text(v, "snapshot");
        if snap.is_empty() {
            text(v, "name")
        } else {
            snap
        }
    };
    let mut rpc = client().await?;
    let resp = rpc
        .restore_volume(req)
        .await
        .context("RestoreVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn flatten_volume(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.flatten_volume(controller_proto::FlattenVolumeRequest { name: ident(v) })
        .await
        .context("FlattenVolume")?;
    Ok(json!({ "ok": true }))
}

async fn format_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::FormatVolumeRequest::default();
    req.name = ident(v);
    req.fs_type = required(v, "fs_type")?;
    req.force = flag(v, "force");
    let mut rpc = client().await?;
    let resp = rpc
        .format_volume(req)
        .await
        .context("FormatVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn mount_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::MountVolumeRequest::default();
    req.name = ident(v);
    req.mount_point = required(v, "mount_point")?;
    req.fs_type = text(v, "fs_type");
    req.persistent = flag(v, "persistent");
    let mut rpc = client().await?;
    let resp = rpc
        .mount_volume(req)
        .await
        .context("MountVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn unmount_volume(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::UnmountVolumeRequest::default();
    req.name = ident(v);
    req.persistent = flag(v, "persistent");
    let mut rpc = client().await?;
    let resp = rpc
        .unmount_volume(req)
        .await
        .context("UnmountVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn encrypt_volume(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    if !flag(v, "destroy_data") {
        return Err(bad("destroy_data must be true"));
    }
    let mut req = controller_proto::EncryptVolumeRequest::default();
    req.name = ident(v);
    req.destroy_data = true;
    let mut rpc = client().await?;
    let resp = rpc
        .encrypt_volume(req)
        .await
        .context("EncryptVolume")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn create_snapshot_policy(v: &Value) -> Result<Value, ApiError> {
    let mut policy = controller_proto::SnapshotPolicy::default();
    policy.name = required(v, "name")?;
    policy.selector_vm = text(v, "selector_vm");
    policy.selector_volume = text(v, "selector_volume");
    policy.schedule = required(v, "schedule")?;
    policy.keep = num_i32(v, "keep");
    policy.enabled = v.get("enabled").is_none() || flag(v, "enabled");
    if policy.keep <= 0 {
        return Err(bad("keep must be greater than 0"));
    }
    let mut rpc = client().await?;
    let resp = rpc
        .create_snapshot_policy(controller_proto::CreateSnapshotPolicyRequest {
            policy: Some(policy),
        })
        .await
        .context("CreateSnapshotPolicy")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn delete_policy(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_snapshot_policy(controller_proto::DeleteSnapshotPolicyRequest { name: ident(v) })
        .await
        .context("DeleteSnapshotPolicy")?;
    Ok(json!({ "ok": true }))
}

async fn create_disk_layout(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    let mut layout = controller_proto::DiskLayout::default();
    layout.name = required(v, "name")?;
    layout.node_id = required(v, "node_id")?;
    layout.layout_nix = required(v, "layout_nix")?;
    layout.evacuate = flag(v, "evacuate");
    let mut rpc = client().await?;
    let resp = rpc
        .create_disk_layout(controller_proto::CreateDiskLayoutRequest {
            disk_layout: Some(layout),
        })
        .await
        .context("CreateDiskLayout")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

async fn delete_disk_layout(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_disk_layout(controller_proto::DeleteDiskLayoutRequest { name: ident(v) })
        .await
        .context("DeleteDiskLayout")?;
    Ok(json!({ "ok": true }))
}

fn disk_layout_from_value(v: &Value) -> Result<controller_proto::DiskLayout, ApiError> {
    let mut layout = controller_proto::DiskLayout::default();
    layout.name = text(v, "name");
    layout.node_id = required(v, "node_id")?;
    layout.layout_nix = required(v, "layout_nix")?;
    layout.evacuate = flag(v, "evacuate");
    Ok(layout)
}

async fn classify_disk_layout(v: &Value) -> Result<Value, ApiError> {
    let layout = if text(v, "layout_nix").is_empty() {
        let name = ident(v);
        if name.is_empty() {
            return Err(bad("name or layout_nix is required"));
        }
        let mut rpc = client().await?;
        let resp = rpc
            .get_disk_layout(controller_proto::GetDiskLayoutRequest { name })
            .await
            .context("GetDiskLayout")?
            .into_inner();
        resp.disk_layout
            .ok_or_else(|| missing("disk layout not found"))?
    } else {
        disk_layout_from_value(v)?
    };
    let mut rpc = client().await?;
    let resp = rpc
        .classify_disk_layout(controller_proto::ClassifyDiskLayoutRequest {
            disk_layout: Some(layout),
        })
        .await
        .context("ClassifyDiskLayout")?
        .into_inner();
    Ok(json!({
        "ok": resp.safe,
        "safe": resp.safe,
        "refusal_reason": resp.refusal_reason,
        "detail": resp.detail,
        "target_devices": resp.target_devices,
    }))
}

fn ceph_nodes(v: &Value) -> Vec<controller_proto::CephClusterNodeSpec> {
    match v.get("nodes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let mut node = controller_proto::CephClusterNodeSpec::default();
                if let Some(id) = item.as_str() {
                    node.node_id = id.to_string();
                    node.osd_device = text(v, "osd_device");
                } else {
                    node.node_id = text(item, "node_id");
                    node.osd_device = text(item, "osd_device");
                    node.mon_addr = text(item, "mon_addr");
                    node.cluster_addr = text(item, "cluster_addr");
                    node.public_iface = text(item, "public_iface");
                    node.cluster_iface = text(item, "cluster_iface");
                    node.osd_devices = strings(item, "osd_devices");
                }
                node
            })
            .filter(|node| !node.node_id.is_empty())
            .collect(),
        Some(Value::String(_)) => strings(v, "nodes")
            .into_iter()
            .map(|id| {
                let mut node = controller_proto::CephClusterNodeSpec::default();
                node.node_id = id;
                node.osd_device = text(v, "osd_device");
                node
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn ceph_cluster(v: &Value) -> Result<controller_proto::CephCluster, ApiError> {
    let mut spec = controller_proto::CephClusterSpec::default();
    spec.fsid = text(v, "fsid");
    spec.public_network = required(v, "public_network")?;
    spec.cluster_network = text(v, "cluster_network");
    spec.size = num_i32(v, "size");
    spec.min_size = num_i32(v, "min_size");
    spec.force_wipe = flag(v, "force_wipe");
    if spec.force_wipe {
        confirmed(v)?;
    }
    spec.encrypt_osds = flag(v, "encrypt_osds");
    spec.nodes = ceph_nodes(v);
    if spec.nodes.is_empty() {
        return Err(bad("nodes is required"));
    }
    let mut cluster = controller_proto::CephCluster::default();
    cluster.name = required(v, "name")?;
    cluster.spec = Some(spec);
    Ok(cluster)
}

async fn create_ceph(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    let cluster = ceph_cluster(v)?;
    let mut rpc = client().await?;
    let resp = rpc
        .create_ceph_cluster(controller_proto::CreateCephClusterRequest {
            ceph_cluster: Some(cluster),
        })
        .await
        .context("CreateCephCluster")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

async fn plan_ceph(v: &Value) -> Result<Value, ApiError> {
    let cluster = if v.get("nodes").is_none() {
        stored_ceph(&ident(v)).await?
    } else {
        ceph_cluster(v)?
    };
    let mut rpc = client().await?;
    let resp = rpc
        .plan_ceph_cluster(controller_proto::PlanCephClusterRequest {
            ceph_cluster: Some(cluster),
        })
        .await
        .context("PlanCephCluster")?
        .into_inner();
    Ok(json!({
        "ok": resp.ok,
        "notice": resp.encrypt_osds_notice,
        "issues": resp.cluster_issues,
        "warnings": resp.cluster_warnings,
        "removing": resp.removing_members,
    }))
}

async fn stored_ceph(name: &str) -> Result<controller_proto::CephCluster, ApiError> {
    if name.is_empty() {
        return Err(bad("name is required"));
    }
    let mut rpc = client().await?;
    let resp = rpc
        .get_ceph_cluster(controller_proto::GetCephClusterRequest {
            name: name.to_string(),
        })
        .await
        .context("GetCephCluster")?
        .into_inner();
    resp.ceph_cluster
        .ok_or_else(|| missing(format!("ceph cluster {name} not found")))
}

fn ceph_form(cluster: &controller_proto::CephCluster) -> Value {
    let spec = cluster.spec.clone().unwrap_or_default();
    json!({
        "name": cluster.name,
        "fsid": spec.fsid,
        "public_network": spec.public_network,
        "cluster_network": spec.cluster_network,
        "size": spec.size,
        "min_size": spec.min_size,
        "encrypt_osds": spec.encrypt_osds,
        "force_wipe": spec.force_wipe,
        "nodes": spec.nodes.iter().map(|node| json!({
            "node_id": node.node_id,
            "osd_device": node.osd_device,
            "osd_devices": node.osd_devices.join(","),
            "mon_addr": node.mon_addr,
            "cluster_addr": node.cluster_addr,
            "public_iface": node.public_iface,
            "cluster_iface": node.cluster_iface,
        })).collect::<Vec<_>>(),
        "confirm": true,
    })
}

async fn delete_ceph(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DeleteCephClusterRequest::default();
    req.name = ident(v);
    req.force = flag(v, "force");
    let mut rpc = client().await?;
    rpc.delete_ceph_cluster(req)
        .await
        .context("DeleteCephCluster")?;
    Ok(json!({ "ok": true }))
}

async fn set_ceph_credentials(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::SetCephDashboardCredentialsRequest::default();
    req.name = ident(v);
    req.url = text(v, "url");
    req.username = required(v, "username")?;
    req.password = required(v, "password")?;
    let mut rpc = client().await?;
    rpc.set_ceph_dashboard_credentials(req)
        .await
        .context("SetCephDashboardCredentials")?;
    Ok(json!({ "ok": true }))
}

async fn show_ceph_credentials(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .get_ceph_dashboard_credentials(controller_proto::GetCephDashboardCredentialsRequest {
            name: ident(v),
        })
        .await
        .context("GetCephDashboardCredentials")?
        .into_inner();
    Ok(json!({
        "ok": true,
        "url": resp.url,
        "username": resp.username,
        "password": resp.password,
    }))
}

async fn create_filesystem(v: &Value) -> Result<Value, ApiError> {
    let mut spec = controller_proto::SharedFilesystemSpec::default();
    spec.ceph_cluster = required(v, "ceph_cluster")?;
    spec.quota_bytes = num_i64(v, "quota_bytes");
    spec.fs_name = text(v, "fs_name");
    let mut fs = controller_proto::SharedFilesystem::default();
    fs.name = required(v, "name")?;
    fs.spec = Some(spec);
    let mut rpc = client().await?;
    let resp = rpc
        .create_shared_filesystem(controller_proto::CreateSharedFilesystemRequest {
            shared_filesystem: Some(fs),
        })
        .await
        .context("CreateSharedFilesystem")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

async fn delete_filesystem(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_shared_filesystem(controller_proto::DeleteSharedFilesystemRequest {
        name: ident(v),
    })
    .await
    .context("DeleteSharedFilesystem")?;
    Ok(json!({ "ok": true }))
}

async fn create_object_store(v: &Value) -> Result<Value, ApiError> {
    let mut spec = controller_proto::ObjectStoreSpec::default();
    spec.ceph_cluster = required(v, "ceph_cluster")?;
    spec.members = strings(v, "members");
    spec.port = num_i32(v, "port");
    spec.tls = flag(v, "tls");
    spec.rgw_id = text(v, "rgw_id");
    let mut store = controller_proto::ObjectStore::default();
    store.name = required(v, "name")?;
    store.spec = Some(spec);
    let mut rpc = client().await?;
    let resp = rpc
        .create_object_store(controller_proto::CreateObjectStoreRequest {
            object_store: Some(store),
        })
        .await
        .context("CreateObjectStore")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

async fn delete_object_store(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_object_store(controller_proto::DeleteObjectStoreRequest { name: ident(v) })
        .await
        .context("DeleteObjectStore")?;
    Ok(json!({ "ok": true }))
}

async fn create_object_user(v: &Value) -> Result<Value, ApiError> {
    let mut spec = controller_proto::ObjectUserSpec::default();
    spec.store = required(v, "store")?;
    let mut user = controller_proto::ObjectUser::default();
    user.name = required(v, "name")?;
    user.spec = Some(spec);
    let mut rpc = client().await?;
    let resp = rpc
        .create_object_user(controller_proto::CreateObjectUserRequest {
            object_user: Some(user),
        })
        .await
        .context("CreateObjectUser")?
        .into_inner();
    Ok(json!({
        "ok": resp.success,
        "access_key": resp.object_user.map(|user| user.access_key).unwrap_or_default(),
        "secret": resp.secret,
    }))
}

async fn delete_object_user(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DeleteObjectUserRequest::default();
    req.name = required(v, "name")?;
    req.store = required(v, "store")?;
    let mut rpc = client().await?;
    rpc.delete_object_user(req)
        .await
        .context("DeleteObjectUser")?;
    Ok(json!({ "ok": true }))
}

fn k8s_node(
    item: &Value,
    default_role: &str,
) -> Result<controller_proto::KubernetesNodeSpec, ApiError> {
    let mut node = controller_proto::KubernetesNodeSpec::default();
    node.node_id = text(item, "node_id");
    if node.node_id.is_empty() {
        return Err(bad("node_id is required on each Kubernetes node"));
    }
    let role = text(item, "role");
    node.role = if role.is_empty() {
        default_role.to_string()
    } else {
        role
    };
    node.advertise_address = text(item, "advertise_address");
    node.iface = text(item, "iface");
    if node.advertise_address.is_empty() || node.iface.is_empty() {
        return Err(bad(format!(
            "{} needs advertise_address and iface",
            node.node_id
        )));
    }
    node.initial_cluster = flag(item, "initial_cluster");
    node.join_existing = flag(item, "join_existing");
    Ok(node)
}

fn k8s_nodes(
    v: &Value,
    key: &str,
    default_role: &str,
) -> Result<Vec<controller_proto::KubernetesNodeSpec>, ApiError> {
    let Some(items) = v.get(key).and_then(|value| value.as_array()) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| k8s_node(item, default_role))
        .collect()
}

fn k8s_spec(v: &Value) -> Result<controller_proto::KubernetesClusterSpec, ApiError> {
    let mut control = k8s_nodes(v, "control_plane_nodes", "control-plane")?;
    if control.is_empty() && !text(v, "node_id").is_empty() {
        let mut node = controller_proto::KubernetesNodeSpec::default();
        node.node_id = required(v, "node_id")?;
        node.role = "control-plane".to_string();
        node.advertise_address = required(v, "advertise_address")?;
        node.iface = required(v, "iface")?;
        node.initial_cluster = true;
        control.push(node);
    }
    if control.is_empty() {
        return Err(bad(
            "control_plane_nodes is required; a blank form would replace the cluster spec",
        ));
    }
    if !control.iter().any(|node| node.initial_cluster) {
        control[0].initial_cluster = true;
    }
    let mut spec = controller_proto::KubernetesClusterSpec::default();
    spec.cluster_cidr = required(v, "cluster_cidr")?;
    spec.service_cidr = required(v, "service_cidr")?;
    spec.cluster_dns = text(v, "cluster_dns");
    spec.api_server_endpoint = required(v, "api_server_endpoint")?;
    spec.cni = {
        let cni = text(v, "cni");
        if cni.is_empty() {
            "cilium".to_string()
        } else {
            cni
        }
    };
    spec.worker_nodes = k8s_nodes(v, "worker_nodes", "worker")?;
    spec.control_plane_nodes = control;
    spec.schedule_on_control_plane = flag(v, "schedule_on_control_plane");
    if flag(v, "ha_enabled") || !text(v, "vip_address").is_empty() {
        let mut ha = controller_proto::HighAvailabilityConfig::default();
        ha.enabled = true;
        ha.vip_address = text(v, "vip_address");
        ha.vip_interface = text(v, "vip_interface");
        let method = text(v, "method");
        ha.method = if method.is_empty() {
            "keepalived".to_string()
        } else {
            method
        };
        if ha.vip_address.is_empty() || ha.vip_interface.is_empty() {
            return Err(bad("high availability needs vip_address and vip_interface"));
        }
        spec.high_availability = Some(ha);
    }
    Ok(spec)
}

fn posted_k8s(v: &Value) -> bool {
    !text(v, "cluster_cidr").is_empty()
        || v.get("control_plane_nodes").is_some()
        || !text(v, "node_id").is_empty()
}

fn k8s_form(name: &str, spec: &controller_proto::KubernetesClusterSpec) -> Value {
    let ha = spec.high_availability.clone().unwrap_or_default();
    let node = |item: &controller_proto::KubernetesNodeSpec| {
        json!({
            "node_id": item.node_id,
            "role": item.role,
            "advertise_address": item.advertise_address,
            "iface": item.iface,
            "initial_cluster": item.initial_cluster,
            "join_existing": item.join_existing,
        })
    };
    json!({
        "name": name,
        "cluster_cidr": spec.cluster_cidr,
        "service_cidr": spec.service_cidr,
        "cluster_dns": spec.cluster_dns,
        "api_server_endpoint": spec.api_server_endpoint,
        "cni": spec.cni,
        "schedule_on_control_plane": spec.schedule_on_control_plane,
        "control_plane_nodes": spec.control_plane_nodes.iter().map(node).collect::<Vec<_>>(),
        "worker_nodes": spec.worker_nodes.iter().map(node).collect::<Vec<_>>(),
        "ha_enabled": ha.enabled,
        "vip_address": ha.vip_address,
        "vip_interface": ha.vip_interface,
        "method": ha.method,
        "confirm": true,
    })
}

async fn create_kubernetes(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    let mut req = controller_proto::CreateKubernetesClusterRequest::default();
    req.name = required(v, "name")?;
    req.spec = Some(k8s_spec(v)?);
    req.confirm = true;
    let mut rpc = client().await?;
    let resp = rpc
        .create_kubernetes_cluster(req)
        .await
        .context("CreateKubernetesCluster")?
        .into_inner();
    Ok(json!({ "ok": true, "name": resp.cluster.map(|c| c.name).unwrap_or_default() }))
}

async fn stored_k8s_spec(name: &str) -> Result<controller_proto::KubernetesClusterSpec, ApiError> {
    if name.is_empty() {
        return Err(bad("name is required"));
    }
    let mut rpc = client().await?;
    let resp = rpc
        .get_kubernetes_cluster(controller_proto::GetKubernetesClusterRequest {
            name: name.to_string(),
        })
        .await
        .context("GetKubernetesCluster")?
        .into_inner();
    resp.cluster
        .and_then(|cluster| cluster.spec)
        .ok_or_else(|| missing(format!("kubernetes cluster {name} has no spec")))
}

async fn update_kubernetes(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    let spec = k8s_spec(v)?;
    let mut req = controller_proto::UpdateKubernetesClusterRequest::default();
    req.name = ident(v);
    if req.name.is_empty() {
        return Err(bad("name is required"));
    }
    req.spec = Some(spec);
    req.confirm = true;
    let mut rpc = client().await?;
    let resp = rpc
        .update_kubernetes_cluster(req)
        .await
        .context("UpdateKubernetesCluster")?
        .into_inner();
    Ok(json!({ "ok": true, "name": resp.cluster.map(|cluster| cluster.name).unwrap_or_default() }))
}

async fn delete_kubernetes(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DeleteKubernetesClusterRequest::default();
    req.name = ident(v);
    req.confirm = true;
    req.force = flag(v, "force");
    let mut rpc = client().await?;
    let resp = rpc
        .delete_kubernetes_cluster(req)
        .await
        .context("DeleteKubernetesCluster")?
        .into_inner();
    Ok(json!({ "ok": true, "message": resp.message }))
}

async fn scale_kubernetes(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::ScaleKubernetesClusterRequest::default();
    req.name = ident(v);
    req.confirm = true;
    let add = text(v, "add_node_id");
    if !add.is_empty() {
        let mut node = controller_proto::KubernetesNodeSpec::default();
        node.node_id = add;
        node.role = {
            let role = text(v, "role");
            if role.is_empty() {
                "worker".to_string()
            } else {
                role
            }
        };
        node.advertise_address = text(v, "advertise_address");
        node.iface = text(v, "iface");
        req.add_nodes = vec![node];
    }
    req.remove_node_ids = strings(v, "remove_node_ids");
    let mut rpc = client().await?;
    rpc.scale_kubernetes_cluster(req)
        .await
        .context("ScaleKubernetesCluster")?;
    Ok(json!({ "ok": true }))
}

async fn plan_kubernetes(v: &Value) -> Result<Value, ApiError> {
    let name = ident(v);
    let mut req = controller_proto::PlanKubernetesClusterRequest::default();
    req.name = name.clone();
    req.spec = Some(if posted_k8s(v) {
        k8s_spec(v)?
    } else {
        stored_k8s_spec(&name).await?
    });
    let mut rpc = client().await?;
    let resp = rpc
        .plan_kubernetes_cluster(req)
        .await
        .context("PlanKubernetesCluster")?
        .into_inner();
    Ok(json!({
        "ok": resp.blocking_issues.is_empty(),
        "action": resp.action,
        "blocking": resp.blocking_issues,
        "warnings": resp.warnings,
    }))
}

fn cluster_update_spec(v: &Value) -> Result<controller_proto::ClusterUpdateSpec, ApiError> {
    let mut target = controller_proto::ClusterUpdateTarget::default();
    target.version = text(v, "version");
    target.flake_ref = text(v, "flake_ref");
    target.flake_rev = text(v, "flake_rev");
    let mut selector = controller_proto::ClusterUpdateSelector::default();
    selector.all_nodes = flag(v, "all_nodes");
    selector.node_ids = strings(v, "node_ids");
    let mut strategy = controller_proto::ClusterUpdateStrategy::default();
    strategy.r#type =
        controller_proto::ClusterUpdateStrategyType::ClusterUpdateStrategyOneAtATime as i32;
    strategy.max_unavailable = 1;
    let mut spec = controller_proto::ClusterUpdateSpec::default();
    spec.name = required(v, "name")?;
    spec.target = Some(target);
    spec.selector = Some(selector);
    spec.strategy = Some(strategy);
    spec.update_kind = {
        let kind = text(v, "update_kind");
        if kind.is_empty() {
            "kcore".to_string()
        } else {
            kind
        }
    };
    spec.drain_vms = flag(v, "drain_vms");
    Ok(spec)
}

async fn create_cluster_update(v: &Value) -> Result<Value, ApiError> {
    confirmed(v)?;
    let mut rpc = client().await?;
    let resp = rpc
        .create_cluster_update(controller_proto::CreateClusterUpdateRequest {
            spec: Some(cluster_update_spec(v)?),
        })
        .await
        .context("CreateClusterUpdate")?
        .into_inner();
    Ok(json!({ "ok": resp.success }))
}

async fn plan_update(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .plan_cluster_update(controller_proto::PlanClusterUpdateRequest {
            spec: Some(cluster_update_spec(v)?),
        })
        .await
        .context("PlanClusterUpdate")?
        .into_inner();
    Ok(json!({
        "ok": resp.viable,
        "nodes": resp.target_node_ids,
        "reboot": resp.likely_requires_reboot,
        "detail": resp.detail,
    }))
}

async fn update_simple(v: &Value, action: &str) -> Result<Value, ApiError> {
    let name = ident(v);
    let mut rpc = client().await?;
    match action {
        "approve" => {
            rpc.approve_cluster_update(controller_proto::ApproveClusterUpdateRequest { name })
                .await
                .context("ApproveClusterUpdate")?;
        }
        "cancel" => {
            rpc.cancel_cluster_update(controller_proto::CancelClusterUpdateRequest { name })
                .await
                .context("CancelClusterUpdate")?;
        }
        "rollback" => {
            rpc.rollback_cluster_update(controller_proto::RollbackClusterUpdateRequest { name })
                .await
                .context("RollbackClusterUpdate")?;
        }
        _ => return Err(bad("unknown update action")),
    }
    Ok(json!({ "ok": true }))
}

async fn reboot_update(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::RebootClusterUpdateNodeRequest::default();
    req.name = ident(v);
    req.node_id = required(v, "node_id")?;
    let mut rpc = client().await?;
    let resp = rpc
        .reboot_cluster_update_node(req)
        .await
        .context("RebootClusterUpdateNode")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

fn containerish(resource: &str, v: &Value) -> bool {
    resource == "containers" || text(v, "kind") == "container"
}

async fn create_workload(v: &Value, force_container: bool) -> Result<Value, ApiError> {
    let container = force_container || text(v, "kind") == "container";
    let mut req = controller_proto::CreateWorkloadRequest::default();
    req.kind = if container {
        controller_proto::WorkloadKind::Container as i32
    } else {
        controller_proto::WorkloadKind::Vm as i32
    };
    req.target_node = text(v, "target_node");
    if container {
        let mut spec = controller_proto::ContainerSpec::default();
        spec.name = required(v, "name")?;
        spec.image = required(v, "image")?;
        spec.network = text(v, "network");
        spec.command = strings(v, "command");
        spec.ports = strings(v, "ports");
        spec.storage_backend = text(v, "storage_backend");
        spec.storage_size_bytes = num_i64(v, "storage_size_bytes");
        spec.mount_target = text(v, "mount_target");
        spec.desired_state = workload_desired(text(v, "desired_state") != "stopped");
        req.container_spec = Some(spec);
    } else {
        let spec = vm_spec(v)?;
        req.storage_backend = backend_enum(&spec.storage_backend)?;
        req.storage_size_bytes = spec.storage_size_bytes;
        req.ssh_key_names = strings(v, "ssh_key_names");
        req.image_url = text(v, "image_url");
        req.image_sha256 = text(v, "image_sha256");
        req.image_path = text(v, "image_path");
        req.image_format = text(v, "image_format");
        req.vm_spec = Some(spec);
    }
    let mut rpc = client().await?;
    let resp = rpc
        .create_workload(req)
        .await
        .context("CreateWorkload")?
        .into_inner();
    Ok(json!({ "ok": true, "id": resp.workload_id, "node": resp.node_id }))
}

async fn set_workload(resource: &str, v: &Value, running: bool) -> Result<Value, ApiError> {
    let mut req = controller_proto::SetWorkloadDesiredStateRequest::default();
    req.kind = if containerish(resource, v) {
        controller_proto::WorkloadKind::Container as i32
    } else {
        controller_proto::WorkloadKind::Vm as i32
    };
    req.workload_id = ident(v);
    req.desired_state = workload_desired(running);
    req.target_node = text(v, "target_node");
    let mut rpc = client().await?;
    rpc.set_workload_desired_state(req)
        .await
        .context("SetWorkloadDesiredState")?;
    Ok(json!({ "ok": true }))
}

async fn delete_workload(resource: &str, v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DeleteWorkloadRequest::default();
    req.kind = if containerish(resource, v) {
        controller_proto::WorkloadKind::Container as i32
    } else {
        controller_proto::WorkloadKind::Vm as i32
    };
    req.workload_id = ident(v);
    req.target_node = text(v, "target_node");
    let mut rpc = client().await?;
    rpc.delete_workload(req).await.context("DeleteWorkload")?;
    Ok(json!({ "ok": true }))
}

async fn cancel_operation(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .cancel_vm_operation(controller_proto::CancelVmOperationRequest { id: ident(v) })
        .await
        .context("CancelVmOperation")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn create_ssh_key(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::CreateSshKeyRequest::default();
    req.name = required(v, "name")?;
    req.public_key = required(v, "public_key")?;
    let mut rpc = client().await?;
    let resp = rpc
        .create_ssh_key(req)
        .await
        .context("CreateSshKey")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn delete_ssh_key(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_ssh_key(controller_proto::DeleteSshKeyRequest { name: ident(v) })
        .await
        .context("DeleteSshKey")?;
    Ok(json!({ "ok": true }))
}

async fn create_operator(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .create_operator(controller_proto::CreateOperatorRequest {
            name: required(v, "name")?,
        })
        .await
        .context("CreateOperator")?
        .into_inner();
    Ok(json!({ "ok": true, "name": resp.operator.map(|op| op.name).unwrap_or_default() }))
}

async fn delete_operator(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_operator(controller_proto::DeleteOperatorRequest { name: ident(v) })
        .await
        .context("DeleteOperator")?;
    Ok(json!({ "ok": true }))
}

fn operator_role(v: &Value) -> Result<i32, ApiError> {
    use controller_proto::OperatorRoleKind::{BackupOperator, ClusterAdmin, ReadOnly, VmAdmin};
    Ok(match text(v, "role").as_str() {
        "read-only" | "read_only" => ReadOnly as i32,
        "vm-admin" | "vm_admin" => VmAdmin as i32,
        "cluster-admin" | "cluster_admin" => ClusterAdmin as i32,
        "backup-operator" | "backup_operator" => BackupOperator as i32,
        other => return Err(bad(format!("unknown role {other}"))),
    })
}

async fn grant_role(v: &Value, grant: bool) -> Result<Value, ApiError> {
    let role = operator_role(v)?;
    let name = ident(v);
    let mut rpc = client().await?;
    if grant {
        rpc.grant_operator_role(controller_proto::GrantOperatorRoleRequest {
            operator_name: name,
            role,
        })
        .await
        .context("GrantOperatorRole")?;
    } else {
        rpc.revoke_operator_role(controller_proto::RevokeOperatorRoleRequest {
            operator_name: name,
            role,
        })
        .await
        .context("RevokeOperatorRole")?;
    }
    Ok(json!({ "ok": true }))
}

async fn issue_operator_cert(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .issue_operator_cert(controller_proto::IssueOperatorCertRequest {
            operator_name: ident(v),
        })
        .await
        .context("IssueOperatorCert")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "cert_pem": resp.cert_pem, "key_pem": resp.key_pem }))
}

async fn create_postgresql(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::CreatePostgresqlRequest::default();
    req.name = required(v, "name")?;
    req.database = text(v, "database");
    req.package = text(v, "package");
    req.port = num_i32(v, "port");
    req.target_node = text(v, "target_node");
    let mut rpc = client().await?;
    let resp = rpc
        .create_postgresql(req)
        .await
        .context("CreatePostgresql")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message, "node": resp.node_id }))
}

async fn delete_postgresql(v: &Value) -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    rpc.delete_postgresql(controller_proto::DeletePostgresqlRequest { name: ident(v) })
        .await
        .context("DeletePostgresql")?;
    Ok(json!({ "ok": true }))
}

async fn node_simple(v: &Value, action: &str) -> Result<Value, ApiError> {
    let node_id = ident(v);
    let mut rpc = client().await?;
    let message = match action {
        "approve" => {
            rpc.approve_node(controller_proto::ApproveNodeRequest { node_id })
                .await
                .context("ApproveNode")?
                .into_inner()
                .message
        }
        "reject" => {
            rpc.reject_node(controller_proto::RejectNodeRequest { node_id })
                .await
                .context("RejectNode")?
                .into_inner()
                .message
        }
        "cordon" => {
            rpc.cordon_node(controller_proto::CordonNodeRequest { node_id })
                .await
                .context("CordonNode")?
                .into_inner()
                .message
        }
        "uncordon" => {
            rpc.uncordon_node(controller_proto::UncordonNodeRequest { node_id })
                .await
                .context("UncordonNode")?
                .into_inner()
                .message
        }
        "delete" => {
            rpc.delete_node(controller_proto::DeleteNodeRequest { node_id })
                .await
                .context("DeleteNode")?
                .into_inner()
                .message
        }
        _ => return Err(bad("unknown node action")),
    };
    Ok(json!({ "ok": true, "message": message }))
}

async fn drain_node(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::DrainNodeRequest::default();
    req.node_id = ident(v);
    req.target_node = text(v, "target_node");
    let mut rpc = client().await?;
    let resp = rpc.drain_node(req).await.context("DrainNode")?.into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message, "migrated": resp.vms_migrated }))
}

async fn backup_cluster() -> Result<Value, ApiError> {
    let mut rpc = client().await?;
    let resp = rpc
        .backup_cluster(controller_proto::BackupClusterRequest {})
        .await
        .context("BackupCluster")?
        .into_inner();
    Ok(json!({
        "ok": true,
        "schema_version": resp.schema_version,
        "sqlite_base64": base64::engine::general_purpose::STANDARD.encode(resp.sqlite),
    }))
}

async fn restore_cluster(v: &Value) -> Result<Value, ApiError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text(v, "sqlite_base64"))
        .map_err(|err| bad(format!("sqlite_base64: {err}")))?;
    let mut rpc = client().await?;
    let resp = rpc
        .restore_cluster(controller_proto::RestoreClusterRequest {
            sqlite: bytes,
            confirm: true,
        })
        .await
        .context("RestoreCluster")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn apply_nix(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::ApplyNixConfigRequest::default();
    req.configuration_nix = required(v, "configuration_nix")?;
    req.rebuild = flag(v, "rebuild");
    let mut rpc = admin().await?;
    let resp = rpc
        .apply_nix_config(req)
        .await
        .context("ApplyNixConfig")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn resolve_conflict(v: &Value) -> Result<Value, ApiError> {
    let id = num_i64(v, "id");
    if id == 0 {
        return Err(bad("id is required"));
    }
    let mut rpc = admin().await?;
    rpc.resolve_replication_conflict(controller_proto::ResolveReplicationConflictRequest { id })
        .await
        .context("ResolveReplicationConflict")?;
    Ok(json!({ "ok": true }))
}

fn revocation_reason(v: &Value) -> i32 {
    use controller_proto::RevocationReason::{
        AffiliationChanged, CaCompromise, CertificateHold, CessationOfOperation, KeyCompromise,
        Superseded, Unspecified,
    };
    match text(v, "reason").as_str() {
        "keyCompromise" | "key-compromise" => KeyCompromise as i32,
        "cACompromise" | "ca-compromise" => CaCompromise as i32,
        "affiliationChanged" => AffiliationChanged as i32,
        "superseded" => Superseded as i32,
        "cessationOfOperation" => CessationOfOperation as i32,
        "certificateHold" => CertificateHold as i32,
        _ => Unspecified as i32,
    }
}

async fn revoke_cert(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::RevokeCertificateRequest::default();
    req.serial_hex = {
        let serial = text(v, "serial_hex");
        if serial.is_empty() {
            text(v, "serial")
        } else {
            serial
        }
    };
    if req.serial_hex.is_empty() {
        req.serial_hex = text(v, "id");
    }
    req.subject_cn = text(v, "subject");
    req.node_id = text(v, "node_id");
    req.reason = revocation_reason(v);
    let mut rpc = client().await?;
    let resp = rpc
        .revoke_certificate(req)
        .await
        .context("RevokeCertificate")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn renew_cert(v: &Value) -> Result<Value, ApiError> {
    let node = {
        let node = text(v, "node_id");
        if node.is_empty() {
            ident(v)
        } else {
            node
        }
    };
    let mut rpc = client().await?;
    let resp = rpc
        .renew_node_cert(controller_proto::RenewNodeCertRequest { node_id: node })
        .await
        .context("RenewNodeCert")?
        .into_inner();
    Ok(
        json!({ "ok": resp.success, "message": resp.message, "cert_pem": resp.cert_pem, "key_pem": resp.key_pem }),
    )
}

async fn rotate_certs(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::RotateNodeCertsRequest::default();
    req.node_id = text(v, "node_id");
    req.all_nodes = flag(v, "all_nodes");
    let mut rpc = client().await?;
    let resp = rpc
        .rotate_node_certs(req)
        .await
        .context("RotateNodeCerts")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn sign_csr(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::SignNodeCsrRequest::default();
    req.node_id = required(v, "node_id")?;
    req.csr_pem = required(v, "csr_pem")?;
    let mut rpc = client().await?;
    let resp = rpc
        .sign_node_csr(req)
        .await
        .context("SignNodeCsr")?
        .into_inner();
    Ok(
        json!({ "ok": resp.success, "message": resp.message, "cert_chain_pem": resp.cert_chain_pem }),
    )
}

async fn bootstrap_cert(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::IssueNodeBootstrapCertRequest::default();
    req.node_id = required(v, "node_id")?;
    req.node_host = text(v, "node_host");
    let mut rpc = client().await?;
    let resp = rpc
        .issue_node_bootstrap_cert(req)
        .await
        .context("IssueNodeBootstrapCert")?
        .into_inner();
    Ok(
        json!({ "ok": resp.success, "message": resp.message, "cert_pem": resp.cert_pem, "key_pem": resp.key_pem }),
    )
}

async fn rotate_sub_ca(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::RotateSubCaRequest::default();
    req.sub_ca_cert_pem = required(v, "sub_ca_cert_pem")?;
    req.sub_ca_key_pem = required(v, "sub_ca_key_pem")?;
    let mut rpc = client().await?;
    let resp = rpc
        .rotate_sub_ca(req)
        .await
        .context("RotateSubCa")?
        .into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

async fn reload_tls(v: &Value) -> Result<Value, ApiError> {
    let mut req = controller_proto::ReloadTlsRequest::default();
    req.cert_pem = required(v, "cert_pem")?;
    req.key_pem = required(v, "key_pem")?;
    let mut rpc = client().await?;
    let resp = rpc.reload_tls(req).await.context("ReloadTls")?.into_inner();
    Ok(json!({ "ok": resp.success, "message": resp.message }))
}

#[cfg(dashboard_spa)]
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("json" | "map") => "application/json",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[cfg(dashboard_spa)]
#[derive(rust_embed::Embed)]
#[folder = "web/dist/"]
struct SpaAssets;

#[cfg(dashboard_spa)]
pub async fn spa(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() {
        if let Some(file) = SpaAssets::get(path) {
            return Response::builder()
                .header(header::CONTENT_TYPE, content_type(path))
                .body(Body::from(file.data.into_owned()))
                .unwrap_or_else(|_| Response::new(Body::from("asset")));
        }
    }
    match SpaAssets::get("index.html") {
        Some(file) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(file.data.into_owned()))
            .unwrap_or_else(|_| Response::new(Body::from("index"))),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("dashboard assets missing"))
            .unwrap_or_else(|_| Response::new(Body::empty())),
    }
}

#[cfg(test)]
mod tests {
    use super::{router, TERRAFORM};
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    #[tokio::test]
    async fn catalog_lists_terraform_resources() {
        let app = router();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for name in TERRAFORM {
            assert!(body.contains(name), "{name} missing from catalog");
        }
    }

    #[test]
    fn catalog_offers_detail_pages_and_repairs() {
        let body = super::catalog_value();
        let resources = body["resources"].as_array().expect("resources");
        let find = |id: &str| {
            resources
                .iter()
                .find(|resource| resource["id"] == id)
                .unwrap_or_else(|| panic!("missing {id}"))
        };
        for id in ["storage", "network", "crl"] {
            assert_eq!(find(id)["list"], true, "{id}");
        }
        assert_eq!(find("network")["id"], "network");
        assert_eq!(find("vms")["inspect"], true);
        let actions = |id: &str| {
            find(id)["actions"]
                .as_array()
                .expect("actions")
                .iter()
                .map(|action| action["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(actions("vms").contains(&"migrate-status".to_string()));
        assert!(actions("vms").contains(&"reset-migrate".to_string()));
        assert!(actions("disk-layouts").contains(&"classify".to_string()));
        let update = find("kubernetes")["actions"]
            .as_array()
            .expect("actions")
            .iter()
            .find(|action| action["name"] == "update")
            .expect("update");
        assert_eq!(update["preload"], true);
        assert!(update["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .any(|field| field["name"] == "worker_nodes"));
    }

    #[test]
    fn kubernetes_spec_keeps_workers_and_refuses_a_blank_form() {
        let spec = super::k8s_spec(&json!({
            "cluster_cidr": "10.200.0.0/16",
            "service_cidr": "10.201.0.0/16",
            "api_server_endpoint": "192.168.40.50:6443",
            "control_plane_nodes": [{
                "node_id": "cp",
                "advertise_address": "192.168.40.10",
                "iface": "enp1"
            }],
            "worker_nodes": [{
                "node_id": "wk",
                "advertise_address": "192.168.40.11",
                "iface": "enp1"
            }],
            "ha_enabled": true,
            "vip_address": "192.168.40.50",
            "vip_interface": "enp1"
        }))
        .expect("spec");
        assert!(spec.control_plane_nodes[0].initial_cluster);
        assert_eq!(spec.worker_nodes.len(), 1);
        assert_eq!(spec.worker_nodes[0].role, "worker");
        let ha = spec.high_availability.expect("ha");
        assert!(ha.enabled);
        assert_eq!(ha.method, "keepalived");

        let (code, msg) = super::into_pair(
            super::k8s_spec(&json!({
                "cluster_cidr": "10.200.0.0/16",
                "service_cidr": "10.201.0.0/16",
                "api_server_endpoint": "192.168.40.50:6443"
            }))
            .unwrap_err(),
        );
        assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
        assert!(msg.contains("blank form"), "{msg}");
    }

    #[test]
    fn forms_accept_lists_of_rules_disks_and_osd_devices() {
        let rules = super::security_rules(&json!({
            "rules": [
                {"protocol": "tcp", "host_port": 80, "target_port": 8080},
                {"protocol": "udp", "host_port": 53, "target_port": 53}
            ]
        }));
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].host_port, 80);

        let disks = super::disks(&json!({
            "disks": [
                {"name": "data", "backend_handle": "/dev/rbd1", "bus": "virtio"},
                {"name": "", "backend_handle": ""}
            ]
        }));
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].device, "");

        let nodes = super::ceph_nodes(&json!({
            "nodes": [
                {"node_id": "a", "osd_device": "/dev/sdb", "osd_devices": "/dev/sdc,/dev/sdd"},
                {"node_id": "b", "osd_devices": ["/dev/nvme0n1"]}
            ]
        }));
        assert_eq!(nodes[0].osd_devices, vec!["/dev/sdc", "/dev/sdd"]);
        assert_eq!(nodes[1].osd_device, "");
        assert_eq!(nodes[1].osd_devices, vec!["/dev/nvme0n1"]);
    }

    #[test]
    fn reset_migrate_requires_force() {
        let (code, msg) = super::into_pair(
            super::migrate_reset_guard(&json!({
                "id": "vm-1",
                "node_id": "node-a",
                "confirm": true
            }))
            .unwrap_err(),
        );
        assert_eq!(code, axum::http::StatusCode::BAD_REQUEST);
        assert!(msg.contains("force"), "{msg}");

        let (vm, node) = super::migrate_reset_guard(&json!({
            "id": "vm-1",
            "node_id": "node-a",
            "confirm": true,
            "force": true
        }))
        .expect("guard");
        assert_eq!(vm, "vm-1");
        assert_eq!(node, "node-a");
    }
}
