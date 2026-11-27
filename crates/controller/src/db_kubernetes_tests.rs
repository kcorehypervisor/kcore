// Comprehensive tests for Kubernetes cluster management

#[cfg(test)]
mod kubernetes_cluster_tests {
    use crate::db::{
        Database, KubernetesClusterConfigRow, KubernetesClusterNodeRow, KubernetesClusterRow,
        NodeRow,
    };

    fn ensure_node(db: &Database, id: &str) {
        db.upsert_node(&NodeRow {
            id: id.to_string(),
            hostname: id.to_string(),
            address: format!("{id}:9091"),
            cpu_cores: 4,
            memory_bytes: 8_000_000_000,
            status: "ready".to_string(),
            last_heartbeat: String::new(),
            gateway_interface: "enp6s0".to_string(),
            cpu_used: 0,
            memory_used: 0,
            storage_backend: "filesystem".to_string(),
            disable_vxlan: false,
            approval_status: "approved".to_string(),
            cert_expiry_days: 30,
            luks_method: "tpm2".to_string(),
            dc_id: "DC1".to_string(),
        })
        .expect("insert node");
    }

    #[test]
    fn test_upsert_kubernetes_cluster() {
        let db = Database::open(":memory:").expect("open db");

        let row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: "cluster is healthy".to_string(),
            ready_control_planes: 3,
            ready_workers: 2,
            created_at: String::new(),
            updated_at: String::new(),
        };

        db.upsert_kubernetes_cluster(&row).unwrap();

        let retrieved = db.get_kubernetes_cluster("test-cluster").unwrap().unwrap();
        assert_eq!(retrieved.name, "test-cluster");
        assert_eq!(retrieved.generation, 1);
        assert_eq!(retrieved.cluster_cidr, "10.244.0.0/16");
        assert_eq!(retrieved.phase, "ready");
        assert_eq!(retrieved.ready_control_planes, 3);
    }

    #[test]
    fn test_upsert_kubernetes_cluster_updates_generation() {
        let db = Database::open(":memory:").expect("open db");

        let row1 = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: "cluster is healthy".to_string(),
            ready_control_planes: 3,
            ready_workers: 2,
            created_at: String::new(),
            updated_at: String::new(),
        };

        db.upsert_kubernetes_cluster(&row1).unwrap();

        let row2 = KubernetesClusterRow {
            generation: 2,
            phase: "updating".to_string(),
            ..row1
        };

        db.upsert_kubernetes_cluster(&row2).unwrap();

        let retrieved = db.get_kubernetes_cluster("test-cluster").unwrap().unwrap();
        assert_eq!(retrieved.generation, 2);
        assert_eq!(retrieved.phase, "updating");
    }

    #[test]
    fn test_list_kubernetes_clusters() {
        let db = Database::open(":memory:").expect("open db");

        for i in 1..=3 {
            let row = KubernetesClusterRow {
                name: format!("cluster-{}", i),
                generation: 1,
                cluster_cidr: "10.244.0.0/16".to_string(),
                service_cidr: "10.96.0.0/12".to_string(),
                cluster_dns: "10.96.0.10".to_string(),
                api_server_endpoint: format!("192.168.40.{}:6443", 200 + i),
                cni: "cilium".to_string(),
                phase: "ready".to_string(),
                status_message: String::new(),
                ready_control_planes: 3,
                ready_workers: 0,
                created_at: String::new(),
                updated_at: String::new(),
            };
            db.upsert_kubernetes_cluster(&row).unwrap();
        }

        let clusters = db.list_kubernetes_clusters().unwrap();
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].name, "cluster-1");
        assert_eq!(clusters[2].name, "cluster-3");
    }

    #[test]
    fn test_delete_kubernetes_cluster() {
        let db = Database::open(":memory:").expect("open db");

        let row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 3,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };

        db.upsert_kubernetes_cluster(&row).unwrap();
        assert!(db.delete_kubernetes_cluster("test-cluster").unwrap());
        assert!(db.get_kubernetes_cluster("test-cluster").unwrap().is_none());
        assert!(!db.delete_kubernetes_cluster("test-cluster").unwrap());
    }

    #[test]
    fn test_kubernetes_cluster_nodes() {
        let db = Database::open(":memory:").expect("open db");

        // Create cluster first
        let cluster_row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_kubernetes_cluster(&cluster_row).unwrap();
        ensure_node(&db, "node-105");

        // Add node
        let node_row = KubernetesClusterNodeRow {
            cluster_name: "test-cluster".to_string(),
            node_id: "node-105".to_string(),
            role: "control-plane".to_string(),
            advertise_address: "192.168.40.105".to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: true,
            join_existing: false,
            kubelet_ready: true,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        };

        db.upsert_kubernetes_cluster_node(&node_row).unwrap();

        let nodes = db.list_kubernetes_cluster_nodes("test-cluster").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, "node-105");
        assert_eq!(nodes[0].role, "control-plane");
        assert!(nodes[0].initial_cluster);
        assert!(nodes[0].kubelet_ready);
    }

    #[test]
    fn test_kubernetes_cluster_config() {
        let db = Database::open(":memory:").expect("open db");

        // Create cluster first
        let cluster_row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_kubernetes_cluster(&cluster_row).unwrap();

        let config_row = KubernetesClusterConfigRow {
            cluster_name: "test-cluster".to_string(),
            ha_enabled: true,
            vip_address: "192.168.40.200".to_string(),
            vip_interface: "enp6s0".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: "192.168.40.220/29".to_string(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: String::new(),
        };

        db.upsert_kubernetes_cluster_config(&config_row).unwrap();

        let config = db
            .get_kubernetes_cluster_config("test-cluster")
            .unwrap()
            .unwrap();
        assert!(config.ha_enabled);
        assert_eq!(config.vip_address, "192.168.40.200");
        assert_eq!(config.cilium_mode, "native-routing");
        assert_eq!(config.cilium_mtu, 1500);
        assert!(config.cilium_auto_direct_routes);
        assert!(config.local_provisioner_json.is_empty());

        let stored = r#"{"enabled":true,"storageClassName":"local-path","volumes":[]}"#;
        let mut with_local = config_row;
        with_local.local_provisioner_json = stored.to_string();
        db.upsert_kubernetes_cluster_config(&with_local).unwrap();
        let config = db
            .get_kubernetes_cluster_config("test-cluster")
            .unwrap()
            .unwrap();
        assert_eq!(config.local_provisioner_json, stored);
        assert!(!config.schedule_on_control_plane);

        let mut schedulable = with_local;
        schedulable.schedule_on_control_plane = true;
        db.upsert_kubernetes_cluster_config(&schedulable).unwrap();
        let config = db
            .get_kubernetes_cluster_config("test-cluster")
            .unwrap()
            .unwrap();
        assert!(config.schedule_on_control_plane);
        assert_eq!(config.local_provisioner_json, stored);
    }

    #[test]
    fn test_delete_cluster_cascades_to_nodes_and_config() {
        let db = Database::open(":memory:").expect("open db");

        // Create cluster
        let cluster_row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_kubernetes_cluster(&cluster_row).unwrap();
        ensure_node(&db, "node-105");

        // Add node
        let node_row = KubernetesClusterNodeRow {
            cluster_name: "test-cluster".to_string(),
            node_id: "node-105".to_string(),
            role: "control-plane".to_string(),
            advertise_address: "192.168.40.105".to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: true,
            join_existing: false,
            kubelet_ready: false,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        };
        db.upsert_kubernetes_cluster_node(&node_row).unwrap();

        // Add config
        let config_row = KubernetesClusterConfigRow {
            cluster_name: "test-cluster".to_string(),
            ha_enabled: true,
            vip_address: "192.168.40.200".to_string(),
            vip_interface: "enp6s0".to_string(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: "192.168.40.220/29".to_string(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: String::new(),
        };
        db.upsert_kubernetes_cluster_config(&config_row).unwrap();

        // Verify they exist
        assert_eq!(
            db.list_kubernetes_cluster_nodes("test-cluster")
                .unwrap()
                .len(),
            1
        );
        assert!(db
            .get_kubernetes_cluster_config("test-cluster")
            .unwrap()
            .is_some());

        // Delete cluster
        assert!(db.delete_kubernetes_cluster("test-cluster").unwrap());

        // Verify nodes and config are gone (cascade)
        assert_eq!(
            db.list_kubernetes_cluster_nodes("test-cluster")
                .unwrap()
                .len(),
            0
        );
        assert!(db
            .get_kubernetes_cluster_config("test-cluster")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_multiple_control_plane_nodes() {
        let db = Database::open(":memory:").expect("open db");

        let cluster_row = KubernetesClusterRow {
            name: "ha-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 3,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_kubernetes_cluster(&cluster_row).unwrap();

        for (i, node_id) in ["node-105", "node-107", "node-119"].iter().enumerate() {
            ensure_node(&db, node_id);
            let node_row = KubernetesClusterNodeRow {
                cluster_name: "ha-cluster".to_string(),
                node_id: node_id.to_string(),
                role: "control-plane".to_string(),
                advertise_address: format!("192.168.40.{}", 105 + i * 2),
                iface: "enp6s0".to_string(),
                initial_cluster: i == 0,
                join_existing: i > 0,
                kubelet_ready: true,
                pending_join: false,
                pending_removal: false,
                k8s_node_name: String::new(),
            };
            db.upsert_kubernetes_cluster_node(&node_row).unwrap();
        }

        let nodes = db.list_kubernetes_cluster_nodes("ha-cluster").unwrap();
        assert_eq!(nodes.len(), 3);

        let initial_nodes: Vec<_> = nodes.iter().filter(|n| n.initial_cluster).collect();
        assert_eq!(initial_nodes.len(), 1);
        assert_eq!(initial_nodes[0].node_id, "node-105");

        let joining_nodes: Vec<_> = nodes.iter().filter(|n| n.join_existing).collect();
        assert_eq!(joining_nodes.len(), 2);
    }

    #[test]
    fn test_update_node_readiness() {
        let db = Database::open(":memory:").expect("open db");

        let cluster_row = KubernetesClusterRow {
            name: "test-cluster".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        };
        db.upsert_kubernetes_cluster(&cluster_row).unwrap();
        ensure_node(&db, "node-105");

        let node_row = KubernetesClusterNodeRow {
            cluster_name: "test-cluster".to_string(),
            node_id: "node-105".to_string(),
            role: "control-plane".to_string(),
            advertise_address: "192.168.40.105".to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: true,
            join_existing: false,
            kubelet_ready: false,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        };
        db.upsert_kubernetes_cluster_node(&node_row).unwrap();

        let nodes = db.list_kubernetes_cluster_nodes("test-cluster").unwrap();
        assert!(!nodes[0].kubelet_ready);

        let updated_node = KubernetesClusterNodeRow {
            kubelet_ready: true,
            ..node_row
        };
        db.upsert_kubernetes_cluster_node(&updated_node).unwrap();

        let nodes = db.list_kubernetes_cluster_nodes("test-cluster").unwrap();
        assert!(nodes[0].kubelet_ready);
    }

    #[test]
    fn new_members_inherit_the_cluster_version_and_membership_upserts_keep_upgrade_state() {
        let db = Database::open(":memory:").expect("open db");
        db.upsert_kubernetes_cluster(&KubernetesClusterRow {
            name: "c".to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "192.168.40.200:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        db.upsert_kubernetes_cluster_config(&KubernetesClusterConfigRow {
            cluster_name: "c".to_string(),
            ha_enabled: false,
            vip_address: String::new(),
            vip_interface: String::new(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: String::new(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: "1.36.6".to_string(),
        })
        .unwrap();
        ensure_node(&db, "node-105");
        let member = KubernetesClusterNodeRow {
            cluster_name: "c".to_string(),
            node_id: "node-105".to_string(),
            role: "worker".to_string(),
            advertise_address: "192.168.40.105".to_string(),
            iface: "enp6s0".to_string(),
            initial_cluster: false,
            join_existing: true,
            kubelet_ready: false,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        };
        db.upsert_kubernetes_cluster_node(&member).unwrap();
        assert_eq!(
            db.kubernetes_node_version("c", "node-105")
                .unwrap()
                .as_deref(),
            Some("1.36.6")
        );

        let mut upgrade = db.list_kubernetes_node_upgrades("c").unwrap().remove(0);
        upgrade.kubernetes_version = "1.37.2".to_string();
        upgrade.upgrade_state = "applying".to_string();
        upgrade.upgrade_apply_id = "k8s-apply-1".to_string();
        upgrade.upgrade_cordoned = true;
        db.set_kubernetes_node_upgrade("c", &upgrade).unwrap();

        db.upsert_kubernetes_cluster_node(&KubernetesClusterNodeRow {
            kubelet_ready: true,
            ..member
        })
        .unwrap();
        assert_eq!(
            db.list_kubernetes_node_upgrades("c").unwrap(),
            vec![upgrade]
        );
        assert_eq!(db.kubernetes_node_version("c", "node-404").unwrap(), None);
    }

    fn seed_cluster(db: &Database, name: &str) {
        db.upsert_kubernetes_cluster(&KubernetesClusterRow {
            name: name.to_string(),
            generation: 1,
            cluster_cidr: "10.244.0.0/16".to_string(),
            service_cidr: "10.96.0.0/12".to_string(),
            cluster_dns: "10.96.0.10".to_string(),
            api_server_endpoint: "10.0.0.1:6443".to_string(),
            cni: "cilium".to_string(),
            phase: "ready".to_string(),
            status_message: String::new(),
            ready_control_planes: 0,
            ready_workers: 0,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .unwrap();
        db.upsert_kubernetes_cluster_config(&KubernetesClusterConfigRow {
            cluster_name: name.to_string(),
            ha_enabled: false,
            vip_address: String::new(),
            vip_interface: String::new(),
            cilium_mode: "native-routing".to_string(),
            cilium_mtu: 1500,
            cilium_tunnel: "disabled".to_string(),
            cilium_auto_direct_routes: true,
            cilium_lb_ip_pools: String::new(),
            local_provisioner_json: String::new(),
            schedule_on_control_plane: false,
            kubernetes_version: String::new(),
        })
        .unwrap();
    }

    #[test]
    fn schema_78_adds_hardening_ops_columns_idempotently() {
        let db = Database::open(":memory:").expect("open db");
        {
            let conn = db.lock_conn().unwrap();
            for (table, column) in [
                ("kubernetes_cluster_nodes", "rendered_hash"),
                ("kubernetes_cluster_config", "auto_reconcile"),
                ("kubernetes_cluster_config", "previous_version"),
                ("kubernetes_cluster_config", "pre_upgrade_snapshot_id"),
                ("kubernetes_cluster_config", "backup_json"),
                ("kubernetes_cluster_config", "repair_json"),
                ("kubernetes_cluster_config", "storage_json"),
                ("kubernetes_cluster_config", "restore_json"),
                ("kubernetes_cluster_config", "oidc_json"),
                ("kubernetes_cluster_config", "cert_json"),
                ("kubernetes_cluster_config", "default_deny_label"),
                ("kubernetes_cluster_config", "encryption_json"),
                ("kubernetes_pki", "leaf_not_after"),
                ("kubernetes_etcd_snapshots", "trigger"),
            ] {
                let present: i64 = conn
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"
                        ),
                        [column],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(present, 1, "{table}.{column}");
            }
            // Rewind and migrate again: the guarded ALTERs must not fail.
            conn.execute_batch(
                "DELETE FROM schema_version; INSERT INTO schema_version (version) VALUES (74);",
            )
            .unwrap();
        }
        db.migrate().expect("re-run 75 through 79");
        assert_eq!(db.schema_version_for_test(), crate::db::SCHEMA_VERSION);
        assert_eq!(crate::db::SCHEMA_VERSION, 80);
    }

    #[test]
    fn schema_79_adds_dual_ca_columns_to_a_78_database() {
        let db = Database::open(":memory:").expect("open db");
        seed_cluster(&db, "c");
        let pki = crate::kubernetes_pki::generate_kubernetes_pki(
            "c",
            "10.0.0.1:6443",
            "10.96.0.0/12",
            &[crate::kubernetes_pki::NodeInfo {
                node_id: "n1".to_string(),
                hostname: "n1".to_string(),
                ip_address: "10.0.0.2".to_string(),
            }],
        )
        .unwrap();
        db.store_kubernetes_pki("c", &pki).unwrap();
        {
            let conn = db.lock_conn().unwrap();
            conn.execute_batch(
                "ALTER TABLE kubernetes_pki DROP COLUMN previous_ca_cert;
                 ALTER TABLE kubernetes_pki DROP COLUMN previous_ca_key;
                 ALTER TABLE kubernetes_pki DROP COLUMN previous_etcd_ca_cert;
                 ALTER TABLE kubernetes_pki DROP COLUMN previous_etcd_ca_key;
                 ALTER TABLE kubernetes_cluster_config DROP COLUMN ca_roll_pending;
                 DELETE FROM schema_version; INSERT INTO schema_version (version) VALUES (78);",
            )
            .unwrap();
        }
        db.migrate().expect("migrate 78 -> 79");
        db.migrate().expect("re-run is a no-op");

        let loaded = db.load_kubernetes_pki("c").unwrap().unwrap();
        assert_eq!(loaded.ca_cert, pki.ca_cert);
        assert!(loaded.previous_ca_cert.is_empty());
        assert_eq!(
            db.get_kubernetes_cluster_ops("c")
                .unwrap()
                .unwrap()
                .ca_roll_pending,
            ""
        );

        let mut rotating = loaded.clone();
        crate::kubernetes_pki::begin_ca_rotation(&mut rotating, true).unwrap();
        db.store_kubernetes_pki("c", &rotating).unwrap();
        db.set_kubernetes_cluster_ca_roll_pending("c", "begin")
            .unwrap();
        let back = db.load_kubernetes_pki("c").unwrap().unwrap();
        assert_eq!(back.previous_ca_cert, pki.ca_cert);
        assert_eq!(back.previous_ca_key, pki.ca_key);
        assert_eq!(back.previous_etcd_ca_cert, pki.etcd_ca_cert);
        assert_eq!(back.previous_etcd_ca_key, pki.etcd_ca_key);
        assert_eq!(back.ca_cert, rotating.ca_cert);
        assert_eq!(
            db.get_kubernetes_cluster_ops("c")
                .unwrap()
                .unwrap()
                .ca_roll_pending,
            "begin"
        );
    }

    #[test]
    fn ops_defaults_and_setters_round_trip_without_touching_the_spec() {
        let db = Database::open(":memory:").expect("open db");
        seed_cluster(&db, "c");
        let ops = db.get_kubernetes_cluster_ops("c").unwrap().unwrap();
        assert!(ops.auto_reconcile);
        assert_eq!(ops.previous_version, "");
        assert_eq!(ops.backup_json, "");

        db.set_kubernetes_cluster_auto_reconcile("c", false)
            .unwrap();
        db.set_kubernetes_cluster_previous_version("c", "1.35.2")
            .unwrap();
        db.set_kubernetes_cluster_pre_upgrade_snapshot_id("c", "snap-1")
            .unwrap();
        db.set_kubernetes_cluster_backup_json("c", "{}").unwrap();
        db.set_kubernetes_cluster_repair_json("c", "{}").unwrap();
        // A spec re-apply upserts the config row; ops state must survive it.
        seed_cluster(&db, "c");
        let ops = db.get_kubernetes_cluster_ops("c").unwrap().unwrap();
        assert!(!ops.auto_reconcile);
        assert_eq!(ops.previous_version, "1.35.2");
        assert_eq!(ops.pre_upgrade_snapshot_id, "snap-1");
        assert_eq!(ops.backup_json, "{}");
        assert_eq!(ops.repair_json, "{}");
        assert!(db.get_kubernetes_cluster_ops("nope").unwrap().is_none());
    }

    #[test]
    fn rendered_hash_survives_membership_upserts() {
        let db = Database::open(":memory:").expect("open db");
        seed_cluster(&db, "c");
        ensure_node(&db, "n1");
        let member = KubernetesClusterNodeRow {
            cluster_name: "c".to_string(),
            node_id: "n1".to_string(),
            role: "worker".to_string(),
            advertise_address: "10.0.0.5".to_string(),
            iface: "br0".to_string(),
            initial_cluster: false,
            join_existing: true,
            kubelet_ready: false,
            pending_join: false,
            pending_removal: false,
            k8s_node_name: String::new(),
        };
        db.upsert_kubernetes_cluster_node(&member).unwrap();
        assert_eq!(
            db.list_kubernetes_node_rendered_hashes("c").unwrap()["n1"],
            ""
        );
        db.set_kubernetes_node_rendered_hash("c", "n1", "abc")
            .unwrap();
        db.upsert_kubernetes_cluster_node(&KubernetesClusterNodeRow {
            kubelet_ready: true,
            ..member
        })
        .unwrap();
        assert_eq!(
            db.list_kubernetes_node_rendered_hashes("c").unwrap()["n1"],
            "abc"
        );
    }

    #[test]
    fn etcd_snapshots_list_newest_first_and_delete() {
        use crate::db::KubernetesEtcdSnapshotRow;
        let db = Database::open(":memory:").expect("open db");
        let row = |id: &str, at: &str| KubernetesEtcdSnapshotRow {
            id: id.to_string(),
            cluster_name: "c".to_string(),
            created_at: at.to_string(),
            sha256: "00".to_string(),
            size_bytes: 10,
            path: format!("/tmp/{id}"),
            trigger: "manual".to_string(),
        };
        db.insert_kubernetes_etcd_snapshot(&row("a", "2026-01-01T00:00:00Z"))
            .unwrap();
        db.insert_kubernetes_etcd_snapshot(&row("b", "2026-01-02T00:00:00Z"))
            .unwrap();
        let ids: Vec<_> = db
            .list_kubernetes_etcd_snapshots("c")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["b", "a"]);
        assert!(db.get_kubernetes_etcd_snapshot("a").unwrap().is_some());
        assert!(db.delete_kubernetes_etcd_snapshot("a").unwrap());
        assert!(!db.delete_kubernetes_etcd_snapshot("a").unwrap());
        assert_eq!(db.list_kubernetes_etcd_snapshots("other").unwrap().len(), 0);
    }
}
