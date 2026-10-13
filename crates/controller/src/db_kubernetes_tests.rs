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
}
