/// Down steps for [`crate::db::Database::migrate_down`].
/// Generated from the `up` blocks in `db.rs`. A version that copies or
/// renames rows is irreversible: rollback refuses it.
pub struct SchemaDown {
    pub version: i32,
    pub sql: &'static str,
    pub irreversible: bool,
}

pub const SCHEMA_DOWNS: &[SchemaDown] = &[
    SchemaDown {
        version: 1,
        sql: "ALTER TABLE vms DROP COLUMN image_format;
ALTER TABLE vms DROP COLUMN image_sha256;
ALTER TABLE vms DROP COLUMN image_url;",
        irreversible: false,
    },
    SchemaDown {
        version: 2,
        sql: "ALTER TABLE vms DROP COLUMN runtime_state;",
        irreversible: false,
    },
    SchemaDown {
        version: 3,
        sql: "ALTER TABLE vms DROP COLUMN cloud_init_user_data;
ALTER TABLE networks DROP COLUMN allowed_udp_ports;
ALTER TABLE networks DROP COLUMN allowed_tcp_ports;
ALTER TABLE nodes DROP COLUMN memory_used;
ALTER TABLE nodes DROP COLUMN cpu_used;",
        irreversible: false,
    },
    SchemaDown {
        version: 4,
        sql: "DROP TABLE IF EXISTS vm_ssh_keys;
DROP TABLE IF EXISTS ssh_keys;",
        irreversible: false,
    },
    SchemaDown {
        version: 5,
        sql: "ALTER TABLE vms DROP COLUMN storage_size_bytes;
ALTER TABLE vms DROP COLUMN storage_backend;
ALTER TABLE nodes DROP COLUMN storage_backend;",
        irreversible: false,
    },
    SchemaDown {
        version: 6,
        sql: "ALTER TABLE networks DROP COLUMN vlan_id;",
        irreversible: false,
    },
    SchemaDown {
        version: 7,
        sql: "ALTER TABLE vms DROP COLUMN vm_ip;
ALTER TABLE networks DROP COLUMN next_ip;
ALTER TABLE networks DROP COLUMN vni;
ALTER TABLE networks DROP COLUMN enable_outbound_nat;
ALTER TABLE networks DROP COLUMN network_type;",
        irreversible: false,
    },
    SchemaDown {
        version: 8,
        sql: "ALTER TABLE nodes DROP COLUMN disable_vxlan;",
        irreversible: false,
    },
    SchemaDown {
        version: 9,
        sql: "ALTER TABLE nodes DROP COLUMN approval_status;",
        irreversible: false,
    },
    SchemaDown {
        version: 10,
        sql: "ALTER TABLE nodes DROP COLUMN cert_expiry_days;",
        irreversible: false,
    },
    SchemaDown {
        version: 11,
        sql: "DROP TABLE IF EXISTS replication_outbox;",
        irreversible: false,
    },
    SchemaDown {
        version: 12,
        sql: "DROP TABLE IF EXISTS replication_ack;",
        irreversible: false,
    },
    SchemaDown {
        version: 13,
        sql: "DROP TABLE IF EXISTS replication_received_ops;",
        irreversible: false,
    },
    SchemaDown {
        version: 14,
        sql: "DROP TABLE IF EXISTS replication_resource_heads;",
        irreversible: false,
    },
    SchemaDown {
        version: 15,
        sql: "DROP TABLE IF EXISTS replication_conflicts;",
        irreversible: false,
    },
    SchemaDown {
        version: 16,
        sql: "ALTER TABLE replication_resource_heads DROP COLUMN last_safety_class;
ALTER TABLE replication_resource_heads DROP COLUMN last_validity;
ALTER TABLE replication_resource_heads DROP COLUMN last_intent_epoch;
ALTER TABLE replication_resource_heads DROP COLUMN last_policy_priority;",
        irreversible: false,
    },
    SchemaDown {
        version: 17,
        sql: "DROP TABLE IF EXISTS replication_compensation_jobs;",
        irreversible: false,
    },
    SchemaDown {
        version: 18,
        sql: "DROP TABLE IF EXISTS replication_materialized_heads;",
        irreversible: false,
    },
    SchemaDown {
        version: 19,
        sql: "DROP TABLE IF EXISTS replication_reservations;",
        irreversible: false,
    },
    SchemaDown {
        version: 20,
        sql: "",
        irreversible: false,
    },
    SchemaDown {
        version: 21,
        sql: "ALTER TABLE nodes DROP COLUMN luks_method;",
        irreversible: false,
    },
    SchemaDown {
        version: 22,
        sql: "DROP INDEX IF EXISTS idx_workloads_runtime_state;
DROP INDEX IF EXISTS idx_workloads_kind;
DROP INDEX IF EXISTS idx_workloads_node_id;
DROP TABLE IF EXISTS workloads;",
        irreversible: false,
    },
    SchemaDown {
        version: 23,
        sql: "DROP TABLE IF EXISTS security_group_network_attachments;
DROP TABLE IF EXISTS security_group_vm_attachments;
DROP TABLE IF EXISTS security_group_rules;
DROP TABLE IF EXISTS security_groups;",
        irreversible: false,
    },
    SchemaDown {
        version: 24,
        sql: "ALTER TABLE replication_compensation_jobs DROP COLUMN loser_body_json;
ALTER TABLE replication_compensation_jobs DROP COLUMN loser_event_type;",
        irreversible: false,
    },
    SchemaDown {
        version: 25,
        sql: "DROP TABLE IF EXISTS controller_peers;",
        irreversible: false,
    },
    SchemaDown {
        version: 26,
        sql: "ALTER TABLE nodes DROP COLUMN dc_id;",
        irreversible: false,
    },
    SchemaDown {
        version: 27,
        sql: "DROP INDEX IF EXISTS idx_disk_layouts_node;
DROP TABLE IF EXISTS disk_layout_status;
DROP TABLE IF EXISTS disk_layouts;",
        irreversible: false,
    },
    SchemaDown {
        version: 28,
        sql: "DROP INDEX IF EXISTS idx_cluster_update_nodes_update;
DROP TABLE IF EXISTS cluster_update_events;
DROP TABLE IF EXISTS cluster_update_nodes;
DROP TABLE IF EXISTS cluster_updates;",
        irreversible: false,
    },
    SchemaDown {
        version: 30,
        sql: "DROP INDEX IF EXISTS idx_audit_events_action;
DROP INDEX IF EXISTS idx_audit_events_created;
DROP TABLE IF EXISTS audit_events;
DROP TABLE IF EXISTS operator_roles;
DROP TABLE IF EXISTS operators;",
        irreversible: false,
    },
    SchemaDown {
        version: 31,
        sql: "DROP INDEX IF EXISTS idx_volumes_vm;
DROP TABLE IF EXISTS volumes;
DROP TABLE IF EXISTS ceph_cluster_status;
DROP TABLE IF EXISTS ceph_clusters;",
        irreversible: false,
    },
    SchemaDown {
        version: 32,
        sql: "ALTER TABLE ceph_clusters DROP COLUMN bootstrap_json;",
        irreversible: false,
    },
    SchemaDown {
        version: 33,
        sql: "DROP INDEX IF EXISTS idx_issued_certs_subject;
DROP INDEX IF EXISTS idx_issued_certs_node;
DROP INDEX IF EXISTS idx_issued_certs_not_after;
DROP INDEX IF EXISTS idx_issued_certs_status;
DROP TABLE IF EXISTS crl_state;
DROP TABLE IF EXISTS issued_certificates;",
        irreversible: false,
    },
    SchemaDown {
        version: 34,
        sql: "ALTER TABLE vms DROP COLUMN pci_devices;",
        irreversible: false,
    },
    SchemaDown {
        version: 35,
        sql: "DROP TABLE IF EXISTS vm_gpu_bindings;
DROP TABLE IF EXISTS node_gpus;",
        irreversible: false,
    },
    SchemaDown {
        version: 36,
        sql: "ALTER TABLE node_gpus DROP COLUMN driver;
ALTER TABLE node_gpus DROP COLUMN characteristics;
ALTER TABLE node_gpus DROP COLUMN class_code;
ALTER TABLE node_gpus DROP COLUMN role;
ALTER TABLE node_gpus DROP COLUMN kind;",
        irreversible: false,
    },
    SchemaDown {
        version: 37,
        sql: "DROP TABLE IF EXISTS postgresql_instances;",
        irreversible: false,
    },
    SchemaDown {
        version: 38,
        sql: "DROP TABLE IF EXISTS ceph_retired_nodes;",
        irreversible: false,
    },
    SchemaDown {
        version: 39,
        sql: "DROP INDEX IF EXISTS idx_volumes_attach;
DROP INDEX IF EXISTS idx_volumes_vm;
DROP TABLE IF EXISTS volumes_v35;",
        irreversible: true,
    },
    SchemaDown {
        version: 40,
        sql: "DROP INDEX IF EXISTS idx_vm_operations_open_vm;
DROP INDEX IF EXISTS idx_vm_operations_vm;
DROP TABLE IF EXISTS vm_operations;",
        irreversible: false,
    },
    SchemaDown {
        version: 41,
        sql: "DROP INDEX IF EXISTS idx_volume_snapshots_volume;
DROP TABLE IF EXISTS volume_snapshots;
ALTER TABLE volumes DROP COLUMN parent_snapshot_id;",
        irreversible: false,
    },
    SchemaDown {
        version: 42,
        sql: "DROP TABLE IF EXISTS snapshot_policies;
ALTER TABLE volumes DROP COLUMN guest_checked_at;
ALTER TABLE volumes DROP COLUMN guest_visible_bytes;",
        irreversible: false,
    },
    SchemaDown {
        version: 43,
        sql: "DROP TABLE IF EXISTS guest_ops_node_keys;
ALTER TABLE volumes DROP COLUMN wrapped_dek;
ALTER TABLE volumes DROP COLUMN encrypted;",
        irreversible: false,
    },
    SchemaDown {
        version: 44,
        sql: "DROP INDEX IF EXISTS idx_object_users_store;
DROP TABLE IF EXISTS object_users;
DROP TABLE IF EXISTS object_store_status;
DROP TABLE IF EXISTS object_stores;
DROP TABLE IF EXISTS shared_filesystem_status;
DROP TABLE IF EXISTS shared_filesystems;",
        irreversible: false,
    },
    SchemaDown {
        version: 45,
        sql: "ALTER TABLE disk_layouts DROP COLUMN evacuate;",
        irreversible: false,
    },
    SchemaDown {
        version: 46,
        sql: "DROP TABLE IF EXISTS vm_nics;",
        irreversible: false,
    },
    SchemaDown {
        version: 47,
        sql: "DROP TABLE IF EXISTS vm_placement;",
        irreversible: false,
    },
    SchemaDown {
        version: 48,
        sql: "DROP TABLE IF EXISTS ipv6_released;
DROP TABLE IF EXISTS vxlan_released_ips;
DROP TABLE IF EXISTS vm_ipv6;
DROP TABLE IF EXISTS network_policy;",
        irreversible: false,
    },
    SchemaDown {
        version: 49,
        sql: "DROP TABLE IF EXISTS pending_node_config_push;",
        irreversible: false,
    },
    SchemaDown {
        version: 50,
        sql: "DROP INDEX IF EXISTS idx_k8s_cluster_nodes_node;
DROP INDEX IF EXISTS idx_k8s_cluster_nodes_cluster;
DROP TABLE IF EXISTS kubernetes_cluster_config;
DROP TABLE IF EXISTS kubernetes_cluster_nodes;
DROP TABLE IF EXISTS kubernetes_clusters;",
        irreversible: false,
    },
    SchemaDown {
        version: 51,
        sql: "DROP TABLE IF EXISTS kubernetes_pki;",
        irreversible: false,
    },
    SchemaDown {
        version: 52,
        sql: "DROP TABLE IF EXISTS ceph_dashboard_credentials;",
        irreversible: false,
    },
    SchemaDown {
        version: 53,
        sql: "ALTER TABLE kubernetes_pki DROP COLUMN etcd_node_certs_json;
ALTER TABLE kubernetes_pki DROP COLUMN etcd_node_peer_certs_json;",
        irreversible: false,
    },
    SchemaDown {
        version: 54,
        sql: "ALTER TABLE volumes DROP COLUMN host_node_id;
ALTER TABLE volumes DROP COLUMN fs_type;
ALTER TABLE volumes DROP COLUMN mount_point;",
        irreversible: false,
    },
    SchemaDown {
        version: 55,
        sql: "ALTER TABLE kubernetes_cluster_config DROP COLUMN local_provisioner_json;",
        irreversible: false,
    },
    SchemaDown {
        version: 56,
        sql: "ALTER TABLE kubernetes_cluster_config DROP COLUMN schedule_on_control_plane;",
        irreversible: false,
    },
    SchemaDown {
        version: 57,
        sql: "DROP TABLE IF EXISTS node_host_status;",
        irreversible: false,
    },
    // 59 repeats the version 57 table. Rolling back to 57 keeps it.
    SchemaDown {
        version: 59,
        sql: "",
        irreversible: false,
    },
    SchemaDown {
        version: 60,
        sql: "ALTER TABLE kubernetes_cluster_nodes DROP COLUMN pending_join;
ALTER TABLE kubernetes_cluster_nodes DROP COLUMN pending_removal;
ALTER TABLE kubernetes_cluster_nodes DROP COLUMN k8s_node_name;",
        irreversible: false,
    },
    SchemaDown {
        version: 61,
        sql: "ALTER TABLE vms DROP COLUMN labels;",
        irreversible: false,
    },
    SchemaDown {
        version: 62,
        sql: "ALTER TABLE node_gpus DROP COLUMN profiles;
ALTER TABLE node_gpus DROP COLUMN profile;
ALTER TABLE node_gpus DROP COLUMN parent;",
        irreversible: false,
    },
    SchemaDown {
        version: 63,
        sql: "DROP TABLE IF EXISTS cluster_leases;
DROP TABLE IF EXISTS ha_events;
DROP TABLE IF EXISTS node_ha;
DROP TABLE IF EXISTS vm_ha;",
        irreversible: false,
    },
    SchemaDown {
        version: 64,
        sql: "ALTER TABLE node_ha DROP COLUMN fence_vms;
ALTER TABLE node_ha DROP COLUMN fence_pending;",
        irreversible: false,
    },
];

#[cfg(test)]
mod tests {
    use super::SCHEMA_DOWNS;
    use crate::db::{Database, NodeRow, SCHEMA_VERSION};

    fn node(id: &str) -> NodeRow {
        NodeRow {
            id: id.into(),
            hostname: id.into(),
            address: "127.0.0.1:9091".into(),
            cpu_cores: 2,
            memory_bytes: 1024,
            status: "ready".into(),
            last_heartbeat: String::new(),
            gateway_interface: "br0".into(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".into(),
            disable_vxlan: false,
            approval_status: "approved".into(),
            cert_expiry_days: -1,
            luks_method: String::new(),
            dc_id: "DC1".into(),
        }
    }

    #[test]
    fn down_to_previous_drops_host_status_and_nodes_still_read() {
        let db = Database::open(":memory:").unwrap();
        assert_eq!(db.schema_version_for_test(), SCHEMA_VERSION);
        db.upsert_node(&node("n1")).unwrap();
        db.upsert_node_host_status(
            "n1",
            "0.5.0",
            "/run/booted",
            "/run/current",
            "nixos-generation",
            57,
            "abc",
        )
        .unwrap();
        assert!(db.get_node_host_status("n1").unwrap().is_some());
        db.migrate_down(56).unwrap();
        assert!(db.get_node_host_status("n1").is_err());
        assert_eq!(db.get_node("n1").unwrap().unwrap().hostname, "n1");
        db.migrate().unwrap();
        db.upsert_node_host_status("n1", "0.5.0", "", "", "nixos-generation", 57, "")
            .unwrap();
        assert!(db.get_node_host_status("n1").unwrap().is_some());
    }

    #[test]
    fn volume_rewrite_is_irreversible() {
        let step = SCHEMA_DOWNS.iter().find(|step| step.version == 39).unwrap();
        assert!(step.irreversible);
        let db = Database::open(":memory:").unwrap();
        let err = db.migrate_down(38).unwrap_err();
        assert!(err.contains("irreversible"));
    }
}
